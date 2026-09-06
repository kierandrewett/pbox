//! Independent saved environments: full PVE clones converted to container templates.
use super::*;
use clap::{Args, Subcommand};
use pbox_core::{LxcCloneRequest, LxcConfig};
use serde::{Deserialize, Serialize};

const MARKER: &str = "pbox-snapshot-v1 ";
const CLONE_MARKER: &str = "pbox-clone-v1 ";
const STAGE: &str = "/etc/pbox-snapshot";

#[derive(Debug, Args)]
pub struct SnapshotCommand {
    #[command(subcommand)]
    command: Action,
}
#[derive(Debug, Subcommand)]
enum Action {
    /// Save an independent environment in PVE. Briefly stops the source while copying.
    Create {
        /// Source box ID, or current when exactly one box exists.
        #[arg(default_value = "current")]
        source: String,
        #[arg(long)]
        name: String,
        /// Target PVE root-directory storage. Defaults to the source storage.
        #[arg(long)]
        storage: Option<String>,
    },
    /// List saved environments independently of their source boxes.
    List,
    /// Show a saved environment.
    Info { snapshot: String },
    /// Delete a saved environment. Existing full clones remain intact.
    #[command(visible_alias = "rm")]
    Delete {
        snapshot: String,
        #[arg(long)]
        yes: bool,
    },
    /// Remove preparation files left on a source box by an interrupted snapshot operation.
    RepairSource { source: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SavedEnvironment {
    #[serde(default)]
    pub ready: bool,
    pub id: String,
    pub name: String,
    pub node: String,
    pub vmid: u64,
    pub source: String,
    pub created: String,
    pub bootstrap_id: String,
    pub recipes: Vec<PboxRecipeProvenance>,
    pub capabilities: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingClone {
    #[serde(default)]
    configuration: LxcConfigUpdateRequest,
    snapshot: SavedEnvironment,
    name: Option<String>,
    stopped: bool,
}

fn validate_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 63
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            && !name.starts_with("psn_"),
        "snapshot names must use 1-63 letters, digits, hyphens or underscores, without the reserved psn_ prefix"
    );
    Ok(())
}
fn saved_description(saved: &SavedEnvironment) -> Result<String> {
    Ok(format!(
        "Independent pbox saved environment.\n{MARKER}{}",
        serde_json::to_string(saved)?
    ))
}
fn parse_saved(description: &str) -> Result<Option<SavedEnvironment>> {
    let lines: Vec<_> = description
        .lines()
        .filter_map(|line| line.strip_prefix(MARKER))
        .collect();
    anyhow::ensure!(lines.len() <= 1, "duplicate snapshot metadata");
    lines
        .first()
        .map(|line| serde_json::from_str(line).context("read snapshot metadata"))
        .transpose()
}
fn inventory(client: &impl PveApi) -> Result<Vec<SavedEnvironment>> {
    let mut result = Vec::new();
    for resource in client.list_cluster_resources()? {
        if resource.resource_type != "lxc" {
            continue;
        }
        let (Some(node), Some(vmid)) = (resource.node, resource.vmid) else {
            continue;
        };
        let config = client.get_lxc_config(&node, vmid)?;
        if let Some(mut saved) = parse_saved(config.description.as_deref().unwrap_or(""))? {
            anyhow::ensure!(
                saved.vmid == vmid && saved.node == node,
                "snapshot {} metadata does not match its PVE location",
                saved.id
            );
            saved.ready = template(&config);
            result.push(saved);
        }
    }
    result.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
    Ok(result)
}
fn find(client: &impl PveApi, reference: &str) -> Result<SavedEnvironment> {
    let matches: Vec<_> = inventory(client)?
        .into_iter()
        .filter(|s| s.id == reference || s.name == reference)
        .collect();
    match matches.len() {
        0 => bail!("snapshot '{reference}' was not found; use `pbox snapshot list`"),
        1 => Ok(matches.into_iter().next().unwrap()),
        _ => bail!("snapshot name '{reference}' is ambiguous; use its psn_ ID"),
    }
}
fn template(config: &LxcConfig) -> bool {
    matches!(
        config.extra.get("template"),
        Some(serde_json::Value::Bool(true))
    ) || config
        .extra
        .get("template")
        .and_then(|value| value.as_u64())
        == Some(1)
}

pub fn run(
    command: SnapshotCommand,
    store: &ConfigStore,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    let config = load_config(store)?;
    let client = client_from_config(&config)?;
    match command.command {
        Action::List => {
            let saved = inventory(&client)?;
            if json {
                ui::json_text(&serde_json::to_string_pretty(&saved)?);
            } else {
                ui::saved_environments(&saved);
            }
        }
        Action::Info { snapshot } => {
            let saved = find(&client, &snapshot)?;
            if json {
                ui::json_text(&serde_json::to_string_pretty(&saved)?);
            } else {
                ui::saved_environments(&[saved]);
            }
        }
        Action::Create {
            source,
            name,
            storage,
        } => create(&config, &client, &source, &name, storage, json)?,
        Action::Delete { snapshot, yes } => {
            let saved = find(&client, &snapshot)?;
            if !yes {
                anyhow::ensure!(
                    !json && io::stdin().is_terminal() && io::stderr().is_terminal(),
                    "repeat with --yes to delete snapshot {}",
                    saved.id
                );
                ui::stderr().section("Delete snapshot");
                ui::stderr().metadata("name", &saved.name);
                ui::stderr().warning(
                    "This permanently deletes the saved environment. Existing boxes remain intact.",
                );
                if !confirm_delete(&mut io::stdin().lock(), &mut io::stderr().lock())? {
                    ui::stderr().hint("Cancelled.");
                    return Ok(());
                }
            }
            wait_for_task(
                &client,
                &saved.node,
                client.delete_lxc(&saved.node, saved.vmid)?,
            )?;
            if json {
                ui::json_text(&serde_json::json!({"id":saved.id,"deleted":true}).to_string());
            } else {
                ui::stdout().success(&format!("Deleted snapshot {}", saved.name));
            }
        }
        Action::RepairSource { source } => {
            let record = find_box_reference(&client, &source)?;
            let was_running = client.get_lxc_state(&record.node, record.vmid)? == "running";
            if !was_running {
                wait_for_task(
                    &client,
                    &record.node,
                    client.start_lxc(&record.node, record.vmid)?,
                )?;
            }
            let prior = runtime()?.block_on(async {
                let mut agent=wait_box(&config,record.id.as_str()).await?;
                let script=format!("if [ -f /etc/pbox-snapshot/prior-state ]; then cat /etc/pbox-snapshot/prior-state; else printf '%s' {}; fi",if was_running {"running"} else {"stopped"});
                let result=agent.exec(vec!["/bin/sh".into(),"-c".into(),script],"/",Vec::<(String,String)>::new(),"root").await?;
                anyhow::ensure!(result.exited && result.code==0,"could not read source recovery state");
                match String::from_utf8(result.stdout)?.trim() {
                    "running"=>Ok(true), "stopped"=>Ok(false), _=>bail!("invalid source recovery state")
                }
            })?;
            restore_source(&config, &client, &record, prior)?;
            if !json {
                ui::stdout().success("Source preparation cleaned up");
            }
        }
    }
    let _ = color;
    Ok(())
}

pub(super) fn clone_full(
    client: &impl PveApi,
    config: &Config,
    node: &str,
    source_vmid: u64,
    hostname: &str,
    description: impl Fn(u64) -> Result<String>,
    storage: Option<String>,
) -> Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let occupied: BTreeSet<_> = client
            .list_cluster_resources()?
            .iter()
            .filter_map(|r| r.vmid)
            .collect();
        let vmid = config.vmid_pattern.allocate_lowest(occupied.iter())?;
        let request = LxcCloneRequest {
            newid: vmid,
            hostname: hostname.into(),
            description: description(vmid)?,
            full: 1,
            storage: storage.clone(),
        };
        let task = match client.clone_lxc(node, source_vmid, &request) {
            Ok(task) => task,
            Err(error)
                if is_vmid_conflict(&error) || error.to_string().contains("locked (disk)") =>
            {
                thread::sleep(Duration::from_millis(500));
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        wait_for_task(client, node, task).with_context(|| {
            format!("clone task failed; inspect PVE VMID {vmid} before retrying")
        })?;
        return Ok(vmid);
    }
    bail!("could not allocate a free VMID for the full clone")
}
fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}
async fn connect_box(config: &Config, id: &str) -> Result<AgentClient> {
    let (_, endpoint) = resolve_agent_endpoint(config, id, None)?;
    let materials = agent_materials(config, id)?;
    let mut client = relay::connect_agent(
        config,
        &endpoint,
        id,
        &materials.ca.certificate_pem,
        &materials.client,
    )
    .await?;
    client.info().await?;
    Ok(client)
}
async fn root(client: &mut AgentClient, script: &str) -> Result<()> {
    let result = client
        .exec(
            vec!["/bin/sh".into(), "-c".into(), script.into()],
            "/",
            Vec::<(String, String)>::new(),
            "root",
        )
        .await?;
    anyhow::ensure!(
        result.exited && result.code == 0 && result.signal == 0,
        "guest preparation failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(())
}
async fn wait_box(config: &Config, id: &str) -> Result<AgentClient> {
    let start = Instant::now();
    loop {
        match connect_box(config, id).await {
            Ok(client) => return Ok(client),
            Err(error) if start.elapsed() > Duration::from_secs(120) => {
                return Err(error).context("wait for source agent");
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

fn create(
    config: &Config,
    client: &impl PveApi,
    source: &str,
    name: &str,
    storage: Option<String>,
    json: bool,
) -> Result<()> {
    validate_name(name)?;
    anyhow::ensure!(
        !inventory(client)?.iter().any(|s| s.name == name),
        "snapshot name '{name}' already exists"
    );
    let record = find_box_reference(client, source)?;
    let original = client.get_lxc_config(&record.node, record.vmid)?;
    for (key, value) in &original.extra {
        anyhow::ensure!(
            !(key.starts_with("dev")
                || key.starts_with("mp") && value.as_str().is_some_and(|v| v.starts_with('/'))),
            "snapshot cannot make an independent copy of external mount/device {key}"
        );
    }
    let host = original
        .hostname
        .as_deref()
        .context("source has no PVE hostname")?;
    anyhow::ensure!(
        host.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".-".contains(&b)),
        "unsupported source hostname"
    );
    let bootstrap_id = PboxId::generate().to_string();
    let mut saved = SavedEnvironment {
        ready: false,
        id: format!("psn_{}", &bootstrap_id[4..]),
        name: name.into(),
        node: record.node.clone(),
        vmid: 0,
        source: record.id.to_string(),
        created: current_timestamp()?,
        bootstrap_id,
        recipes: vec![],
        capabilities: record.capabilities.clone(),
    };
    if let Some(metadata) = parse_metadata(original.description.as_deref().unwrap_or(""))? {
        saved.recipes = metadata.recipes;
    }
    let was_running = client.get_lxc_state(&record.node, record.vmid)? == "running";
    if !was_running {
        wait_for_task(
            client,
            &record.node,
            client.start_lxc(&record.node, record.vmid)?,
        )?;
    }
    let rt = runtime()?;
    let mut owned = false;
    let result: Result<()> = (|| {
        if !json {
            ui::stderr().progress("Preparing the source for an independent copy...");
        }
        rt.block_on(async {
            let mut agent = wait_box(config,record.id.as_str()).await?;
            root(&mut agent,&format!("set -eu; mkdir -m 0700 /etc/pbox-snapshot; printf '%s' {} > /etc/pbox-snapshot/prior-state", if was_running {"running"} else {"stopped"})).await.context("source already has snapshot preparation; wait for the current capture or run `pbox snapshot repair-source BOX`")?;
            owned = true;
            prepare_source(config,&mut agent,&saved,host).await
        })?;
        if !json {
            ui::stderr()
                .progress("Temporarily stopping the source and saving a full copy in PVE...");
        }
        wait_for_task(
            client,
            &record.node,
            client.shutdown_lxc(&record.node, record.vmid)?,
        )?;
        anyhow::ensure!(
            client.get_lxc_state(&record.node, record.vmid)? == "stopped",
            "source did not stop; snapshot was not created"
        );
        saved.vmid = clone_full(
            client,
            config,
            &record.node,
            record.vmid,
            &format!("pbox-snapshot-{}", &saved.id[4..]),
            |vmid| {
                let mut s = saved.clone();
                s.vmid = vmid;
                saved_description(&s)
            },
            storage,
        )?;
        let previous_tasks: BTreeSet<_> = client
            .template_tasks(&saved.node, saved.vmid)?
            .into_iter()
            .map(|task| task.upid)
            .collect();
        client.template_lxc(&saved.node, saved.vmid)?;
        let started = Instant::now();
        loop {
            if let Some(task) = client
                .template_tasks(&saved.node, saved.vmid)?
                .into_iter()
                .find(|task| !previous_tasks.contains(&task.upid))
            {
                wait_for_task(client, &saved.node, task)?;
                break;
            }
            anyhow::ensure!(
                started.elapsed() < Duration::from_secs(30),
                "template task was not reported; inspect snapshot {} (VMID {})",
                saved.id,
                saved.vmid
            );
            thread::sleep(Duration::from_millis(250));
        }
        anyhow::ensure!(
            template(&client.get_lxc_config(&saved.node, saved.vmid)?),
            "PVE did not convert {} into a template",
            saved.id
        );
        Ok(())
    })();
    let cleanup = if owned {
        restore_source(config, client, &record, was_running)
    } else if !was_running {
        wait_for_task(
            client,
            &record.node,
            client.shutdown_lxc(&record.node, record.vmid)?,
        )
    } else {
        Ok(())
    };
    if let Err(error) = result {
        return Err(match cleanup {
            Ok(()) => error,
            Err(cleanup) => error.context(format!(
                "source recovery also failed: {cleanup:#}; run `pbox snapshot repair-source {}`",
                record.id
            )),
        });
    }
    cleanup.with_context(|| format!("snapshot {} was saved, but source cleanup failed; run `pbox snapshot repair-source {}`",saved.id,record.id))?;
    saved.ready = true;
    if json {
        ui::json_text(&serde_json::to_string_pretty(&saved)?);
    } else {
        ui::stdout().success(&format!("Saved snapshot {}", saved.name));
        ui::saved_environments(&[saved]);
    }
    Ok(())
}

fn restore_source(
    config: &Config,
    client: &impl PveApi,
    record: &BoxRecord,
    running: bool,
) -> Result<()> {
    if client.get_lxc_state(&record.node, record.vmid)? != "running" {
        wait_for_task(
            client,
            &record.node,
            client.start_lxc(&record.node, record.vmid)?,
        )?;
    }
    let result = runtime()?.block_on(async {
        let mut agent = wait_box(config, record.id.as_str()).await?;
        root(&mut agent, SOURCE_CLEANUP).await
    });
    if !running {
        wait_for_task(
            client,
            &record.node,
            client.shutdown_lxc(&record.node, record.vmid)?,
        )?;
    }
    result
}
const SOURCE_CLEANUP: &str = "set -eu\nrm -f /etc/systemd/system/pbox-agent.service.d/90-snapshot.conf /etc/systemd/system/multi-user.target.wants/pbox-snapshot.service /etc/systemd/system/pbox-snapshot.service\nrm -rf /etc/pbox-snapshot\nsystemctl daemon-reload\n";

async fn prepare_source(
    config: &Config,
    agent: &mut AgentClient,
    saved: &SavedEnvironment,
    host: &str,
) -> Result<()> {
    root(
        agent,
        "set -eu; mkdir -p /etc/systemd/system/pbox-agent.service.d",
    )
    .await?;
    // Exclusive directory rejects simultaneous preparations of the same source.

    root(
        agent,
        "set -eu; stat -c %a / > /etc/pbox-snapshot/root-mode",
    )
    .await?;
    let materials = agent_materials(config, &saved.bootstrap_id)?;
    for (name, contents) in [
        ("server.pem", materials.server.certificate_pem),
        ("server-key.pem", materials.server.private_key_pem),
        ("client-ca.pem", materials.ca.certificate_pem),
    ] {
        agent
            .put_file(
                format!("{STAGE}/{name}"),
                contents.into_bytes(),
                0o600,
                true,
            )
            .await?;
    }
    agent
        .put_file(
            format!("{STAGE}/pbox-agent"),
            fs::read(resolve_agent_binary(config)?)?,
            0o700,
            true,
        )
        .await?;
    let relay_arg = if config.relay.url.is_some() {
        let access = relay::snapshot_access(config, &saved.bootstrap_id)?;
        agent
            .put_file(
                format!("{STAGE}/relay.json"),
                serde_json::to_vec(&access)?,
                0o600,
                true,
            )
            .await?;
        " --relay-config /etc/pbox-snapshot/relay.json --snapshot-bootstrap"
    } else {
        ""
    };
    agent
        .put_file(
            format!("{STAGE}/sanitize"),
            SANITIZE.as_bytes().to_vec(),
            0o700,
            true,
        )
        .await?;
    let unit = format!(
        "[Unit]\nDescription=pbox snapshot bootstrap\nConditionHost=!{host}\nAfter=network.target\nBefore=ssh.service sshd.service\n[Service]\nExecStartPre=/etc/pbox-snapshot/sanitize\nExecStart=/etc/pbox-snapshot/pbox-agent --listen 0.0.0.0:{} --box-id {} --certificate /etc/pbox-snapshot/server.pem --private-key /etc/pbox-snapshot/server-key.pem --client-ca /etc/pbox-snapshot/client-ca.pem{relay_arg}\nRestart=always\nRestartSec=2\n[Install]\nWantedBy=multi-user.target\n",
        config.agent.port, saved.bootstrap_id
    );
    agent
        .put_file(
            "/etc/systemd/system/pbox-snapshot.service",
            unit.into_bytes(),
            0o644,
            true,
        )
        .await?;
    agent
        .put_file(
            "/etc/systemd/system/pbox-agent.service.d/90-snapshot.conf",
            format!("[Unit]\nConditionHost={host}\n").into_bytes(),
            0o644,
            true,
        )
        .await?;
    root(
        agent,
        "set -eu; systemctl daemon-reload; systemctl enable pbox-snapshot.service",
    )
    .await
}
const SANITIZE: &str = r#"#!/bin/sh
set -eu
if [ ! -f /etc/pbox-snapshot/sanitized ]; then
    # Some PVE storage backends make the template root directory mode 0444.
    # Preserve the source image's permissions when turning it back into a box.
    chmod "$(cat /etc/pbox-snapshot/root-mode)" /
    rm -f /etc/pbox/server.pem /etc/pbox/server-key.pem /etc/pbox/client-ca.pem /etc/pbox/relay.json
    rm -f /etc/machine-id /var/lib/dbus/machine-id /etc/ssh/ssh_host_*
    systemd-machine-id-setup
    ssh-keygen -A
    touch /etc/pbox-snapshot/sanitized
fi
"#;

fn fresh_secondary_networks(
    config: &LxcConfig,
) -> Result<std::collections::BTreeMap<String, String>> {
    let mut result = std::collections::BTreeMap::new();
    for (key, value) in &config.extra {
        if !key
            .strip_prefix("net")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let value = value.as_str().context("invalid saved network interface")?;
        let fields: Vec<_> = value
            .split(',')
            .filter_map(|field| {
                let (name, value) = field.split_once('=')?;
                match name {
                    "hwaddr" | "gw" | "gw6" => None,
                    "ip" if value != "manual" => Some("ip=dhcp".to_owned()),
                    "ip6" if value != "manual" => Some("ip6=auto".to_owned()),
                    _ => Some(field.to_owned()),
                }
            })
            .collect();
        result.insert(key.clone(), fields.join(","));
    }
    Ok(result)
}

pub fn restore(config: &Config, command: NewCommand, json: bool, color: ColorChoice) -> Result<()> {
    let client = client_from_config(config)?;
    let saved = find(
        &client,
        command.snapshot.as_deref().context("missing snapshot")?,
    )?;
    let template_config = client.get_lxc_config(&saved.node, saved.vmid)?;
    anyhow::ensure!(
        template(&template_config),
        "snapshot {} is not a completed PVE template",
        saved.id
    );
    anyhow::ensure!(
        command.node.as_ref().is_none_or(|node| node == &saved.node),
        "snapshot is on node {}; cross-node cloning requires shared-storage support",
        saved.node
    );
    anyhow::ensure!(
        command.rootfs.is_none(),
        "--rootfs cannot replace a snapshot disk; clone retains its saved disks"
    );
    let id = generate_unique_id(&discover_boxes(&client)?)?;
    let configuration = LxcConfigUpdateRequest {
        networks: fresh_secondary_networks(&template_config)?,
        cores: command.cores,
        memory: command.memory,
        swap: command.swap,
        net0: Some(
            command
                .net0
                .clone()
                .unwrap_or_else(|| format!("name=eth0,bridge={},ip=dhcp", config.pve.bridge)),
        ),
        ..Default::default()
    };
    let pending = PendingClone {
        configuration,
        snapshot: saved.clone(),
        name: command.name.clone(),
        stopped: command.stopped,
    };
    let hostname = format!("pbox-{}", &id.as_str()[4..]);
    if !json {
        ui::stderr().progress(&format!("Cloning snapshot {} in Proxmox...", saved.name));
    }
    clone_full(
        &client,
        config,
        &saved.node,
        saved.vmid,
        &hostname,
        |vmid| {
            let mut metadata = PboxMetadata::new(id.clone(), vmid).with_node(&saved.node);
            metadata.recipes = saved.recipes.clone();
            metadata.capabilities = saved.capabilities.clone();
            Ok(format!(
                "{}\n{CLONE_MARKER}{}",
                encode_metadata(&metadata)?,
                serde_json::to_string(&pending)?
            ))
        },
        None,
    )?;
    finalize(config, &client, id.as_str(), &pending, json, color).with_context(|| {
        format!(
            "clone {} is retained for recovery; run `pbox repair {}`",
            id, id
        )
    })
}

pub fn repair(config: &Config, id: &str, json: bool, color: ColorChoice) -> Result<bool> {
    let client = client_from_config(config)?;
    let Some(record) = discover_boxes(&client)?
        .into_iter()
        .find(|record| record.id.as_str() == id)
    else {
        return Ok(false);
    };
    let conf = client.get_lxc_config(&record.node, record.vmid)?;
    let Some(line) = conf
        .description
        .as_deref()
        .unwrap_or("")
        .lines()
        .find_map(|line| line.strip_prefix(CLONE_MARKER))
    else {
        return Ok(false);
    };
    let pending: PendingClone = serde_json::from_str(line)?;
    finalize(config, &client, id, &pending, json, color)?;
    Ok(true)
}
fn finalize(
    config: &Config,
    client: &impl PveApi,
    id: &str,
    pending: &PendingClone,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    let _ = color;
    let record = find_box(client, id)?;
    if pending.configuration != LxcConfigUpdateRequest::default() {
        client.update_lxc_config(&record.node, record.vmid, &pending.configuration)?;
    }
    if client.get_lxc_state(&record.node, record.vmid)? != "running" {
        wait_for_task(
            client,
            &record.node,
            client.start_lxc(&record.node, record.vmid)?,
        )?;
    }
    let rt = runtime()?;
    if !json {
        ui::stderr().progress("Giving the new box its own identity...");
    }
    rt.block_on(async {
        // Repair also accepts a clone which already completed its credential handoff.
        if let Ok(mut agent) = connect_box(config,id).await {
            root(&mut agent, SOURCE_CLEANUP).await?;
            return Ok(());
        }
        let materials=agent_materials(config,&pending.snapshot.bootstrap_id)?;
        let route=format!("{}~{id}",pending.snapshot.bootstrap_id);
        let started=Instant::now();
        let mut agent=loop {
            let connection: Result<AgentClient> = async {
                let endpoint = if config.relay.url.is_some() { relay::ENDPOINT.to_owned() } else { resolve_agent_endpoint(config,id,None)?.1 };
                let access = if config.relay.url.is_some() { Some(relay::route_access(config,&route)?) } else { None };
                Ok(AgentClient::connect_with_relay_route(&endpoint,&pending.snapshot.bootstrap_id,&materials.ca.certificate_pem,&materials.client,access.as_ref(),&route).await?)
            }.await;
            match connection {
                Ok(mut agent) => { agent.info().await?; break agent; },
                Err(error) if started.elapsed()>Duration::from_secs(120) => return Err(anyhow!(error)).context(if config.relay.url.is_some() { "wait for clone bootstrap agent; the relay must support snapshot bootstrap routes" } else { "wait for clone bootstrap agent; check the guest route or configure a relay" }),
                Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        };
        let workspace=BootstrapKey::generate(id)?;
        let payload=workspace.operation_directory().join("payload");
        let result:Result<()> = async {
            relay::write_payload(&payload,config,id)?;
            upload_directory(&mut agent,&payload,&payload).await?;
            root(&mut agent,&format!("{SOURCE_CLEANUP}systemctl enable pbox-agent.service\n")).await?;
            Ok(())
        }.await;
        workspace.cleanup()?;
        result
    })?;
    // A fresh boot also makes the regenerated machine ID effective for PID 1.
    wait_for_task(
        client,
        &record.node,
        client.shutdown_lxc(&record.node, record.vmid)?,
    )?;
    anyhow::ensure!(
        client.get_lxc_state(&record.node, record.vmid)? == "stopped",
        "clone did not shut down after personalisation"
    );
    if let Some(name) = &pending.name {
        client.update_lxc_config(
            &record.node,
            record.vmid,
            &LxcConfigUpdateRequest {
                hostname: Some(name.clone()),
                ..Default::default()
            },
        )?;
    }
    if !pending.stopped {
        wait_for_task(
            client,
            &record.node,
            client.start_lxc(&record.node, record.vmid)?,
        )?;
        rt.block_on(wait_box(config, id))?;
    }
    let conf = client.get_lxc_config(&record.node, record.vmid)?;
    let description = conf
        .description
        .as_deref()
        .unwrap_or("")
        .lines()
        .filter(|line| !line.starts_with(CLONE_MARKER))
        .collect::<Vec<_>>()
        .join("\n");
    client.update_lxc_config(
        &record.node,
        record.vmid,
        &LxcConfigUpdateRequest {
            digest: conf.digest,
            description: Some(description),
            ..Default::default()
        },
    )?;
    let mut record = find_box(client, id)?;
    record.state = client.get_lxc_state(&record.node, record.vmid)?;
    if !json {
        if !pending.stopped {
            let access = rt.block_on(async {
                let mut agent = wait_box(config, id).await?;
                Ok::<_, anyhow::Error>(guest::check_client(&mut agent, "pbox").await)
            })?;
            ui::user_access(id, "pbox", access);
        }
        ui::stdout().success(&format!(
            "Created {id} from snapshot {}",
            pending.snapshot.name
        ));
        if let Some(ip) = &record.ip {
            ui::stdout().stdout_metadata("ipv4", ip);
        }
        if let Some(ip) = &record.ipv6 {
            ui::stdout().stdout_metadata("ipv6", ip);
        }
        ui::stdout().command(&format!(
            "pbox {} {id}",
            if pending.stopped { "start" } else { "ssh" }
        ));
    } else {
        ui::json_text(
            &serde_json::json!({"id":id,"vmid":record.vmid,"node":record.node,
            "snapshot":pending.snapshot.id,"name":record.name,"ip":record.ip,"ipv6":record.ipv6,
            "recipes":record.recipes,"capabilities":record.capabilities,"state":record.state})
            .to_string(),
        );
    }
    Ok(())
}
async fn upload_directory(agent: &mut AgentClient, base: &Path, directory: &Path) -> Result<()> {
    let mut pending = vec![directory.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            let meta = entry.metadata()?;
            if meta.is_dir() {
                pending.push(path);
            } else {
                let remote = format!("/{}", path.strip_prefix(base)?.to_string_lossy());
                agent
                    .put_file(
                        remote,
                        fs::read(&path)?,
                        meta.permissions().mode() & 0o777,
                        true,
                    )
                    .await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secondary_interfaces_do_not_reuse_static_addresses_or_macs() {
        let config:LxcConfig=serde_json::from_value(serde_json::json!({
            "net1":"name=eth1,bridge=vmbr1,tag=42,hwaddr=00:11:22:33:44:55,ip=10.0.0.2/24,gw=10.0.0.1,ip6=fd00::2/64,gw6=fd00::1",
            "net2":"name=eth2,bridge=vmbr2,ip=manual,ip6=manual"
        })).unwrap();
        let networks = fresh_secondary_networks(&config).unwrap();
        assert_eq!(
            networks["net1"],
            "name=eth1,bridge=vmbr1,tag=42,ip=dhcp,ip6=auto"
        );
        assert_eq!(
            networks["net2"],
            "name=eth2,bridge=vmbr2,ip=manual,ip6=manual"
        );
    }

    #[test]
    fn saved_environment_metadata_is_independent_of_box_metadata() {
        let saved = SavedEnvironment {
            ready: true,
            id: "psn_12345678".into(),
            name: "llm-ready".into(),
            node: "pve".into(),
            vmid: 9100,
            source: "pbx_abcdefgh".into(),
            created: "2026-09-06T00:00:00Z".into(),
            bootstrap_id: "pbx_12345678".into(),
            recipes: vec![],
            capabilities: vec![],
        };
        let description = saved_description(&saved).unwrap();
        assert!(parse_metadata(&description).unwrap().is_none());
        let restored = parse_saved(&description).unwrap().unwrap();
        assert_eq!(restored.id, saved.id);
        assert_eq!(restored.source, saved.source);
        assert!(parse_saved(&format!("{description}\n{description}")).is_err());
    }
    #[test]
    fn snapshot_commands_use_snapshot_identity_except_when_capturing() {
        assert!(Cli::try_parse_from(["pbox", "snapshot", "list"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "pbox",
                "snapshot",
                "create",
                "current",
                "--name",
                "llm-ready"
            ])
            .is_ok()
        );
        assert!(Cli::try_parse_from(["pbox", "snapshot", "rm", "llm-ready", "--yes"]).is_ok());
        assert!(Cli::try_parse_from(["pbox", "new", "--snapshot", "llm-ready"]).is_ok());
        assert!(
            Cli::try_parse_from([
                "pbox",
                "new",
                "--snapshot",
                "llm-ready",
                "--image",
                "debian"
            ])
            .is_err()
        );
        assert!(Cli::try_parse_from(["pbox", "snapshot", "rollback", "current", "base"]).is_err());
        assert!(validate_name("../outside").is_err());
        assert!(validate_name("psn_reserved").is_err());
    }
}
