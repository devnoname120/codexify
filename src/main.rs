use std::io::Write as _;
use std::path::PathBuf;

use clap::Parser;
use serde::Serialize;

use anyhow::Context;
use codexify::config::{
    BrowserCommand, Cli, CliCommand, ProjectsCommand, ProjectsListArgs, ServiceCommand,
    config_path_for_quickstart, config_path_for_service, load_config, load_config_quiet,
    load_config_quiet_read_only, load_project_catalog_for_cli,
};
use codexify::doctor;
use codexify::legacy_migration;
use codexify::logging;
use codexify::project_catalog::{
    MAX_PROJECT_LIMIT, ProjectCatalogDiagnostic, ProjectListOutput, ProjectSource,
    ProjectTrustLevel,
};
use codexify::quickstart;
use codexify::server::start_http_server;
use codexify::service;

#[derive(Serialize)]
struct CliProjectListOutput {
    #[serde(flatten)]
    list: ProjectListOutput,
    diagnostics: Vec<ProjectCatalogDiagnostic>,
}

fn source_name(source: ProjectSource) -> &'static str {
    match source {
        ProjectSource::CodexConfig => "codex_config",
        ProjectSource::ExplicitMetadata => "explicit_metadata",
    }
}

fn trust_name(trust: Option<ProjectTrustLevel>) -> &'static str {
    match trust {
        Some(ProjectTrustLevel::Trusted) => "trusted",
        Some(ProjectTrustLevel::Untrusted) => "untrusted",
        None => "explicit",
    }
}

fn run_projects_list(cli: &Cli, args: &ProjectsListArgs) -> Result<(), String> {
    if !(1..=MAX_PROJECT_LIMIT).contains(&args.limit) {
        return Err(format!("--limit must be between 1 and {MAX_PROJECT_LIMIT}"));
    }

    let catalog = load_project_catalog_for_cli(cli)?;
    let output = catalog.list(args.query.as_deref(), args.limit);
    if args.json {
        let output = CliProjectListOutput {
            list: output,
            diagnostics: if args.show_skipped {
                catalog.diagnostics
            } else {
                Vec::new()
            },
        };
        println!(
            "{}",
            serde_json::to_string_pretty(&output)
                .map_err(|error| format!("failed to serialize project catalogue: {error}"))?
        );
        return Ok(());
    }

    use codexify::terminal::{ACCENT, EMPHASIS, HEADING, MUTED, SUCCESS, WARNING, paint};

    let mut rendered = format!(
        "{} {}\n{} showing {} of {} matches\n",
        paint(EMPHASIS, "Access root:"),
        paint(ACCENT, &output.access_root),
        paint(HEADING, "Projects:"),
        output.projects.len(),
        output.total
    );
    if output.projects.is_empty() {
        rendered.push_str(&format!(
            "  {}\n",
            paint(MUTED, "No selectable projects matched.")
        ));
    }
    for project in &output.projects {
        let trust = trust_name(project.trust_level);
        let trust_style = match project.trust_level {
            Some(ProjectTrustLevel::Trusted) => SUCCESS,
            Some(ProjectTrustLevel::Untrusted) => WARNING,
            None => MUTED,
        };
        rendered.push_str(&format!("- {}\n", paint(EMPHASIS, &project.name)));
        rendered.push_str(&format!(
            "  {} {}\n",
            paint(MUTED, "selector:"),
            paint(ACCENT, &project.selector)
        ));
        rendered.push_str(&format!(
            "  {} {}\n",
            paint(MUTED, "trust:"),
            paint(trust_style, trust)
        ));
        rendered.push_str(&format!(
            "  {} {}\n",
            paint(MUTED, "sources:"),
            project
                .sources
                .iter()
                .copied()
                .map(source_name)
                .collect::<Vec<_>>()
                .join(", ")
        ));
        if !project.aliases.is_empty() {
            rendered.push_str(&format!(
                "  {} {}\n",
                paint(MUTED, "aliases:"),
                project.aliases.join(", ")
            ));
        }
        if let Some(description) = &project.description {
            rendered.push_str(&format!(
                "  {} {}\n",
                paint(MUTED, "description:"),
                description
            ));
        }
    }
    if !output.warnings.is_empty() {
        rendered.push_str(&format!("{}\n", paint(WARNING, "Warnings:")));
        for warning in &output.warnings {
            rendered.push_str(&format!("- {}\n", paint(WARNING, warning)));
        }
    }
    if args.show_skipped && !catalog.diagnostics.is_empty() {
        rendered.push_str(&format!("{}\n", paint(WARNING, "Detailed diagnostics:")));
        for diagnostic in &catalog.diagnostics {
            rendered.push_str(&format!("- {}\n", paint(MUTED, diagnostic.render_local())));
        }
    }
    codexify::terminal::write_stdout(&rendered)
        .map_err(|error| format!("write project catalogue: {error}"))?;
    Ok(())
}

fn main() {
    if std::env::args_os().nth(1).as_deref()
        == Some(std::ffi::OsStr::new(codexify::tools::grep::WORKER_ARGUMENT))
    {
        if let Err(error) = codexify::tools::grep::run_worker() {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    run_main();
}

#[tokio::main]
async fn run_main() {
    // We build reqwest with `rustls-no-provider`, so a rustls crypto provider
    // must be installed process-wide before any HTTP client is built. Every
    // client factory installs it too, but do it once up front so any client
    // constructed by a dependency also finds a provider.
    codexify::tls::ensure_crypto_provider();

    if let Err(error) = run(Cli::parse()).await {
        let message = format!(
            "{} {error:#}\n",
            codexify::terminal::paint(codexify::terminal::FAILURE, "Error:")
        );
        if codexify::terminal::write_stderr(&message).is_err() {
            eprintln!("Error: {error:#}");
        }
        std::process::exit(1);
    }
}

async fn run(mut cli: Cli) -> anyhow::Result<()> {
    if let Some(command) = cli.command.take() {
        match command {
            CliCommand::Browser { command } => {
                return run_browser(cli, command).await;
            }
            CliCommand::Chat => {
                println!("{}", codexify::owner_chat::dashboard_url().await?);
                return Ok(());
            }
            CliCommand::Config(args) => {
                return codexify::config_cli::run(&cli, args);
            }
            CliCommand::Update => {
                let config = load_config_quiet_read_only(cli).map_err(anyhow::Error::msg)?;
                codexify::terminal::write_stdout("Checking for a verified Codexify update…\n")?;
                let receipt = codexify::self_update::trigger_for_cli(config.port).await?;
                use codexify::self_update::SelfUpdateStatus;
                let message = match receipt.status {
                    SelfUpdateStatus::UpToDate => format!(
                        "Codexify {} is already up to date.\n",
                        receipt.current_version
                    ),
                    SelfUpdateStatus::AheadOfLatest => format!(
                        "Codexify {} is newer than the latest release ({}). No update scheduled.\n",
                        receipt.current_version, receipt.target_version
                    ),
                    SelfUpdateStatus::Scheduled => format!(
                        "Codexify {} has been downloaded and verified. Update {} is scheduled.\n{}\nLog: {}\n",
                        receipt.target_version,
                        receipt.update_id.as_deref().unwrap_or("<unknown>"),
                        if receipt.service_restart {
                            "The running background service will restart after the replacement."
                        } else if cfg!(windows) {
                            "Close any foreground Codexify server so its executable can be replaced."
                        } else {
                            "The installed executable will be replaced shortly. Restart any foreground Codexify process to use the new version."
                        },
                        receipt.log_path
                    ),
                };
                codexify::terminal::write_stdout(&message)?;
                return Ok(());
            }
            CliCommand::Doctor(args) => {
                let report = doctor::run(cli).await;
                let output = if args.json {
                    format!("{}\n", serde_json::to_string_pretty(&report)?)
                } else {
                    report.render_terminal()
                };
                codexify::terminal::write_stdout(&output)?;
                if !report.ok {
                    std::process::exit(1);
                }
                return Ok(());
            }
            CliCommand::Projects {
                command: ProjectsCommand::List(args),
            } => {
                return run_projects_list(&cli, &args).map_err(anyhow::Error::msg);
            }
            CliCommand::Service { command } => {
                return run_service(&cli, command).await;
            }
            CliCommand::MigrateLegacyInstall => {
                let outcome = legacy_migration::migrate_default_home_with_work_dir(
                    cli.work_dir.as_deref().map(std::path::Path::new),
                )?;
                if outcome.found {
                    println!(
                        "Migrated Codex Free state to ~/.codexify ({} config fields, {} state entries).",
                        outcome.config_fields_added, outcome.moved_entries
                    );
                    if outcome.config_conflicts > 0 {
                        println!(
                            "Kept {} existing Codexify config value(s) instead of conflicting legacy values.",
                            outcome.config_conflicts
                        );
                    }
                    for warning in outcome.warnings {
                        eprintln!("Warning: {warning}");
                    }
                }
                return Ok(());
            }
            CliCommand::Quickstart => {
                logging::init(cli.verbose);
                let config = config_path_for_quickstart(&cli).map_err(anyhow::Error::msg)?;
                let service_installed = service::is_installed().unwrap_or_else(|error| {
                    eprintln!("Warning: could not determine service state: {error:#}");
                    false
                });
                let args = quickstart::QuickstartArgs {
                    config: config.path,
                    config_explicit: config.explicit,
                    work_dir: cli.work_dir.clone().map(PathBuf::from),
                    service_installed,
                };
                let outcome = quickstart::run(args)?;
                if outcome.restart_service {
                    return service::install(&outcome.config_path);
                }
                if !outcome.start_server {
                    return Ok(());
                }
                cli.work_dir = Some(outcome.work_dir.to_string_lossy().into_owned());
            }
        }
    } else {
        logging::init(cli.verbose);
    }

    let config = load_config(cli).map_err(anyhow::Error::msg)?;
    let result = start_http_server(config)
        .await
        .context("start Codexify server");
    tracing::info!(
        process_id = std::process::id(),
        success = result.is_ok(),
        "server runtime returned"
    );
    result
}

async fn run_browser(cli: Cli, command: BrowserCommand) -> anyhow::Result<()> {
    use codexify::agent_automation::{
        BrowserCompanionControl, BrowserCompanionManager, BrowserLifecycleState,
    };

    let config = if matches!(
        command,
        BrowserCommand::Status(_) | BrowserCommand::StopForRestart
    ) {
        load_config_quiet_read_only(cli)
    } else {
        load_config_quiet(cli)
    }
    .map_err(anyhow::Error::msg)?;
    let manager = BrowserCompanionManager::from_config(&config.markdown_chat.automation)
        .map_err(anyhow::Error::msg)?;
    match command {
        BrowserCommand::Install => {
            let receipt = manager.install().await.map_err(anyhow::Error::msg)?;
            codexify::terminal::write_stdout(&format!(
                "Installed browser companion {} for {} at {}\n",
                receipt.version,
                receipt.platform,
                receipt.runtime_dir.display()
            ))?;
        }
        BrowserCommand::Start => {
            BrowserCompanionControl::new(&config.markdown_chat.automation)
                .map_err(anyhow::Error::msg)?
                .start(&tokio_util::sync::CancellationToken::new())
                .await
                .map_err(anyhow::Error::msg)?;
            let status = manager.status().await;
            codexify::terminal::write_stdout(&render_browser_status(&status))?;
        }
        BrowserCommand::Stop => {
            let status = BrowserCompanionControl::new(&config.markdown_chat.automation)
                .map_err(anyhow::Error::msg)?
                .stop(&tokio_util::sync::CancellationToken::new())
                .await
                .map_err(anyhow::Error::msg)?;
            codexify::terminal::write_stdout(&render_browser_status(&status))?;
        }
        BrowserCommand::StopForRestart => {
            BrowserCompanionControl::new(&config.markdown_chat.automation)
                .map_err(anyhow::Error::msg)?
                .stop_for_restart()
                .await
                .map_err(anyhow::Error::msg)?;
            codexify::terminal::write_stdout(&render_browser_status(&manager.status().await))?;
        }
        BrowserCommand::Login => {
            BrowserCompanionControl::new(&config.markdown_chat.automation)
                .map_err(anyhow::Error::msg)?
                .start(&tokio_util::sync::CancellationToken::new())
                .await
                .map_err(anyhow::Error::msg)?;
            let status = manager.login().await.map_err(anyhow::Error::msg)?;
            codexify::terminal::write_stdout(&render_browser_status(&status))?;
        }
        BrowserCommand::Reveal => {
            BrowserCompanionControl::new(&config.markdown_chat.automation)
                .map_err(anyhow::Error::msg)?
                .start(&tokio_util::sync::CancellationToken::new())
                .await
                .map_err(anyhow::Error::msg)?;
            let status = manager.reveal().await.map_err(anyhow::Error::msg)?;
            codexify::terminal::write_stdout(&render_browser_status(&status))?;
        }
        BrowserCommand::Status(args) => {
            let status = manager.status().await;
            if args.json {
                codexify::terminal::write_stdout(&format!(
                    "{}\n",
                    serde_json::to_string_pretty(&status)?
                ))?;
            } else {
                codexify::terminal::write_stdout(&render_browser_status(&status))?;
            }
            std::io::stdout().flush()?;
            let exit_code = match status.state {
                BrowserLifecycleState::Ready | BrowserLifecycleState::Busy => 0,
                BrowserLifecycleState::Stopped => 3,
                BrowserLifecycleState::Disabled | BrowserLifecycleState::NotInstalled => 4,
                BrowserLifecycleState::Incompatible
                | BrowserLifecycleState::AuthenticationRequired
                | BrowserLifecycleState::AccountChanged
                | BrowserLifecycleState::Unavailable => 1,
            };
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
        }
    }
    Ok(())
}

fn render_browser_status(status: &codexify::agent_automation::BrowserCompanionStatus) -> String {
    use codexify::terminal::{ACCENT, EMPHASIS, FAILURE, MUTED, SUCCESS, WARNING, paint};

    let (label, style) = match status.state {
        codexify::agent_automation::BrowserLifecycleState::Ready => ("ready", SUCCESS),
        codexify::agent_automation::BrowserLifecycleState::Busy => ("busy", ACCENT),
        codexify::agent_automation::BrowserLifecycleState::Stopped => ("stopped", WARNING),
        codexify::agent_automation::BrowserLifecycleState::Disabled => ("disabled", MUTED),
        codexify::agent_automation::BrowserLifecycleState::NotInstalled => {
            ("not installed", WARNING)
        }
        codexify::agent_automation::BrowserLifecycleState::AuthenticationRequired => {
            ("authentication required", WARNING)
        }
        codexify::agent_automation::BrowserLifecycleState::AccountChanged => {
            ("account changed", WARNING)
        }
        codexify::agent_automation::BrowserLifecycleState::Incompatible => {
            ("incompatible", FAILURE)
        }
        codexify::agent_automation::BrowserLifecycleState::Unavailable => ("unavailable", FAILURE),
    };
    let mut output = format!(
        "{} {}\n{} {}\n{} {}\n",
        paint(EMPHASIS, "Browser companion:"),
        paint(style, label),
        paint(MUTED, "Version:"),
        status.version,
        paint(MUTED, "Platform:"),
        status.platform
    );
    if let Some(path) = &status.runtime_dir {
        output.push_str(&format!(
            "{} {}\n",
            paint(MUTED, "Runtime:"),
            path.display()
        ));
    }
    output.push_str(&format!(
        "{} {}\n",
        paint(MUTED, "Log:"),
        status.log_path.display()
    ));
    if let Some(detail) = &status.detail {
        output.push_str(&format!("{}\n", paint(style, detail)));
    }
    output
}

async fn run_service(cli: &Cli, command: ServiceCommand) -> anyhow::Result<()> {
    match command {
        ServiceCommand::Install => {
            let config = config_path_for_service(cli).map_err(anyhow::Error::msg)?;
            service::install(&config)
        }
        ServiceCommand::Start => service::start(),
        ServiceCommand::Stop => service::stop(),
        ServiceCommand::Restart => service::restart(),
        ServiceCommand::Enable => service::enable(),
        ServiceCommand::Disable => service::disable(),
        ServiceCommand::Remove => service::remove(),
        ServiceCommand::Status(args) => {
            let status = service::status()?;
            if args.json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                codexify::terminal::write_stdout(&status.render_terminal())?;
            }
            std::io::stdout().flush()?;
            let exit_code = status.exit_code();
            if exit_code != 0 {
                std::process::exit(exit_code);
            }
            Ok(())
        }
        ServiceCommand::Logs(args) => service::print_logs(args.follow).await,
        ServiceCommand::Run => {
            let config = config_path_for_service(cli).map_err(anyhow::Error::msg)?;
            service::run_supervisor(config).await
        }
        ServiceCommand::WaitReady(args) => {
            service::wait_ready(args.port, std::time::Duration::from_millis(args.timeout_ms)).await
        }
    }
}
