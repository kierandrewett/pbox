//! Copy pbox credentials from another machine over the user's SSH connection.
use crate::ui;
use anyhow::{Context, Result, bail};
use clap::Args;
use serde::Serialize;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize)]
struct PullOutput {
    file: PathBuf,
    unchanged: bool,
}

#[derive(Debug, Args)]
pub(super) struct RemoteCopyArgs {
    /// Replace the local credential if its contents differ from the remote file.
    #[arg(long)]
    force: bool,
    /// OpenSSH options followed by the destination as the final argument. Put `--` before them.
    #[arg(
        last = true,
        required = true,
        allow_hyphen_values = true,
        value_name = "SSH_ARGUMENT"
    )]
    pub(super) ssh_args: Vec<OsString>,
}

#[derive(Debug, Args)]
pub(crate) struct SshCopyKeyCommand {
    #[command(flatten)]
    pub(super) common: RemoteCopyArgs,
}

#[derive(Debug, Args)]
pub(crate) struct SshCopyConfigCommand {
    #[command(flatten)]
    pub(super) common: RemoteCopyArgs,
}

pub(crate) fn run_key(command: SshCopyKeyCommand, json: bool) -> Result<()> {
    let path = dirs::config_dir()
        .context("find the user configuration directory")?
        .join("pbox/relay.key");
    copy_file(command.common, "relay.key", path, json)
}

pub(crate) fn run_config(command: SshCopyConfigCommand, json: bool) -> Result<()> {
    copy_file(
        command.common,
        "config.toml",
        pbox_core::config::default_config_path(),
        json,
    )
}

fn copy_file(command: RemoteCopyArgs, name: &str, path: PathBuf, json: bool) -> Result<()> {
    let Some(destination) = command.ssh_args.last() else {
        bail!("provide an SSH destination after the SSH options");
    };
    // OpenSSH options are passed as individual OS arguments; the last supplied
    // argument is the SSH destination, and SSH's agent environment is inherited.
    let options = &command.ssh_args[..command.ssh_args.len() - 1];
    let output = Command::new("ssh")
        .args(options)
        .arg(destination)
        .arg(format!("cat -- \"$HOME\"/.config/pbox/{name}"))
        .stderr(Stdio::inherit())
        .output()
        .with_context(|| format!("start ssh to {}", destination.to_string_lossy()))?;
    if !output.status.success() {
        bail!("ssh failed while reading remote pbox {name}");
    }
    if output.stdout.is_empty() {
        bail!("remote pbox {name} is empty");
    }
    if path.exists() {
        let existing = fs::read(&path).with_context(|| format!("read local {name}"))?;
        if existing == output.stdout {
            report_result(&path, true, json)?;
            return Ok(());
        }
        if !command.force {
            bail!(
                "{} already exists at {} with different contents; use --force to replace it",
                name,
                path.display()
            );
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).context("create local pbox configuration directory")?;
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("read system clock")?
        .as_nanos();
    let temp_path = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut temp = options
            .open(&temp_path)
            .context("create temporary credential file")?;
        temp.write_all(&output.stdout)
            .with_context(|| format!("write downloaded {name}"))?;
        temp.sync_all()
            .with_context(|| format!("flush downloaded {name}"))?;
        fs::rename(&temp_path, &path).with_context(|| format!("install downloaded {name}"))?;
        set_private_permissions(&path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result?;
    report_result(&path, false, json)
}

fn report_result(path: &std::path::Path, unchanged: bool, json: bool) -> Result<()> {
    if json {
        ui::json_text(&serde_json::to_string(&PullOutput {
            file: path.to_owned(),
            unchanged,
        })?);
    } else if unchanged {
        ui::stdout().success("pbox credential already matches remote");
        ui::stdout().metadata("file", &path.display().to_string());
    } else {
        ui::stdout().success("pbox credential copied");
        ui::stdout().metadata("installed", &path.display().to_string());
    }
    Ok(())
}

#[cfg(unix)]
fn set_private_permissions(path: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .context("restrict downloaded credential permissions")
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &std::path::Path) -> Result<()> {
    Ok(())
}
