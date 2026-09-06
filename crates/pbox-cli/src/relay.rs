//! Relay credentials stay outside URLs, logs and guest images (except the scoped guest token).
use anyhow::{Context, Result};
use pbox_agent_client::AgentClient;
use pbox_core::config::Config;
use pbox_crypto::CertificateMaterial;
use pbox_relay::RelayAccess;

pub const ENDPOINT: &str = "https://pbox-relay.invalid:443";

pub fn access(config: &Config, box_id: &str, role: &str) -> Result<RelayAccess> {
    let url = config
        .relay
        .url
        .as_ref()
        .context("relay.url is not configured")?;
    pbox_relay::websocket_url(url, if role == "snapshot" { "agent" } else { role }, box_id)?;
    let path = config
        .relay
        .key_file
        .as_ref()
        .context("set relay.key-file to the relay master key file")?;
    let key = std::fs::read_to_string(path).context("read relay master key file")?;
    anyhow::ensure!(
        key.trim().len() >= 32,
        "relay key must contain at least 32 characters"
    );
    Ok(RelayAccess {
        url: url.clone(),
        token: pbox_relay::scoped_token(key.trim(), role, box_id),
    })
}

pub fn snapshot_access(config: &Config, parent: &str) -> Result<RelayAccess> {
    access(config, parent, "snapshot")
}
pub fn route_access(config: &Config, route: &str) -> Result<RelayAccess> {
    access(config, route, "client")
}

pub async fn connect_agent(
    config: &Config,
    endpoint: &str,
    box_id: &str,
    ca_pem: &str,
    identity: &CertificateMaterial,
) -> Result<AgentClient> {
    let relay = if endpoint == ENDPOINT {
        Some(access(config, box_id, "client")?)
    } else {
        None
    };
    Ok(AgentClient::connect_with_relay(endpoint, box_id, ca_pem, identity, relay.as_ref()).await?)
}

use super::{
    BootstrapKey, BootstrapOperation, BoxInfo, CliStyle, ColorChoice, NewCommand, PveApi,
    agent_materials, client_from_config, create_lxc_with_retry, discover_boxes, find_box,
    generate_unique_id, print_box_info, resolve_agent_binary, resolve_new_command_with_template,
    resolve_new_node, select_pve_storage, wait_for_task_with_progress,
};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

pub(crate) fn write_payload(directory: &Path, config: &Config, box_id: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let materials = agent_materials(config, box_id)?;
    let etc = directory.join("etc/pbox");
    fs::create_dir_all(&etc)?;
    fs::set_permissions(&etc, fs::Permissions::from_mode(0o700))?;
    let mut files = vec![
        ("server.pem", materials.server.certificate_pem),
        ("server-key.pem", materials.server.private_key_pem),
        ("client-ca.pem", materials.ca.certificate_pem),
    ];
    if config.relay.url.is_some() {
        files.push((
            "relay.json",
            serde_json::to_string(&access(config, box_id, "agent")?)?,
        ));
    }
    for (name, contents) in files {
        fs::write(etc.join(name), contents)?;
        fs::set_permissions(etc.join(name), fs::Permissions::from_mode(0o600))?;
    }
    let binary = directory.join("usr/local/bin/pbox-agent");
    fs::create_dir_all(binary.parent().unwrap())?;
    fs::copy(resolve_agent_binary(config)?, &binary)?;
    fs::set_permissions(binary, fs::Permissions::from_mode(0o755))?;
    let unit = directory.join("etc/systemd/system/pbox-agent.service");
    fs::create_dir_all(unit.parent().unwrap())?;
    let listen = if config.relay.url.is_some() {
        "127.0.0.1"
    } else {
        "0.0.0.0"
    };
    let relay_arg = if config.relay.url.is_some() {
        " --relay-config /etc/pbox/relay.json"
    } else {
        ""
    };
    let agent_args = format!(
        "--listen {listen}:{} --box-id {box_id} --certificate /etc/pbox/server.pem --private-key /etc/pbox/server-key.pem --client-ca /etc/pbox/client-ca.pem{relay_arg}",
        config.agent.port
    );
    // Start immediately; the agent retries outbound connections while DHCP becomes ready.
    fs::write(
        unit,
        format!(
            "[Unit]\nDescription=pbox guest agent\nWants=network.target\nAfter=network.target\n\n[Service]\nExecStart=/usr/local/bin/pbox-agent {agent_args}\nRestart=always\nRestartSec=2\n\n[Install]\nWantedBy=multi-user.target\n"
        ),
    )?;
    let openrc = directory.join("etc/init.d/pbox-agent");
    fs::create_dir_all(openrc.parent().unwrap())?;
    fs::write(
        &openrc,
        format!(
            "#!/sbin/openrc-run\nname=pbox-agent\ncommand=/usr/local/bin/pbox-agent\ncommand_args=\"{agent_args}\"\ncommand_background=true\npidfile=/run/${{RC_SVCNAME}}.pid\nrespawn_delay=2\nrespawn_max=0\ndepend() {{\n    need net\n}}\n"
        ),
    )?;
    fs::set_permissions(&openrc, fs::Permissions::from_mode(0o755))?;
    let runit = directory.join("etc/service/pbox-agent/run");
    fs::create_dir_all(runit.parent().unwrap())?;
    fs::write(
        &runit,
        format!("#!/bin/sh\nexec /usr/local/bin/pbox-agent {agent_args}\n"),
    )?;
    fs::set_permissions(&runit, fs::Permissions::from_mode(0o755))?;
    // First-boot presets can remove an enable symlink on distributions such as
    // Fedora. Keep the managed agent enabled when systemd applies that policy.
    let presets = directory.join("etc/systemd/system-preset");
    fs::create_dir_all(&presets)?;
    fs::write(
        presets.join("00-pbox.preset"),
        "enable pbox-agent.service\n",
    )?;
    Ok(())
}

/// Remove only this operation's private template after PVE has finished extracting it.
pub fn cleanup_template(
    client: &impl PveApi,
    key: &BootstrapKey,
    operation: &mut BootstrapOperation,
) -> Result<()> {
    if let Some(upid) = &operation.relay_task {
        let status = client.get_task_status(&operation.node, upid)?;
        anyhow::ensure!(
            status.status == "stopped",
            "PVE task {upid} is still running; retry repair after it finishes"
        );
        operation.relay_task = None;
        key.save_operation(operation)?;
    }
    let Some(volume) = operation.relay_template.as_ref() else {
        return Ok(());
    };
    let (storage, filename) = volume
        .split_once(":vztmpl/")
        .context("invalid recorded bootstrap template")?;
    let exists = client
        .list_storage_content(&operation.node, storage, "vztmpl")?
        .iter()
        .any(|item| item.volid == *volume);
    if exists {
        let task = client.delete_bootstrap_template(&operation.node, storage, filename)?;
        wait_for_task_with_progress(
            client,
            &operation.node,
            task,
            None,
            "temporary template deletion",
        )?;
    }
    operation.relay_template = None;
    key.save_operation(operation)?;
    Ok(())
}

pub fn run_new(config: &Config, command: NewCommand, json: bool, color: ColorChoice) -> Result<()> {
    anyhow::ensure!(
        command.ostemplate.is_none(),
        "relay bootstrap requires --image (an OCI image); existing PVE templates cannot be personalised through the PVE API"
    );
    let progress = super::progress::verbose().then(|| CliStyle::for_stderr(color, json));
    let creation = super::progress::CreationProgress::new(json);
    let client = client_from_config(config)?;
    let id = generate_unique_id(&discover_boxes(&client)?)?;
    let box_id = id.to_string();
    access(config, &box_id, "client")?;
    let node = resolve_new_node(&client, config, &command, None)?;
    let storages = client.list_node_storages(&node)?;
    let storage = select_pve_storage(
        &storages,
        &config.pve.template_storage,
        "vztmpl",
        "templates",
    )?;
    let filename = format!("pbox-bootstrap-{box_id}");
    let volume = format!("{storage}:vztmpl/{filename}.tar.zst");
    let mut resolved =
        resolve_new_command_with_template(&client, config, &command, Some(&volume), Some(&node))?;
    let key = BootstrapKey::generate(&box_id)?;
    let mut operation =
        BootstrapOperation::new(&box_id, &node, config.agent.port, key.remote_stage());
    operation.relay = true;
    operation.relay_template = Some(volume);
    key.save_operation(&operation)?;
    let mut upload_attempted = false;
    let result: Result<()> = (|| {
        let payload = key.operation_directory().join("payload");
        write_payload(&payload, config, &box_id)?;
        let image = super::images::oci_reference_for_image(
            command.image.as_deref().unwrap_or(&config.images.default),
        );
        let image = super::images::ImageReference::parse(&image)?.canonical();
        resolved.image = image.clone();
        creation.phase(&format!("Preparing {image}"));
        let archive = super::images::build_local_oci_archive(&image, &filename, Some(&payload))?;
        resolved.ostype = Some(archive.ostype.clone());
        creation.phase(&format!("Creating your box on {node}"));
        let archive_size = super::ui::byte_size(fs::metadata(&archive.path)?.len());
        super::progress::substep(&format!("Uploading {archive_size} to PVE {node}/{storage}"));
        upload_attempted = true;
        let upload = client.upload_storage_template(
            &node,
            storage,
            &format!("{filename}.tar.zst"),
            &archive.path,
        );
        // The API upload has consumed the file, so no local credential archive needs to remain.
        if let Some(parent) = archive.path.parent() {
            fs::remove_dir_all(parent).context("remove private local image archive")?;
        }
        fs::remove_dir_all(payload).context("remove local agent payload")?;
        let upload = upload?;
        operation.relay_task = Some(upload.upid.clone());
        key.save_operation(&operation)?;
        wait_for_task_with_progress(&client, &node, upload, progress, "private template upload")?;
        let hostname = resolved
            .name
            .clone()
            .unwrap_or_else(|| format!("pbox-{}", &box_id[4..]));
        operation.phase = "relay-creating".to_owned();
        key.save_operation(&operation)?;
        let (vmid, task) = create_lxc_with_retry(&client, config, &resolved, &id, &hostname, &key)?;
        operation.vmid = Some(vmid);
        operation.relay_task = Some(task.upid.clone());
        key.save_operation(&operation)?;
        wait_for_task_with_progress(&client, &node, task, progress, "container creation")?;
        operation.phase = "relay-waiting".to_owned();
        key.save_operation(&operation)?;
        cleanup_template(&client, &key, &mut operation)?;
        creation.phase("Connecting to your box");
        super::progress::substep("Waiting for the outbound agent");
        wait_ready(config, &box_id)?;
        super::progress::substep("Checking guest access");
        let access = if !json {
            Some(super::guest::check(config, &box_id, ENDPOINT))
        } else {
            None
        };
        if resolved.stopped {
            creation.phase("Stopping your box");
            let task = client.shutdown_lxc(&node, vmid)?;
            wait_for_task_with_progress(&client, &node, task, progress, "box shutdown")?;
        }
        let record = find_box(&client, &box_id)?;
        key.cleanup()?;
        creation.finish();
        if let Some(access) = access {
            super::ui::user_access(&box_id, "pbox", access);
        }
        if !json && !super::progress::verbose() {
            let style = CliStyle::for_stdout(color, json);
            if resolved.stopped {
                style.success(&format!("Created {box_id} (stopped)"));
                style.stdout_metadata("image", &image);
                style.command(&format!("pbox start {box_id}"));
            } else {
                style.success(&format!("Ready: {box_id}"));
                style.stdout_metadata("image", &image);
                if let Some(ip) = &record.ip {
                    style.stdout_metadata("ipv4", ip);
                }
                if let Some(ip) = &record.ipv6 {
                    style.stdout_metadata("ipv6", ip);
                }
                style.command(&format!("pbox ssh {box_id}"));
            }
            return Ok(());
        }
        print_box_info(
            &BoxInfo {
                id,
                vmid,
                node,
                state: record.state,
                ip: record.ip.map(|ip| ip.to_string()),
                ipv6: record.ipv6,
                name: Some(hostname),
                recipes: Vec::new(),
                capabilities: Vec::new(),
            },
            json,
            color,
        )?;
        if !json {
            CliStyle::for_stdout(color, json).stdout_metadata("image", &image);
        }
        Ok(())
    })();
    if result.is_err() && !upload_attempted {
        key.cleanup()
            .context("remove unused image preparation credentials")?;
        return result;
    }
    result.with_context(|| format!("relay bootstrap for {box_id}; use `pbox repair {box_id}` or `pbox delete {box_id} --yes` to recover; operation {}", key.operation_directory().display()))
}

pub fn wait_ready(config: &Config, box_id: &str) -> Result<()> {
    let materials = agent_materials(config, box_id)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let started = Instant::now();
        let mut explained_wait = false;
        loop {
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                let mut client = connect_agent(
                    config,
                    ENDPOINT,
                    box_id,
                    &materials.ca.certificate_pem,
                    &materials.client,
                )
                .await?;
                client.info().await?;
                Ok::<(), anyhow::Error>(())
            })
            .await;
            if matches!(result, Ok(Ok(()))) {
                return Ok(());
            }
            if !explained_wait && started.elapsed() >= Duration::from_secs(30) {
                super::progress::substep(&format!("Still waiting for {box_id}; the guest service or outbound connection may need attention"));
                explained_wait = true;
            }
            if started.elapsed() > Duration::from_secs(120) {
                let failure = match result {
                    Ok(Err(error)) => {
                        Err(error).context("agent did not become ready through relay")
                    }
                    _ => Err(anyhow::anyhow!("agent relay connection timed out")),
                };
                return failure.context(AgentStartupFailure { box_id: box_id.to_owned() });
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
}

#[derive(Debug)]
pub(crate) struct AgentStartupFailure {
    pub box_id: String,
}

impl std::fmt::Display for AgentStartupFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "No agent connection for {} after 120 seconds; the box has been kept",
            self.box_id
        )
    }
}

pub fn repair(
    config: &Config,
    key: BootstrapKey,
    mut operation: BootstrapOperation,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    let client = client_from_config(config)?;
    let record = discover_boxes(&client)?
        .into_iter()
        .find(|r| r.id.to_string() == operation.box_id);
    if let Some(record) = record {
        anyhow::ensure!(
            client.get_lxc_state(&record.node, record.vmid)? == "running",
            "start {} before repairing its relay connection",
            record.id
        );
        // A successful authenticated probe proves that PVE finished extracting the template.
        wait_ready(config, &operation.box_id)?;
        cleanup_template(&client, &key, &mut operation)?;
        key.cleanup()?;
        print_box_info(
            &BoxInfo {
                id: record.id,
                vmid: record.vmid,
                node: record.node,
                state: record.state,
                ip: record.ip.map(|ip| ip.to_string()),
                ipv6: record.ipv6,
                name: record.name,
                recipes: Vec::new(),
                capabilities: Vec::new(),
            },
            json,
            color,
        )
    } else {
        cleanup_template(&client, &key, &mut operation)?;
        key.cleanup()?;
        anyhow::bail!(
            "no guest was created; removed private bootstrap material, run pbox new again"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pbox_core::Secret;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn payload_contains_only_scoped_guest_credentials_and_restricts_private_files() {
        let key = BootstrapKey::generate("pbx_12ab34cd").unwrap();
        let key_file = key.operation_directory().join("relay-master");
        let master = "test-relay-master-that-must-never-reach-the-guest";
        fs::write(&key_file, master).unwrap();
        let mut config = Config::default();
        config.pve.token_id = Some("test@pve!cli".to_owned());
        config.pve.token_secret = Some(Secret::new("test-only"));
        config.agent.binary = Some(std::env::current_exe().unwrap());
        config.relay.url = Some("https://relay.example.com".to_owned());
        config.relay.key_file = Some(key_file);
        let directory = key.operation_directory().join("payload");
        write_payload(&directory, &config, "pbx_12ab34cd").unwrap();
        let contents = fs::read_to_string(directory.join("etc/pbox/relay.json")).unwrap();
        assert!(!contents.contains(master));
        let guest: RelayAccess = serde_json::from_str(&contents).unwrap();
        assert_eq!(
            guest.token,
            pbox_relay::scoped_token(master, "agent", "pbx_12ab34cd")
        );
        assert_ne!(
            guest.token,
            access(&config, "pbx_12ab34cd", "client").unwrap().token
        );
        assert_eq!(
            fs::metadata(directory.join("etc/pbox/server-key.pem"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let unit =
            fs::read_to_string(directory.join("etc/systemd/system/pbox-agent.service")).unwrap();
        assert!(unit.contains("--relay-config /etc/pbox/relay.json"));
        assert!(unit.contains("--listen 127.0.0.1:7443"));
        assert!(unit.contains("After=network.target"));
        assert!(!unit.contains("After=network-online.target"));
        let openrc = fs::read_to_string(directory.join("etc/init.d/pbox-agent")).unwrap();
        assert!(openrc.contains("command_background=true"));
        assert!(openrc.contains("/usr/local/bin/pbox-agent"));
        let runit = fs::read_to_string(directory.join("etc/service/pbox-agent/run")).unwrap();
        assert!(runit.starts_with("#!/bin/sh\nexec /usr/local/bin/pbox-agent"));
        let relay_script = include_str!("guest-scripts/relay.sh");
        assert!(relay_script.contains("systemd-networkd.service"));
        // Fedora applies a disable-all preset on first boot. Exercise systemd's
        // real preset resolution against the generated guest filesystem.
        let presets = directory.join("usr/lib/systemd/system-preset");
        fs::create_dir_all(&presets).unwrap();
        fs::write(presets.join("99-default.preset"), "disable *\n").unwrap();
        fs::write(
            directory.join("etc/systemd/system/multi-user.target"),
            "[Unit]\nDescription=Multi-user target\n",
        )
        .unwrap();
        for action in ["enable", "preset", "is-enabled"] {
            let output = std::process::Command::new("systemctl")
                .arg("--root")
                .arg(&directory)
                .args([action, "pbox-agent.service"])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "systemctl {action}: {} {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        key.cleanup().unwrap();
    }
}
