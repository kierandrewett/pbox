//! Shared terminal design system. Command handlers must use this module for presentation.
#![allow(clippy::print_stdout, clippy::print_stderr)]
use super::{
    BoxInfo, BoxRecord, Cli, ColorChoice, RecipeCatalog, SnapshotActionOutput, box_state_colour,
    format_snapshot_time, recipes,
};
use anyhow::{Context, Result};
use pbox_core::LxcSnapshot;
use serde::Serialize;
use std::io::{self, BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicU8, Ordering};
static MODE: AtomicU8 = AtomicU8::new(0);

pub(crate) fn recipe_heading(recipe: &str, box_id: &str) {
    stderr().heading(&format!("{recipe} → {box_id}"));
}

pub(crate) fn snapshot_heading(name: &str, source: &str) {
    stderr().heading(&format!("Snapshot {name} ← {source}"));
}

#[derive(Debug, PartialEq)]
pub(crate) enum RecipeEvent {
    Task(String),
    Result { success: bool, skipped: bool },
    Log(String),
    Finish,
}

pub(crate) struct RecipeStage {
    sender: Option<std::sync::mpsc::Sender<RecipeEvent>>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl RecipeStage {
    pub(crate) fn events(&self) -> Option<std::sync::mpsc::Sender<RecipeEvent>> {
        self.sender.clone()
    }
    pub(crate) fn start(label: &str, visible: bool) -> Self {
        let mut stage = Self {
            sender: None,
            worker: None,
        };
        if visible {
            let label = label.to_owned();
            let (sender, receiver) = std::sync::mpsc::channel();
            stage.sender = Some(sender);
            stage.worker = Some(std::thread::spawn(move || {
                let style = stderr();
                let animated = style.can_animate();
                let mut display = CreationDisplay::new();
                let start = std::time::Instant::now();
                let mut active: Option<(String, std::time::Instant)> = None;
                if animated {
                    display.phase(label.clone());
                } else {
                    style.progress(&label);
                }
                loop {
                    match receiver.recv_timeout(std::time::Duration::from_millis(120)) {
                        Ok(RecipeEvent::Task(task)) => {
                            active = Some((task.clone(), std::time::Instant::now()));
                            if animated {
                                display.substep(task);
                            } else {
                                style.progress(&task);
                            }
                        }
                        Ok(RecipeEvent::Result { success, skipped }) => {
                            if let Some((task, started)) = active.take() {
                                let task = if skipped {
                                    format!("{task} (skipped)")
                                } else {
                                    task
                                };
                                if animated {
                                    display.substep_done(
                                        task,
                                        started.elapsed().as_secs(),
                                        success,
                                    );
                                } else if skipped {
                                    style.hint(&task);
                                } else if success {
                                    style.completed_step(&task, started.elapsed().as_secs());
                                } else {
                                    style.error(&task);
                                }
                            }
                        }
                        Ok(RecipeEvent::Log(line)) => {
                            if animated {
                                display.log(line);
                            } else {
                                style.hint(&line);
                            }
                        }
                        Ok(RecipeEvent::Finish) => {
                            if animated {
                                display.finish(true);
                            } else {
                                style.completed_step(&label, start.elapsed().as_secs());
                            }
                            break;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                            if animated {
                                display.finish(false);
                            }
                            break;
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    if animated {
                        display.tick();
                    }
                }
            }));
        }
        stage
    }
    pub(crate) fn finish(mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(RecipeEvent::Finish);
        }
    }
}
impl Drop for RecipeStage {
    fn drop(&mut self) {
        self.sender.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

pub(crate) fn recipe_raw_output(bytes: &[u8]) -> io::Result<()> {
    io::stderr().lock().write_all(bytes)
}

pub(crate) fn recipe_failure(reason: &str, log: &std::path::Path) {
    let style = stderr();
    style.error("Recipe failed");
    for line in wrap_diagnostic(reason, 84) {
        style.hint(&line);
    }
    style.metadata("full log", &log.display().to_string());
    style.hint("Use --verbose to stream Ansible output.");
}

pub(crate) fn image_preparation_error(image: &str, reason: &str) {
    let style = stderr();
    style.error("Image preparation failed");
    style.metadata("image", image);
    style.metadata("box", "Not created; nothing uploaded to PVE");
    style.section("What failed");
    let reason = reason
        .strip_prefix("Image compatibility check failed:")
        .unwrap_or(reason)
        .trim();
    for line in wrap_diagnostic(reason, 84) {
        style.hint(&line);
    }
    style.section("Next steps");
    style.hint(
        "Fix the reported requirement in your Dockerfile, or choose a compatible base image.",
    );
    style.hint("Rebuild and publish your image before retrying:");
    style.command("pbox new --image YOUR_IMAGE");
    style.hint("Add --verbose to retain the full preparation logs.");
}

fn wrap_diagnostic(value: &str, width: usize) -> Vec<String> {
    let safe = safe_terminal_text(value);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in safe.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    lines
}

pub(crate) fn byte_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}
/// Completion scripts are machine output, independent of colour and JSON mode.
pub(crate) fn completions(shell: clap_complete::Shell) -> Result<()> {
    let mut script = Vec::new();
    clap_complete::env::Shells::builtins()
        .completer(&shell.to_string())
        .context("unsupported completion shell")?
        .write_registration(super::completions::ENV, "pbox", "pbox", "pbox", &mut script)?;
    if shell == clap_complete::Shell::Zsh {
        // Support both `source <(...)` and autoloading an installed `_pbox` file.
        script.extend_from_slice(b"\nif [[ $funcstack[1] == _pbox ]]; then\n    _clap_dynamic_completer_pbox \"$@\"\nfi\n");
    }
    io::stdout()
        .lock()
        .write_all(&script)
        .context("write shell completion script")
}

pub(crate) fn agent_startup_help(box_id: &str) {
    let style = stderr();
    style.section("Check guest startup");
    style.hint("Image checks cannot verify guest networking before boot.");
    style.hint("Find the guest location:");
    style.command_stderr(&format!("pbox info {box_id}"));
    style.hint("In a root shell inside a workspace container, inspect its logs:");
    style.command_stderr("cat /var/log/pbox-agent.log");
    style.command_stderr("cat /var/log/pbox-network.log");
    style.hint("For an older box using systemd, inspect the service:");
    style.command_stderr("systemctl status pbox-agent.service");
    style.command_stderr("journalctl -u pbox-agent.service -b --no-pager");
    style.hint("If it is disabled, enable it:");
    style.command_stderr("systemctl enable --now pbox-agent.service");
    style.hint("For connection errors, inspect networking:");
    style.command_stderr("ip route");
    style.command_stderr("cat /etc/resolv.conf");
    style.command_stderr("curl -I https://your-relay-host/healthz");
    style
        .hint("If container login is unavailable, a PVE administrator can enter it from the node:");
    style.command_stderr("pct enter VMID");
    style.hint("After fixing the cause, retry the relay connection:");
    style.command_stderr(&format!("pbox repair {box_id}"));
}

pub(crate) fn delete_details(
    record: &BoxRecord,
    metadata: Option<&pbox_core::PboxMetadata>,
    wait: bool,
    style: CliStyle,
) {
    style.section("Delete box");
    style.metadata("id", &record.id.to_string());
    style.metadata("name", record.name.as_deref().unwrap_or("unnamed"));
    style.metadata(
        "image",
        metadata
            .and_then(|value| value.image.as_deref())
            .unwrap_or("Not recorded"),
    );
    if let Some(snapshot) = metadata.and_then(|value| value.snapshot.as_deref()) {
        style.metadata("snapshot", snapshot);
    }
    style.metadata("state", &record.state);
    style.warning("This permanently deletes the box and its data.");
    if !wait {
        style.hint("Running boxes will be stopped immediately. Deletion continues in Proxmox.");
    }
}
pub(crate) fn configure(color: ColorChoice, json: bool) {
    MODE.store(
        if json {
            3
        } else {
            match color {
                ColorChoice::Auto => 0,
                ColorChoice::Always => 1,
                ColorChoice::Never => 2,
            }
        },
        Ordering::Relaxed,
    );
}
fn options() -> (ColorChoice, bool) {
    match MODE.load(Ordering::Relaxed) {
        1 => (ColorChoice::Always, false),
        2 => (ColorChoice::Never, false),
        3 => (ColorChoice::Never, true),
        _ => (ColorChoice::Auto, false),
    }
}
pub(crate) fn stderr() -> CliStyle {
    let (color, json) = options();
    CliStyle::for_stderr(color, json)
}
pub(crate) fn stdout() -> CliStyle {
    let (color, json) = options();
    CliStyle::for_stdout(color, json)
}
/// Only already-serialised machine output belongs here.
pub(crate) fn json_text(value: &str) {
    println!("{value}");
}
pub(crate) fn blank_stderr() {
    eprintln!();
}
pub(crate) fn blank_stdout() {
    println!();
}
pub(crate) const PROMPT_LABEL_WIDTH: usize = 30;
pub(crate) const ANSI_BOLD: &str = "\x1b[1m";
pub(crate) const ANSI_DIM: &str = "\x1b[2m";
pub(crate) const ANSI_CYAN: &str = "\x1b[36m";
pub(crate) const ANSI_BOLD_CYAN: &str = "\x1b[1;36m";
pub(crate) const ANSI_GREEN: &str = "\x1b[1;32m";
pub(crate) const ANSI_YELLOW: &str = "\x1b[1;33m";
pub(crate) const ANSI_RED: &str = "\x1b[1;31m";
pub(crate) const ANSI_RESET: &str = "\x1b[0m";

#[derive(Clone, Copy)]
pub(crate) struct CliStyle {
    enabled: bool,
    interactive: bool,
}

impl CliStyle {
    pub(crate) fn for_stderr(color: ColorChoice, json: bool) -> Self {
        let terminal = io::stderr().is_terminal();
        let enabled = color_enabled_for(color, json, terminal);
        Self {
            enabled,
            interactive: terminal && !json && std::env::var("TERM").as_deref() != Ok("dumb"),
        }
    }

    pub(crate) fn for_stdout(color: ColorChoice, json: bool) -> Self {
        let terminal = io::stdout().is_terminal();
        let enabled = color_enabled_for(color, json, terminal);
        Self {
            enabled,
            interactive: terminal && !json && std::env::var("TERM").as_deref() != Ok("dumb"),
        }
    }

    pub(crate) fn from_enabled(enabled: bool) -> Self {
        Self {
            enabled,
            interactive: false,
        }
    }

    pub(crate) fn text(self, value: &str) -> String {
        safe_terminal_text(value)
    }

    pub(crate) fn paint(self, code: &str, value: &str) -> String {
        let value = self.text(value);
        if self.enabled && !code.is_empty() {
            format!("{code}{value}{ANSI_RESET}")
        } else {
            value
        }
    }

    pub(crate) fn status(self, marker: &str, code: &str, message: &str) -> String {
        format!(
            "{} {}",
            self.paint(code, marker),
            self.paint(ANSI_BOLD, message)
        )
    }

    pub(crate) fn heading(self, message: &str) {
        eprintln!("{}", self.paint(ANSI_BOLD_CYAN, message));
    }

    pub(crate) fn section(self, message: &str) {
        eprintln!();
        eprintln!("{}", self.paint(ANSI_BOLD_CYAN, message));
    }

    pub(crate) fn hint(self, message: &str) {
        eprintln!("  {}", self.paint(ANSI_DIM, message));
    }

    pub(crate) fn metadata(self, label: &str, value: &str) {
        let label = format!("{label:<12}");
        eprintln!("  {} {}", self.paint(ANSI_DIM, &label), self.text(value));
    }

    pub(crate) fn warning(self, message: &str) {
        eprintln!("{}", self.status("!", ANSI_YELLOW, message));
    }

    pub(crate) fn error(self, message: &str) {
        eprintln!("{}", self.status("x", ANSI_RED, message));
    }

    pub(crate) fn progress(self, message: &str) {
        eprintln!("{}", self.status(">", ANSI_CYAN, message));
    }
    pub(crate) fn progress_live(self, message: &str) {
        if self.interactive {
            eprint!("\r\x1b[2K{}", self.status(">", ANSI_CYAN, message));
            let _ = io::stderr().flush();
        } else {
            self.progress(message);
        }
    }
    pub(crate) fn can_animate(self) -> bool {
        self.interactive
    }

    pub(crate) fn spinner(self, frame: char, message: &str) {
        if self.interactive {
            eprint!(
                "\r\x1b[2K{} {}",
                self.paint(ANSI_CYAN, &frame.to_string()),
                self.paint(ANSI_BOLD, message)
            );
            let _ = io::stderr().flush();
        } else {
            self.progress(message);
        }
    }

    pub(crate) fn clear_progress_line(self) {
        if self.interactive {
            eprint!("\r\x1b[2K");
            let _ = io::stderr().flush();
        }
    }

    pub(crate) fn prompt_text(self, label: &str, default: Option<&str>) -> String {
        let label = format!("{label:<PROMPT_LABEL_WIDTH$}");
        let default = default
            .filter(|value| !value.is_empty())
            .map(|value| format!(" {}", self.paint(ANSI_DIM, &format!("[{value}]"))))
            .unwrap_or_default();
        format!(
            "{} {}{default}: ",
            self.paint(ANSI_CYAN, "?"),
            self.paint(ANSI_BOLD, &label)
        )
    }
    pub(crate) fn prompt(self, label: &str, default: Option<&str>) -> Result<()> {
        eprint!("{}", self.prompt_text(label, default));
        io::stderr().flush().context("flush prompt")
    }
    pub(crate) fn success(self, message: &str) {
        self.stdout_status("ok", ANSI_GREEN, message);
    }
    pub(crate) fn stdout_heading(self, message: &str) {
        println!("{}", self.paint(ANSI_BOLD_CYAN, message));
    }
    pub(crate) fn stdout_hint(self, message: &str) {
        println!("  {}", self.paint(ANSI_DIM, message));
    }
    pub(crate) fn command(self, command: &str) {
        println!(
            "\n  {} {}",
            self.paint(ANSI_DIM, "$"),
            self.paint(ANSI_CYAN, command)
        );
    }
    pub(crate) fn command_stderr(self, command: &str) {
        eprintln!(
            "  {} {}",
            self.paint(ANSI_DIM, "$"),
            self.paint(ANSI_CYAN, command)
        );
    }
    pub(crate) fn diagnostic(self, message: &str) {
        self.hint(message);
    }
    pub(crate) fn completed_step(self, phase: &str, elapsed: u64) {
        eprintln!(
            "{} {}",
            self.status("ok", ANSI_GREEN, phase),
            self.paint(ANSI_DIM, &format!("({elapsed}s)"))
        );
    }
    pub(crate) fn stdout_status(self, marker: &str, code: &str, message: &str) {
        println!("{}", self.status(marker, code, message));
    }

    pub(crate) fn stdout_fields(self, fields: &std::collections::BTreeMap<String, String>) {
        let width = fields
            .keys()
            .map(|key| key.len())
            .max()
            .unwrap_or(12)
            .max(12);
        for (label, value) in fields {
            let label = format!("{label:<width$}");
            println!("  {} {}", self.paint(ANSI_DIM, &label), self.text(value));
        }
    }
    pub(crate) fn stdout_metadata(self, label: &str, value: &str) {
        let label = format!("{label:<12}");
        println!("  {} {}", self.paint(ANSI_DIM, &label), self.text(value));
    }
}

pub(crate) fn safe_terminal_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

pub(crate) fn concise_error(error: &anyhow::Error) -> String {
    let message = format!("{error:#}");
    for reason in [
        "session moved to another connection",
        "terminal session closed",
    ] {
        if message.contains(reason) {
            return reason.to_owned();
        }
    }
    if message.contains("peer closed connection without sending TLS close_notify") {
        return "pbox-agent connection closed unexpectedly; retry the command".to_owned();
    }
    for cause in error.chain() {
        if let Some(pbox_agent_client::AgentClientError::Rpc(status)) =
            cause.downcast_ref::<pbox_agent_client::AgentClientError>()
        {
            return safe_terminal_text(status.message());
        }
    }
    safe_terminal_text(&message)
}

/// Observe modes without modifying guest bytes, including split control sequences.
#[derive(Default)]
pub(crate) struct TerminalDisplay(std::cell::RefCell<(vte::Parser, AlternateScreen)>);
#[derive(Default)]
struct AlternateScreen(bool);
impl vte::Perform for AlternateScreen {
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if !ignore
            && intermediates == b"?"
            && matches!(action, 'h' | 'l')
            && params.iter().any(|p| matches!(p, [47] | [1047] | [1049]))
        {
            self.0 = action == 'h';
        }
    }
    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8) {
        if !ignore && intermediates.is_empty() && byte == b'c' {
            self.0 = false;
        }
    }
}
impl TerminalDisplay {
    pub(crate) fn observe(&self, bytes: &[u8]) {
        let mut state = self.0.borrow_mut();
        let (parser, modes) = &mut *state;
        parser.advance(modes, bytes);
    }
    fn cleanup(&self) -> Vec<u8> {
        let mut bytes = b"\x1b[<16u\x1b[=0u\x1b[>4;0m\x1b[?2004l\x1b[?1004l\x1b[0m\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l".to_vec();
        if self.0.borrow().1.0 {
            bytes.extend_from_slice(b"\x1b[?1049l");
        }
        bytes
    }
    pub(crate) fn restore(&self) {
        let mut stdout = io::stdout();
        if stdout.is_terminal() {
            let _ = stdout.write_all(&self.cleanup());
            let _ = stdout.flush();
        }
    }
}

#[cfg(test)]
mod terminal_display_tests {
    use super::*;
    #[test]
    fn cleanup_preserves_output_and_only_leaves_an_active_alternate_screen() {
        let display = TerminalDisplay::default();
        display.observe(b"shell output");
        let plain = display.cleanup();
        assert!(
            !plain
                .windows(2)
                .any(|b| b == b"2K" || b == b"2J" || b == b"3J")
        );
        assert!(!plain.windows(5).any(|b| b == b"1049l"));
        display.observe(b"\x1b[?10");
        display.observe(b"49h");
        assert!(display.cleanup().ends_with(b"\x1b[?1049l"));
        display.observe(b"\x1b[?1049l");
        assert_eq!(display.cleanup(), plain);
        display.observe(b"\x1b]0;ignore [ ?1049h\x07");
        assert_eq!(display.cleanup(), plain);
    }
}

pub(crate) fn print_recipe_catalog(
    catalog: &RecipeCatalog,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(catalog)?);
        return Ok(());
    }
    let style = CliStyle::for_stdout(color, json);
    style.stdout_heading("Recipes");
    style.stdout_metadata("repository", &catalog.repository);
    style.stdout_metadata("revision", &catalog.revision);
    style.stdout_heading(&format!("{:<28} {:<10} DESCRIPTION", "ID", "KIND"));
    for recipe in &catalog.recipes {
        println!(
            "{:<28} {:<10} {}",
            format_box_cell(style, &recipe.id, 28, ANSI_CYAN),
            recipe.kind.as_str(),
            safe_terminal_text(recipe.metadata.description.as_deref().unwrap_or("")),
        );
    }
    if catalog.recipes.is_empty() {
        style.stdout_hint("No recipes found.");
    }
    Ok(())
}

pub(crate) fn desktop_ready(session: &str, local: &str) {
    let style = stdout();
    style.stdout_heading("Desktop");
    style.stdout_metadata("session", &safe_terminal_text(session));
    style.stdout_metadata("VNC", local);
    style.stdout_hint("Press Ctrl-C to close the tunnel. Your desktop stays running.");
}

pub(crate) fn desktop_install_help(box_id: &str) {
    let style = stderr();
    let box_id = safe_terminal_text(box_id);
    style.error(&format!("No desktop installed on {box_id}"));
    style.command_stderr(&format!("pbox recipe apply desktop/xfce --box-id {box_id}"));
    style.command_stderr(&format!("pbox desktop {box_id}"));
}

pub(crate) fn print_recipe_info(recipe: &recipes::Recipe, colour: bool) {
    let style = CliStyle::from_enabled(colour);
    style.stdout_heading(&recipe.id);
    style.stdout_metadata("kind", recipe.kind.as_str());
    style.stdout_metadata("path", &recipe.path);
    if let Some(description) = &recipe.metadata.description {
        style.stdout_metadata("description", description);
    }
    for (label, values) in [
        ("requires", &recipe.metadata.requires),
        ("supports", &recipe.metadata.supports),
        ("capabilities", &recipe.metadata.capabilities),
    ] {
        if !values.is_empty() {
            style.stdout_metadata(label, &values.join(", "));
        }
    }
    let resources = &recipe.metadata.resources;
    if resources.cores.is_some() || resources.memory.is_some() || resources.disk.is_some() {
        style.stdout_metadata(
            "resources",
            &format!(
                "cores={} memory={} disk={}",
                resources
                    .cores
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                resources
                    .memory
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_owned()),
                safe_terminal_text(resources.disk.as_deref().unwrap_or("-"))
            ),
        );
    }
}

pub(crate) fn print_snapshot_action(
    output: &SnapshotActionOutput,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(output)?);
        return Ok(());
    }
    let style = CliStyle::for_stdout(color, json);
    style.success(&format!(
        "{} checkpoint {} on {}",
        output.action, output.name, output.id
    ));
    if output.started == Some(true) {
        style.success(&format!("Started {}", output.id));
    }
    Ok(())
}

pub(crate) fn print_snapshot_list(snapshots: &[LxcSnapshot], colour: bool) {
    let style = CliStyle::from_enabled(colour);
    style.stdout_heading(&format!(
        "{:<24} {:<24} {:<20} DESCRIPTION",
        "NAME", "CREATED", "PARENT"
    ));
    for snapshot in snapshots {
        let name = format_box_cell(style, &snapshot.name, 24, ANSI_CYAN);
        println!(
            "{name} {:<24} {:<20} {}",
            format_snapshot_time(snapshot.snaptime),
            safe_terminal_text(snapshot.parent.as_deref().unwrap_or("-")),
            safe_terminal_text(snapshot.description.as_deref().unwrap_or("-")),
        );
    }
    if snapshots.is_empty() {
        style.stdout_hint("No snapshots found.");
    }
}

fn disk_allocation(size: Option<&str>) -> String {
    let Some(size) = size else {
        return "Not reported".into();
    };
    let split = size
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(size.len());
    let (number, unit) = size.split_at(split);
    if number.parse::<f64>().ok() == Some(0.0) {
        return "No quota".into();
    }
    match unit {
        "K" => format!("{number} KiB"),
        "M" => format!("{number} MiB"),
        "G" => format!("{number} GiB"),
        "T" => format!("{number} TiB"),
        _ => size.to_owned(),
    }
}

pub(crate) fn print_box_info(info: &BoxInfo, json: bool, color: ColorChoice) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(info)?);
    } else {
        let style = CliStyle::for_stdout(color, json);
        style.stdout_status("box", ANSI_CYAN, &info.id.to_string());
        style.stdout_metadata("vmid", &info.vmid.to_string());
        style.stdout_metadata("state", &info.state);
        style.stdout_metadata("node", &info.node);
        style.stdout_metadata("ipv4", info.ip.as_deref().unwrap_or("-"));
        if let Some(ipv6) = &info.ipv6 {
            style.stdout_metadata("ipv6", ipv6);
        }
        style.stdout_metadata("name", info.name.as_deref().unwrap_or("-"));
        if let Some(resources) = &info.resources {
            style.stdout_metadata(
                "cpu cores",
                &resources
                    .cores
                    .map_or_else(|| "Not set".into(), |cores| cores.to_string()),
            );
            for (label, mib) in [
                ("memory", resources.memory_mib),
                ("swap", resources.swap_mib),
            ] {
                let value = mib.map_or_else(
                    || "Not set".into(),
                    |mib| {
                        if mib == 0 {
                            "0 MiB".into()
                        } else {
                            byte_size(mib.saturating_mul(1024 * 1024))
                        }
                    },
                );
                style.stdout_metadata(label, &value);
            }
            style.stdout_metadata(
                "root disk",
                &disk_allocation(resources.disk_size.as_deref()),
            );
            style.stdout_metadata(
                "storage",
                resources.storage.as_deref().unwrap_or("Not reported"),
            );
            if let Some(capacity) = resources.filesystem_size_bytes {
                let shared = disk_allocation(resources.disk_size.as_deref()) == "No quota";
                let used = resources
                    .filesystem_used_bytes
                    .map_or_else(|| "unknown".into(), byte_size);
                style.stdout_metadata(
                    "filesystem",
                    &format!(
                        "{used} / {} used{}",
                        byte_size(capacity),
                        if shared { " (shared)" } else { "" }
                    ),
                );
            }
        }
        let recipes = info
            .recipes
            .iter()
            .map(|recipe| safe_terminal_text(&recipe.id))
            .collect::<Vec<_>>()
            .join(", ");
        style.stdout_metadata("recipes", if recipes.is_empty() { "-" } else { &recipes });
        let capabilities = safe_terminal_text(&info.capabilities.join(", "));
        style.stdout_metadata(
            "capabilities",
            if capabilities.is_empty() {
                "-"
            } else {
                &capabilities
            },
        );
    }
    Ok(())
}

pub(crate) fn format_box_cell(style: CliStyle, value: &str, width: usize, code: &str) -> String {
    let value = style.text(value);
    style.paint(code, &format!("{value:<width$}"))
}

pub(crate) fn print_box_records(records: &[BoxRecord], colour: bool) {
    let style = CliStyle::from_enabled(colour);
    let has_ipv6 = records.iter().any(|record| record.ipv6.is_some());
    let width = |header: &str, values: Vec<String>| {
        values
            .into_iter()
            .map(|value| value.chars().count())
            .chain(std::iter::once(header.chars().count()))
            .max()
            .unwrap_or(header.len())
    };
    let id_width = width("ID", records.iter().map(|r| r.id.to_string()).collect());
    let state_width = width("STATE", records.iter().map(|r| r.state.clone()).collect());
    let ping_width = width(
        "PING",
        records
            .iter()
            .map(|r| r.ping.as_deref().unwrap_or("-").to_owned())
            .collect(),
    );
    let node_width = width("NODE", records.iter().map(|r| r.node.clone()).collect());
    let ipv4_width = width(
        "IPV4",
        records
            .iter()
            .map(|r| r.ip.as_deref().unwrap_or("-").to_owned())
            .collect(),
    );
    let ipv6_width = width(
        "IPV6",
        records
            .iter()
            .map(|r| r.ipv6.as_deref().unwrap_or("-").to_owned())
            .collect(),
    );
    let name_width = width(
        "NAME",
        records
            .iter()
            .map(|r| r.name.as_deref().unwrap_or("-").to_owned())
            .collect(),
    );
    let gap = "  ";
    let header = format!(
        "{id:<id_width$}{gap}{state:<state_width$}{gap}{ping:<ping_width$}{gap}{node:<node_width$}{gap}{ipv4:<ipv4_width$}{gap}{ipv6}{name:<name_width$}{gap}IMAGE",
        name = "NAME",
        id = "ID",
        state = "STATE",
        ping = "PING",
        node = "NODE",
        ipv4 = "IPV4",
        ipv6 = if has_ipv6 {
            format!("{:<width$}{gap}", "IPV6", width = ipv6_width)
        } else {
            String::new()
        },
        gap = gap,
    );
    println!("{}", style.paint(ANSI_BOLD_CYAN, &header));
    for record in records {
        let id = format_box_cell(style, &record.id.to_string(), id_width, ANSI_CYAN);
        let state = format_box_cell(
            style,
            &record.state,
            state_width,
            box_state_colour(&record.state),
        );
        let ping = format_box_cell(style, record.ping.as_deref().unwrap_or("-"), ping_width, "");
        let node = format_box_cell(style, &record.node, node_width, ANSI_CYAN);
        let ip = format_box_cell(style, record.ip.as_deref().unwrap_or("-"), ipv4_width, "");
        let name = format_box_cell(style, record.name.as_deref().unwrap_or("-"), name_width, "");
        let image = style.text(record.image.as_deref().unwrap_or("-"));
        let ipv6 = if has_ipv6 {
            format!(
                "{}{gap}",
                format_box_cell(style, record.ipv6.as_deref().unwrap_or("-"), ipv6_width, "")
            )
        } else {
            String::new()
        };
        println!("{id}{gap}{state}{gap}{ping}{gap}{node}{gap}{ip}{gap}{ipv6}{name}{gap}{image}");
    }
    if records.is_empty() {
        println!(
            "{}",
            style.paint(ANSI_DIM, "No pbox-managed containers found.")
        );
    }
}

pub(crate) fn print_value<T: Serialize>(value: &T, json: bool, color: ColorChoice) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
        return Ok(());
    }
    let value = serde_json::to_value(value)?;
    if let Some(id) = value.get("id").and_then(serde_json::Value::as_str) {
        CliStyle::for_stdout(color, false).stdout_heading(id);
    } else {
        println!("{value}");
    }
    Ok(())
}

pub(crate) fn color_enabled_for(color: ColorChoice, json: bool, is_terminal: bool) -> bool {
    let mode = match color {
        ColorChoice::Auto => pbox_core::ui::ColorMode::Auto,
        ColorChoice::Always => pbox_core::ui::ColorMode::Always,
        ColorChoice::Never => pbox_core::ui::ColorMode::Never,
    };
    mode.enabled(is_terminal, std::env::var_os("NO_COLOR").is_some(), json)
}

pub(crate) fn color_enabled(color: ColorChoice, json: bool) -> bool {
    color_enabled_for(color, json, io::stdout().is_terminal())
}

pub(crate) fn confirm_delete(input: &mut impl BufRead, output: &mut impl Write) -> Result<bool> {
    confirm_action(input, output, "Delete permanently?")
}

pub(crate) fn confirm_action(
    input: &mut impl BufRead,
    output: &mut impl Write,
    question: &str,
) -> Result<bool> {
    loop {
        write!(output, "{}", stderr().prompt_text(question, Some("y/N")))?;
        output.flush()?;
        let mut answer = String::new();
        if input.read_line(&mut answer)? == 0 {
            writeln!(output)?;
            return Ok(false);
        }
        match answer.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "" | "n" | "no" => return Ok(false),
            _ => writeln!(
                output,
                "{}",
                stderr().status("!", ANSI_YELLOW, "Enter y or n.")
            )?,
        }
    }
}

pub(crate) fn help_styles() -> clap::builder::Styles {
    use clap::builder::styling::AnsiColor;
    clap::builder::Styles::styled()
        .header(AnsiColor::Cyan.on_default().bold())
        .usage(AnsiColor::Cyan.on_default().bold())
        .literal(AnsiColor::Cyan.on_default())
        .placeholder(AnsiColor::White.on_default().dimmed())
        .error(AnsiColor::Red.on_default().bold())
        .valid(AnsiColor::Green.on_default())
        .invalid(AnsiColor::Yellow.on_default())
}

/// Resolve presentation flags before Clap exits early for help or a usage error.
pub(crate) fn parse_cli() -> Cli {
    use clap::{CommandFactory, FromArgMatches};
    let args: Vec<_> = std::env::args_os().collect();
    let mut color = ColorChoice::Auto;
    let mut json = false;
    let mut iter = args.iter().skip(1);
    while let Some(arg) = iter.next() {
        if arg == "--" {
            break;
        }
        if arg == "--json" {
            json = true;
        }
        let value = if arg == "--color" {
            iter.next().and_then(|value| value.to_str())
        } else {
            arg.to_str()
                .and_then(|value| value.strip_prefix("--color="))
        };
        if let Some(value) = value {
            color = match value {
                "always" => ColorChoice::Always,
                "never" => ColorChoice::Never,
                _ => ColorChoice::Auto,
            };
        }
    }
    configure(color, json);
    let clap_color = if json || std::env::var_os("NO_COLOR").is_some() {
        clap::ColorChoice::Never
    } else {
        match color {
            ColorChoice::Auto => clap::ColorChoice::Auto,
            ColorChoice::Always => clap::ColorChoice::Always,
            ColorChoice::Never => clap::ColorChoice::Never,
        }
    };
    let matches = Cli::command().color(clap_color).get_matches_from(args);
    Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
}

/// Explain an image's access restrictions without changing its existing policy.
pub(crate) fn user_access(box_id: &str, user: &str, access: super::guest::UserAccess) {
    use super::guest::UserAccess;
    let style = stderr();
    match access {
        UserAccess::Passwordless | UserAccess::ImageDefined => return,
        UserAccess::Restricted => {
            style.warning(&format!("User {user} cannot use passwordless sudo."))
        }
        UserAccess::Unknown => {
            style.warning(&format!("Could not check sudo access for user {user}."))
        }
    }
    style.hint("Use a root shell to install tools or change the sudo policy:");
    style.hint(&format!("pbox ssh {box_id} --user root"));
}

/// A bounded live block; completing a phase replaces the block with one summary row.
pub(crate) struct CreationDisplay {
    style: CliStyle,
    animated: bool,
    phase: Option<String>,
    started: std::time::Instant,
    active: Option<(String, std::time::Instant)>,
    completed: std::collections::VecDeque<(String, u64, bool)>,
    logs: std::collections::VecDeque<String>,
    drawn: usize,
    frame: usize,
    last_draw: std::time::Instant,
}
impl CreationDisplay {
    pub(crate) fn new() -> Self {
        Self {
            style: stderr(),
            animated: stderr().can_animate() && !super::progress::verbose(),
            phase: None,
            started: std::time::Instant::now(),
            active: None,
            completed: Default::default(),
            logs: Default::default(),
            drawn: 0,
            frame: 0,
            last_draw: std::time::Instant::now(),
        }
    }
    pub(crate) fn phase(&mut self, phase: String) {
        self.finish(true);
        self.phase = Some(phase);
        self.started = std::time::Instant::now();
        self.last_draw = self.started;
        self.completed.clear();
        self.logs.clear();
        self.active = None;
        if !self.animated {
            self.style
                .progress(self.phase.as_deref().unwrap_or_default());
        }
    }
    pub(crate) fn substep(&mut self, action: String) {
        if !self.animated {
            self.style.hint(&action);
        }
        self.active = Some((action, std::time::Instant::now()));
        self.logs.clear();
    }
    pub(crate) fn substep_done(&mut self, action: String, elapsed: u64, success: bool) {
        self.active = None;
        if !self.animated && !success {
            self.style.error(&action);
        }
        self.completed.push_back((action, elapsed, success));
        while self.completed.len() > 6 {
            self.completed.pop_front();
        }
    }
    pub(crate) fn log(&mut self, line: String) {
        if !line.trim().is_empty() {
            if !self.animated {
                self.style.hint(&line);
            }
            self.logs.push_back(line);
            while self.logs.len() > 3 {
                self.logs.pop_front();
            }
        }
    }
    fn clear(&mut self) {
        if self.drawn == 0 {
            return;
        }
        eprint!("\r\x1b[2K");
        for _ in 1..self.drawn {
            eprint!("\x1b[1A\r\x1b[2K");
        }
        self.drawn = 0;
    }
    pub(crate) fn tick(&mut self) {
        if !self.animated {
            if let Some(phase) = &self.phase
                && self.last_draw.elapsed() >= std::time::Duration::from_secs(5)
            {
                self.style.progress(&format!(
                    "{phase} ({}s elapsed)",
                    self.started.elapsed().as_secs()
                ));
                self.last_draw = std::time::Instant::now();
            }
            return;
        }
        if self.phase.is_none()
            || (self.drawn > 0 && self.last_draw.elapsed() < std::time::Duration::from_millis(120))
        {
            return;
        }
        self.clear();
        let width = terminal_columns().saturating_sub(1).max(10);
        let clipped = |value: &str, reserve: usize| -> String {
            clip_terminal_text(value, width.saturating_sub(reserve))
        };
        let phase = self.phase.as_deref().unwrap_or_default();
        let marker = ["|", "/", "-", "\\"][self.frame % 4];
        let mut rows = vec![format!(
            "{} {}",
            self.style.status(marker, ANSI_CYAN, &clipped(phase, 12)),
            self.style.paint(
                ANSI_DIM,
                &format!("({}s)", self.started.elapsed().as_secs())
            )
        )];
        for (action, elapsed, success) in &self.completed {
            rows.push(format!(
                "  {} {} {}",
                self.style.paint(
                    if *success { ANSI_GREEN } else { ANSI_RED },
                    if *success { "ok" } else { "x" }
                ),
                clipped(action, 14),
                self.style.paint(ANSI_DIM, &format!("({elapsed}s)"))
            ));
        }
        if let Some((action, started)) = &self.active {
            rows.push(format!(
                "  {} {} {}",
                self.style.paint(ANSI_CYAN, marker),
                clipped(action, 14),
                self.style
                    .paint(ANSI_DIM, &format!("({}s)", started.elapsed().as_secs()))
            ));
        }
        for line in &self.logs {
            rows.push(format!(
                "    {}",
                self.style.paint(ANSI_DIM, &clipped(line, 4))
            ));
        }
        self.drawn = rows.len();
        eprint!("{}", rows.join("\r\n"));
        let _ = io::stderr().flush();
        self.frame += 1;
        self.last_draw = std::time::Instant::now();
    }
    pub(crate) fn finish(&mut self, success: bool) {
        self.clear();
        if let Some(phase) = self.phase.take() {
            if success {
                self.style
                    .completed_step(&phase, self.started.elapsed().as_secs());
            } else {
                self.style.error(&phase);
                if self.animated {
                    for line in &self.logs {
                        self.style.hint(line);
                    }
                }
            }
        }
    }
}
fn clip_terminal_text(value: &str, width: usize) -> String {
    let text = safe_terminal_text(value);
    if text.chars().count() <= width {
        text
    } else {
        format!(
            "{}...",
            text.chars()
                .take(width.saturating_sub(3))
                .collect::<String>()
        )
    }
}
fn terminal_columns() -> usize {
    #[cfg(unix)]
    {
        let mut size = nix::libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        if unsafe { nix::libc::ioctl(nix::libc::STDERR_FILENO, nix::libc::TIOCGWINSZ, &mut size) }
            == 0
            && size.ws_col > 0
        {
            return size.ws_col as usize;
        }
    }
    80
}

/// Save/restore the host title without consuming terminal replies or guest output.
/// Guest OSC titles are prefixed separately by TitlePrefix.
pub(crate) struct TerminalTitleGuard {
    pub(crate) name: String,
}
impl TerminalTitleGuard {
    pub(crate) fn enter(name: &str) -> Option<Self> {
        if !io::stdout().is_terminal()
            || !io::stderr().is_terminal()
            || std::env::var("TERM").is_ok_and(|term| term == "dumb")
        {
            return None;
        }
        let mut output = io::stderr().lock();
        let _ = write!(output, "\x1b[22;0t\x1b]2;{}\x07", safe_terminal_text(name));
        let _ = output.flush();
        Some(Self {
            name: safe_terminal_text(name),
        })
    }
    pub(crate) fn restore(&self) {
        let mut output = io::stderr().lock();
        let _ = write!(output, "\x1b[23;0t");
        let _ = output.flush();
    }
}

impl Drop for TerminalTitleGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Recognise only OSC title headers; all other bytes pass through unchanged.
/// Buffer at most the four-byte header, including across transport chunks.
pub(crate) struct TitlePrefix {
    prefix: Option<Vec<u8>>,
    pending: Vec<u8>,
    in_osc: bool,
    escaped: bool,
}
impl TitlePrefix {
    pub(crate) fn new(name: Option<&str>) -> Self {
        Self {
            prefix: name.map(|name| format!("{} · ", safe_terminal_text(name)).into_bytes()),
            pending: Vec::new(),
            in_osc: false,
            escaped: false,
        }
    }
    pub(crate) fn push(&mut self, data: &[u8]) -> Vec<u8> {
        let Some(prefix) = &self.prefix else {
            return data.to_vec();
        };
        let mut output = Vec::with_capacity(data.len());
        for &byte in data {
            if self.in_osc {
                output.push(byte);
                if byte == 7 || (self.escaped && byte == b'\\') {
                    self.in_osc = false;
                }
                self.escaped = byte == 27;
                continue;
            }
            self.pending.push(byte);
            if self.pending == b"\x1b"
                || self.pending == b"\x1b]"
                || matches!(self.pending.as_slice(), b"\x1b]0" | b"\x1b]1" | b"\x1b]2")
            {
                continue;
            }
            if matches!(
                self.pending.as_slice(),
                b"\x1b]0;" | b"\x1b]1;" | b"\x1b]2;"
            ) {
                output.append(&mut self.pending);
                output.extend_from_slice(prefix);
                self.in_osc = true;
                self.escaped = false;
            } else {
                // Other OSC commands (hyperlinks, clipboard, colours) are opaque.
                if self.pending.starts_with(b"\x1b]") {
                    self.in_osc = byte != 7;
                    self.escaped = byte == 27;
                }
                let trailing_escape = !self.in_osc && self.pending.len() > 1 && byte == 27;
                if trailing_escape {
                    self.pending.pop();
                }
                output.append(&mut self.pending);
                if trailing_escape {
                    self.pending.push(27);
                }
            }
        }
        output
    }
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

pub(crate) fn snapshot_groups(groups: &[(BoxRecord, Vec<LxcSnapshot>)], colour: bool) {
    let style = CliStyle::from_enabled(colour);
    if groups.is_empty() {
        style.stdout_hint("No boxes found.");
    }
    for (record, snapshots) in groups {
        style.stdout_heading(&format!(
            "{} ({})",
            record.name.as_deref().unwrap_or("unnamed"),
            record.id
        ));
        print_snapshot_list(snapshots, colour);
    }
}

pub(crate) fn image_search_results(entries: &[super::images::ImageSearchEntry]) {
    let style = stdout();
    style.stdout_heading("Images");
    if entries.is_empty() {
        style.stdout_hint("No images found. Try a broader name or another --registry.");
        return;
    }
    for entry in entries {
        style.stdout_metadata("image", &entry.name);
        if !entry.description.is_empty() {
            style.stdout_hint(&clip_terminal_text(
                &entry.description,
                terminal_columns().saturating_sub(4),
            ));
        }
        if !entry.official.is_empty() {
            style.stdout_hint("Official image");
        }
    }
    style.stdout_hint("List versions: pbox image tags IMAGE");
    style.stdout_hint("Create a box:  pbox new --image IMAGE:TAG");
}

pub(crate) fn session_connected(box_id: &str, name: &str) {
    stderr().progress(&format!("Connected to {box_id} · {name}"));
    stderr().hint("Ctrl-] detaches. Type exit to end this shell.");
}

/// A decoded guest screen is data, without headings, styling or terminal escapes.
pub(crate) fn session_screen(text: &str) {
    println!("{text}");
}

pub(crate) fn terminal_sessions(
    filter: Option<&str>,
    groups: &[super::sessions::SessionGroup],
    incomplete: bool,
) {
    let style = stdout();
    style.stdout_heading("Terminal sessions");
    if incomplete {
        style.stdout_hint("Incomplete list: some boxes could not be checked.");
    }
    if groups.is_empty() {
        style.stdout_hint("No boxes found.");
        return;
    }
    let columns = terminal_columns();
    for group in groups {
        println!();
        style.stdout_heading(&format!(
            "{} · {} · {}",
            safe_terminal_text(group.box_name.as_deref().unwrap_or("Unnamed box")),
            group.box_id,
            safe_terminal_text(&group.state)
        ));
        if group.unavailable {
            style.stdout_hint("Sessions unavailable: agent could not be reached.");
            continue;
        }
        if group.sessions.is_empty() {
            style.stdout_hint("No running sessions.");
            continue;
        }
        let name_width = group
            .sessions
            .iter()
            .map(|s| s.name.len())
            .max()
            .unwrap_or(7)
            .clamp(7, 20);
        let user_width = group
            .sessions
            .iter()
            .map(|s| s.user.len())
            .max()
            .unwrap_or(4)
            .clamp(4, 12);
        let directory_width = group
            .sessions
            .iter()
            .map(|s| session_directory(s).chars().count())
            .max()
            .unwrap_or(9)
            .clamp(9, (columns / 3).max(9));
        style.stdout_heading(&format!(
            "  {:<name_width$}  {:<8}  {:<user_width$}  {:<directory_width$}  RUNNING",
            "SESSION", "STATE", "USER", "DIRECTORY"
        ));
        for session in &group.sessions {
            println!(
                "  {:<name_width$}  {:<8}  {:<user_width$}  {:<directory_width$}  {}",
                clip_terminal_text(&session.name, name_width),
                if session.attached {
                    "attached"
                } else {
                    "detached"
                },
                clip_terminal_text(&session.user, user_width),
                clip_terminal_text(session_directory(session), directory_width),
                clip_terminal_text(
                    &session_running(session),
                    columns.saturating_sub(name_width + user_width + directory_width + 18)
                )
            );
        }
    }
    style.command(&format!("pbox attach {}:NAME", filter.unwrap_or("BOX")));
}

fn session_directory(session: &pbox_agent_client::TerminalSession) -> &str {
    if session.current_cwd.is_empty() {
        &session.cwd
    } else {
        &session.current_cwd
    }
}
fn session_running(session: &pbox_agent_client::TerminalSession) -> String {
    if session.processes.is_empty() {
        return session.argv.join(" ");
    }
    let mut chain = Vec::new();
    let mut pid = session.foreground_pid;
    for _ in 0..session.processes.len() {
        let Some(process) = session.processes.iter().find(|p| p.pid == pid) else {
            break;
        };
        chain.push(process.name.clone());
        pid = process.parent_pid;
    }
    if chain.is_empty() {
        return session.argv.join(" ");
    }
    let foreground = chain.remove(0);
    chain.reverse();
    let parents = if chain.is_empty() {
        String::new()
    } else {
        format!(" · {}", chain.join(" → "))
    };
    format!("{foreground} [{}]{parents}", session.foreground_pid)
}

pub(crate) fn snapshot_deletion_queued(saved: &super::snapshots::SavedEnvironment, upid: &str) {
    let style = stdout();
    style.success(&format!("Deletion queued: {}", saved.name));
    style.stdout_hint("Proxmox will finish deleting the saved disks in the background.");
    style.stdout_hint(&format!(
        "Check Proxmox → {} → Tasks (VMID {}) for the result.",
        saved.node, saved.vmid
    ));
    if super::progress::verbose() {
        style.stdout_metadata("task", upid);
    }
}

pub(crate) fn saved_environments(saved: &[super::snapshots::SavedEnvironment]) {
    let style = stdout();
    style.stdout_heading("Snapshots");
    if saved.is_empty() {
        style.stdout_hint("No saved environments. Use pbox snapshot create current --name NAME.");
    }
    for snapshot in saved {
        style.stdout_metadata("name", &snapshot.name);
        style.stdout_metadata("id", &snapshot.id);
        style.stdout_metadata(
            "state",
            if snapshot.ready {
                "ready"
            } else {
                "incomplete"
            },
        );
        style.stdout_metadata("created", &snapshot.created);
        style.stdout_metadata(
            "location",
            &format!("{} / VMID {}", snapshot.node, snapshot.vmid),
        );
        style.stdout_metadata("source", &snapshot.source);
    }
    if !saved.is_empty() {
        style.command("pbox new --snapshot NAME");
    }
}

#[cfg(test)]
mod design_tests {
    use super::*;

    #[test]
    fn resource_sizes_keep_missing_values_zero_quotas_and_fractional_sizes_distinct() {
        assert_eq!(disk_allocation(None), "Not reported");
        assert_eq!(disk_allocation(Some("0T")), "No quota");
        assert_eq!(disk_allocation(Some("0")), "No quota");
        assert_eq!(disk_allocation(Some("8G")), "8 GiB");
        assert_eq!(disk_allocation(Some("1.5G")), "1.5 GiB");
        assert_eq!(disk_allocation(Some("unknown")), "unknown");
    }

    #[test]
    fn titles_are_prefixed_across_every_chunk_boundary() {
        let input = b"prompt \x1b]0;shell\x07\x1b]2;editor\x1b\\\x1b]1;icon\x07 done";
        let expected = "prompt \x1b]0;my-box · shell\x07\x1b]2;my-box · editor\x1b\\\x1b]1;my-box · icon\x07 done".as_bytes();
        for size in 1..=input.len() {
            let mut filter = TitlePrefix::new(Some("my-box"));
            let mut output = Vec::new();
            for chunk in input.chunks(size) {
                output.extend(filter.push(chunk));
            }
            output.extend(filter.finish());
            assert_eq!(output, expected, "chunk size {size}");
        }
    }

    #[test]
    fn title_proxy_preserves_other_output_and_redirected_streams() {
        let input = b"\x1b[31mred\x1b[0m\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x07\x1b]52;c;data\x07\xff\x1b";
        let mut filter = TitlePrefix::new(Some("box"));
        let mut output = Vec::new();
        for byte in input {
            output.extend(filter.push(&[*byte]));
        }
        output.extend(filter.finish());
        assert_eq!(output, input);
        assert_eq!(
            TitlePrefix::new(None).push(b"\x1b]2;title\x07"),
            b"\x1b]2;title\x07"
        );
    }

    #[test]
    fn live_logs_are_bounded_and_cannot_move_the_cursor() {
        let mut display = CreationDisplay::new();
        for n in 0..100 {
            display.log(format!("line {n}"));
        }
        assert_eq!(display.logs.len(), 3);
        assert_eq!(display.logs.front().unwrap(), "line 97");
        assert_eq!(clip_terminal_text("abc\x1b[2Jdef", 6), "abc...");
        display.substep("next".to_owned());
        assert!(display.logs.is_empty());
    }

    #[test]
    fn confirmation_uses_shared_prompt_tokens_in_colour_and_plain_text() {
        let plain = CliStyle::from_enabled(false).prompt_text("Delete permanently?", Some("y/N"));
        assert_eq!(plain, "? Delete permanently?            [y/N]: ");
        let colour = CliStyle::from_enabled(true).prompt_text("Delete permanently?", Some("y/N"));
        assert!(colour.starts_with(&format!("{ANSI_CYAN}?{ANSI_RESET}")));
        assert!(colour.contains(&format!("{ANSI_DIM}[y/N]{ANSI_RESET}")));
        assert!(!plain.contains('\x1b'));
    }

    #[test]
    fn status_roles_are_distinguishable_without_colour() {
        let style = CliStyle::from_enabled(false);
        for (marker, code) in [
            ("ok", ANSI_GREEN),
            (">", ANSI_CYAN),
            ("!", ANSI_YELLOW),
            ("x", ANSI_RED),
        ] {
            assert_eq!(
                style.status(marker, code, "Message"),
                format!("{marker} Message")
            );
        }
    }

    #[test]
    fn metadata_and_prompt_values_cannot_inject_terminal_controls() {
        let style = CliStyle::from_enabled(true);
        let rendered = style.prompt_text("Name\x1b[2J", Some("test\nnext"));
        assert!(!rendered.contains("\x1b[2J"));
        assert!(!rendered.contains('\n'));
    }

    #[test]
    fn agent_disconnect_errors_are_actionable_and_compact() {
        let error = anyhow::anyhow!(
            "read pbox-agent PTY output: peer closed connection without sending TLS close_notify"
        );
        assert_eq!(
            concise_error(&error),
            "pbox-agent connection closed unexpectedly; retry the command"
        );
    }
}

#[cfg(test)]
mod image_feedback_preview {
    use super::*;

    /// Manual terminal/plain/JSON-mode design-system preview; no PVE access.
    #[test]
    #[ignore = "manual design-system preview"]
    fn render_image_feedback() {
        let mode = std::env::var("PBOX_PREVIEW_MODE").unwrap_or_default();
        configure(
            if mode == "color" {
                ColorChoice::Always
            } else {
                ColorChoice::Never
            },
            mode == "json",
        );
        image_preparation_error(
            "docker.io/library/alpine:latest",
            "Image compatibility check failed: Alpine uses musl and OpenRC; this agent requires glibc and systemd. Choose a supported systemd image.",
        );
        let record = BoxRecord {
            image: None,
            id: pbox_core::PboxId::parse("pbx_test1234").unwrap(),
            vmid: 9000,
            state: "running".to_owned(),
            node: "pve".to_owned(),
            ip: None,
            ipv6: None,
            name: Some("pbox-test".to_owned()),
            recipes: Vec::new(),
            capabilities: Vec::new(),
            ping: None,
        };
        let mut metadata = pbox_core::PboxMetadata::new(record.id.clone(), record.vmid);
        metadata.image = Some("docker.io/cachyos/cachyos:latest".to_owned());
        delete_details(&record, Some(&metadata), false, stderr());
    }
}

/// Show the exact terminal affected before asking for destructive confirmation.
pub(crate) fn confirm_terminal_close(
    reference: &str,
    box_id: &str,
    session: &pbox_agent_client::TerminalSession,
) {
    let style = stderr();
    style.section("Close terminal session");
    style.metadata(
        "box",
        &if reference == box_id {
            box_id.to_owned()
        } else {
            format!("{reference} · {box_id}")
        },
    );
    style.metadata("session", &session.name);
    style.metadata(
        "state",
        if session.attached {
            "attached"
        } else {
            "detached"
        },
    );
    style.metadata("user", &session.user);
    style.metadata("started in", &session.cwd);
    style.metadata("command", &session.argv.join(" "));
    style
        .warning("This ends the terminal and all its running processes. Unsaved work may be lost.");
    if session.attached {
        style.hint("The connected terminal will be disconnected.");
    }
}

pub(crate) fn confirm_agent_session_shutdown(
    box_id: &str,
    sessions: &[pbox_agent_client::TerminalSession],
) {
    let style = stderr();
    style.section("End legacy terminals and update agent");
    style.metadata("box", box_id);
    for session in sessions {
        style.metadata(
            "session",
            &format!(
                "{} · {} · {}",
                session.name,
                if session.attached {
                    "attached"
                } else {
                    "detached"
                },
                session.user
            ),
        );
        style.metadata("started in", &session.cwd);
        style.metadata("command", &session.argv.join(" "));
    }
    style.warning(
        "This ends every listed terminal and its running processes. Unsaved work may be lost.",
    );
}

pub(crate) fn agent_update_result(
    reference: &str,
    box_id: &str,
    status: &str,
    version: &str,
    blocked: &[String],
    reason: Option<&str>,
) {
    let style = stdout();
    style.stdout_heading("Guest agent");
    style.stdout_metadata(
        "box",
        &if reference == box_id {
            box_id.to_owned()
        } else {
            format!("{reference} · {box_id}")
        },
    );
    style.stdout_metadata("version", version);
    style.stdout_metadata("status", status);
    if !blocked.is_empty() {
        style.stdout_metadata("terminals", &blocked.join(", "));
        stderr().warning("Update blocked: the old agent owns these terminals. Ending them will stop their running processes.");
        style.command(&format!("pbox agent update {box_id} --kill-sessions"));
        style.stdout_hint("Add --yes to confirm without a prompt.");
    } else if let Some(reason) = reason {
        stderr().error(reason);
    }
}

/// Status for the decoded, read-only viewer only. Never wrap live guest output:
/// cursor tracking cannot reproduce every control supported by the host terminal.
struct TerminalStatus(std::cell::RefCell<StatusState>);
struct StatusState {
    parser: vt100::Parser<NativeControls>,
    filter: ViewportFilter,
    rows: u16,
    cols: u16,
    label: String,
    finished: bool,
    read_only: bool,
    colour: bool,
    resources: Option<std::sync::mpsc::Receiver<Option<pbox_core::pve::LxcUsage>>>,
    usage: Option<(std::time::Instant, pbox_core::pve::LxcUsage)>,
}
impl StatusState {
    fn content_rows(&self) -> u16 {
        if self.rows >= 3 {
            self.rows - 1
        } else {
            self.rows
        }
    }
    fn margins(&self) -> Vec<u8> {
        let (row, col) = self.parser.screen().cursor_position();
        format!(
            "\x1b[?6l\x1b[1;{}r{}\x1b[{};{}H",
            self.content_rows(),
            if self.filter.origin_mode {
                "\x1b[?6h"
            } else {
                ""
            },
            (row + 1).min(self.content_rows()),
            col + 1
        )
        .into_bytes()
    }
    fn bar(&self) -> Vec<u8> {
        if self.rows < 3 {
            return Vec::new();
        }
        let usage = self.resources.as_ref().map(|_| {
            resource_label(
                self.usage
                    .as_ref()
                    .filter(|(time, _)| time.elapsed().as_secs() < 15)
                    .map(|(_, usage)| usage),
            )
        });
        let suffix = if self.read_only {
            "Read-only · Ctrl+C exit"
        } else {
            "Ctrl+] detach"
        };
        let label = fit_resource_bar(&self.label, suffix, usage.as_deref(), self.cols as usize);
        let label = colour_resource_bar(&label, usage.as_deref(), self.colour);
        let mut output = format!("\x1b[?6l\x1b[{};1H\x1b[0m{label}\x1b[0m", self.rows).into_bytes();
        if self.filter.origin_mode {
            output.extend_from_slice(b"\x1b[?6h");
        }
        let cursor = self.parser.screen().cursor_state_formatted();
        // Painting never changes visibility or shape, so only restore position.
        // Reasserting ?25h here resets cursor blinking in some terminals.
        let cursor = cursor
            .strip_prefix(b"\x1b[?25h")
            .or_else(|| cursor.strip_prefix(b"\x1b[?25l"))
            .unwrap_or(&cursor)
            .to_vec();
        output.extend(if self.filter.origin_mode {
            relative_cursor(cursor, self.filter.top.max(1))
        } else {
            cursor
        });
        output.extend(self.parser.screen().attributes_formatted());
        output
    }
    fn output(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        for (bytes, reset_margins) in self.filter.push(bytes, self.content_rows()) {
            self.parser.process(&bytes);
            output.extend(bytes);
            if reset_margins {
                self.filter.top = 1;
                let margins = self.margins();
                self.parser.process(&margins);
                output.extend(margins);
            }
        }
        // Do not put a status line inside an unfinished OSC/DCS string.
        if self.filter.can_paint() {
            output.extend(self.bar());
        }
        output
    }
}
impl TerminalStatus {
    fn enter(box_name: &str, session: &str) -> Option<Self> {
        if !io::stdin().is_terminal() || !stdout().can_animate() {
            return None;
        }
        let (rows, cols) = super::terminal_size();
        if rows < 3 || cols < 8 {
            return None;
        }
        let mut state = StatusState {
            parser: vt100::Parser::new_with_callbacks(rows as u16, cols as u16, 0, NativeControls),
            filter: ViewportFilter::default(),
            rows: rows as u16,
            cols: cols as u16,
            label: format!("{box_name}:{session}"),
            read_only: true,
            colour: stdout().enabled,
            resources: None,
            usage: None,
            finished: false,
        };
        let initial = format!(
            "\r\n\r\n\x1b[1;{}r\x1b[{};1H",
            state.content_rows(),
            state.content_rows()
        );
        state.parser.process(initial.as_bytes());
        let mut output = initial.into_bytes();
        output.extend(state.bar());
        let _ = io::stdout().write_all(&output);
        let _ = io::stdout().flush();
        Some(Self(std::cell::RefCell::new(state)))
    }
    pub(crate) fn monitor_resources(
        &self,
        receiver: std::sync::mpsc::Receiver<Option<pbox_core::pve::LxcUsage>>,
    ) {
        self.0.borrow_mut().resources = Some(receiver);
    }
    pub(crate) fn refresh_resources(&self) {
        let mut state = self.0.borrow_mut();
        let before = resource_label(state.usage.as_ref().map(|(_, usage)| usage));
        let sample = state.resources.as_ref().and_then(|r| r.try_iter().last());
        if let Some(sample) = sample {
            state.usage = sample.map(|s| (std::time::Instant::now(), s));
        }
        if state
            .usage
            .as_ref()
            .is_some_and(|(time, _)| time.elapsed().as_secs() >= 15)
        {
            state.usage = None;
        }
        let after = resource_label(state.usage.as_ref().map(|(_, usage)| usage));
        if before != after && state.filter.can_paint() {
            write_view(&state.bar());
        }
    }
    pub(crate) fn size(&self) -> (u32, u32) {
        let state = self.0.borrow();
        (u32::from(state.content_rows()), u32::from(state.cols))
    }
    pub(crate) fn output(&self, bytes: Vec<u8>) -> Vec<u8> {
        self.0.borrow_mut().output(&bytes)
    }
    pub(crate) fn resize(&self, rows: u32, cols: u32) {
        let mut state = self.0.borrow_mut();
        state.rows = rows.max(1) as u16;
        state.cols = cols.max(1) as u16;
        state
            .parser
            .screen_mut()
            .set_size(rows.max(1) as u16, cols.max(1) as u16);
        state.filter.top = 1;
        let margins = state.margins();
        state.parser.process(&margins);
        let mut output = margins;
        output.extend(state.bar());
        let _ = io::stdout().write_all(&output);
        let _ = io::stdout().flush();
    }
    pub(crate) fn finish(&self) {
        let mut state = self.0.borrow_mut();
        if state.finished {
            return;
        }
        state.finished = true;
        // Keep the final status as context in scrollback, then restore the full
        // terminal area. CAN cancels an incomplete control string after a drop.
        let output = format!("\x18\x1b[?6l\x1b[r\x1b[{};1H\r\n", state.rows);
        let _ = io::stdout().write_all(output.as_bytes());
        let _ = io::stdout().flush();
    }
}
impl Drop for TerminalStatus {
    fn drop(&mut self) {
        self.finish();
    }
}

struct NativeControls;
impl vt100::Callbacks for NativeControls {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        prefix: Option<u8>,
        _: Option<u8>,
        params: &[&[u16]],
        action: char,
    ) {
        pbox_agent_client::terminal_extension(screen, prefix, params, action);
    }
}
fn write_view(bytes: &[u8]) {
    if !bytes.is_empty() {
        let _ = io::stdout().write_all(bytes);
        let _ = io::stdout().flush();
    }
}
fn resource_label(usage: Option<&pbox_core::pve::LxcUsage>) -> String {
    let Some(usage) = usage else {
        return "⚙ CPU —  🧠 RAM —  💾 DISK —".into();
    };
    fn percent(used: Option<u64>, total: Option<u64>) -> String {
        match (used, total.filter(|n| *n > 0)) {
            (Some(used), Some(total)) => format!("{:.0}%", used as f64 / total as f64 * 100.0),
            _ => "—".into(),
        }
    }
    let cpu = usage
        .cpu
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| format!("{:.0}%", n * 100.0))
        .unwrap_or_else(|| "—".into());
    format!(
        "⚙ CPU {cpu}  🧠 RAM {}  💾 DISK {}",
        percent(usage.mem, usage.maxmem),
        percent(usage.disk, usage.maxdisk)
    )
}
fn colour_resource_bar(bar: &str, usage: Option<&str>, enabled: bool) -> String {
    if !enabled {
        return bar.to_owned();
    }
    let style = CliStyle::from_enabled(true);
    let (left, metrics) = usage
        .and_then(|usage| bar.rfind(usage).map(|start| (&bar[..start], &bar[start..])))
        .unwrap_or((bar, ""));
    let mut result = if let Some((name, controls)) = left.split_once("  |  ") {
        format!(
            "{}{}",
            style.paint(&format!("{ANSI_DIM}{ANSI_CYAN}"), name),
            style.paint(ANSI_DIM, &format!("  |  {controls}"))
        )
    } else {
        style.paint(&format!("{ANSI_DIM}{ANSI_CYAN}"), left)
    };
    for (i, metric) in metrics.split("  ").enumerate() {
        if i > 0 {
            result.push_str("  ");
        }
        let Some((label, value)) = metric.trim_end().rsplit_once(' ') else {
            result.push_str(metric);
            continue;
        };
        result.push_str(&style.paint(ANSI_DIM, &format!("{label} ")));
        let percent = value.strip_suffix('%').and_then(|n| n.parse::<u32>().ok());
        let colour = match percent {
            Some(95..) => "\x1b[31m",
            Some(80..) => "\x1b[33m",
            Some(_) => "\x1b[32m",
            None => ANSI_DIM,
        };
        result.push_str(&style.paint(colour, value));
        result.push_str(&metric[metric.trim_end().len()..]);
    }
    result
}

fn fit_resource_bar(label: &str, suffix: &str, usage: Option<&str>, width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    if let Some(usage) = usage.filter(|usage| width >= usage.width() + 40) {
        let left = fit_view_label(label, suffix, width - usage.width() - 2);
        format!("{left} {usage} ")
    } else {
        fit_view_label(label, suffix, width)
    }
}

fn fit_view_label(label: &str, suffix: &str, width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    let suffix_width = suffix.width();
    let room = width.saturating_sub(suffix_width + 6);
    let mut name = String::new();
    let mut used = 0;
    for c in safe_terminal_text(label).chars() {
        let cells = c.width().unwrap_or(0);
        if used + cells > room {
            break;
        }
        name.push(c);
        used += cells;
    }
    let text = format!(" {name}  |  {suffix}");
    let mut output = String::new();
    let mut used = 0;
    for c in text.chars() {
        let cells = c.width().unwrap_or(0);
        if used + cells > width {
            break;
        }
        output.push(c);
        used += cells;
    }
    output.push_str(&" ".repeat(width - used));
    output
}
#[derive(Default)]
struct ViewportFilter {
    state: u8, // ground, escape, CSI, string, string escape, escape intermediate
    pending: Vec<u8>,
    osc: bool,
    origin_mode: bool,
    saved_origin: bool,
    top: u16,
    utf8_remaining: u8,
}
impl ViewportFilter {
    fn can_paint(&self) -> bool {
        self.state == 0 && self.utf8_remaining == 0
    }
    fn push(&mut self, bytes: &[u8], rows: u16) -> Vec<(Vec<u8>, bool)> {
        let mut pieces = Vec::new();
        let mut output = Vec::new();
        for &byte in bytes {
            match self.state {
                0 if byte == 0x1b => {
                    self.pending.push(byte);
                    self.state = 1;
                }
                0 => {
                    self.utf8_remaining = match byte {
                        0xc2..=0xdf => 1,
                        0xe0..=0xef => 2,
                        0xf0..=0xf4 => 3,
                        0x80..=0xbf => self.utf8_remaining.saturating_sub(1),
                        _ => 0,
                    };
                    output.push(byte);
                }
                1 => {
                    self.pending.push(byte);
                    if byte == b'[' {
                        self.state = 2;
                        continue;
                    }
                    output.append(&mut self.pending);
                    if byte == b'7' {
                        self.saved_origin = self.origin_mode;
                    }
                    if byte == b'8' {
                        self.origin_mode = self.saved_origin;
                    }
                    if byte == b'c' {
                        self.origin_mode = false;
                        self.saved_origin = false;
                    }
                    if matches!(byte, b']' | b'P' | b'X' | b'^' | b'_') {
                        self.osc = byte == b']';
                        self.state = 3;
                    } else if (0x20..=0x2f).contains(&byte) {
                        self.state = 5;
                    } else {
                        self.state = 0;
                        if byte == b'c' {
                            pieces.push((std::mem::take(&mut output), true));
                        }
                    }
                }
                2 => {
                    self.pending.push(byte);
                    if (0x40..=0x7e).contains(&byte) {
                        let body = &self.pending[2..self.pending.len() - 1];
                        if byte == b'r' && body.iter().all(|b| b.is_ascii_digit() || *b == b';') {
                            let text = String::from_utf8_lossy(body);
                            let mut values = text.split(';');
                            let top = values
                                .next()
                                .and_then(|v| v.parse::<u16>().ok())
                                .filter(|v| *v > 0)
                                .unwrap_or(1);
                            let bottom = values
                                .next()
                                .and_then(|v| v.parse::<u16>().ok())
                                .filter(|v| *v > 0)
                                .unwrap_or(rows)
                                .min(rows);
                            if top < bottom {
                                self.top = top;
                                output.extend(format!("\x1b[{top};{bottom}r").bytes());
                            }
                        } else {
                            output.extend_from_slice(&self.pending);
                        }
                        if matches!(byte, b'h' | b'l')
                            && body.strip_prefix(b"?").is_some_and(|body| {
                                body.split(|b| *b == b';').any(|mode| mode == b"6")
                            })
                        {
                            self.origin_mode = byte == b'h';
                        }
                        let buffer_switch = matches!(byte, b'h' | b'l')
                            && body.strip_prefix(b"?").is_some_and(|body| {
                                body.split(|b| *b == b';')
                                    .any(|mode| matches!(mode, b"47" | b"1047" | b"1049"))
                            });
                        self.pending.clear();
                        self.state = 0;
                        if buffer_switch {
                            pieces.push((std::mem::take(&mut output), true));
                        }
                    } else if self.pending.len() >= 256 {
                        output.append(&mut self.pending);
                        self.state = 0;
                    }
                }
                3 => {
                    output.push(byte);
                    if byte == 0x1b {
                        self.state = 4;
                    } else if self.osc && byte == 7 || matches!(byte, 0x18 | 0x1a) {
                        self.state = 0;
                    }
                }
                4 => {
                    output.push(byte);
                    self.state = if byte == b'\\' || self.osc && byte == 7 {
                        0
                    } else if byte == 0x1b {
                        4
                    } else {
                        3
                    };
                }
                5 => {
                    output.push(byte);
                    if !(0x20..=0x2f).contains(&byte) {
                        self.state = 0;
                    }
                }
                _ => unreachable!(),
            }
        }
        if !output.is_empty() {
            pieces.push((output, false));
        }
        pieces
    }
}

/// Older owners send a full-screen snapshot as their first attachment event.
/// Convert that known replay envelope too, without requiring their PTYs to end.
pub(crate) fn append_initial_screen(bytes: Vec<u8>, rows: u32, cols: u32) -> Vec<u8> {
    if !bytes.starts_with(b"\x1b[?1049l") {
        return bytes;
    }
    let Some(modes) = bytes
        .windows(b"\x1b[<16u".len())
        .rposition(|part| part == b"\x1b[<16u")
    else {
        return bytes;
    };
    let mut parser = vt100::Parser::new(rows.clamp(1, 200) as u16, cols.clamp(1, 500) as u16, 0);
    parser.process(&bytes);
    let mut output = pbox_agent_client::append_terminal_screen(parser.screen());
    output.extend_from_slice(&bytes[modes..]);
    output
}

// Cursor snapshots contain VT cursor/attribute commands, not arbitrary OSC data.
fn relative_cursor(bytes: Vec<u8>, top: u16) -> Vec<u8> {
    let mut output = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"\x1b[")
            && let Some(end) = bytes[index + 2..]
                .iter()
                .position(|byte| (0x40..=0x7e).contains(byte))
        {
            let end = index + 2 + end;
            if bytes[end] == b'H' {
                let params = String::from_utf8_lossy(&bytes[index + 2..end]);
                let mut params = params.split(';');
                let row = params
                    .next()
                    .and_then(|value| value.parse::<u16>().ok())
                    .unwrap_or(1);
                let col = params
                    .next()
                    .and_then(|value| value.parse::<u16>().ok())
                    .unwrap_or(1);
                output
                    .extend(format!("\x1b[{};{col}H", row.saturating_sub(top - 1).max(1)).bytes());
            } else {
                output.extend_from_slice(&bytes[index..=end]);
            }
            index = end + 1;
        } else {
            output.push(bytes[index]);
            index += 1;
        }
    }
    output
}

/// An inline, read-only screen viewer. Large screens are printed in full so
/// their content remains available in scrollback instead of being truncated.
pub(crate) struct SessionViewer {
    status: Option<TerminalStatus>,
    previous: Vec<String>,
    previous_size: (u32, u32),
}
impl SessionViewer {
    pub(crate) fn monitor_resources(&self, config: super::Config, id: String) {
        if let Some(status) = &self.status {
            status.monitor_resources(super::sessions::resource_monitor(config, id));
        }
    }
    pub(crate) fn new(box_name: &str, session: &str) -> Self {
        let status = TerminalStatus::enter(box_name, session);
        if status.is_none() {
            stdout().stdout_heading(&format!("Watching {box_name}:{session} (read-only)"));
            stdout().stdout_hint("Ctrl+C exits the viewer. The session keeps running.");
        }
        Self {
            status,
            previous: Vec::new(),
            previous_size: super::terminal_size(),
        }
    }
    pub(crate) fn render(&mut self, text: &str) {
        if let Some(status) = &self.status {
            status.refresh_resources();
        }
        use unicode_width::UnicodeWidthChar;
        let size = super::terminal_size();
        if size != self.previous_size
            && let Some(status) = &self.status
        {
            status.resize(size.0, size.1);
        }
        let width = size.1.saturating_sub(1).max(1) as usize;
        let mut lines = Vec::new();
        for line in text.lines() {
            let mut part = String::new();
            let mut used = 0;
            for character in safe_terminal_text(line).chars() {
                let cells = character.width().unwrap_or(0);
                if used + cells > width && !part.is_empty() {
                    lines.push(std::mem::take(&mut part));
                    used = 0;
                }
                part.push(character);
                used += cells;
            }
            lines.push(part);
        }
        if lines == self.previous && size == self.previous_size {
            return;
        }
        let mut output = Vec::new();
        if let Some(status) = &self.status {
            let available = status.size().0.saturating_sub(1) as usize;
            let inline = self.previous_size == size
                && self.previous.len() <= available
                && lines.len() <= available;
            let count = if inline {
                self.previous.len().max(lines.len())
            } else {
                lines.len()
            };
            if inline && !self.previous.is_empty() {
                output.extend(format!("\x1b[{}A", self.previous.len()).bytes());
            }
            for i in 0..count {
                if inline {
                    output.extend_from_slice(b"\r\x1b[2K");
                }
                if let Some(line) = lines.get(i) {
                    output.extend_from_slice(line.as_bytes());
                }
                output.extend_from_slice(b"\r\n");
            }
            if count > lines.len() {
                output.extend(format!("\x1b[{}A", count - lines.len()).bytes());
            }
            let output = status.output(output);
            let _ = io::stdout().write_all(&output);
            let _ = io::stdout().flush();
        } else {
            println!("{text}\n");
        }
        self.previous = lines;
        self.previous_size = size;
    }
    pub(crate) fn finish(&self) {
        if let Some(status) = &self.status {
            status.finish();
        }
    }
}

#[cfg(test)]
mod viewport_tests {
    use super::*;
    fn state() -> StatusState {
        let mut parser = vt100::Parser::new_with_callbacks(6, 40, 0, NativeControls);
        parser.process(b"\x1b[1;5r\x1b[5;1H");
        StatusState {
            parser,
            filter: ViewportFilter::default(),
            rows: 6,
            cols: 40,
            label: "work:main".into(),
            read_only: false,
            colour: true,
            resources: None,
            usage: None,
            finished: false,
        }
    }
    #[test]
    fn bar_colours_keep_the_background_and_plain_layout() {
        let usage = "⚙ CPU 12%  🧠 RAM 85%  💾 DISK 96%";
        let plain = fit_resource_bar("work:main", "Ctrl+] detach", Some(usage), 120);
        let coloured = colour_resource_bar(&plain, Some(usage), true);
        let mut terminal = vt100::Parser::new(2, 120, 0);
        terminal.process(coloured.as_bytes());
        assert_eq!(terminal.screen().rows(0, 120).next().unwrap(), plain);
        assert_eq!(
            terminal.screen().cell(0, 1).unwrap().fgcolor(),
            vt100::Color::Idx(6)
        );
        for col in 0..120 {
            let cell = terminal.screen().cell(0, col).unwrap();
            assert_eq!(cell.bgcolor(), vt100::Color::Default);
            assert!(!cell.inverse());
        }
        assert_eq!(colour_resource_bar(&plain, Some(usage), false), plain);
        assert!(coloured.contains("\x1b[33m85%"));
        assert!(coloured.contains("\x1b[31m96%"));
    }
    #[test]
    fn resource_bar_is_bounded_and_missing_values_are_not_zero() {
        use unicode_width::UnicodeWidthStr;
        let usage = pbox_core::pve::LxcUsage {
            cpu: Some(0.125),
            mem: Some(250),
            maxmem: Some(1000),
            disk: Some(5),
            maxdisk: Some(0),
        };
        assert_eq!(
            resource_label(Some(&usage)),
            "⚙ CPU 12%  🧠 RAM 25%  💾 DISK —"
        );
        let bar = fit_resource_bar(
            "pbox-test:viewer-1234567890",
            "Ctrl+] detach",
            Some("⚙ CPU 12%  🧠 RAM 25%  💾 DISK —"),
            100,
        );
        assert!(bar.contains("pbox-test:viewer-1234567890"));
        assert!(bar.contains("Ctrl+] detach"));
        for width in [8, 40, 80, 120] {
            let bar = fit_resource_bar(
                "box:main",
                "Ctrl+] detach",
                Some("⚙ CPU 12%  🧠 RAM 25%  💾 DISK —"),
                width,
            );
            assert_eq!(bar.width(), width);
            if width >= 40 {
                assert!(bar.contains("Ctrl+] detach"));
            }
            if width >= 80 {
                assert!(bar.contains("CPU 12%"));
            }
        }
    }
    #[test]
    fn native_screen_mouse_and_cursor_controls_pass_through() {
        let mut state = state();
        let input = b"\x1b[?1000h\x1b[?1006h\x1b[6 q\x1b[?12l\x1b[?25l\x1b[6n";
        let output = state.output(input);
        assert!(output.starts_with(input));
        assert!(!output.windows(8).any(|part| part == b"\x1b[?1049h"));
        let output = state.output(b"plain output");
        assert!(output.starts_with(b"plain output"));
        assert!(!output.windows(5).any(|part| part == b"\x1b[?25"));
        assert!(!output.windows(8).any(|part| part == b"\x1b[?1000h"));
        assert!(state.parser.screen().hide_cursor());
    }
    #[test]
    fn reserved_row_survives_scrolling_and_guest_margin_resets() {
        let mut state = state();
        let mut terminal = vt100::Parser::new(6, 40, 200);
        terminal.process(b"\x1b[1;5r\x1b[5;1H");
        for line in 0..20 {
            terminal.process(&state.output(format!("\x1b[rline {line}\r\n").as_bytes()));
        }
        let rows = terminal.screen().rows(0, 40).collect::<Vec<_>>();
        assert!(rows[5].contains("work:main"));
        assert!(rows[..5].iter().any(|row| row == "line 19"));
        assert_eq!(state.content_rows(), 5);
        terminal.process(&state.output(b"\x1b[?1049h\x1b[5;1HALL FIVE GUEST ROWS"));
        assert!(
            terminal
                .screen()
                .rows(0, 40)
                .nth(4)
                .unwrap()
                .contains("ALL FIVE GUEST ROWS")
        );
        assert!(
            terminal
                .screen()
                .rows(0, 40)
                .nth(5)
                .unwrap()
                .contains("work:main")
        );
    }
    #[test]
    fn origin_mode_does_not_put_the_status_inside_guest_content() {
        let mut state = state();
        let mut terminal = vt100::Parser::new(6, 40, 0);
        terminal.process(b"\x1b[1;5r\x1b[5;1H");
        terminal.process(&state.output(b"\x1b[2;5r\x1b[?6h\x1b[2;1HORIGIN"));
        assert!(
            terminal
                .screen()
                .rows(0, 40)
                .nth(5)
                .unwrap()
                .contains("work:main")
        );
        assert_eq!(
            terminal.screen().cursor_position(),
            state.parser.screen().cursor_position()
        );
        terminal.process(&state.output(b" CONTENT"));
        assert!(terminal.screen().contents().contains("ORIGIN CONTENT"));
    }

    #[test]
    fn status_does_not_overwrite_the_guest_saved_cursor() {
        let mut state = state();
        let mut terminal = vt100::Parser::new(6, 40, 0);
        terminal.process(b"\x1b[1;5r\x1b[5;1H");
        terminal.process(&state.output(b"\x1b[2;4H\x1b7"));
        terminal.process(&state.output(b"elsewhere"));
        terminal.process(&state.output(b"\x1b8Z"));
        assert_eq!(terminal.screen().cell(1, 3).unwrap().contents(), "Z");
    }
    #[test]
    fn painting_preserves_split_unicode_and_last_column_wrapping() {
        let mut state = state();
        let mut terminal = vt100::Parser::new(6, 40, 0);
        terminal.process(b"\x1b[1;5r\x1b[5;1H");
        let input = format!("\x1b[2;1H\x1b(B{}界next", "a".repeat(38));
        for byte in input.as_bytes() {
            terminal.process(&state.output(&[*byte]));
        }
        for row in 0..5 {
            for col in 0..40 {
                assert_eq!(
                    terminal.screen().cell(row, col).unwrap().contents(),
                    state.parser.screen().cell(row, col).unwrap().contents(),
                    "row {row} column {col}"
                );
            }
        }
        assert!(
            terminal
                .screen()
                .rows(0, 40)
                .nth(2)
                .unwrap()
                .starts_with("next")
        );
    }
    #[test]
    fn viewport_filter_handles_split_sequences_and_does_not_rewrite_strings() {
        let input = b"before\x1b]2;title \x1b[r\x07after\x1b[r";
        for chunk in 1..=input.len() {
            let mut filter = ViewportFilter::default();
            let mut output = Vec::new();
            for part in input.chunks(chunk) {
                for (bytes, _) in filter.push(part, 5) {
                    output.extend(bytes);
                }
            }
            assert_eq!(output, b"before\x1b]2;title \x1b[r\x07after\x1b[1;5r");
        }
    }
    #[test]
    fn legacy_main_screen_restore_keeps_local_output_visible() {
        let mut old = vt100::Parser::new(24, 80, 0);
        old.process(b"$ pending");
        let mut bytes = b"\x1b[?1049l".to_vec();
        bytes.extend(old.screen().state_formatted());
        bytes.extend_from_slice(b"\x1b[<16u\x1b[=0u");
        let bytes = append_initial_screen(bytes, 24, 80);
        let mut local = vt100::Parser::new(24, 80, 0);
        local.process(b"existing local output");
        local.process(&bytes);
        assert_eq!(
            local.screen().contents(),
            "existing local output\n$ pending"
        );
    }

    #[test]
    fn legacy_replay_uses_the_new_attachment_dimensions() {
        // An older owner expands the saved screen before sending its snapshot.
        let mut remote = vt100::Parser::new(40, 120, 0);
        remote.process(b"\x1b[37;1Hresponse end\x1b[39;1H> unsent draft");
        let mut replay = b"\x1b[?1049l".to_vec();
        replay.extend(remote.screen().state_formatted());
        replay.extend_from_slice(b"\x1b[<16u\x1b[=0u");
        let output = append_initial_screen(replay, 40, 120);
        let mut local = vt100::Parser::new(40, 120, 0);
        local.process(&output);
        assert!(local.screen().contents().contains("response end"));
        assert!(local.screen().contents().contains("> unsent draft"));
        assert_eq!(local.screen().cursor_position().1, 14);
    }
}
