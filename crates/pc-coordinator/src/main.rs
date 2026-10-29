use std::path::PathBuf;

use clap::Parser;
use pc_coordinator::config::Config;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Coordinador de Phone Cluster: proxy OpenAI y dashboard"
)]
struct Cli {
    /// Fichero de configuración.
    #[arg(long, default_value = "coordinator.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    let api = tokio::net::TcpListener::bind(cfg.listen).await?;
    let dashboard = tokio::net::TcpListener::bind(cfg.dashboard_listen).await?;
    pc_coordinator::serve(cfg, api, dashboard).await
}
