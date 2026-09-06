//! Desktop recipes own session startup; the CLI owns the viewer and tunnel.
use super::*;

#[derive(Debug, Args)]
pub(crate) struct DesktopCommand {
    #[arg(default_value = "current")]
    id: String,
    /// Agent endpoint override (the box identity is still authenticated).
    #[arg(long)]
    endpoint: Option<String>,
    /// Registered desktop session (required when several are installed).
    #[arg(long)]
    session: Option<String>,
    /// Keep a tunnel open without launching a viewer (also implied by --json).
    #[arg(long)]
    no_viewer: bool,
    /// TigerVNC-compatible viewer executable.
    #[arg(long, default_value = "vncviewer")]
    viewer: String,
}

#[derive(serde::Deserialize)]
struct Session {
    port: u16,
    session: String,
}

#[derive(Debug)]
pub(crate) struct MissingDesktop {
    pub(crate) box_id: String,
}

impl std::fmt::Display for MissingDesktop {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "No desktop installed on {}", self.box_id)
    }
}

impl std::error::Error for MissingDesktop {}

fn check_launcher(result: &ExecResult, box_id: &str) -> Result<()> {
    if result.exited && result.code == 1 {
        return Err(MissingDesktop {
            box_id: box_id.to_owned(),
        }
        .into());
    }
    anyhow::ensure!(
        result.exited && result.code == 0,
        "Could not check the desktop installation: {}",
        safe_terminal_text(&String::from_utf8_lossy(&result.stderr))
    );
    Ok(())
}

fn parse_session(bytes: &[u8]) -> Result<Session> {
    let session: Session =
        serde_json::from_slice(bytes).context("read desktop launcher response")?;
    anyhow::ensure!(
        session.port >= 1024,
        "desktop launcher returned an invalid VNC port"
    );
    Ok(session)
}

pub(crate) fn run(store: &ConfigStore, command: DesktopCommand, json: bool) -> Result<()> {
    let config = load_config(store)?;
    let (id, endpoint) = resolve_agent_endpoint(&config, &command.id, command.endpoint.as_deref())?;
    let materials = agent_materials(&config, &id)?;
    tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
        let mut agent = relay::connect_agent(&config, &endpoint, &id, &materials.ca.certificate_pem, &materials.client).await?;
        agent.info().await?;
        let installed = agent.exec(
            vec!["/bin/sh".to_owned(), "-c".to_owned(), "test -e /usr/local/bin/pbox-desktop".to_owned()],
            "/", Vec::<(String, String)>::new(), "root"
        ).await.context("check desktop installation")?;
        check_launcher(&installed, &id)?;
        let mut argv = vec!["/usr/local/bin/pbox-desktop".to_owned()];
        if let Some(session) = command.session {
            argv.extend(["--session".to_owned(), session]);
        }
        let result = agent.exec(argv, "/", Vec::<(String, String)>::new(), "root").await
            .context("start the installed desktop")?;
        anyhow::ensure!(result.exited && result.code == 0, "desktop startup failed: {}", safe_terminal_text(&String::from_utf8_lossy(&result.stderr)));
        let session = parse_session(&result.stdout)?;
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let local = listener.local_addr()?;
        let mut viewer = if command.no_viewer || json { None } else {
            Some(tokio::process::Command::new(&command.viewer)
                .arg(format!("127.0.0.1::{}", local.port()))
                .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null()).kill_on_drop(true).spawn()
                .context("open VNC viewer; install TigerVNC viewer or use --no-viewer")?)
        };
        if json {
            ui::json_text(&serde_json::to_string(&serde_json::json!({"id": id, "session": session.session, "local": local.to_string(), "remote": format!("127.0.0.1:{}", session.port)}))?);
        } else {
            ui::desktop_ready(&session.session, &local.to_string());
        }
        let mut connections = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                result = tokio::signal::ctrl_c() => { result?; break; }
                status = async { match &mut viewer { Some(child) => child.wait().await, None => std::future::pending().await } } => {
                    anyhow::ensure!(status?.success(), "VNC viewer exited unsuccessfully");
                    break;
                }
                Some(result) = connections.join_next(), if !connections.is_empty() => {
                    match result {
                        Ok(Ok(())) => {},
                        Ok(Err(error)) => ui::stderr().error(&safe_terminal_text(&format!("VNC connection: {error:#}"))),
                        Err(error) => return Err(error.into()),
                    }
                }
                accepted = listener.accept(), if connections.len() < MAX_FORWARD_CONNECTIONS => {
                    let (socket, _) = accepted?;
                    let mut client = agent.clone();
                    connections.spawn(async move { client.forward_tcp(socket, "127.0.0.1", session.port).await });
                }
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_launcher_is_distinct_from_a_failed_probe() {
        let mut result = ExecResult {
            exited: true,
            code: 1,
            ..Default::default()
        };
        assert!(
            check_launcher(&result, "pbx_12345678")
                .unwrap_err()
                .is::<MissingDesktop>()
        );
        result.code = 0;
        assert!(check_launcher(&result, "pbx_12345678").is_ok());
        result.code = 2;
        assert!(
            !check_launcher(&result, "pbx_12345678")
                .unwrap_err()
                .is::<MissingDesktop>()
        );
        result.code = 1;
        result.exited = false;
        assert!(
            !check_launcher(&result, "pbx_12345678")
                .unwrap_err()
                .is::<MissingDesktop>()
        );
    }
    #[test]
    fn launcher_protocol_rejects_invalid_ports_and_non_json_output() {
        assert!(parse_session(br#"{"port":5901,"session":"xfce"}"#).is_ok());
        assert!(parse_session(br#"{"port":0,"session":"xfce"}"#).is_err());
        assert!(parse_session(b"started desktop").is_err());
    }
}
