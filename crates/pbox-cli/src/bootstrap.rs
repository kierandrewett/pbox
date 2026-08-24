use anyhow::{Context, Result, anyhow, bail};
use pbox_agent_client::AgentClient;
use pbox_crypto::{CertificateMaterial, CertificatePurpose, issue_certificate};
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SSH_CONNECT_TIMEOUT: &str = "10";
const AGENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(60);
const AGENT_READY_INTERVAL: Duration = Duration::from_millis(500);

pub struct BootstrapKey {
    directory: PathBuf,
    private_key: PathBuf,
    known_hosts: PathBuf,
    public_key: String,
}

impl BootstrapKey {
    pub fn generate(box_id: &str) -> Result<Self> {
        let state_root = dirs::state_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pbox")
            .join("operations");
        fs::create_dir_all(&state_root).context("create pbox operation state directory")?;
        set_mode(&state_root, 0o700).context("restrict pbox operation state directory")?;

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("read system clock")?
            .as_nanos();
        let directory =
            state_root.join(format!("bootstrap-{box_id}-{}-{stamp}", std::process::id()));
        fs::create_dir(&directory).with_context(|| {
            format!(
                "create bootstrap operation directory {}",
                directory.display()
            )
        })?;
        set_mode(&directory, 0o700).context("restrict bootstrap operation directory")?;

        let private_key = directory.join("ssh-key");
        let public_key_path = directory.join("ssh-key.pub");
        let output = match Command::new("ssh-keygen")
            .arg("-q")
            .arg("-t")
            .arg("ed25519")
            .arg("-N")
            .arg("")
            .arg("-f")
            .arg(&private_key)
            .stdin(Stdio::null())
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                let _ = fs::remove_dir_all(&directory);
                return Err(error).context("run ssh-keygen for bootstrap");
            }
        };
        if let Err(error) = ensure_success(output, "generate bootstrap SSH key") {
            let _ = fs::remove_dir_all(&directory);
            return Err(error);
        }
        let public_key = match fs::read_to_string(&public_key_path) {
            Ok(public_key) => public_key,
            Err(error) => {
                let _ = fs::remove_dir_all(&directory);
                return Err(error).with_context(|| {
                    format!("read generated public key {}", public_key_path.display())
                });
            }
        };
        let public_key = public_key.trim().to_owned();
        if !public_key.starts_with("ssh-ed25519 ") {
            let _ = fs::remove_dir_all(&directory);
            bail!("ssh-keygen returned an unexpected public key format");
        }
        let known_hosts = directory.join("known_hosts");
        Ok(Self {
            directory,
            private_key,
            known_hosts,
            public_key,
        })
    }

    pub fn operation_directory(&self) -> &Path {
        &self.directory
    }
    pub fn public_key(&self) -> &str {
        &self.public_key
    }

    pub fn cleanup(&self) -> Result<()> {
        fs::remove_dir_all(&self.directory).with_context(|| {
            format!(
                "remove completed bootstrap operation {}",
                self.directory.display()
            )
        })?;
        Ok(())
    }
}

pub struct BootstrapRequest<'a> {
    pub box_id: &'a str,
    pub ip: Ipv4Addr,
    pub port: u16,
    pub key: &'a BootstrapKey,
    pub agent_binary: &'a Path,
    pub server_identity: &'a CertificateMaterial,
    pub client_ca: &'a CertificateMaterial,
    pub client_subject: &'a str,
}

pub fn bootstrap_box(request: &BootstrapRequest<'_>) -> Result<()> {
    if !request.agent_binary.is_file() {
        bail!(
            "pbox-agent binary does not exist: {}",
            request.agent_binary.display()
        );
    }
    if request.port == 0 {
        bail!("agent port cannot be zero");
    }

    let stage = format!(
        "/tmp/pbox-bootstrap-{}-{}-{}",
        request.box_id,
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("read system clock")?
            .as_nanos()
    );
    let local_files = write_local_material(request)?;
    let ssh = SshSession::new(request.ip, request.key);

    ssh.run(&format!("install -d -m 0700 {}", shell_quote(&stage)))
        .context("create guest bootstrap staging directory")?;
    ssh.copy(request.agent_binary, &format!("{stage}/pbox-agent"))
        .context("upload pbox-agent binary")?;
    for (local, remote) in [
        (&local_files.server_certificate, "server.pem"),
        (&local_files.server_private_key, "server-key.pem"),
        (&local_files.client_ca, "client-ca.pem"),
        (&local_files.service_unit, "pbox-agent.service"),
    ] {
        ssh.copy(local, &format!("{stage}/{remote}"))
            .with_context(|| format!("upload bootstrap file {remote}"))?;
    }

    let install_script = format!(
        "set -eu\n\
if ! command -v sudo >/dev/null 2>&1; then\n\
  if ! command -v apt-get >/dev/null 2>&1; then echo 'sudo is missing and apt-get is unavailable' >&2; exit 1; fi\n\
  export DEBIAN_FRONTEND=noninteractive\n\
  apt-get update\n\
  apt-get install -y --no-install-recommends sudo\n\
fi\n\
if ! command -v systemctl >/dev/null 2>&1; then echo 'systemd is required for pbox-agent' >&2; exit 1; fi\n\
if ! id -u pbox >/dev/null 2>&1; then useradd --create-home --shell /bin/bash pbox; fi\n\
install -d -o pbox -g pbox -m 0755 /home/pbox\n\
install -d -m 0750 /etc/pbox\n\
install -m 0755 {stage}/pbox-agent /usr/local/bin/pbox-agent\n\
install -m 0644 {stage}/server.pem /etc/pbox/server.pem\n\
install -m 0600 {stage}/server-key.pem /etc/pbox/server-key.pem\n\
install -m 0644 {stage}/client-ca.pem /etc/pbox/client-ca.pem\n\
install -m 0644 {stage}/pbox-agent.service /etc/systemd/system/pbox-agent.service\n\
sudo -n -u pbox -- true\n\
systemctl daemon-reload\n\
systemctl enable pbox-agent.service\n\
systemctl restart pbox-agent.service\n",
        stage = shell_quote(&stage),
    );
    ssh.run(&install_script)
        .context("install and start pbox-agent in guest")?;

    wait_for_agent(request).context("wait for authenticated pbox-agent")?;

    let cleanup = format!(
        "set -eu\n\
if [ -f /root/.ssh/authorized_keys ]; then\n\
  temporary=/root/.ssh/.pbox-authorized-keys-cleanup\n\
  grep -Fvx -- {} /root/.ssh/authorized_keys > \"$temporary\" || true\n\
  install -m 0600 \"$temporary\" /root/.ssh/authorized_keys\n\
  rm -f \"$temporary\"\n\
fi\n\
rm -rf {}\n",
        shell_quote(request.key.public_key()),
        shell_quote(&stage),
    );
    ssh.run(&cleanup)
        .context("remove temporary guest bootstrap credentials")?;
    Ok(())
}

struct LocalMaterial {
    server_certificate: PathBuf,
    server_private_key: PathBuf,
    client_ca: PathBuf,
    service_unit: PathBuf,
}

fn write_local_material(request: &BootstrapRequest<'_>) -> Result<LocalMaterial> {
    let directory = &request.key.directory;
    let server_certificate = directory.join("server.pem");
    let server_private_key = directory.join("server-key.pem");
    let client_ca = directory.join("client-ca.pem");
    let service_unit = directory.join("pbox-agent.service");
    write_restricted(
        &server_certificate,
        &request.server_identity.certificate_pem,
        0o644,
    )?;
    write_restricted(
        &server_private_key,
        &request.server_identity.private_key_pem,
        0o600,
    )?;
    write_restricted(&client_ca, &request.client_ca.certificate_pem, 0o644)?;
    let unit = format!(
        "[Unit]\nDescription=pbox authenticated guest agent\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart=/usr/local/bin/pbox-agent --listen 0.0.0.0:{port} --box-id {box_id} --certificate /etc/pbox/server.pem --private-key /etc/pbox/server-key.pem --client-ca /etc/pbox/client-ca.pem\nRestart=on-failure\nRestartSec=2\nUser=root\n\n[Install]\nWantedBy=multi-user.target\n",
        port = request.port,
        box_id = request.box_id,
    );
    write_restricted(&service_unit, &unit, 0o644)?;
    Ok(LocalMaterial {
        server_certificate,
        server_private_key,
        client_ca,
        service_unit,
    })
}

fn write_restricted(path: &Path, contents: &str, mode: u32) -> Result<()> {
    fs::write(path, contents)
        .with_context(|| format!("write bootstrap file {}", path.display()))?;
    set_mode(path, mode).with_context(|| format!("restrict bootstrap file {}", path.display()))?;
    Ok(())
}

fn wait_for_agent(request: &BootstrapRequest<'_>) -> Result<()> {
    let endpoint = format!("https://{}:{}", request.ip, request.port);
    let ca_pem = request.client_ca.certificate_pem.clone();
    let identity = issue_certificate(
        request.client_ca,
        request.client_subject,
        CertificatePurpose::Client,
    )
    .context("create short-lived pbox agent client certificate")?;
    let box_id = request.box_id.to_owned();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create async runtime for bootstrap health check")?;
    let started = std::time::Instant::now();
    let mut last_error = String::from("agent did not respond");
    while started.elapsed() < AGENT_READY_TIMEOUT {
        let result = runtime.block_on(async {
            tokio::time::timeout(AGENT_CONNECT_TIMEOUT, async {
                let mut client =
                    AgentClient::connect(&endpoint, &box_id, &ca_pem, &identity).await?;
                client.info().await
            })
            .await
        });
        match result {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(error)) => last_error = error.to_string(),
            Err(_) => last_error = "agent connection timed out".to_owned(),
        }
        thread::sleep(AGENT_READY_INTERVAL);
    }
    Err(anyhow!(
        "{last_error} after {} seconds",
        AGENT_READY_TIMEOUT.as_secs()
    ))
}

struct SshSession<'a> {
    ip: Ipv4Addr,
    key: &'a BootstrapKey,
}

impl<'a> SshSession<'a> {
    fn new(ip: Ipv4Addr, key: &'a BootstrapKey) -> Self {
        Self { ip, key }
    }

    fn run(&self, command: &str) -> Result<Output> {
        let output = Command::new("ssh")
            .args(self.common_args())
            .arg(format!("root@{}", self.ip))
            .arg(command)
            .stdin(Stdio::null())
            .output()
            .context("run SSH bootstrap command")?;
        ensure_success(output, "run SSH bootstrap command")
    }

    fn copy(&self, local: &Path, remote: &str) -> Result<()> {
        let target = format!("root@{}:{}", self.ip, remote);
        let output = Command::new("scp")
            .arg("-q")
            .args(self.common_args())
            .arg(local)
            .arg(target)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("copy {} with scp", local.display()))?;
        ensure_success(output, "copy bootstrap file with scp").map(|_| ())
    }

    fn common_args(&self) -> Vec<String> {
        vec![
            "-i".to_owned(),
            self.key.private_key.to_string_lossy().into_owned(),
            "-o".to_owned(),
            "BatchMode=yes".to_owned(),
            "-o".to_owned(),
            "IdentitiesOnly=yes".to_owned(),
            "-o".to_owned(),
            "StrictHostKeyChecking=accept-new".to_owned(),
            "-o".to_owned(),
            format!("UserKnownHostsFile={}", self.key.known_hosts.display()),
            "-o".to_owned(),
            format!("ConnectTimeout={SSH_CONNECT_TIMEOUT}"),
            "-o".to_owned(),
            "LogLevel=ERROR".to_owned(),
        ]
    }
}

fn ensure_success(output: Output, operation: &str) -> Result<Output> {
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = stderr.trim();
    if detail.is_empty() {
        bail!("{operation} failed with {}", output.status);
    }
    bail!("{operation} failed: {detail}");
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quote_handles_single_quotes() {
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
    }

    #[test]
    fn bootstrap_key_uses_ed25519_and_cleans_state() {
        let key = BootstrapKey::generate("pbx_12345678").unwrap();
        let directory = key.operation_directory().to_owned();
        assert!(key.public_key().starts_with("ssh-ed25519 "));
        assert!(key.private_key.is_file());
        key.cleanup().unwrap();
        assert!(!directory.exists());
    }
}
