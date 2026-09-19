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
    /// Enable PVE visibility authentication using an administrator-owned JSON file.
    #[arg(long, env = "PBOX_RELAY_ACCESS_FILE")]
    access_file: Option<PathBuf>,
    /// Export identity files for an enrolled box instead of starting the relay.
    #[arg(long, requires_all = ["access_file", "guest_directory"])]
    export_guest: Option<String>,
    #[arg(long, requires = "export_guest")]
    guest_directory: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let key = tokio::fs::read_to_string(&args.key_file)
        .await
        .context("read relay key file")?;
    let mut app = pbox_relay::server::router(key.trim().to_owned(), args.max_connections)?;
    if let Some(path) = args.access_file {
        let settings: pbox_relay::access::Settings =
            serde_json::from_slice(&tokio::fs::read(path).await?)?;
        settings.validate()?;
        if let Some(id) = args.export_guest {
            anyhow::ensure!(settings.boxes.contains_key(&id), "box is not enrolled");
            let directory = args
                .guest_directory
                .context("guest directory is required")?;
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new().mode(0o700).create(&directory)?;
            let (identity, ca) = pbox_relay::access::guest_identity(key.trim(), &id)?;
            for (name, contents) in [
                ("server.pem", identity.certificate_pem),
                ("server-key.pem", identity.private_key_pem),
                ("client-ca.pem", ca),
            ] {
                use std::{io::Write, os::unix::fs::OpenOptionsExt};
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(directory.join(name))?;
                file.write_all(contents.as_bytes())?;
            }
            return Ok(());
        }
        app = app.merge(pbox_relay::access::router(settings, key.trim().to_owned())?);
    }
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .context("bind relay listener")?;
    eprintln!("[relay] listening on {}", listener.local_addr()?);
    // PID 1 does not get the usual default SIGTERM behaviour in a container.
    // Closing the process drops tunnels; agents reconnect to the replacement relay.
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;
    tokio::select! {
        result = axum::serve(listener, app) => result.context("serve relay"),
        _ = terminate.recv() => Ok(()),
        result = tokio::signal::ctrl_c() => result.context("wait for shutdown signal"),
    }
}
