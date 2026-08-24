use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand, ValueEnum};
use pbox_core::{
    Config, ConfigStore, LxcCreateRequest, PboxId, PboxMetadata, PveApi, PveClient,
    PveClientConfig, PveError, PveTaskResponse, encode_metadata, parse_metadata,
};
use serde::Serialize;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

const PVE_TASK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const PVE_TASK_POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Debug, Parser)]
#[command(
    name = "pbox",
    about = "Proxmox-backed developer sandbox CLI",
    arg_required_else_help = true
)]
struct Cli {
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto)]
    color: ColorChoice,

    #[arg(long, global = true)]
    json: bool,

    #[arg(long, global = true, env = "PBOX_CONFIG_FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ColorChoice {
    Auto,
    Always,
    Never,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate a public pbox identifier.
    Id,
    /// Manage local pbox configuration.
    Config(ConfigCommand),
    /// Create a pbox-managed LXC container.
    New(NewCommand),
    /// List pbox-managed containers discovered from PVE metadata.
    List,
    /// Show one pbox discovered from PVE metadata.
    Info { id: String },
    /// Start a stopped pbox-managed container.
    Start { id: String },
    /// Stop a running pbox-managed container.
    Stop {
        id: String,
        /// Stop immediately instead of requesting a graceful shutdown.
        #[arg(long)]
        force: bool,
    },
    /// Permanently delete a pbox-managed container.
    Delete {
        id: String,
        /// Confirm the destructive operation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Args)]
struct NewCommand {
    /// PVE node on which to create the container.
    #[arg(long)]
    node: String,
    /// PVE template volume, for example local:vztmpl/debian-13.tar.zst.
    #[arg(long)]
    ostemplate: String,
    /// Root filesystem volume, for example local-zfs:8.
    #[arg(long)]
    rootfs: String,
    /// Container network configuration, for example name=eth0,bridge=vmbr0,ip=dhcp.
    #[arg(long)]
    net0: String,
    /// Container hostname. Defaults to a name derived from the public ID.
    #[arg(long)]
    name: Option<String>,
    /// Memory limit in MiB.
    #[arg(long)]
    memory: Option<u64>,
    /// Swap limit in MiB.
    #[arg(long)]
    swap: Option<u64>,
    /// CPU core count.
    #[arg(long)]
    cores: Option<u64>,
    /// Leave the new container stopped.
    #[arg(long)]
    stopped: bool,
}

#[derive(Debug, Args)]
struct ConfigCommand {
    #[command(subcommand)]
    command: ConfigSubcommand,
}

#[derive(Debug, Subcommand)]
enum ConfigSubcommand {
    /// Read one configuration value.
    Get { key: String },
    /// Show all configuration values with secrets redacted.
    List,
    /// Set one configuration value.
    Set { key: String, value: Option<String> },
    /// Reset one configuration value to its default.
    Unset { key: String },
}

#[derive(Debug, Serialize)]
struct IdOutput {
    id: PboxId,
}

#[derive(Debug, Serialize)]
struct BoxRecord {
    id: PboxId,
    vmid: u64,
    state: String,
    node: String,
    name: Option<String>,
}

#[derive(Debug, Serialize)]
struct BoxInfo {
    id: PboxId,
    vmid: u64,
    state: String,
    node: String,
    name: Option<String>,
}

#[derive(Debug, Serialize)]
struct DeleteOutput {
    id: PboxId,
    vmid: u64,
    node: String,
    deleted: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("error: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let store = ConfigStore::new(
        cli.config
            .unwrap_or_else(pbox_core::config::default_config_path),
    );
    match cli.command {
        Command::Id => print_value(
            &IdOutput {
                id: PboxId::generate(),
            },
            cli.json,
            cli.color,
        ),
        Command::Config(command) => run_config(command.command, &store, cli.json),
        Command::New(command) => run_new(&store, command, cli.json, cli.color),
        Command::List => run_list(&store, cli.json, cli.color),
        Command::Info { id } => run_info(&store, &id, cli.json, cli.color),
        Command::Start { id } => run_start(&store, &id, cli.json, cli.color),
        Command::Stop { id, force } => run_stop(&store, &id, force, cli.json, cli.color),
        Command::Delete { id, yes } => run_delete(&store, &id, yes, cli.json, cli.color),
    }
}

fn run_config(command: ConfigSubcommand, store: &ConfigStore, json: bool) -> Result<()> {
    match command {
        ConfigSubcommand::Get { key } => {
            let config = store.load_file().context("load pbox configuration")?;
            let value = config
                .get_redacted(&key)
                .ok_or_else(|| anyhow!("unknown config key: {key}"))?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({"key": key, "value": value}))?
                );
            } else {
                println!("{key} = {value}");
            }
        }
        ConfigSubcommand::List => {
            let config = store.load_file().context("load pbox configuration")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&config.redacted())?);
            } else {
                for (key, value) in config.redacted_pairs() {
                    println!("{key:<24} {value}");
                }
            }
        }
        ConfigSubcommand::Set { key, value } => {
            let value = value.unwrap_or_else(read_value_from_stdin);
            let mut config = store.load_file().context("load pbox configuration")?;
            config
                .set_value(&key, &value)
                .context("validate configuration value")?;
            store.save(&config).context("save pbox configuration")?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"key": key, "value": config.get_redacted(&key)})
                    )?
                );
            } else {
                println!(
                    "{key}: {}",
                    config
                        .get_redacted(&key)
                        .unwrap_or_else(|| "<unset>".to_owned())
                );
            }
        }
        ConfigSubcommand::Unset { key } => {
            let mut config = store.load_file().context("load pbox configuration")?;
            config
                .unset_value(&key)
                .context("reset configuration value")?;
            store.save(&config).context("save pbox configuration")?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({"key": key, "value": config.get_redacted(&key)})
                    )?
                );
            } else {
                println!(
                    "{key}: {}",
                    config
                        .get_redacted(&key)
                        .unwrap_or_else(|| "<unset>".to_owned())
                );
            }
        }
    }
    Ok(())
}

fn read_value_from_stdin() -> String {
    eprint!("value: ");
    let _ = io::Write::flush(&mut io::stderr());
    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .expect("read configuration value");
    value.trim_end_matches(['\r', '\n']).to_owned()
}
fn run_new(store: &ConfigStore, command: NewCommand, json: bool, color: ColorChoice) -> Result<()> {
    let config = load_config(store)?;
    let client = client_from_config(&config)?;
    let resources = client
        .list_cluster_resources()
        .context("list PVE resources for VMID allocation")?;
    let used_vmids = resources.iter().filter_map(|resource| resource.vmid);
    let vmid = config
        .vmid_pattern
        .allocate_lowest(used_vmids)
        .context("allocate a free PVE VMID")?;
    let existing = discover_boxes(&client)?;
    let id = generate_unique_id(&existing)?;
    let metadata = PboxMetadata::new(id.clone(), vmid).with_node(&command.node);
    let description = format!(
        "Managed by `pbox`.\n{}",
        encode_metadata(&metadata).context("encode pbox metadata")?
    );
    let hostname = command
        .name
        .unwrap_or_else(|| format!("pbox-{}", id.to_string().trim_start_matches("pbx_")));
    let request = LxcCreateRequest {
        ostemplate: Some(command.ostemplate),
        hostname: Some(hostname.clone()),
        memory: command.memory,
        swap: command.swap,
        cores: command.cores,
        rootfs: Some(command.rootfs),
        net0: Some(command.net0),
        unprivileged: Some(true),
        description: Some(description),
        start: Some(!command.stopped),
    };
    let task = client
        .create_lxc(&command.node, vmid, &request)
        .with_context(|| format!("create box {id} with VMID {vmid} on {}", command.node))?;
    wait_for_task(&client, &command.node, task).with_context(|| {
        format!(
            "PVE did not finish creating box {id} with VMID {vmid} on {}",
            command.node
        )
    })?;
    let info = BoxInfo {
        id,
        vmid,
        state: if command.stopped {
            "stopped".to_owned()
        } else {
            "running".to_owned()
        },
        node: command.node,
        name: Some(hostname),
    };
    print_box_info(&info, json, color)
}

fn run_start(
    store: &ConfigStore,
    requested_id: &str,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    run_box_task(
        store,
        requested_id,
        "start",
        "running",
        json,
        color,
        |client, record| client.start_lxc(&record.node, record.vmid),
    )
}

fn run_stop(
    store: &ConfigStore,
    requested_id: &str,
    force: bool,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    let action = if force {
        "force stop"
    } else {
        "graceful shutdown"
    };
    run_box_task(
        store,
        requested_id,
        action,
        "stopped",
        json,
        color,
        move |client, record| {
            if force {
                client.stop_lxc(&record.node, record.vmid)
            } else {
                client.shutdown_lxc(&record.node, record.vmid)
            }
        },
    )
}

fn run_box_task<F>(
    store: &ConfigStore,
    requested_id: &str,
    action: &str,
    state: &str,
    json: bool,
    color: ColorChoice,
    operation: F,
) -> Result<()>
where
    F: FnOnce(&PveClient, &BoxRecord) -> Result<PveTaskResponse, PveError>,
{
    let client = client_from_store(store)?;
    let record = find_box(&client, requested_id)?;
    let task =
        operation(&client, &record).with_context(|| format!("{action} box {}", record.id))?;
    wait_for_task(&client, &record.node, task)
        .with_context(|| format!("PVE did not finish {action} for box {}", record.id))?;
    let info = BoxInfo {
        id: record.id,
        vmid: record.vmid,
        state: state.to_owned(),
        node: record.node,
        name: record.name,
    };
    print_box_info(&info, json, color)
}

fn run_delete(
    store: &ConfigStore,
    requested_id: &str,
    confirmed: bool,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    if !confirmed {
        return Err(anyhow!(
            "deleting a box is permanent; repeat the command with --yes"
        ));
    }
    let client = client_from_store(store)?;
    let record = find_box(&client, requested_id)?;
    let task = client
        .delete_lxc(&record.node, record.vmid)
        .with_context(|| format!("delete box {}", record.id))?;
    wait_for_task(&client, &record.node, task)
        .with_context(|| format!("PVE did not finish deleting box {}", record.id))?;
    let output = DeleteOutput {
        id: record.id,
        vmid: record.vmid,
        node: record.node,
        deleted: true,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        let _ = color_enabled(color, json);
        println!("deleted {}", output.id);
    }
    Ok(())
}

fn generate_unique_id(existing: &[BoxRecord]) -> Result<PboxId> {
    for _ in 0..16 {
        let id = PboxId::generate();
        if existing.iter().all(|record| record.id != id) {
            return Ok(id);
        }
    }
    Err(anyhow!("could not allocate a unique pbox identifier"))
}

fn wait_for_task(client: &impl PveApi, node: &str, task: PveTaskResponse) -> Result<()> {
    let started = Instant::now();
    loop {
        let status = client
            .get_task_status(node, &task.upid)
            .context("read PVE task status")?;
        if status.status == "stopped" {
            if status.is_successful() {
                return Ok(());
            }
            return Err(anyhow!(
                "PVE task failed: {}",
                status
                    .exitstatus
                    .as_deref()
                    .unwrap_or("missing exit status")
            ));
        }
        if started.elapsed() >= PVE_TASK_TIMEOUT {
            return Err(anyhow!("PVE task did not finish within five minutes"));
        }
        thread::sleep(PVE_TASK_POLL_INTERVAL);
    }
}

fn run_list(store: &ConfigStore, json: bool, color: ColorChoice) -> Result<()> {
    let client = client_from_store(store)?;
    let records = discover_boxes(&client)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&records)?);
    } else {
        let colour = color_enabled(color, json);
        print_box_records(&records, colour);
    }
    Ok(())
}

fn run_info(store: &ConfigStore, requested_id: &str, json: bool, color: ColorChoice) -> Result<()> {
    let client = client_from_store(store)?;
    let record = find_box(&client, requested_id)?;
    let info = BoxInfo {
        id: record.id,
        vmid: record.vmid,
        state: record.state,
        node: record.node,
        name: record.name,
    };
    print_box_info(&info, json, color)
}

fn load_config(store: &ConfigStore) -> Result<Config> {
    store
        .load(&Default::default())
        .context("load pbox configuration")
}

fn client_from_store(store: &ConfigStore) -> Result<PveClient> {
    let config = load_config(store)?;
    client_from_config(&config)
}

fn client_from_config(config: &Config) -> Result<PveClient> {
    let url = config
        .pve
        .url
        .clone()
        .ok_or_else(|| anyhow!("pve.url is not configured"))?;
    let token_id = config
        .pve
        .token_id
        .clone()
        .ok_or_else(|| anyhow!("pve.token_id is not configured"))?;
    let token_secret = config
        .pve
        .token_secret
        .clone()
        .ok_or_else(|| anyhow!("pve.token_secret is not configured"))?;
    let mut client_config = PveClientConfig::new(url, token_id, token_secret);
    client_config.tls_insecure = config.pve.tls_insecure;
    PveClient::new(client_config).context("create PVE client")
}

fn discover_boxes(client: &impl PveApi) -> Result<Vec<BoxRecord>> {
    let resources = client
        .list_cluster_resources()
        .context("list PVE resources")?;
    let mut records = Vec::new();
    for resource in resources
        .into_iter()
        .filter(|resource| resource.resource_type == "lxc")
    {
        let Some(node) = resource.node.as_deref() else {
            continue;
        };
        let Some(vmid) = resource.vmid else { continue };
        let config = client
            .get_lxc_config(node, vmid)
            .with_context(|| format!("read metadata for PVE container on {node}"))?;
        let Some(description) = config.description.as_deref() else {
            continue;
        };
        let Some(metadata) = parse_metadata(description).context("parse pbox metadata")? else {
            continue;
        };
        if metadata.vmid != vmid
            || metadata
                .node
                .as_deref()
                .is_some_and(|metadata_node| metadata_node != node)
        {
            continue;
        }
        let name = config.hostname.or(resource.name);
        records.push(BoxRecord {
            id: metadata.id,
            vmid,
            state: resource.status.unwrap_or_else(|| "unknown".to_owned()),
            node: node.to_owned(),
            name,
        });
    }
    records.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(records)
}

fn find_box(client: &impl PveApi, requested_id: &str) -> Result<BoxRecord> {
    let id: PboxId = requested_id.parse().context("parse pbox id")?;
    discover_boxes(client)?
        .into_iter()
        .find(|record| record.id == id)
        .ok_or_else(|| anyhow!("box '{id}' was not found"))
}

fn print_box_info(info: &BoxInfo, json: bool, color: ColorChoice) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(info)?);
    } else {
        let _ = color_enabled(color, json);
        println!("box");
        println!("  id:     {}", info.id);
        println!("  vmid:   {}", info.vmid);
        println!("  state:  {}", info.state);
        println!("  node:   {}", info.node);
        println!("  name:   {}", info.name.as_deref().unwrap_or("-"));
    }
    Ok(())
}

fn print_box_records(records: &[BoxRecord], colour: bool) {
    println!("{:<16} {:<10} {:<16} NAME", "ID", "STATE", "NODE");
    for record in records {
        let id = if colour {
            format!("\x1b[1;36m{}\x1b[0m", record.id)
        } else {
            record.id.to_string()
        };
        println!(
            "{id:<16} {:<10} {:<16} {}",
            record.state,
            record.node,
            record.name.as_deref().unwrap_or("-")
        );
    }
    if records.is_empty() {
        println!("No pbox-managed containers found.");
    }
}

fn print_value<T: Serialize>(value: &T, json: bool, color: ColorChoice) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(value)?);
        return Ok(());
    }
    let value = serde_json::to_value(value)?;
    if let Some(id) = value.get("id").and_then(serde_json::Value::as_str) {
        if color_enabled(color, false) {
            println!("\x1b[1;36m{id}\x1b[0m");
        } else {
            println!("{id}");
        }
    } else {
        println!("{value}");
    }
    Ok(())
}

fn color_enabled(color: ColorChoice, json: bool) -> bool {
    let mode = match color {
        ColorChoice::Auto => pbox_core::ui::ColorMode::Auto,
        ColorChoice::Always => pbox_core::ui::ColorMode::Always,
        ColorChoice::Never => pbox_core::ui::ColorMode::Never,
    };
    mode.enabled(
        io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
        json,
    )
}

#[cfg(test)]
mod tests {
    use pbox_core::ui::ColorMode;

    #[test]
    fn colour_is_disabled_for_json_and_no_color() {
        assert!(!ColorMode::Always.enabled(true, false, true));
        assert!(!ColorMode::Auto.enabled(true, true, false));
    }
}
