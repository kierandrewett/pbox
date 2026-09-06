//! Guest updates are a connection operation, independent of terminal attachment.
use super::*;
use pbox_agent_client::InfoResponse;

#[derive(Debug, Args)]
pub struct AgentCommand {
    #[command(subcommand)]
    action: Action,
}
#[derive(Debug, Subcommand)]
enum Action {
    /// Install the local pbox-agent in a guest, without opening a terminal.
    Update {
        id: String,
        /// Allow ending legacy terminals that prevent the update.
        #[arg(long)]
        kill_sessions: bool,
        /// Confirm ending the listed legacy terminals without prompting.
        #[arg(long, requires = "kill_sessions")]
        yes: bool,
    },
}

#[derive(Default)]
pub(super) struct Options {
    pub json: bool,
    pub kill_sessions: bool,
    pub yes: bool,
}

pub(super) enum Outcome {
    Current,
    Updated,
    Blocked(Vec<String>),
    Unavailable(String),
}

pub(super) struct Connection {
    pub client: AgentClient,
    pub info: InfoResponse,
    pub outcome: Outcome,
    pub ended_sessions: Vec<String>,
}
impl Connection {
    pub fn require(&self, capability: &str, box_id: &str) -> Result<()> {
        if self.info.capabilities.iter().any(|cap| cap == capability) {
            return Ok(());
        }
        match &self.outcome {
            Outcome::Blocked(names) => bail!(
                "agent update on {box_id} is blocked by legacy terminals: {}. Run `pbox agent update {box_id} --kill-sessions` to review and end them; add --yes for non-interactive confirmation",
                names.join(", ")
            ),
            Outcome::Unavailable(reason) => {
                bail!("{reason}; install pbox-agent, then run `pbox agent update {box_id}`")
            }
            _ => bail!(
                "the local pbox-agent does not support {capability}; install a newer pbox-agent, then run `pbox agent update {box_id}`"
            ),
        }
    }
}

pub(super) async fn raw_connect(
    config: &Config,
    box_id: &str,
    endpoint: &str,
) -> Result<(AgentClient, InfoResponse)> {
    let materials = agent_materials(config, box_id)?;
    let mut client = relay::connect_agent(
        config,
        endpoint,
        box_id,
        &materials.ca.certificate_pem,
        &materials.client,
    )
    .await?;
    let info = client.info().await?;
    Ok((client, info))
}

pub(super) async fn connect(
    config: &Config,
    box_id: &str,
    endpoint: &str,
    options: Options,
) -> Result<Connection> {
    let connection = raw_connect(config, box_id, endpoint).await?;
    prepare(config, box_id, endpoint, connection, options).await
}

pub(super) async fn prepare(
    config: &Config,
    box_id: &str,
    endpoint: &str,
    (client, info): (AgentClient, InfoResponse),
    options: Options,
) -> Result<Connection> {
    let mut connection = Connection {
        client,
        info,
        outcome: Outcome::Current,
        ended_sessions: Vec::new(),
    };
    let binary = match resolve_agent_binary(config).and_then(|path| Ok((file_digest(&path)?, path)))
    {
        Ok(binary) => binary,
        Err(error) => {
            connection.outcome = Outcome::Unavailable(ui::concise_error(&error));
            return Ok(connection);
        }
    };
    let (mut expected, path) = binary;
    if connection.info.agent_digest == expected {
        return Ok(connection);
    }
    // Serialize automatic updates from harnesses on this control machine. Reconnect
    // under the lock: another process may already have replaced the remote agent.
    let _lock = update_lock(box_id).await?;
    (connection.client, connection.info) = raw_connect(config, box_id, endpoint).await?;
    if connection.info.agent_digest == expected {
        return Ok(connection);
    }
    if connection
        .info
        .capabilities
        .iter()
        .any(|cap| cap == "terminal-sessions")
        && !connection
            .info
            .capabilities
            .iter()
            .any(|cap| cap == "durable-sessions")
    {
        let sessions = connection.client.list_sessions().await?;
        if !sessions.is_empty() {
            let names: Vec<_> = sessions
                .iter()
                .map(|session| session.name.clone())
                .collect();
            if !options.kill_sessions {
                connection.outcome = Outcome::Blocked(names);
                return Ok(connection);
            }
            if !options.yes {
                anyhow::ensure!(
                    !options.json && io::stdin().is_terminal() && io::stderr().is_terminal(),
                    "ending legacy terminals requires confirmation; repeat `pbox agent update {box_id} --kill-sessions --yes` to end their running processes"
                );
                ui::confirm_agent_session_shutdown(box_id, &sessions);
                if !ui::confirm_action(
                    &mut io::stdin().lock(),
                    &mut io::stderr().lock(),
                    "End sessions and update?",
                )? {
                    connection.outcome = Outcome::Blocked(names);
                    return Ok(connection);
                }
            }
            for name in names {
                if !options.json {
                    ui::stderr().progress(&format!("Ending legacy terminal {box_id}:{name}"));
                }
                connection
                    .client
                    .close_session(&name)
                    .await
                    .with_context(|| format!("close legacy terminal {box_id}:{name}"))?;
                connection.ended_sessions.push(name);
            }
        }
        let remaining = connection.client.list_sessions().await?;
        if !remaining.is_empty() {
            connection.outcome =
                Outcome::Blocked(remaining.into_iter().map(|session| session.name).collect());
            return Ok(connection);
        }
    }
    if !options.json {
        ui::stderr().progress(&format!("Updating pbox-agent on {box_id}"));
    }
    let data =
        fs::read(&path).with_context(|| format!("read pbox-agent binary {}", path.display()))?;
    expected = format!("{:x}", Sha256::digest(&data));
    // Unique uploads avoid corruption when two control machines update concurrently.
    let staged = format!("/tmp/pbox-agent-update-{:016x}", rand::random::<u64>());
    connection
        .client
        .put_file(&staged, data, 0o755, true)
        .await
        .context("upload pbox-agent update")?;
    let result = connection
        .client
        .exec(
            vec![
                "/bin/sh".into(),
                "-c".into(),
                include_str!("guest-scripts/update-agent.sh").into(),
                "pbox-agent-update".into(),
                staged,
            ],
            "/",
            [],
            "root",
        )
        .await;
    // Restart can cut off its own reply. The reconnect and digest check below
    // decide success; never retry an ambiguous restart or the user's command.
    if let Ok(result) = &result {
        anyhow::ensure!(
            exec_exit_code(result) == 0,
            "pbox-agent update failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    let refreshed = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Ok((client, info)) = raw_connect(config, box_id, endpoint).await
                && info.agent_digest == expected
            {
                break (client, info);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .with_context(|| match result {
        Err(error) => format!(
            "agent update did not become ready after restart: {}",
            ui::concise_error(&error.into())
        ),
        Ok(_) => "agent update did not become ready; check the guest agent service".to_owned(),
    })?;
    (connection.client, connection.info) = refreshed;
    connection.outcome = Outcome::Updated;
    Ok(connection)
}

async fn update_lock(box_id: &str) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let directory = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .context("resolve pbox state directory")?
        .join("pbox/agent-updates");
    fs::create_dir_all(&directory)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(directory.join(format!("{box_id}.lock")))?;
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            match file.try_lock() {
                Ok(()) => return Ok::<_, anyhow::Error>(()),
                Err(std::fs::TryLockError::WouldBlock) => {
                    tokio::time::sleep(Duration::from_millis(100)).await
                }
                Err(error) => return Err(anyhow!("lock agent update: {error}")),
            }
        }
    })
    .await
    .context("another agent update is still running; retry shortly")??;
    Ok(file)
}

pub fn run(store: &ConfigStore, command: AgentCommand, json: bool) -> Result<RunOutcome> {
    let Action::Update {
        id,
        kill_sessions,
        yes,
    } = command.action;
    let config = load_config(store)?;
    let (box_id, endpoint) = sessions::endpoint(&config, &id)?;
    let connection = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(connect(
            &config,
            &box_id,
            &endpoint,
            Options {
                json,
                kill_sessions,
                yes,
            },
        ))?;
    let (status, blocked, reason) = match &connection.outcome {
        Outcome::Current => ("current", Vec::new(), None),
        Outcome::Updated => ("updated", Vec::new(), None),
        Outcome::Blocked(names) => (
            "blocked",
            names.clone(),
            Some("legacy terminals are still running"),
        ),
        Outcome::Unavailable(reason) => ("unavailable", Vec::new(), Some(reason.as_str())),
    };
    if json {
        ui::json_text(&serde_json::json!({"box_id":box_id,"status":status,"agent_version":connection.info.agent_version,"blocking_sessions":blocked,"ended_sessions":connection.ended_sessions,"reason":reason}).to_string());
    } else {
        ui::agent_update_result(
            &id,
            &box_id,
            status,
            &connection.info.agent_version,
            &blocked,
            reason,
        );
        if !connection.ended_sessions.is_empty() {
            ui::stdout().stdout_metadata("ended", &connection.ended_sessions.join(", "));
        }
    }
    Ok(
        if matches!(
            connection.outcome,
            Outcome::Blocked(_) | Outcome::Unavailable(_)
        ) {
            RunOutcome::Exit(1)
        } else {
            RunOutcome::Success
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ending_legacy_sessions_requires_an_explicit_option() {
        for args in [
            vec!["pbox", "agent", "update", "work"],
            vec!["pbox", "agent", "update", "work", "--kill-sessions"],
            vec![
                "pbox",
                "--json",
                "agent",
                "update",
                "work",
                "--kill-sessions",
                "--yes",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        assert!(Cli::try_parse_from(["pbox", "agent", "update", "work", "--yes"]).is_err());
    }
}
