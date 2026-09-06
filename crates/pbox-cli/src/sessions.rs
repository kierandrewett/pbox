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
        #[arg(default_value = "current")]
        id: String,
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

pub fn run(store: &ConfigStore, command: SessionCommand, json: bool) -> Result<RunOutcome> {
    let id = match &command.action {
        Action::List { id } | Action::Close { id, .. } => id,
    };
    let config = load_config(store)?;
    let (box_id, endpoint) = resolve_agent_endpoint(&config, id, None)?;
    let materials = agent_materials(&config, &box_id)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create session runtime")?;
    let mut client = runtime.block_on(async {
        let mut client = relay::connect_agent(&config, &endpoint, &box_id, &materials.ca.certificate_pem, &materials.client).await?;
        let info = client.info().await?;
        anyhow::ensure!(info.capabilities.iter().any(|cap| cap == "terminal-sessions"),
            "the guest agent needs an update for terminal sessions; connect with `pbox ssh {box_id}` to update it");
        Ok::<_, anyhow::Error>(client)
    })?;
    match command.action {
        Action::List { .. } => {
            let sessions = runtime.block_on(client.list_sessions())?;
            if json {
                let values: Vec<_> = sessions.iter().map(|session| serde_json::json!({
                    "name": session.name, "user": session.user, "cwd": session.cwd,
                    "argv": session.argv, "attached": session.attached, "created_unix": session.created_unix,
                    "rows": session.rows, "cols": session.cols
                })).collect();
                ui::json_text(&serde_json::to_string_pretty(&values)?);
            } else {
                ui::terminal_sessions(&box_id, &sessions);
            }
        }
        Action::Close { name, yes, .. } => {
            validate_name(&name)?;
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
