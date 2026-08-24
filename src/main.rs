use std::path::PathBuf;

use clap::Parser;
use serde::Serialize;

use anyhow::Context;
use codexify::config::{
    Cli, CliCommand, ProjectsCommand, ProjectsListArgs, load_config, load_project_catalog_for_cli,
};
use codexify::project_catalog::{
    MAX_PROJECT_LIMIT, ProjectCatalogDiagnostic, ProjectListOutput, ProjectSource,
    ProjectTrustLevel,
};
use codexify::quickstart;
use codexify::server::start_http_server;

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
    let cli = Cli::parse();
    if let Some(CliCommand::Projects {
        command: ProjectsCommand::List(args),
    }) = &cli.command
    {
        if let Err(error) = run_projects_list(&cli, args) {
            eprintln!("Error: {error}");
            std::process::exit(1);
        }
        return;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    if let Err(error) = run().await {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let mut cli = Cli::parse();
    if let Some(command) = cli.command.take() {
        match command {
            CliCommand::Quickstart => {
                let args = quickstart::QuickstartArgs {
                    config: cli
                        .config
                        .clone()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("codex.config.json")),
                    work_dir: cli.work_dir.clone().map(PathBuf::from),
                };
                let outcome = quickstart::run(args)?;
                if !outcome.start_server {
                    return Ok(());
                }
                cli.work_dir = Some(outcome.work_dir.to_string_lossy().into_owned());
                cli.config = Some(outcome.config_path.to_string_lossy().into_owned());
            }
            CliCommand::Projects { .. } => {
                // Project catalogue subcommands are dispatched in `main` before
                // the server path is reached; there is nothing to start here.
                return Ok(());
            }
        }
    }

    let config = load_config(cli).map_err(anyhow::Error::msg)?;
    start_http_server(config)
        .await
        .context("start Codexify server")
}
