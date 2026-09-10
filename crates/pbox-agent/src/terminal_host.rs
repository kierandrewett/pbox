//! Local PTY owner. Its service lifetime is independent of the network agent.
//! The private socket reuses the additive session RPCs; no TCP listener or credentials.
use super::*;
use pbox_proto::agent::agent_client::AgentClient;
use std::os::unix::fs::PermissionsExt;
use tokio::net::{UnixListener, UnixStream};
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::{Channel, Endpoint};

pub const SOCKET: &str = "/run/pbox-terminals/control.sock";
pub const UNIT: &str = "[Unit]\nDescription=pbox terminal supervisor\nAfter=local-fs.target\nStartLimitIntervalSec=0\n[Service]\nExecStart=/usr/local/bin/pbox-agent --terminal-host\nRestart=always\nRestartSec=1\nRuntimeDirectory=pbox-terminals\nRuntimeDirectoryMode=0700\nUMask=0077\n";
pub type Client = AgentClient<Channel>;

pub async fn connect(path: &Path) -> Result<Client> {
    let path = path.to_owned();
    let channel = Endpoint::from_static("http://localhost")
        .connect_timeout(Duration::from_secs(3))
        .connect_with_connector(tower::service_fn(move |_| {
            let path = path.clone();
            async move {
                UnixStream::connect(path)
                    .await
                    .map(hyper_util::rt::TokioIo::new)
            }
        }))
        .await
        .context("connect to terminal supervisor")?;
    Ok(AgentClient::new(channel))
}

/// systemd owns the supervisor in a separate cgroup. Minimal pbox PID 1 starts
/// it as a sibling and passes the socket explicitly. Other init systems retain
/// the safe legacy update deferral until they provide an independent owner.
pub async fn managed(max_exec: usize) -> Result<Option<Client>> {
    let systemd = Path::new("/run/systemd/system").is_dir()
        && std::env::var_os("PBOX_TERMINAL_SOCKET").is_none();
    let path = if let Some(path) = std::env::var_os("PBOX_TERMINAL_SOCKET") {
        PathBuf::from(path)
    } else if Path::new("/run/systemd/system").is_dir() && nix::unistd::Uid::effective().is_root() {
        let unit = UNIT.replace(
            "--terminal-host\n",
            &format!("--terminal-host {SOCKET} {max_exec}\n"),
        );
        let path = Path::new("/etc/systemd/system/pbox-terminals.service");
        if tokio::fs::read_to_string(path).await.ok().as_deref() != Some(unit.as_str()) {
            let temporary = path.with_extension("service.tmp");
            tokio::fs::write(&temporary, &unit).await?;
            tokio::fs::rename(temporary, path).await?;
            checked_systemctl(&["daemon-reload"]).await?;
        }
        // Starting an already running service does not restart its processes.
        checked_systemctl(&["start", "pbox-terminals.service"]).await?;
        PathBuf::from(SOCKET)
    } else {
        return Ok(None);
    };
    let mut client = wait_ready(&path).await?;
    if systemd
        && client.info(InfoRequest {}).await?.into_inner().agent_digest != agent_digest()
        && client
            .list_sessions(pbox_proto::agent::ListSessionsRequest {
                protocol_version: PROTOCOL,
            })
            .await?
            .into_inner()
            .sessions
            .is_empty()
    {
        checked_systemctl(&["restart", "pbox-terminals.service"]).await?;
        client = wait_ready(&path).await?;
    }
    Ok(Some(client))
}

async fn wait_ready(path: &Path) -> Result<Client> {
    let client = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match connect(path).await {
                Ok(client) => return client,
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    })
    .await
    .context("terminal supervisor did not become ready")?;
    Ok(client)
}

async fn checked_systemctl(args: &[&str]) -> Result<()> {
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        Command::new("systemctl").args(args).output(),
    )
    .await
    .context("terminal service setup timed out")??;
    anyhow::ensure!(
        output.status.success(),
        "terminal service setup failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}

pub async fn serve(path: &Path, max_exec: usize) -> Result<()> {
    let directory = path.parent().context("terminal socket needs a directory")?;
    std::fs::create_dir_all(directory)?;
    let metadata = std::fs::symlink_metadata(directory)?;
    anyhow::ensure!(
        metadata.is_dir() && metadata.uid() == nix::unistd::Uid::effective().as_raw(),
        "terminal directory must be owned by the supervisor user"
    );
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
    // Hold the lock for the server lifetime. Never unlink another live owner's socket.
    use std::os::unix::fs::MetadataExt;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(directory.join("owner.lock"))?;
    use std::os::unix::fs::OpenOptionsExt;
    anyhow::ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "terminal supervisor is already running"
    );
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    let service = AgentService {
        box_id: String::new(),
        handshake_slots: Arc::new(Semaphore::new(64)),
        forward_slots: Arc::new(Semaphore::new(0)),
        file_slots: Arc::new(Semaphore::new(0)),
        forward_allow: Vec::new(),
        exec_slots: Arc::new(Semaphore::new(max_exec)),
        sessions: sessions::Sessions::default(),
        terminal_host: None,
    };
    Server::builder()
        .add_service(AgentServer::new(service))
        .serve_with_incoming(UnixListenerStream::new(listener))
        .await?;
    drop(lock);
    Ok(())
}
