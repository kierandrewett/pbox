//! Persistent shell commands. SSH remains the normal way to open a terminal.
use super::*;

#[derive(Debug, Args)]
pub struct SessionCommand {
    #[command(subcommand)]
    action: Action,
}
#[derive(Debug, Subcommand)]
enum Action {
    /// Show running terminals, including detached shells.
    #[command(visible_alias = "ls")]
    List {
        /// Limit the list to one box (all boxes by default).
        id: Option<String>,
    },
    /// Start a detached terminal without a local TTY.
    Start {
        id: String,
        name: String,
        #[arg(long, default_value = "")]
        cwd: String,
        #[arg(long, default_value = "")]
        user: String,
        #[arg(long = "env")]
        env: Vec<String>,
        #[arg(long, default_value_t = 24)]
        rows: u32,
        #[arg(long, default_value_t = 100)]
        cols: u32,
        #[arg(last = true)]
        argv: Vec<String>,
    },
    /// Read the current screen as plain text without attaching.
    Read { id: String, name: String },
    /// Send text, then keys, without attaching. Does not retry input.
    Send {
        id: String,
        name: String,
        #[arg(long, conflicts_with = "stdin")]
        text: Option<String>,
        /// Read up to 64 KiB of text from standard input.
        #[arg(long)]
        stdin: bool,
        /// Key name (repeat for a sequence): Enter, Tab, Escape, Up, Ctrl+C.
        #[arg(long = "key")]
        keys: Vec<String>,
    },
    /// End a terminal and its running processes.
    Close {
        id: String,
        name: String,
        /// Skip confirmation.
        #[arg(long)]
        yes: bool,
    },
}

pub(super) fn validate_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 64
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c)),
        "session names use 1–64 letters, digits, hyphens or underscores"
    );
    Ok(())
}

/// Recognise detach without changing pasted data or other terminal sequences.
#[derive(Default)]
pub(super) struct TerminalInput {
    pending: Vec<u8>,
    pasted: bool,
}
impl TerminalInput {
    pub fn pending(&self) -> bool {
        !self.pending.is_empty()
    }
    pub fn flush(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
    pub fn push(&mut self, bytes: &[u8]) -> (Vec<u8>, bool) {
        let mut output = Vec::new();
        for &byte in bytes {
            if self.pending.is_empty() {
                if byte == 0x1d && !self.pasted {
                    return (output, true);
                }
                if byte == 0x1b {
                    self.pending.push(byte);
                } else {
                    output.push(byte);
                }
                continue;
            }
            self.pending.push(byte);
            if self.pending.len() == 2 && byte == b'[' {
                continue;
            }
            if self.pending.len() > 2 && !(0x40..=0x7e).contains(&byte) && self.pending.len() < 64 {
                continue;
            }
            if self.pending == b"\x1b[200~" {
                self.pasted = true;
            } else if self.pending == b"\x1b[201~" {
                self.pasted = false;
            } else if !self.pasted && detach_key(&self.pending) {
                self.pending.clear();
                return (output, true);
            }
            output.extend(self.flush());
        }
        (output, false)
    }
}
fn detach_key(sequence: &[u8]) -> bool {
    let Some(body) = sequence.strip_prefix(b"\x1b[") else {
        return false;
    };
    let Ok(body) = std::str::from_utf8(body) else {
        return false;
    };
    let (key, modifiers, event) = if let Some(body) = body.strip_suffix('u') {
        let mut fields = body.split(';');
        let key = fields
            .next()
            .and_then(|p| p.split(':').next())
            .and_then(|p| p.parse::<u32>().ok());
        let mut modifiers = fields.next().unwrap_or("1").split(':');
        let modifier = modifiers.next().and_then(|p| p.parse::<u32>().ok());
        let event = modifiers.next().unwrap_or("1").parse::<u32>().ok();
        (key, modifier, event)
    } else if let Some(body) = body.strip_suffix('~') {
        let mut fields = body.split(';');
        if fields.next() != Some("27") {
            return false;
        }
        let modifiers = fields.next().and_then(|p| p.parse::<u32>().ok());
        let key = fields.next().and_then(|p| p.parse::<u32>().ok());
        (key, modifiers, Some(1))
    } else {
        return false;
    };
    key == Some(93)
        && matches!(event, Some(1 | 2))
        && modifiers.is_some_and(|value| value > 0 && (value - 1) & !(64 | 128) == 4)
}

type SessionQuery = (BoxRecord, Result<Vec<pbox_agent_client::TerminalSession>>);

pub(super) struct SessionGroup {
    pub box_id: String,
    pub box_name: Option<String>,
    pub state: String,
    pub sessions: Vec<pbox_agent_client::TerminalSession>,
    pub unavailable: bool,
}

async fn query_sessions(
    config: &Config,
    record: &BoxRecord,
) -> Result<Vec<pbox_agent_client::TerminalSession>> {
    let box_id = record.id.to_string();
    let endpoint = agent_endpoint(config, record)?;
    let materials = agent_materials(config, &box_id)?;
    let mut client = relay::connect_agent(
        config,
        &endpoint,
        &box_id,
        &materials.ca.certificate_pem,
        &materials.client,
    )
    .await?;
    let info = client.info().await?;
    // Agents predating persistent terminals cannot hold any sessions.
    if !info
        .capabilities
        .iter()
        .any(|cap| cap == "terminal-sessions")
    {
        return Ok(Vec::new());
    }
    Ok(client.list_sessions().await?)
}

fn show_sessions(
    mut results: Vec<SessionQuery>,
    filter: Option<&str>,
    json: bool,
) -> Result<RunOutcome> {
    results.sort_by(|a, b| a.0.name.cmp(&b.0.name).then(a.0.id.cmp(&b.0.id)));
    let mut groups = Vec::new();
    let mut incomplete = false;
    for (record, result) in results {
        let unavailable = result.is_err();
        let sessions = match result {
            Ok(mut sessions) => {
                sessions.sort_by(|a, b| a.name.cmp(&b.name));
                sessions
            }
            Err(error) => {
                incomplete = true;
                ui::stderr().warning(&format!("{}: {}", record.id, ui::concise_error(&error)));
                Vec::new()
            }
        };
        groups.push(SessionGroup {
            box_id: record.id.to_string(),
            box_name: record.name,
            state: record.state,
            sessions,
            unavailable,
        });
    }
    if json {
        let values: Vec<_> = groups.iter().flat_map(|group| group.sessions.iter().map(|session| serde_json::json!({
            "box_id": group.box_id, "box_name": group.box_name,
            "name": session.name, "user": session.user, "cwd": session.cwd,
            "argv": session.argv, "attached": session.attached, "created_unix": session.created_unix,
            "rows": session.rows, "cols": session.cols
        }))).collect();
        ui::json_text(&serde_json::to_string_pretty(&values)?);
    } else {
        ui::terminal_sessions(filter, &groups, incomplete);
    }
    Ok(if incomplete {
        RunOutcome::Exit(1)
    } else {
        RunOutcome::Success
    })
}

fn endpoint(config: &Config, id: &str) -> Result<(String, String)> {
    // A known relay identity is enough to contact its agent. Reading or sending
    // to a running terminal must not depend on the Proxmox management API.
    if config.relay.url.is_some() && id.parse::<PboxId>().is_ok() {
        return Ok((id.to_owned(), relay::ENDPOINT.to_owned()));
    }
    resolve_agent_endpoint(config, id, None)
}

async fn connect(
    config: &Config,
    box_id: &str,
    endpoint: &str,
    capability: &str,
) -> Result<AgentClient> {
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
    anyhow::ensure!(
        info.capabilities.iter().any(|cap| cap == capability),
        "the guest agent needs an update for this command; end its running sessions, then connect with `pbox ssh {box_id}` to update it"
    );
    Ok(client)
}

pub fn run(store: &ConfigStore, command: SessionCommand, json: bool) -> Result<RunOutcome> {
    let config = load_config(store)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create session runtime")?;
    match command.action {
        Action::List { id } => {
            let pve = client_from_config(&config)?;
            let records = if let Some(id) = &id {
                vec![find_box(&pve, id)?]
            } else {
                discover_boxes(&pve)?
            };
            let filter = id.as_ref().map(|_| records[0].id.to_string());
            let results = runtime.block_on(async {
                let mut results = Vec::new();
                // Bound simultaneous agent connections and time spent on an unreachable box.
                for batch in records.chunks(8) {
                    let mut queries = tokio::task::JoinSet::new();
                    for record in batch {
                        if record.state != "running" {
                            results.push((record.clone(), Ok(Vec::new())));
                            continue;
                        }
                        let config = config.clone();
                        let record = record.clone();
                        queries.spawn(async move {
                            let result = tokio::time::timeout(
                                Duration::from_secs(5),
                                query_sessions(&config, &record),
                            )
                            .await
                            .context("agent did not respond within 5 seconds")
                            .and_then(|result| result);
                            (record, result)
                        });
                    }
                    while let Some(result) = queries.join_next().await {
                        results.push(result.context("query terminal sessions")?);
                    }
                }
                Ok::<_, anyhow::Error>(results)
            })?;
            return show_sessions(results, filter.as_deref(), json);
        }
        action @ (Action::Start { .. } | Action::Read { .. } | Action::Send { .. }) => {
            let (id, name) = match &action {
                Action::Start { id, name, .. }
                | Action::Read { id, name }
                | Action::Send { id, name, .. } => (id, name),
                _ => unreachable!(),
            };
            validate_name(name)?;
            let (box_id, endpoint) = endpoint(&config, id)?;
            let mut client =
                runtime.block_on(connect(&config, &box_id, &endpoint, "session-control"))?;
            match action {
                Action::Start {
                    name,
                    cwd,
                    user,
                    env,
                    rows,
                    cols,
                    argv,
                    ..
                } => {
                    let env = ssh_environment(&env, Some("xterm-256color".to_owned()), None)?;
                    runtime.block_on(client.start_session(pbox_agent_client::ExecRequest {
                        session_name: name.clone(),
                        argv: ssh_command_argv(&argv),
                        cwd,
                        user,
                        env: env.into_iter().collect(),
                        terminal_rows: rows,
                        terminal_cols: cols,
                        ..Default::default()
                    }))?;
                    if json {
                        ui::json_text(
                            &serde_json::json!({"box_id":box_id,"name":name,"started":true})
                                .to_string(),
                        );
                    } else {
                        ui::stdout().success(&format!("Started session {name} on {box_id}"));
                    }
                }
                Action::Read { name, .. } => {
                    let screen = runtime.block_on(client.read_session(&name))?;
                    if json {
                        let session = screen.session.context("agent omitted terminal details")?;
                        ui::json_text(&serde_json::json!({
                            "box_id": box_id, "name": session.name, "user": session.user,
                            "cwd": session.cwd, "argv": session.argv, "attached": session.attached,
                            "rows": session.rows, "cols": session.cols, "created_unix": session.created_unix,
                            "text": screen.text, "cursor_row": screen.cursor_row, "cursor_col": screen.cursor_col,
                        }).to_string());
                    } else {
                        ui::session_screen(&screen.text);
                    }
                }
                Action::Send {
                    name,
                    text,
                    stdin,
                    keys,
                    ..
                } => {
                    anyhow::ensure!(
                        text.is_some() || stdin || !keys.is_empty(),
                        "provide --text, --stdin or --key"
                    );
                    let text = if stdin {
                        let mut bytes = Vec::new();
                        io::stdin().lock().take(65537).read_to_end(&mut bytes)?;
                        anyhow::ensure!(bytes.len() <= 65536, "text exceeds 64 KiB");
                        String::from_utf8(bytes).context("text must be UTF-8")?
                    } else {
                        text.unwrap_or_default()
                    };
                    // Never retry this mutation: a lost reply does not mean input was not delivered.
                    runtime.block_on(client.send_session(&name, text, keys))?;
                    if json {
                        ui::json_text(
                            &serde_json::json!({"box_id":box_id,"name":name,"sent":true})
                                .to_string(),
                        );
                    } else {
                        ui::stdout().success(&format!("Sent input to {name}"));
                    }
                }
                _ => unreachable!(),
            }
        }
        Action::Close { id, name, yes } => {
            validate_name(&name)?;
            let (box_id, endpoint) = endpoint(&config, &id)?;
            let mut client =
                runtime.block_on(connect(&config, &box_id, &endpoint, "terminal-sessions"))?;
            if !yes {
                anyhow::ensure!(
                    !json && io::stdin().is_terminal() && io::stderr().is_terminal(),
                    "repeat with --yes to close session {name}"
                );
                ui::stderr().section("Close terminal session");
                ui::stderr().metadata("session", &name);
                ui::stderr().warning("This ends the shell and its running processes.");
                if !ui::confirm_action(
                    &mut io::stdin().lock(),
                    &mut io::stderr().lock(),
                    "Close session?",
                )? {
                    ui::stderr().hint("Cancelled.");
                    return Ok(RunOutcome::Success);
                }
            }
            runtime.block_on(client.close_session(&name))?;
            if json {
                ui::json_text(&serde_json::json!({"name":name,"closed":true}).to_string());
            } else {
                ui::stdout().success(&format!("Closed session {name}"));
            }
        }
    }
    Ok(RunOutcome::Success)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detach_handles_split_kitty_and_legacy_keys_without_consuming_other_input() {
        for key in [
            &b"\x1d"[..],
            b"\x1b[93;5u",
            b"\x1b[93;5:1u",
            b"\x1b[93;69:2u",
            b"\x1b[27;5;93~",
        ] {
            for chunk in 1..=key.len() {
                let mut input = TerminalInput::default();
                let mut detached = false;
                for bytes in key.chunks(chunk) {
                    let (output, done) = input.push(bytes);
                    assert!(output.is_empty());
                    detached |= done;
                }
                assert!(detached);
            }
        }
        for bytes in [
            &b"hello\x1b[A\x1b[93;5:3u\x04"[..],
            b"\x1b[200~hello\x1d\x1b[201~",
            b"\x1b",
        ] {
            for chunk in 1..=bytes.len() {
                let mut input = TerminalInput::default();
                let mut output = Vec::new();
                for part in bytes.chunks(chunk) {
                    let (data, detached) = input.push(part);
                    assert!(!detached);
                    output.extend(data);
                }
                output.extend(input.flush());
                assert_eq!(output, bytes);
            }
        }
    }
    #[test]
    fn list_defaults_to_all_boxes_and_accepts_an_explicit_filter() {
        for (args, expected) in [
            (vec!["pbox", "session", "list"], None),
            (vec!["pbox", "session", "list", "current"], Some("current")),
            (vec!["pbox", "session", "ls", "work"], Some("work")),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            let Command::Session(SessionCommand {
                action: Action::List { id },
            }) = cli.command
            else {
                panic!("expected session list")
            };
            assert_eq!(id.as_deref(), expected);
        }
    }

    #[test]
    fn relay_session_control_with_an_id_does_not_need_proxmox() {
        let mut config = Config::default();
        config.relay.url = Some("https://relay.invalid".into());
        assert_eq!(
            endpoint(&config, "pbx_t3yzd9y3").unwrap(),
            ("pbx_t3yzd9y3".into(), relay::ENDPOINT.into())
        );
    }

    #[test]
    fn headless_commands_parse_text_keys_and_command_arguments() {
        for args in [
            vec![
                "pbox",
                "session",
                "start",
                "work",
                "codex",
                "--",
                "codex",
                "--no-alt-screen",
            ],
            vec!["pbox", "session", "read", "work", "codex"],
            vec![
                "pbox", "session", "send", "work", "codex", "--text", "hello", "--key", "Enter",
            ],
            vec![
                "pbox", "session", "send", "work", "codex", "--stdin", "--key", "Enter",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        assert!(
            Cli::try_parse_from([
                "pbox", "session", "send", "work", "codex", "--text", "hello", "--stdin"
            ])
            .is_err()
        );
    }

    #[test]
    fn partial_session_results_have_a_failure_exit_status() {
        assert_eq!(
            show_sessions(
                vec![(
                    BoxRecord {
                        id: "pbx_t3yzd9y3".parse().unwrap(),
                        name: Some("work".into()),
                        state: "running".into(),
                        vmid: 9000,
                        node: "pve".into(),
                        ip: None,
                        ipv6: None,
                        image: None,
                        recipes: Vec::new(),
                        capabilities: Vec::new(),
                        ping: None,
                    },
                    Err(anyhow!("unreachable"))
                )],
                None,
                true
            )
            .unwrap(),
            RunOutcome::Exit(1)
        );
        assert_eq!(
            show_sessions(Vec::new(), None, true).unwrap(),
            RunOutcome::Success
        );
    }

    #[test]
    fn ssh_session_layout_keeps_command_arguments_and_simple_defaults() {
        for args in [
            vec!["pbox", "ssh", "current"],
            vec!["pbox", "ssh", "current", "--session", "build"],
            vec!["pbox", "session", "list", "current"],
            vec!["pbox", "session", "close", "current", "build", "--yes"],
        ] {
            assert!(Cli::try_parse_from(args).is_ok());
        }
        let cli = Cli::try_parse_from(["pbox", "ssh", "current", "--", "codex", "--help"]).unwrap();
        assert!(
            matches!(cli.command, Command::Ssh(SshCommand { argv, session: None, .. }) if argv == ["codex", "--help"])
        );
    }
}
