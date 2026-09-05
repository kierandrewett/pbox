use anyhow::{Context, Result, bail};
use pbox_core::metadata::parse_metadata;
use std::fs::File;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// A node-local pct connection. PVE metadata must match before a guest is changed.
pub struct HostGuest<'a> {
    pub host: &'a str,
    pub container: Option<&'a str>,
    pub node: &'a str,
    pub vmid: u64,
    pub box_id: &'a str,
}

impl HostGuest<'_> {
    fn prefix(&self) -> String {
        self.container
            .map(|container| format!("docker exec -i {} ", quote(container)))
            .unwrap_or_default()
    }

    fn command(&self, command: &str) -> Command {
        let mut ssh = Command::new("ssh");
        ssh.args([
            "-T",
            "-a",
            "-o",
            "BatchMode=yes",
            "-o",
            "StrictHostKeyChecking=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=15",
            "-o",
            "ServerAliveCountMax=3",
            "--",
            self.host,
        ])
        .arg(format!("{}{}", self.prefix(), command));
        ssh
    }

    pub fn verify(&self) -> Result<()> {
        let output = self
            .command(&format!("pct config {}", self.vmid))
            .stdin(Stdio::null())
            .output()
            .context("read guest identity through PVE host")?;
        let output = success(output)?;
        let config = String::from_utf8(output.stdout).context("decode pct configuration")?;
        let metadata = parse_metadata(&config)?.context("PVE host guest has no pbox metadata")?;
        if metadata.id.to_string() != self.box_id
            || metadata.vmid != self.vmid
            || metadata.node.as_deref() != Some(self.node)
        {
            bail!(
                "PVE host guest identity does not match {} on node {} (VMID {})",
                self.box_id,
                self.node,
                self.vmid
            );
        }
        // A shared PVE cluster config can describe a guest on a different node.
        let output = self.command("hostname -s").stdin(Stdio::null()).output()?;
        let output = success(output)?;
        if String::from_utf8_lossy(&output.stdout).trim() != self.node {
            bail!(
                "pve.ssh-host does not run the selected PVE node {}",
                self.node
            );
        }
        Ok(())
    }

    pub fn run(&self, script: &str) -> Result<Output> {
        let output = self
            .command(&format!(
                "timeout 180 pct exec {} -- /bin/sh -c {}",
                self.vmid,
                quote(script)
            ))
            .stdin(Stdio::null())
            .output()
            .context("execute guest command through PVE host")?;
        success(output)
    }

    pub fn copy(&self, local: &Path, remote: &str) -> Result<()> {
        let input = File::open(local).with_context(|| format!("open {}", local.display()))?;
        // Stream directly into the guest so private keys never become host files.
        let script = format!("umask 077; cat > {}", quote(remote));
        let output = self
            .command(&format!(
                "timeout 180 pct exec {} -- /bin/sh -c {}",
                self.vmid,
                quote(&script)
            ))
            .stdin(Stdio::from(input))
            .output()
            .context("copy file through PVE host")?;
        success(output).map(|_| ())
    }
}

fn success(output: Output) -> Result<Output> {
    if !output.status.success() {
        bail!(
            "PVE host command failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output)
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_copy_path_is_literal_in_both_shells() {
        let path = "/tmp/a'b;$(false)";
        let script = format!("umask 077; printf ok > {}", quote(path));
        let output = Command::new("sh")
            .args(["-c", &format!("printf '%s' {}", quote(&script))])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8(output.stdout).unwrap(), script);
    }
}
