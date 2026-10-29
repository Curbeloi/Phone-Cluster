use clap::Parser;
use pc_agent::config::{Cli, Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Logs a stderr para que `--once` deje en stdout solo el JSON.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cfg = Config::load(Cli::parse())?;
    pc_agent::run(cfg).await
}
