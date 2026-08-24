use anyhow::{Context, Result, anyhow};
use base64::Engine;
use clap::{Args, Parser, Subcommand, ValueEnum};
use pbox_agent_client::{AgentClient, ExecResult};
use pbox_core::{
    Config, ConfigStore, LxcCreateRequest, PboxId, PboxMetadata, PveApi, PveClient,
    PveClientConfig, PveError, PveTaskResponse, encode_metadata, parse_metadata,
};
use pbox_crypto::{
    CertificatePurpose, client_subject, derive_context_seed, generate_context_ca, issue_certificate,
};
use serde::Serialize;
use std::io::{self, IsTerminal, Write};
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
    /// Execute a non-interactive command through pbox-agent.
    Exec(ExecCommand),
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
struct ExecCommand {
    /// Public pbox identifier.
    id: String,
    /// Agent endpoint, for example https://10.0.20.43:7443.
    #[arg(long)]
    endpoint: String,
    /// Working directory for the command.
    #[arg(long, default_value = "/home/pbox")]
    cwd: String,
    /// Guest user for the command.
    #[arg(long, default_value = "pbox")]
    user: String,
    /// Environment entry in KEY=VALUE form. May be repeated.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// Command and arguments. Use -- before arguments that start with a hyphen.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    argv: Vec<String>,
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

#[derive(Debug, Serialize)]
struct ExecOutput {
    stdout_base64: String,
    stderr_base64: String,
    code: i32,
    signal: i32,
    exited: bool,
    exit_code: i32,
}

#[derive(Debug, PartialEq, Eq)]
enum RunOutcome {
    Success,
    Exit(i32),
}

fn main() {
    match run() {
        Ok(RunOutcome::Success) => {}
        Ok(RunOutcome::Exit(code)) => std::process::exit(code),
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::exit(1);
        }
    }
}

fn run() -> Result<RunOutcome> {
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
        )
        .map(|_| RunOutcome::Success),
        Command::Config(command) => {
            run_config(command.command, &store, cli.json).map(|_| RunOutcome::Success)
        }
        Command::New(command) => {
            run_new(&store, command, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
        Command::Exec(command) => run_exec(&store, command, cli.json),
        Command::List => run_list(&store, cli.json, cli.color).map(|_| RunOutcome::Success),
        Command::Info { id } => {
            run_info(&store, &id, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
        Command::Start { id } => {
            run_start(&store, &id, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
        Command::Stop { id, force } => {
            run_stop(&store, &id, force, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
        Command::Delete { id, yes } => {
            run_delete(&store, &id, yes, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
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

fn run_exec(store: &ConfigStore, command: ExecCommand, json: bool) -> Result<RunOutcome> {
    let config = load_config(store)?;
    let token_id = config
        .pve
        .token_id
        .as_deref()
        .ok_or_else(|| anyhow!("pve.token_id is not configured"))?;
    let token_secret = config
        .pve
        .token_secret
        .as_ref()
        .map(|secret| secret.expose())
        .ok_or_else(|| anyhow!("pve.token_secret is not configured"))?;
    let seed = derive_context_seed(token_id, token_secret);
    let ca = generate_context_ca(&seed).context("derive pbox agent trust root")?;
    let client_identity =
        issue_certificate(&ca, &client_subject(&seed), CertificatePurpose::Client)
            .context("create short-lived pbox agent client certificate")?;
    let env = command
        .env
        .iter()
        .map(|entry| parse_env_entry(entry))
        .collect::<Result<Vec<_>>>()?;
    validate_exec_arguments(&command)?;

    let endpoint = command.endpoint;
    let box_id = command.id;
    let cwd = command.cwd;
    let user = command.user;
    let argv = command.argv;
    let ca_pem = ca.certificate_pem.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create async runtime for pbox-agent")?;
    let result = runtime.block_on(async move {
        let mut client = AgentClient::connect(&endpoint, &box_id, &ca_pem, &client_identity)
            .await
            .context("connect to pbox-agent")?;
        client
            .info()
            .await
            .context("validate pbox-agent identity")?;
        client
            .exec(argv, cwd, env, user)
            .await
            .context("execute command through pbox-agent")
    })?;

    if json {
        let output = ExecOutput {
            stdout_base64: base64::engine::general_purpose::STANDARD.encode(&result.stdout),
            stderr_base64: base64::engine::general_purpose::STANDARD.encode(&result.stderr),
            code: result.code,
            signal: result.signal,
            exited: result.exited,
            exit_code: exec_exit_code(&result),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        write_exec_streams(&result)?;
    }

    let exit_code = exec_exit_code(&result);
    if exit_code == 0 {
        Ok(RunOutcome::Success)
    } else {
        Ok(RunOutcome::Exit(exit_code))
    }
}

fn parse_env_entry(entry: &str) -> Result<(String, String)> {
    let (name, value) = entry
        .split_once('=')
        .ok_or_else(|| anyhow!("environment entry must use KEY=VALUE form: {entry}"))?;
    if name.is_empty() {
        return Err(anyhow!("environment variable name cannot be empty"));
    }
    if name.contains('\0') || value.contains('\0') {
        return Err(anyhow!("environment entries cannot contain NUL bytes"));
    }
    Ok((name.to_owned(), value.to_owned()))
}

fn validate_exec_arguments(command: &ExecCommand) -> Result<()> {
    for (label, value) in [
        ("agent endpoint", command.endpoint.as_str()),
        ("box id", command.id.as_str()),
        ("working directory", command.cwd.as_str()),
        ("guest user", command.user.as_str()),
    ] {
        if value.contains('\0') {
            return Err(anyhow!("{label} cannot contain NUL bytes"));
        }
    }
    if command.argv.is_empty() {
        return Err(anyhow!("exec requires a command"));
    }
    if command.argv.iter().any(|argument| argument.contains('\0')) {
        return Err(anyhow!("command arguments cannot contain NUL bytes"));
    }
    Ok(())
}

fn write_exec_streams(result: &ExecResult) -> Result<()> {
    let mut stdout = io::stdout();
    stdout
        .write_all(&result.stdout)
        .context("write pbox-agent stdout")?;
    stdout.flush().context("flush pbox-agent stdout")?;
    let mut stderr = io::stderr();
    stderr
        .write_all(&result.stderr)
        .context("write pbox-agent stderr")?;
    stderr.flush().context("flush pbox-agent stderr")?;
    Ok(())
}

fn exec_exit_code(result: &ExecResult) -> i32 {
    let code = if result.signal > 0 {
        result.signal.saturating_add(128)
    } else {
        result.code
    };
    if code < 0 { 1 } else { code.min(255) }
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
    use super::{exec_exit_code, parse_env_entry};
    use pbox_agent_client::ExecResult;
    use pbox_core::ui::ColorMode;
    #[test]
    fn colour_is_disabled_for_json_and_no_color() {
        assert!(!ColorMode::Always.enabled(true, false, true));
        assert!(!ColorMode::Auto.enabled(true, true, false));
    }

    #[test]
    fn parse_env_entry_splits_on_first_equals() {
        assert_eq!(
            parse_env_entry("GREETING=hello=world").unwrap(),
            ("GREETING".to_owned(), "hello=world".to_owned())
        );
    }

    #[test]
    fn parse_env_entry_rejects_invalid_entries() {
        assert!(parse_env_entry("MISSING_EQUALS").is_err());
        assert!(parse_env_entry("=missing-name").is_err());
        assert!(parse_env_entry("BAD\0VALUE=x").is_err());
    }

    #[test]
    fn exec_exit_code_maps_signals_and_invalid_codes() {
        assert_eq!(
            exec_exit_code(&ExecResult {
                code: 0,
                ..Default::default()
            }),
            0
        );
        assert_eq!(
            exec_exit_code(&ExecResult {
                code: 7,
                ..Default::default()
            }),
            7
        );
        assert_eq!(
            exec_exit_code(&ExecResult {
                signal: 9,
                ..Default::default()
            }),
            137
        );
        assert_eq!(
            exec_exit_code(&ExecResult {
                code: -1,
                ..Default::default()
            }),
            1
        );
    }
}
