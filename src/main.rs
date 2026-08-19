use clap::Parser;

use codexify::config::{Cli, load_config};
use codexify::server::start_http_server;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    let config = match load_config(cli) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = start_http_server(config).await {
        eprintln!("Server error: {e}");
        std::process::exit(1);
    }
}
