use std::net::SocketAddr;

use clap::Parser;
use mock_llama::{MockOptions, MockState, router};

#[derive(Debug, Parser)]
#[command(version, about = "llama-server falso para pruebas sin teléfonos")]
struct Cli {
    #[arg(long, default_value_t = 9001)]
    port: u16,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[command(flatten)]
    opts: MockOptions,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let mut cli = Cli::parse();
    if cli.opts.label == "mock" {
        cli.opts.label = format!("mock-{}", cli.port);
    }
    let addr: SocketAddr = format!("{}:{}", cli.host, cli.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, model = %cli.opts.model, fail = cli.opts.fail, "mock-llama escuchando");
    axum::serve(listener, router(MockState::new(cli.opts))).await?;
    Ok(())
}
