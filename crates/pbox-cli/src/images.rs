use anyhow::{Context, Result, bail};
#[cfg(unix)]
use nix::sys::signal::{Signal, killpg};
#[cfg(unix)]
use nix::unistd::Pid;
use pbox_core::{PveApi, PveError, PveTaskResponse};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    registry: String,
    repository: String,
    tag: Option<String>,
    digest: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OciSearchResult {
    pub repository: String,
    pub tags: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OciTemplate {
    pub reference: String,
    pub filename: String,
    pub volume: String,
    pub task: Option<PveTaskResponse>,
}

impl ImageReference {
    pub fn parse(input: &str) -> Result<Self> {
        let trimmed = input.trim();
        let value = trimmed.strip_prefix("docker://").unwrap_or(trimmed);
        if value.is_empty() || value.chars().any(|character| character.is_ascii_control()) {
            bail!("OCI image reference cannot be empty or contain control characters");
        }

        let (name, tag, digest) = if let Some((name, digest)) = value.split_once('@') {
            if digest.is_empty() || value.matches('@').count() != 1 {
                bail!("OCI image digest reference is invalid: {input}");
            }
            let slash = name.rfind('/').unwrap_or(0);
            if name.rfind(':').is_some_and(|position| position > slash) {
                bail!("OCI image reference cannot contain both a tag and a digest");
            }
            (name, None, Some(validate_digest(digest)?))
        } else {
            let slash = value.rfind('/').unwrap_or(0);
            let colon = value.rfind(':');
            if colon.is_some_and(|position| position > slash) {
                let position = colon.expect("colon position exists");
                let (name, tag) = value.split_at(position);
                (name, Some(validate_tag(&tag[1..])?), None)
            } else {
                (value, Some("latest".to_owned()), None)
            }
        };

        let parts: Vec<&str> = name.split('/').collect();
        let has_path = parts.len() > 1;
        let first = parts.first().copied().unwrap_or_default();
        let (registry, repository) = if has_path && is_registry_host(first) {
            let mut repository = parts[1..].join("/");
            if first.eq_ignore_ascii_case("docker.io") && !repository.contains('/') {
                repository = format!("library/{repository}");
            }
            (first.to_owned(), repository)
        } else {
            let repository = if has_path {
                name.to_owned()
            } else {
                format!("library/{name}")
            };
            ("docker.io".to_owned(), repository)
        };
        let registry = validate_name_component(&registry, "registry")?;
        let repository = validate_repository(&repository.to_ascii_lowercase())?;

        Ok(Self {
            registry,
            repository,
            tag,
            digest,
        })
    }

    pub fn repository(&self) -> String {
        format!("{}/{}", self.registry, self.repository)
    }

    pub fn canonical(&self) -> String {
        match (&self.tag, &self.digest) {
            (_, Some(digest)) => format!("{}@{digest}", self.repository()),
            (Some(tag), None) => format!("{}:{tag}", self.repository()),
            (None, None) => format!("{}:latest", self.repository()),
        }
    }

    pub fn is_digest(&self) -> bool {
        self.digest.is_some()
    }

    pub fn filename(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(self.canonical().as_bytes());
        format!("pbox-oci-{}", hex_lower(&digest.finalize()))
    }
}

pub fn is_oci_reference(input: &str) -> bool {
    let value = input.trim();
    if value.starts_with("docker://") {
        return true;
    }
    let slash = value.rfind('/').unwrap_or(0);
    value.contains('/')
        || value.contains('@')
        || value
            .rfind(':')
            .is_some_and(|position| position > slash && position + 1 < value.len())
}

/// Convert a pbox image value into an OCI reference when the local PVE template is absent.
pub fn oci_reference_for_image(input: &str) -> String {
    let value = input.trim();
    if is_oci_reference(value) {
        return value.to_owned();
    }
    if let Some((repository, tag)) = value.rsplit_once('-')
        && !repository.is_empty()
        && !tag.is_empty()
        && tag
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return format!("{repository}:{tag}");
    }
    value.to_owned()
}

#[derive(Debug, Deserialize)]
struct LocalOciTags {
    #[serde(rename = "Tags")]
    tags: Vec<String>,
}

fn parse_local_oci_tags(value: &str, limit: usize) -> Result<Vec<String>> {
    let mut tags = serde_json::from_str::<LocalOciTags>(value)
        .context("parse local OCI tag response")?
        .tags;
    tags.sort();
    tags.dedup();
    tags.truncate(limit);
    Ok(tags)
}

fn search_local_oci_repository(repository: &str, limit: usize) -> Result<Vec<String>> {
    let reference = format!("docker://{repository}");
    let output = run_local_command_output(
        "skopeo",
        &["list-tags", reference.as_str()],
        "query OCI tags with local skopeo",
    )?;
    parse_local_oci_tags(&output, limit)
}

pub fn search_oci_repository(
    client: &impl PveApi,
    node: &str,
    input: &str,
    limit: usize,
) -> Result<OciSearchResult> {
    let reference = ImageReference::parse(input)?;
    let repository = reference.repository();
    let mut tags = match client.list_oci_repo_tags(node, &repository) {
        Ok(tags) => tags,
        Err(error) if is_missing_skopeo_error(&error) => {
            search_local_oci_repository(&repository, limit)?
        }
        Err(error) => {
            return Err(error).with_context(|| format!("search OCI repository {repository}"));
        }
    };
    tags.sort();
    tags.dedup();
    tags.truncate(limit);
    Ok(OciSearchResult { repository, tags })
}

pub fn prepare_oci_template(
    client: &impl PveApi,
    node: &str,
    storage: &str,
    input: &str,
) -> Result<OciTemplate> {
    let reference = ImageReference::parse(input)?;
    if reference.is_digest() {
        bail!(
            "PVE OCI registry pull requires a tagged image reference; use {}:tag",
            reference.repository()
        );
    }
    let canonical = reference.canonical();
    let filename = reference.filename();
    let volume = format!("{storage}:vztmpl/{filename}.tar");
    if oci_template_present(client, node, storage, &volume)? {
        return Ok(OciTemplate {
            reference: canonical,
            filename,
            volume,
            task: None,
        });
    }
    let task = match client.pull_oci_registry(node, storage, &canonical, &filename) {
        Ok(task) => Some(task),
        Err(error) if is_existing_oci_template_error(&error) => {
            if oci_template_present(client, node, storage, &volume)? {
                None
            } else {
                return Err(error).with_context(|| {
                    format!("pull OCI image {canonical} into PVE storage {storage}")
                });
            }
        }
        Err(error) if is_missing_skopeo_error(&error) => {
            return prepare_local_oci_template(client, node, storage, &canonical, &filename);
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("pull OCI image {canonical} into PVE storage {storage}"));
        }
    };
    Ok(OciTemplate {
        reference: canonical,
        filename,
        volume,
        task,
    })
}

fn prepare_local_oci_template(
    client: &impl PveApi,
    node: &str,
    storage: &str,
    reference: &str,
    filename: &str,
) -> Result<OciTemplate> {
    let archive = build_local_oci_archive(reference, filename)?;
    let result = upload_local_oci_template(client, node, storage, reference, filename, &archive);
    if let Some(workspace) = archive.parent() {
        let _ = fs::remove_dir_all(workspace);
    }
    result
}

pub fn upload_local_oci_template(
    client: &impl PveApi,
    node: &str,
    storage: &str,
    reference: &str,
    filename: &str,
    archive: &Path,
) -> Result<OciTemplate> {
    let volume = format!("{storage}:vztmpl/{filename}.tar.zst");
    if oci_template_present(client, node, storage, &volume)? {
        return Ok(OciTemplate {
            reference: reference.to_owned(),
            filename: filename.to_owned(),
            volume,
            task: None,
        });
    }
    let task = client
        .upload_storage_template(node, storage, &format!("{filename}.tar.zst"), archive)
        .with_context(|| {
            format!("upload local OCI template {reference} to PVE storage {storage}")
        })?;
    Ok(OciTemplate {
        reference: reference.to_owned(),
        filename: filename.to_owned(),
        volume,
        task: Some(task),
    })
}

const OCI_GUEST_PREPARATION: &str = r#"set -eu
if [ -x /sbin/init ] && command -v sshd >/dev/null 2>&1; then
    exit 0
fi
if command -v apt-get >/dev/null 2>&1; then
    printf '%s\n' '#!/bin/sh' 'exit 101' > /usr/sbin/policy-rc.d
    chmod 755 /usr/sbin/policy-rc.d
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    apt-get install -y --no-install-recommends systemd-sysv openssh-server sudo python3 ca-certificates
    rm -rf /var/lib/apt/lists/* /usr/sbin/policy-rc.d
elif command -v dnf >/dev/null 2>&1; then
    dnf install -y systemd openssh-server sudo python3 ca-certificates
    dnf clean all
elif [ -x /sbin/init ] && command -v sshd >/dev/null 2>&1; then
    exit 0
else
    echo 'pbox needs a systemd init and OpenSSH server in the OCI image' >&2
    exit 1
fi
"#;

fn build_local_oci_archive(reference: &str, filename: &str) -> Result<PathBuf> {
    let workspace = create_local_oci_workspace(filename)?;
    let result = (|| {
        let image = ImageReference::parse(reference)?.canonical();
        let pull_action = format!("Pulling OCI image {image}");
        run_local_command("podman", &["pull", "--quiet", image.as_str()], &pull_action)?;
        let container = workspace
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow::anyhow!("local OCI workspace has no valid container name"))?
            .to_owned();
        run_local_command(
            "podman",
            &[
                "create",
                "--quiet",
                "--name",
                container.as_str(),
                "--network",
                "host",
                image.as_str(),
                "/bin/sh",
                "-c",
                OCI_GUEST_PREPARATION,
            ],
            "Creating temporary OCI container",
        )?;
        let tar = workspace.join(format!("{filename}.tar"));
        let compressed = workspace.join(format!("{filename}.tar.zst"));
        let operation_result = (|| {
            let tar_text = path_text(&tar)?;
            let compressed_text = path_text(&compressed)?;
            run_local_command(
                "podman",
                &["start", "--attach", container.as_str()],
                "Installing guest prerequisites (systemd, OpenSSH, sudo, Python)",
            )?;
            run_local_command(
                "podman",
                &["export", "--output", tar_text.as_str(), container.as_str()],
                "Exporting OCI container root filesystem",
            )?;
            run_local_command(
                "zstd",
                &[
                    "--quiet",
                    "--threads=0",
                    "--force",
                    tar_text.as_str(),
                    "-o",
                    compressed_text.as_str(),
                ],
                "Compressing OCI root filesystem",
            )?;
            Ok::<(), anyhow::Error>(())
        })();
        let cleanup_result = run_local_command(
            "podman",
            &["rm", "--force", container.as_str()],
            "Removing temporary OCI container",
        );
        operation_result?;
        cleanup_result?;
        Ok(compressed)
    })();
    match result {
        Ok(path) => Ok(path),
        Err(error) => {
            let _ = fs::remove_dir_all(&workspace);
            Err(error)
        }
    }
}

fn create_local_oci_workspace(filename: &str) -> Result<PathBuf> {
    let root = std::env::temp_dir();
    let pid = std::process::id();
    for attempt in 0..100 {
        let workspace = root.join(format!("pbox-oci-{filename}-{pid}-{attempt}"));
        match fs::create_dir(&workspace) {
            Ok(()) => return Ok(workspace),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("create local OCI workspace {}", workspace.display())
                });
            }
        }
    }
    bail!("could not allocate a local OCI workspace for {filename}")
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("local OCI path is not valid UTF-8: {}", path.display()))
}

const LOCAL_OCI_COMMAND_TIMEOUT: Duration = Duration::from_secs(30 * 60);
const LOCAL_OCI_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

fn run_local_command(program: &str, args: &[&str], action: &str) -> Result<()> {
    let output = run_local_process(program, args, action)?;
    ensure_local_command_success(&output, action)
}

fn ensure_local_command_success(output: &Output, action: &str) -> Result<()> {
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if detail.is_empty() {
        bail!("{action} failed: {}", output.status);
    }
    bail!("{action} failed: {}: {detail}", output.status);
}

fn run_local_command_output(program: &str, args: &[&str], action: &str) -> Result<String> {
    let output = run_local_process(program, args, action)?;
    ensure_local_command_success(&output, action)?;
    let value = String::from_utf8(output.stdout).context("read local OCI command output")?;
    let value = value.trim();
    if value.is_empty() {
        bail!("{action} returned no container identifier");
    }
    Ok(value.to_owned())
}

fn run_local_process(program: &str, args: &[&str], action: &str) -> Result<Output> {
    eprintln!("[image] {action}");
    let mut command = Command::new(program);
    command
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("{action}; command '{program}' is unavailable"))?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            terminate_local_process(&mut child);
            bail!("{action} did not expose stdout");
        }
    };
    let stderr = match child.stderr.take() {
        Some(stderr) => stderr,
        None => {
            terminate_local_process(&mut child);
            bail!("{action} did not expose stderr");
        }
    };
    let stdout_reader = thread::spawn(move || read_command_stream(stdout));
    let stderr_reader = thread::spawn(move || read_command_stream(stderr));
    let status = wait_for_local_process(&mut child, action)?;
    let stdout = stdout_reader
        .join()
        .map_err(|_| anyhow::anyhow!("{action} stdout reader stopped unexpectedly"))??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| anyhow::anyhow!("{action} stderr reader stopped unexpectedly"))??;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn read_command_stream(mut stream: impl Read) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    stream.read_to_end(&mut output)?;
    Ok(output)
}

fn wait_for_local_process(child: &mut Child, action: &str) -> Result<ExitStatus> {
    let started = Instant::now();
    let mut next_progress = LOCAL_OCI_PROGRESS_INTERVAL;
    loop {
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => {
                terminate_local_process(child);
                finish_local_progress(action, started.elapsed(), "failed");
                return Err(error).with_context(|| format!("wait for {action}"));
            }
        };
        if let Some(status) = status {
            finish_local_progress(
                action,
                started.elapsed(),
                if status.success() { "done" } else { "failed" },
            );
            return Ok(status);
        }
        let elapsed = started.elapsed();
        if elapsed >= LOCAL_OCI_COMMAND_TIMEOUT {
            terminate_local_process(child);
            finish_local_progress(action, elapsed, "timed out");
            bail!(
                "{action} did not finish within {} minutes",
                LOCAL_OCI_COMMAND_TIMEOUT.as_secs() / 60
            );
        }
        if elapsed >= next_progress {
            report_local_progress(action, elapsed);
            next_progress += LOCAL_OCI_PROGRESS_INTERVAL;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn report_local_progress(action: &str, elapsed: Duration) {
    if io::stderr().is_terminal() {
        eprint!("\r\x1b[2K[image] {action} ({}s elapsed)", elapsed.as_secs());
        let _ = io::stderr().flush();
    } else {
        eprintln!(
            "[image] {action} still running ({}s elapsed)",
            elapsed.as_secs()
        );
    }
}

fn finish_local_progress(action: &str, elapsed: Duration, outcome: &str) {
    if io::stderr().is_terminal() {
        eprintln!(
            "\r\x1b[2K[image] {action} {outcome} ({}s elapsed)",
            elapsed.as_secs()
        );
    } else {
        eprintln!(
            "[image] {action} {outcome} ({}s elapsed)",
            elapsed.as_secs()
        );
    }
}

fn terminate_local_process(child: &mut Child) {
    #[cfg(unix)]
    {
        let _ = killpg(Pid::from_raw(child.id() as i32), Signal::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

pub fn oci_template_present(
    client: &impl PveApi,
    node: &str,
    storage: &str,
    volume: &str,
) -> Result<bool> {
    let contents = client
        .list_storage_content(node, storage, "vztmpl")
        .with_context(|| format!("inspect OCI templates in PVE storage {storage}"))?;
    Ok(contents.iter().any(|content| content.volid == volume))
}

fn is_existing_oci_template_error(error: &PveError) -> bool {
    let PveError::Http { message, .. } = error else {
        return false;
    };
    is_existing_oci_template_message(message)
}

fn is_existing_oci_template_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("refusing to override existing file")
        || message.contains("file already exists")
        || message.contains("file exists")
}

fn is_missing_skopeo_error(error: &PveError) -> bool {
    let PveError::Http { message, .. } = error else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    message.contains("skopeo") && (message.contains("install") || message.contains("not found"))
}

fn is_registry_host(value: &str) -> bool {
    value == "localhost" || value.contains('.') || value.contains(':')
}

fn validate_name_component(value: &str, label: &str) -> Result<String> {
    let (host, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(host, port)| (host, Some(port)));
    if host.is_empty()
        || host.split('.').any(|component| {
            let bytes = component.as_bytes();
            bytes.is_empty()
                || !bytes[0].is_ascii_alphanumeric()
                || !bytes[bytes.len() - 1].is_ascii_alphanumeric()
                || !bytes
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        })
    {
        bail!("OCI {label} is invalid: {value}");
    }
    if let Some(port) = port
        && port.parse::<u16>().ok().filter(|port| *port > 0).is_none()
    {
        bail!("OCI registry is invalid: {value}");
    }
    Ok(value.to_ascii_lowercase())
}

fn validate_repository(value: &str) -> Result<String> {
    if value.is_empty()
        || value
            .split('/')
            .any(|component| !valid_repository_component(component))
    {
        bail!("OCI repository is invalid: {value}");
    }
    Ok(value.to_owned())
}

fn valid_repository_component(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.is_empty() || !bytes[0].is_ascii_alphanumeric() {
        return false;
    }
    let mut index = 1;
    while index < bytes.len() {
        if bytes[index].is_ascii_alphanumeric() {
            index += 1;
            continue;
        }
        match bytes[index] {
            b'.' => index += 1,
            b'_' => {
                index += 1;
                if index < bytes.len() && bytes[index] == b'_' {
                    index += 1;
                }
            }
            b'-' => {
                while index < bytes.len() && bytes[index] == b'-' {
                    index += 1;
                }
            }
            _ => return false,
        }
        if index >= bytes.len() || !bytes[index].is_ascii_alphanumeric() {
            return false;
        }
        index += 1;
    }
    true
}

fn validate_tag(value: &str) -> Result<String> {
    if value.is_empty()
        || value.len() > 128
        || value.chars().enumerate().any(|(index, character)| {
            (!character.is_ascii_alphanumeric() && !matches!(character, '.' | '-' | '_'))
                || (index == 0 && !character.is_ascii_alphanumeric() && character != '_')
        })
    {
        bail!("OCI image tag is invalid: {value}");
    }
    Ok(value.to_owned())
}

fn validate_digest(value: &str) -> Result<String> {
    let Some(hex) = value.strip_prefix("sha256:") else {
        bail!("OCI image digest must use sha256");
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("OCI image digest is invalid: {value}");
    }
    Ok(format!("sha256:{hex}"))
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(HEX[(byte >> 4) as usize] as char);
        value.push(HEX[(byte & 0x0f) as usize] as char);
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_hub_equivalents_and_rejects_trailing_paths() {
        let shorthand = ImageReference::parse("debian:13").unwrap();
        let explicit = ImageReference::parse("docker.io/debian:13").unwrap();
        assert_eq!(shorthand.canonical(), explicit.canonical());
        assert!(ImageReference::parse("ghcr.io/example/base/:latest").is_err());
    }

    #[test]
    fn parses_explicit_registry_and_digest() {
        let reference = ImageReference::parse(
            "ghcr.io/KieranAndrewett/pbox-base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        assert_eq!(reference.repository(), "ghcr.io/kieranandrewett/pbox-base");
        assert!(reference.is_digest());
        assert!(reference.canonical().contains("@sha256:"));
    }

    #[test]
    fn identifies_only_explicit_oci_references() {
        assert!(!is_oci_reference("debian-13"));
        assert!(is_oci_reference("docker://debian-13"));
        assert!(is_oci_reference("debian:13"));
        assert!(is_oci_reference("ghcr.io/example/base"));
    }

    #[test]
    fn maps_versioned_pve_aliases_to_oci_tags() {
        assert_eq!(oci_reference_for_image("debian-13"), "debian:13");
        assert_eq!(oci_reference_for_image("ubuntu-24.04"), "ubuntu:24.04");
        assert_eq!(
            oci_reference_for_image("ghcr.io/example/base:latest"),
            "ghcr.io/example/base:latest"
        );
    }

    #[test]
    fn parses_registry_port() {
        let reference = ImageReference::parse("localhost:5000/pbox/base:dev").unwrap();
        assert_eq!(reference.repository(), "localhost:5000/pbox/base");
        assert_eq!(reference.canonical(), "localhost:5000/pbox/base:dev");
    }

    #[test]
    fn rejects_invalid_tags_and_repositories() {
        assert!(ImageReference::parse("debian: bad").is_err());
        assert!(ImageReference::parse("ghcr.io/example//base").is_err());
        assert!(ImageReference::parse("ghcr.io/example-/base:latest").is_err());
        assert!(ImageReference::parse("ghcr.io/example/base:latest?x").is_err());
        assert!(ImageReference::parse(
            "ghcr.io/example/base:latest@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
        )
        .is_err());
    }
    #[test]
    fn detects_existing_template_pull_errors() {
        assert!(is_existing_oci_template_message(
            "refusing to override existing file 'pbox.tar'"
        ));
        assert!(!is_existing_oci_template_message(
            "manifest is not supported"
        ));
    }

    #[test]
    fn detects_missing_skopeo_pve_errors() {
        let error = PveError::Http {
            status: "500".parse().unwrap(),
            message: "Install 'skopeo' to list tags from OCI registries.".to_owned(),
        };
        assert!(is_missing_skopeo_error(&error));
        assert!(!is_missing_skopeo_error(&PveError::Unsupported(
            "OCI registry unavailable".to_owned(),
        )));
    }

    #[test]
    fn parses_local_skopeo_tags_sorted_and_limited() {
        let tags = parse_local_oci_tags(r#"{"Tags":["latest","13","latest","12"]}"#, 2).unwrap();
        assert_eq!(tags, ["12", "13"]);
    }
}
