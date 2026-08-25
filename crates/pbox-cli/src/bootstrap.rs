use anyhow::{Context, Result, anyhow, bail};
use pbox_agent_client::AgentClient;
use pbox_crypto::{CertificateMaterial, CertificatePurpose, issue_certificate};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SSH_CONNECT_TIMEOUT: &str = "10";
const AGENT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(60);
const AGENT_READY_INTERVAL: Duration = Duration::from_millis(500);
const OPERATION_FILE: &str = "operation.json";
const OPERATION_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BootstrapOperation {
    pub schema: u32,
    pub box_id: String,
    pub node: String,
    pub vmid: Option<u64>,
    pub ip: Option<Ipv4Addr>,
    pub port: u16,
    pub stage: String,
    pub phase: String,
}

impl BootstrapOperation {
    pub fn new(box_id: &str, node: &str, port: u16, stage: &str) -> Self {
        Self {
            schema: OPERATION_SCHEMA,
            box_id: box_id.to_owned(),
            node: node.to_owned(),
            vmid: None,
            ip: None,
            port,
            stage: stage.to_owned(),
            phase: "created".to_owned(),
        }
    }
}

pub struct BootstrapKey {
    directory: PathBuf,
    private_key: PathBuf,
    known_hosts: PathBuf,
    public_key: String,
    host_key_alias: String,
    remote_stage: String,
}

impl BootstrapKey {
    pub fn generate(box_id: &str) -> Result<Self> {
        let state_root = dirs::state_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pbox");
        let operations_root = state_root.join("operations");
        fs::create_dir_all(&operations_root).context("create pbox operation state directory")?;
        set_mode(&state_root, 0o700).context("restrict pbox state directory")?;
        set_mode(&operations_root, 0o700).context("restrict pbox operation state directory")?;
        let known_hosts = state_root.join("known_hosts");
        ensure_known_hosts_file(&known_hosts)?;

        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("read system clock")?
            .as_nanos();
        let directory =
            operations_root.join(format!("bootstrap-{box_id}-{}-{stamp}", std::process::id()));
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
        Ok(Self {
            directory,
            private_key,
            known_hosts,
            public_key,
            host_key_alias: format!("pbox-{box_id}"),
            remote_stage: format!(
                "/tmp/pbox-bootstrap-{box_id}-{}-{stamp}",
                std::process::id()
            ),
        })
    }

    pub fn from_operation(directory: &Path, operation: &BootstrapOperation) -> Result<Self> {
        if operation.schema != OPERATION_SCHEMA {
            bail!(
                "unsupported bootstrap operation schema {}",
                operation.schema
            );
        }
        if operation.box_id.is_empty() || operation.stage.is_empty() {
            bail!("bootstrap operation is missing its box identity or staging path");
        }
        validate_stage(&operation.stage)?;
        let directory = directory.to_owned();
        let private_key = directory.join("ssh-key");
        let public_key_path = directory.join("ssh-key.pub");
        if !private_key.is_file() || !public_key_path.is_file() {
            bail!(
                "bootstrap operation {} is missing its SSH key",
                directory.display()
            );
        }
        let public_key = fs::read_to_string(&public_key_path)
            .with_context(|| format!("read bootstrap public key {}", public_key_path.display()))?;
        let public_key = public_key.trim().to_owned();
        if !public_key.starts_with("ssh-ed25519 ") {
            bail!("bootstrap operation contains an unexpected public key format");
        }
        let state_root = dirs::state_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pbox");
        let known_hosts = state_root.join("known_hosts");
        ensure_known_hosts_file(&known_hosts)?;
        Ok(Self {
            directory,
            private_key,
            known_hosts,
            public_key,
            host_key_alias: format!("pbox-{}", operation.box_id),
            remote_stage: operation.stage.clone(),
        })
    }

    pub fn operation_directory(&self) -> &Path {
        &self.directory
    }
    pub fn public_key(&self) -> &str {
        &self.public_key
    }
    pub fn remote_stage(&self) -> &str {
        &self.remote_stage
    }

    pub fn save_operation(&self, operation: &BootstrapOperation) -> Result<()> {
        if operation.box_id != self.host_key_alias.trim_start_matches("pbox-") {
            bail!("bootstrap operation box identity does not match its key");
        }
        if operation.stage != self.remote_stage {
            bail!("bootstrap operation staging path does not match its key");
        }
        let path = self.directory.join(OPERATION_FILE);
        let temporary = self.directory.join(".operation.json.tmp");
        let contents =
            serde_json::to_vec_pretty(operation).context("serialise bootstrap operation")?;
        fs::write(&temporary, contents)
            .with_context(|| format!("write bootstrap operation {}", path.display()))?;
        set_mode(&temporary, 0o600).context("restrict bootstrap operation")?;
        fs::rename(&temporary, &path)
            .with_context(|| format!("commit bootstrap operation {}", path.display()))?;
        Ok(())
    }

    pub fn find_pending(box_id: &str) -> Result<Option<(Self, BootstrapOperation)>> {
        let operations_root = dirs::state_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("pbox")
            .join("operations");
        let entries = match fs::read_dir(&operations_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("list bootstrap operations {}", operations_root.display())
                });
            }
        };
        let mut found = None;
        for entry in entries {
            let entry = entry.context("read bootstrap operation entry")?;
            if !entry
                .file_type()
                .context("inspect bootstrap operation entry")?
                .is_dir()
            {
                continue;
            }
            let path = entry.path().join(OPERATION_FILE);
            if !path.is_file() {
                continue;
            }
            let contents = fs::read(&path)
                .with_context(|| format!("read bootstrap operation {}", path.display()))?;
            let operation: BootstrapOperation = serde_json::from_slice(&contents)
                .with_context(|| format!("parse bootstrap operation {}", path.display()))?;
            if operation.box_id != box_id || operation.phase == "complete" {
                continue;
            }
            if found.is_some() {
                bail!("multiple pending bootstrap operations exist for {box_id}");
            }
            let key = Self::from_operation(&entry.path(), &operation)?;
            found = Some((key, operation));
        }
        Ok(found)
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
    validate_stage(request.key.remote_stage())?;

    let stage = request.key.remote_stage();
    let local_files = write_local_material(request)?;
    let ssh = SshSession::new(request.ip, request.key);

    ssh.run(&format!("install -d -m 0700 {}", shell_quote(stage)))
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
        stage = shell_quote(stage),
    );
    ssh.run(&install_script)
        .context("install and start pbox-agent in guest")?;
    let probe = AgentProbeRequest {
        box_id: request.box_id,
        ip: request.ip,
        port: request.port,
        client_ca: request.client_ca,
        client_subject: request.client_subject,
    };
    wait_for_agent(&probe).context("wait for authenticated pbox-agent")?;

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
        shell_quote(stage),
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

pub struct AgentProbeRequest<'a> {
    pub box_id: &'a str,
    pub ip: Ipv4Addr,
    pub port: u16,
    pub client_ca: &'a CertificateMaterial,
    pub client_subject: &'a str,
}

pub fn wait_for_agent(request: &AgentProbeRequest<'_>) -> Result<()> {
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
            format!("HostKeyAlias={}", self.key.host_key_alias),
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
fn validate_stage(stage: &str) -> Result<()> {
    if !stage.starts_with("/tmp/pbox-bootstrap-")
        || stage.len() <= "/tmp/pbox-bootstrap-".len()
        || !stage
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_'))
    {
        bail!("invalid bootstrap staging path");
    }
    Ok(())
}

fn ensure_known_hosts_file(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!(
                "pbox known-hosts file must not be a symbolic link: {}",
                path.display()
            );
        }
        Ok(metadata) if !metadata.is_file() => {
            bail!(
                "pbox known-hosts path is not a regular file: {}",
                path.display()
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .with_context(|| format!("create pbox known-hosts file {}", path.display()))?;
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("inspect pbox known-hosts file {}", path.display()));
        }
    }
    set_mode(path, 0o600)
        .with_context(|| format!("restrict pbox known-hosts file {}", path.display()))
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

    #[test]
    fn bootstrap_stage_validation_rejects_escape_paths() {
        assert!(validate_stage("/tmp/pbox-bootstrap-box-123").is_ok());
        assert!(validate_stage("/tmp/pbox-bootstrap-box/../../etc").is_err());
        assert!(validate_stage("/var/tmp/pbox-bootstrap-box").is_err());
    }

    #[test]
    fn bootstrap_operation_manifest_round_trips_and_is_discoverable() {
        let key = BootstrapKey::generate("pbx_abcdefgh").unwrap();
        let operation = BootstrapOperation::new("pbx_abcdefgh", "pve01", 7443, key.remote_stage());
        key.save_operation(&operation).unwrap();

        let contents = fs::read(key.operation_directory().join(OPERATION_FILE)).unwrap();
        let loaded: BootstrapOperation = serde_json::from_slice(&contents).unwrap();
        assert_eq!(loaded, operation);
        let (found_key, found_operation) =
            BootstrapKey::find_pending("pbx_abcdefgh").unwrap().unwrap();
        assert_eq!(found_operation, operation);
        found_key.cleanup().unwrap();
    }
}
