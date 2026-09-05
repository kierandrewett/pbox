use anyhow::{Context, Result};
use clap::Parser;
use std::{net::SocketAddr, path::PathBuf};

#[derive(Parser)]
#[command(about = "Relay encrypted pbox agent connections over WebSockets")]
struct Args {
    #[arg(long, env = "PBOX_RELAY_LISTEN", default_value = "0.0.0.0:8080")]
    listen: SocketAddr,
    #[arg(long, env = "PBOX_RELAY_KEY_FILE")]
    key_file: PathBuf,
    #[arg(long, default_value_t = 1024)]
    max_connections: usize,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let key = tokio::fs::read_to_string(&args.key_file)
        .await
        .context("read relay key file")?;
    let app = pbox_relay::server::router(key.trim().to_owned(), args.max_connections)?;
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .context("bind relay listener")?;
    eprintln!("[relay] listening on {}", listener.local_addr()?);
    axum::serve(listener, app).await.context("serve relay")
}
