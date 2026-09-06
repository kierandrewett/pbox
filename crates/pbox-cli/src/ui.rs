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
    style.hint("In a root shell inside the container, inspect the service:");
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
    let width = |header: &str, values: Vec<String>, minimum: usize| {
        values
            .into_iter()
            .map(|value| value.chars().count())
            .chain(std::iter::once(header.chars().count()))
            .max()
            .unwrap_or(minimum)
            .max(minimum)
    };
    let id_width = width("ID", records.iter().map(|r| r.id.to_string()).collect(), 16);
    let state_width = width(
        "STATE",
        records.iter().map(|r| r.state.clone()).collect(),
        10,
    );
    let ping_width = width(
        "PING",
        records
            .iter()
            .map(|r| r.ping.as_deref().unwrap_or("-").to_owned())
            .collect(),
        8,
    );
    let node_width = width("NODE", records.iter().map(|r| r.node.clone()).collect(), 16);
    let ipv4_width = width(
        "IPV4",
        records
            .iter()
            .map(|r| r.ip.as_deref().unwrap_or("-").to_owned())
            .collect(),
        16,
    );
    let ipv6_width = width(
        "IPV6",
        records
            .iter()
            .map(|r| r.ipv6.as_deref().unwrap_or("-").to_owned())
            .collect(),
        39,
    );
    let header = format!(
        "{id:<id_width$} {state:<state_width$} {ping:<ping_width$} {node:<node_width$} {ipv4:<ipv4_width$} {ipv6}NAME",
        id = "ID",
        state = "STATE",
        ping = "PING",
        node = "NODE",
        ipv4 = "IPV4",
        ipv6 = if has_ipv6 {
            format!("{:<width$} ", "IPV6", width = ipv6_width)
        } else {
            String::new()
        },
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
        let name = style.text(record.name.as_deref().unwrap_or("-"));
        let ipv6 = if has_ipv6 {
            format!(
                "{} ",
                format_box_cell(style, record.ipv6.as_deref().unwrap_or("-"), ipv6_width, "")
            )
        } else {
            String::new()
        };
        println!("{id} {state} {ping} {node} {ip} {ipv6}{name}");
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
    loop {
        write!(
            output,
            "{}",
            stderr().prompt_text("Delete permanently?", Some("y/N"))
        )?;
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
        UserAccess::Passwordless => return,
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
        self.completed.clear();
        self.logs.clear();
        self.active = None;
    }
    pub(crate) fn substep(&mut self, action: String) {
        self.active = Some((action, std::time::Instant::now()));
        self.logs.clear();
    }
    pub(crate) fn substep_done(&mut self, action: String, elapsed: u64, success: bool) {
        self.active = None;
        self.completed.push_back((action, elapsed, success));
        while self.completed.len() > 6 {
            self.completed.pop_front();
        }
    }
    pub(crate) fn log(&mut self, line: String) {
        if !line.trim().is_empty() {
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
                for line in &self.logs {
                    self.style.hint(line);
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
