use std::path::PathBuf;

use clap::Parser;
use serde::Serialize;

use anyhow::Context;
use codexify::config::{
    Cli, CliCommand, ProjectsCommand, ProjectsListArgs, ServiceCommand, config_path_for_quickstart,
    config_path_for_service, load_config, load_project_catalog_for_cli,
};
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

    println!("Access root: {}", output.access_root);
    println!(
        "Projects: showing {} of {} matches",
        output.projects.len(),
        output.total
    );
    if output.projects.is_empty() {
        println!("  No selectable projects matched.");
    }
    for project in &output.projects {
        println!("- {}", project.name);
        println!("  selector: {}", project.selector);
        println!("  trust: {}", trust_name(project.trust_level));
        println!(
            "  sources: {}",
            project
                .sources
                .iter()
                .copied()
                .map(source_name)
                .collect::<Vec<_>>()
                .join(", ")
        );
        if !project.aliases.is_empty() {
            println!("  aliases: {}", project.aliases.join(", "));
        }
        if let Some(description) = &project.description {
            println!("  description: {description}");
        }
    }
    if !output.warnings.is_empty() {
        println!("Warnings:");
        for warning in &output.warnings {
            println!("- {warning}");
        }
    }
    if args.show_skipped && !catalog.diagnostics.is_empty() {
        println!("Detailed diagnostics:");
        for diagnostic in &catalog.diagnostics {
            println!("- {}", diagnostic.render_local());
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    // We build reqwest with `rustls-no-provider`, so a rustls crypto provider
    // must be installed process-wide before any HTTP client is built. Every
    // client factory installs it too, but do it once up front so any client
    // constructed by a dependency also finds a provider.
    codexify::tls::ensure_crypto_provider();

    if let Err(error) = run(Cli::parse()).await {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

async fn run(mut cli: Cli) -> anyhow::Result<()> {
    if let Some(command) = cli.command.take() {
        match command {
            CliCommand::Projects {
                command: ProjectsCommand::List(args),
            } => {
                return run_projects_list(&cli, &args).map_err(anyhow::Error::msg);
            }
            CliCommand::Service { command } => {
                return run_service(&cli, command).await;
            }
            CliCommand::MigrateLegacyInstall => {
                let outcome = legacy_migration::migrate_default_home()?;
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
    start_http_server(config)
        .await
        .context("start Codexify server")
}

async fn run_service(cli: &Cli, command: ServiceCommand) -> anyhow::Result<()> {
    match command {
        ServiceCommand::Install => {
            let config = config_path_for_service(cli).map_err(anyhow::Error::msg)?;
            service::install(&config)
        }
        ServiceCommand::Enable => service::enable(),
        ServiceCommand::Disable => service::disable(),
        ServiceCommand::Remove => service::remove(),
        ServiceCommand::Logs(args) => service::print_logs(args.follow).await,
        ServiceCommand::Run => {
            let config = config_path_for_service(cli).map_err(anyhow::Error::msg)?;
            service::run_supervisor(config).await
        }
    }
}
