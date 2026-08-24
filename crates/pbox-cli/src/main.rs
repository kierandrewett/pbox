mod bootstrap;
mod recipes;
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use bootstrap::{BootstrapKey, BootstrapRequest, bootstrap_box};
use clap::{Args, Parser, Subcommand, ValueEnum};
use pbox_agent_client::{AgentClient, ExecResult};
use pbox_core::{
    Config, ConfigStore, LxcCreateRequest, PboxId, PboxMetadata, PveApi, PveClient,
    PveClientConfig, PveError, PveTaskResponse, encode_metadata, parse_metadata, select_lxc_ipv4,
};
use pbox_crypto::{
    CertificateMaterial, CertificatePurpose, client_subject, derive_context_seed,
    generate_context_ca, issue_certificate, server_subject,
};
use recipes::{RecipeCatalog, RecipeRepository};
use serde::Serialize;
use std::io::{self, IsTerminal, Write};
use std::net::Ipv4Addr;
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
    /// Discover and apply Ansible recipes.
    Recipe(RecipeCommand),
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
    /// Leave the new container stopped after bootstrap.
    #[arg(long)]
    stopped: bool,
}

#[derive(Debug, Args)]
struct ExecCommand {
    /// Public pbox identifier.
    id: String,
    /// Agent endpoint override. By default pbox resolves the guest address from PVE.
    #[arg(long)]
    endpoint: Option<String>,
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
struct RecipeCommand {
    #[command(subcommand)]
    command: RecipeSubcommand,
}

#[derive(Debug, Subcommand)]
enum RecipeSubcommand {
    /// Synchronise the configured recipe repository.
    Sync,
    /// List recipes discovered in the configured repository.
    List,
    /// Search recipe identifiers and descriptions.
    Search { text: String },
    /// Show one recipe and its metadata.
    Info { recipe: String },
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
    ip: Option<String>,
    name: Option<String>,
}

#[derive(Debug, Serialize)]
struct BoxInfo {
    id: PboxId,
    vmid: u64,
    state: String,
    node: String,
    ip: Option<String>,
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
        Command::Recipe(command) => {
            run_recipe(command.command, &store, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
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

fn run_recipe(
    command: RecipeSubcommand,
    store: &ConfigStore,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    match command {
        RecipeSubcommand::Sync => {
            let config = load_config(store)?;
            let catalog = recipe_repository(&config)?.sync()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&catalog)?);
            } else {
                println!(
                    "synced {} recipes from {} @ {}",
                    catalog.recipes.len(),
                    catalog.repository,
                    catalog.revision
                );
            }
        }
        RecipeSubcommand::List => {
            let catalog = load_recipe_catalog(store)?;
            print_recipe_catalog(&catalog, json, color)?;
        }
        RecipeSubcommand::Search { text } => {
            let catalog = load_recipe_catalog(store)?;
            let query = text.to_lowercase();
            let filtered =
                RecipeCatalog {
                    recipes: catalog
                        .recipes
                        .iter()
                        .filter(|recipe| {
                            recipe.id.to_lowercase().contains(&query)
                                || recipe.metadata.description.as_deref().is_some_and(
                                    |description| description.to_lowercase().contains(&query),
                                )
                        })
                        .cloned()
                        .collect(),
                    ..catalog
                };
            print_recipe_catalog(&filtered, json, color)?;
        }
        RecipeSubcommand::Info { recipe } => {
            let catalog = load_recipe_catalog(store)?;
            let result = catalog
                .recipes
                .iter()
                .find(|candidate| candidate.id == recipe)
                .ok_or_else(|| anyhow!("recipe not found: {recipe}"))?;
            if json {
                println!("{}", serde_json::to_string_pretty(result)?);
            } else {
                print_recipe_info(result, color_enabled(color, json));
            }
        }
    }
    Ok(())
}

fn recipe_repository(config: &Config) -> Result<RecipeRepository> {
    RecipeRepository::new(
        Some(config.recipes.repository.as_str()),
        &config.recipes.reference,
    )
}

fn load_recipe_catalog(store: &ConfigStore) -> Result<RecipeCatalog> {
    let config = load_config(store)?;
    let repository = recipe_repository(&config)?;
    if config.recipes.auto_sync {
        repository.sync()
    } else {
        repository.discover()
    }
}

fn print_recipe_catalog(catalog: &RecipeCatalog, json: bool, color: ColorChoice) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(catalog)?);
        return Ok(());
    }
    let colour = color_enabled(color, json);
    let accent = if colour { "\x1b[36m" } else { "" };
    let reset = if colour { "\x1b[0m" } else { "" };
    println!(
        "{accent}recipes{reset} {} @ {}",
        catalog.repository, catalog.revision
    );
    println!("{:<28} {:<10} DESCRIPTION", "ID", "KIND");
    for recipe in &catalog.recipes {
        println!(
            "{:<28} {:<10} {}",
            recipe.id,
            recipe.kind.as_str(),
            recipe.metadata.description.as_deref().unwrap_or("")
        );
    }
    Ok(())
}

fn print_recipe_info(recipe: &recipes::Recipe, colour: bool) {
    let accent = if colour { "\x1b[36m" } else { "" };
    let reset = if colour { "\x1b[0m" } else { "" };
    println!("{accent}{}{reset}", recipe.id);
    println!("kind: {}", recipe.kind.as_str());
    println!("path: {}", recipe.path);
    if let Some(description) = recipe.metadata.description.as_deref() {
        println!("description: {description}");
    }
    if !recipe.metadata.requires.is_empty() {
        println!("requires: {}", recipe.metadata.requires.join(", "));
    }
    if !recipe.metadata.supports.is_empty() {
        println!("supports: {}", recipe.metadata.supports.join(", "));
    }
    if !recipe.metadata.capabilities.is_empty() {
        println!("capabilities: {}", recipe.metadata.capabilities.join(", "));
    }
    let resources = &recipe.metadata.resources;
    if resources.cores.is_some() || resources.memory.is_some() || resources.disk.is_some() {
        println!(
            "resources: cores={} memory={} disk={}",
            resources
                .cores
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            resources
                .memory
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_owned()),
            resources.disk.as_deref().unwrap_or("-")
        );
    }
}

fn run_exec(store: &ConfigStore, command: ExecCommand, json: bool) -> Result<RunOutcome> {
    validate_exec_arguments(&command)?;
    let config = load_config(store)?;
    let (box_id, endpoint) = resolve_agent_target(&config, &command)?;
    let materials = agent_materials(&config, &box_id)?;
    let env = command
        .env
        .iter()
        .map(|entry| parse_env_entry(entry))
        .collect::<Result<Vec<_>>>()?;

    let cwd = command.cwd;
    let user = command.user;
    let argv = command.argv;
    let ca_pem = materials.ca.certificate_pem.clone();
    let client_identity = materials.client;
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
        ("box id", command.id.as_str()),
        ("working directory", command.cwd.as_str()),
        ("guest user", command.user.as_str()),
    ] {
        if value.contains('\0') {
            return Err(anyhow!("{label} cannot contain NUL bytes"));
        }
    }
    if let Some(endpoint) = command.endpoint.as_deref() {
        if endpoint.is_empty() {
            return Err(anyhow!("agent endpoint cannot be empty"));
        }
        if endpoint.contains('\0') {
            return Err(anyhow!("agent endpoint cannot contain NUL bytes"));
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
    let agent_binary = resolve_agent_binary(&config)?;
    if !agent_binary.is_file() {
        return Err(anyhow!(
            "pbox-agent binary does not exist: {}",
            agent_binary.display()
        ));
    }
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
    let id_text = id.to_string();
    let materials = agent_materials(&config, &id_text)?;
    let key = BootstrapKey::generate(&id_text).context("create temporary bootstrap SSH key")?;
    let metadata = PboxMetadata::new(id.clone(), vmid).with_node(&command.node);
    let description = format!(
        "Managed by `pbox`.\n{}",
        encode_metadata(&metadata).context("encode pbox metadata")?
    );
    let hostname = command
        .name
        .unwrap_or_else(|| format!("pbox-{}", id_text.trim_start_matches("pbx_")));
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
        ssh_public_keys: Some(key.public_key().to_owned()),
        start: Some(true),
    };
    let task = match client.create_lxc(&command.node, vmid, &request) {
        Ok(task) => task,
        Err(error) => {
            let _ = key.cleanup();
            return Err(error)
                .with_context(|| format!("create box {id} with VMID {vmid} on {}", command.node));
        }
    };
    if let Err(error) = wait_for_task(&client, &command.node, task) {
        let cleanup = key.cleanup().err();
        return Err(error)
            .context(format!(
                "PVE did not finish creating box {id} with VMID {vmid} on {}",
                command.node
            ))
            .context(format_bootstrap_cleanup_failure(&key, cleanup.as_ref()));
    }

    let ip = match wait_for_lxc_ip(&client, &command.node, vmid) {
        Ok(ip) => ip,
        Err(error) => {
            let cleanup = key.cleanup().err();
            return Err(error)
                .context("wait for the new container IPv4 address")
                .context(format_bootstrap_cleanup_failure(&key, cleanup.as_ref()));
        }
    };
    let bootstrap_request = BootstrapRequest {
        box_id: &id_text,
        ip,
        port: config.agent.port,
        key: &key,
        agent_binary: &agent_binary,
        server_identity: &materials.server,
        client_ca: &materials.ca,
        client_subject: &materials.client_subject,
    };
    bootstrap_box(&bootstrap_request).with_context(|| {
        format!(
            "bootstrap box {id}; temporary SSH credentials remain in {} for repair",
            key.operation_directory().display()
        )
    })?;
    key.cleanup()
        .context("remove completed bootstrap credentials")?;

    let (state, output_ip) = if command.stopped {
        let task = client
            .shutdown_lxc(&command.node, vmid)
            .with_context(|| format!("stop bootstrapped box {id}"))?;
        wait_for_task(&client, &command.node, task)
            .with_context(|| format!("PVE did not finish stopping box {id}"))?;
        ("stopped".to_owned(), None)
    } else {
        ("running".to_owned(), Some(ip.to_string()))
    };
    let info = BoxInfo {
        id,
        vmid,
        state,
        node: command.node,
        ip: output_ip,
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
    let ip = if state == "running" {
        Some(
            wait_for_lxc_ip(&client, &record.node, record.vmid)
                .with_context(|| format!("discover IPv4 address for box {}", record.id))?
                .to_string(),
        )
    } else {
        None
    };
    let info = BoxInfo {
        id: record.id,
        vmid: record.vmid,
        state: state.to_owned(),
        node: record.node,
        ip,
        name: record.name,
    };
    print_box_info(&info, json, color)
}
fn format_bootstrap_cleanup_failure(
    key: &BootstrapKey,
    cleanup_error: Option<&anyhow::Error>,
) -> String {
    match cleanup_error {
        Some(error) => format!(
            "could not remove temporary bootstrap credentials in {}: {error}",
            key.operation_directory().display()
        ),
        None => format!(
            "temporary bootstrap credentials were removed from {}",
            key.operation_directory().display()
        ),
    }
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
        ip: record.ip,
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

struct AgentMaterials {
    ca: CertificateMaterial,
    client: CertificateMaterial,
    client_subject: String,
    server: CertificateMaterial,
}

fn agent_materials(config: &Config, box_id: &str) -> Result<AgentMaterials> {
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
    let client_subject_text = client_subject(&seed);
    let client = issue_certificate(&ca, &client_subject_text, CertificatePurpose::Client)
        .context("create short-lived pbox agent client certificate")?;
    let server_name = server_subject(box_id).context("create pbox agent server identity")?;
    let server = issue_certificate(&ca, &server_name, CertificatePurpose::Server)
        .context("create pbox agent server certificate")?;
    Ok(AgentMaterials {
        ca,
        client,
        client_subject: client_subject_text,
        server,
    })
}

fn resolve_agent_target(config: &Config, command: &ExecCommand) -> Result<(String, String)> {
    let id: PboxId = command.id.parse().context("parse pbox id")?;
    if let Some(endpoint) = command.endpoint.as_ref() {
        return Ok((id.to_string(), endpoint.clone()));
    }
    let client = client_from_config(config)?;
    let record = find_box(&client, &id.to_string())?;
    if record.state != "running" {
        return Err(anyhow!("box {} is not running", record.id));
    }
    let ip = record
        .ip
        .ok_or_else(|| anyhow!("box {} has no discovered IPv4 address", record.id))?;
    Ok((
        record.id.to_string(),
        format!("https://{ip}:{}", config.agent.port),
    ))
}

fn resolve_agent_binary(config: &Config) -> Result<PathBuf> {
    if let Some(path) = config.agent.binary.as_ref() {
        return Ok(path.clone());
    }
    let mut candidates = Vec::new();
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        candidates.push(parent.join("pbox-agent"));
    }
    if let Ok(current_dir) = std::env::current_dir() {
        candidates.push(current_dir.join("target/debug/pbox-agent"));
        candidates.push(current_dir.join("target/release/pbox-agent"));
    }
    if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
        return Ok(path);
    }
    Err(anyhow!(
        "pbox-agent binary was not found; set agent.binary with `pbox config set agent.binary /path/to/pbox-agent`"
    ))
}

const LXC_IP_TIMEOUT: Duration = Duration::from_secs(60);
const LXC_IP_POLL_INTERVAL: Duration = Duration::from_secs(1);

fn wait_for_lxc_ip(client: &impl PveApi, node: &str, vmid: u64) -> Result<Ipv4Addr> {
    let started = Instant::now();
    let mut last_error = None;
    while started.elapsed() < LXC_IP_TIMEOUT {
        match discover_lxc_ip(client, node, vmid) {
            Ok(Some(address)) => {
                return address
                    .parse()
                    .with_context(|| format!("parse discovered IPv4 address {address}"));
            }
            Ok(None) => {}
            Err(error) => last_error = Some(error.to_string()),
        }
        thread::sleep(LXC_IP_POLL_INTERVAL);
    }
    Err(anyhow!(
        "could not discover an IPv4 address within {} seconds{}",
        LXC_IP_TIMEOUT.as_secs(),
        last_error
            .as_deref()
            .map(|error| format!(": {error}"))
            .unwrap_or_default()
    ))
}

fn discover_lxc_ip(client: &impl PveApi, node: &str, vmid: u64) -> Result<Option<String>> {
    let interfaces = client
        .list_lxc_interfaces(node, vmid)
        .context("read runtime LXC interfaces")?;
    Ok(select_lxc_ipv4(&interfaces).map(|address| address.to_string()))
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
        let state = resource.status.unwrap_or_else(|| "unknown".to_owned());
        let ip = if state == "running" {
            discover_lxc_ip(client, node, vmid).with_context(|| {
                format!(
                    "discover IPv4 address for box {metadata_id}",
                    metadata_id = metadata.id
                )
            })?
        } else {
            None
        };
        let name = config.hostname.or(resource.name);
        records.push(BoxRecord {
            id: metadata.id,
            vmid,
            state,
            node: node.to_owned(),
            ip,
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
        println!("  ip:     {}", info.ip.as_deref().unwrap_or("-"));
        println!("  name:   {}", info.name.as_deref().unwrap_or("-"));
    }
    Ok(())
}

fn print_box_records(records: &[BoxRecord], colour: bool) {
    println!(
        "{:<16} {:<10} {:<16} {:<16} NAME",
        "ID", "STATE", "NODE", "IP"
    );
    for record in records {
        let id = if colour {
            format!("\x1b[1;36m{}\x1b[0m", record.id)
        } else {
            record.id.to_string()
        };
        println!(
            "{id:<16} {:<10} {:<16} {:<16} {}",
            record.state,
            record.node,
            record.ip.as_deref().unwrap_or("-"),
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
