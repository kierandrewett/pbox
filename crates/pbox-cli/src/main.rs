#![deny(clippy::print_stdout, clippy::print_stderr)]
mod ansible;
mod bootstrap;
mod guest;
mod images;
mod progress;
mod snapshots;
mod ui;
use ui::*;
mod recipes;
mod relay;
use ansible::{AnsibleRun, apply_recipe};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use bootstrap::{
    AgentProbeRequest, BootstrapKey, BootstrapOperation, BootstrapRequest, bootstrap_box,
    cleanup_bootstrap_with_fallback, wait_for_agent,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use images::{
    oci_reference_for_image, oci_template_present, prepare_oci_template, search_oci_repository,
};
#[cfg(unix)]
use nix::sys::signal::Signal;
#[cfg(unix)]
use nix::sys::termios::{LocalFlags, SetArg, cfmakeraw, tcgetattr, tcsetattr};
use pbox_agent_client::{AgentClient, ExecInput, ExecResult, exec_event};
use pbox_core::{
    Config, ConfigStore, LxcConfigUpdateRequest, LxcCreateRequest, LxcSnapshotRequest, PboxId,
    PboxMetadata, PboxRecipeProvenance, PveApi, PveClient, PveClientConfig, PveError,
    PveNetworkInterface, PveStorage, PveStorageContent, PveTaskResponse, encode_metadata,
    parse_duration, parse_metadata, preserve_metadata, select_lxc_ipv4, select_lxc_ipv6,
};
use pbox_crypto::{
    CertificateMaterial, CertificatePurpose, client_subject, derive_context_seed,
    generate_context_ca, issue_certificate, server_subject,
};
use recipes::{RecipeCatalog, RecipeRepository};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::net::Ipv4Addr;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

const PVE_TASK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_FORWARD_CONNECTIONS: usize = 64;
const PVE_TASK_POLL_INTERVAL: Duration = Duration::from_millis(250);
const MAX_FILE_TRANSFER_BYTES: u64 = 64 * 1024 * 1024;
const SSH_POST_EXIT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Parser)]
#[command(
    name = "pbox",
    about = "Proxmox-backed developer sandbox CLI",
    styles = ui::help_styles(),
    arg_required_else_help = true
)]
struct Cli {
    /// Show image preparation and provisioning details.
    #[arg(short, long, global = true)]
    verbose: bool,
    #[arg(long, global = true, value_enum, default_value_t = ColorChoice::Auto)]
    color: ColorChoice,

    #[arg(long, global = true)]
    json: bool,

    #[arg(long, global = true, env = "PBOX_CONFIG_FILE")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}
#[derive(Debug, Args)]
struct SetupCommand {
    /// Save the configuration without checking the PVE connection.
    #[arg(long)]
    skip_verify: bool,
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
    /// Configure pbox for first use.
    #[command(alias = "onboard")]
    Setup(SetupCommand),
    /// Manage local pbox configuration.
    Config(ConfigCommand),
    /// Search OCI images, list tags, and prepare PVE templates.
    Image(ImageCommand),
    /// Create a pbox-managed LXC container.
    New(NewCommand),
    /// Resume an interrupted guest-agent bootstrap.
    Repair { id: String },
    /// Open an interactive shell through pbox-agent.
    Ssh(SshCommand),
    /// Execute a non-interactive command through pbox-agent.
    Exec(ExecCommand),
    /// Copy files between the control machine and a pbox.
    Scp(ScpCommand),
    /// Forward a local TCP port to a pbox guest.
    Forward(ForwardCommand),
    /// Discover and apply Ansible recipes.
    Recipe(RecipeCommand),
    /// Manage independent saved environments stored as PVE templates.
    Snapshot(snapshots::SnapshotCommand),
    /// Manage per-box PVE rollback checkpoints (deleted with their box).
    Checkpoint(CheckpointCommand),
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
    #[command(visible_alias = "rm")]
    Delete {
        /// Public pbox identifier, or `current` when exactly one box exists.
        #[arg(value_name = "ID|current")]
        id: String,
        /// Skip the interactive deletion confirmation.
        #[arg(long)]
        yes: bool,
        /// Gracefully shut down and wait for deletion instead of queuing an immediate stop/delete.
        #[arg(long)]
        wait: bool,
    },
}

#[derive(Debug, Args)]
struct NewCommand {
    /// Create an independent box from a saved snapshot ID or name.
    #[arg(long, conflicts_with_all = ["image", "ostemplate"])]
    snapshot: Option<String>,
    /// PVE node. Defaults to automatic selection from online nodes.
    #[arg(long)]
    node: Option<String>,
    /// PVE template alias or OCI image reference. Missing templates are pulled through PVE.
    #[arg(long)]
    image: Option<String>,
    /// Raw PVE template volume override.
    #[arg(long)]
    ostemplate: Option<String>,
    /// Root filesystem volume override, for example local-zfs:8G.
    #[arg(long)]
    rootfs: Option<String>,
    /// Container network configuration override.
    #[arg(long)]
    net0: Option<String>,
    /// Container hostname. Defaults to a name derived from the public ID.
    #[arg(long)]
    name: Option<String>,
    /// Memory limit in MiB. Defaults to pve.defaults.memory.
    #[arg(long)]
    memory: Option<u64>,
    /// Swap limit in MiB. Defaults to pve.defaults.swap.
    #[arg(long)]
    swap: Option<u64>,
    /// CPU core count. Defaults to pve.defaults.cores.
    #[arg(long)]
    cores: Option<u64>,
    /// Leave the new container stopped after bootstrap.
    #[arg(long)]
    stopped: bool,
}
#[derive(Debug, Args)]
struct ImageCommand {
    #[command(subcommand)]
    command: ImageSubcommand,
}

#[derive(Debug, Subcommand)]
enum ImageSubcommand {
    /// Search registries for images by name or keyword.
    Search(ImageSearchCommand),
    /// List available tags for a specific OCI repository.
    Tags(ImageTagsCommand),
    /// Pull a tagged OCI image into PVE template storage.
    Pull(ImagePullCommand),
}

#[derive(Debug, Args)]
struct ImageSearchCommand {
    /// Search term (e.g. debian or docker.io/debian). Prompts when omitted.
    query: Option<String>,
    /// Registry for an unqualified search term.
    #[arg(long, default_value = "docker.io")]
    registry: String,
    /// Maximum number of images to show.
    #[arg(long, default_value_t = 25)]
    limit: usize,
}

#[derive(Debug, Args)]
struct ImageTagsCommand {
    /// OCI repository or image reference, for example ghcr.io/example/base.
    repository: String,
    /// PVE node used for the registry request.
    #[arg(long)]
    node: Option<String>,
    /// Maximum number of tags to print.
    #[arg(long, default_value_t = 25)]
    limit: usize,
}

#[derive(Debug, Args)]
struct ImagePullCommand {
    /// OCI image reference, for example ghcr.io/example/base:latest. Defaults to :latest.
    reference: String,
    /// PVE node used for the pull.
    #[arg(long)]
    node: Option<String>,
    /// PVE storage for the downloaded template. Defaults to pve.template-storage.
    #[arg(long)]
    storage: Option<String>,
}

#[derive(Debug, Serialize)]
struct ImagePullOutput {
    reference: String,
    node: String,
    storage: String,
    volume: String,
    downloaded: bool,
}
#[derive(Debug, Args)]
struct SshCommand {
    /// Public pbox identifier.
    id: String,
    /// Agent endpoint override. By default pbox resolves the guest address from PVE.
    #[arg(long)]
    endpoint: Option<String>,
    /// Working directory for the shell.
    #[arg(long, default_value = "/home/pbox")]
    cwd: String,
    /// Guest user for the shell.
    #[arg(long, default_value = "pbox")]
    user: String,
    /// Environment entry in KEY=VALUE form. May be repeated.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// Shell command and arguments. Defaults to the guest user's login shell.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    argv: Vec<String>,
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
struct ScpCommand {
    /// Local or remote source path.
    source: String,
    /// Local or remote destination path.
    destination: String,
}
#[derive(Debug, Args)]
struct ForwardCommand {
    /// Public pbox identifier.
    id: String,
    /// Local TCP port to listen on.
    local_port: u16,
    /// Local address to bind. Defaults to loopback.
    #[arg(long, default_value = "127.0.0.1")]
    listen: String,
    /// Agent endpoint override. By default pbox resolves the guest address from PVE.
    #[arg(long)]
    endpoint: Option<String>,
    /// Guest TCP host. Defaults to 127.0.0.1.
    #[arg(long, default_value = "127.0.0.1")]
    remote_host: String,
    /// Guest TCP port. Defaults to the local port.
    #[arg(long)]
    remote_port: Option<u16>,
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
    /// Apply a recipe to a box through the pbox Ansible connection.
    ///
    /// Recipe repositories are trusted controller-side code. They can run
    /// Ansible local actions and use controller file-transfer operations.
    Apply {
        /// Recipe identifier from `pbox recipe list`.
        recipe: String,
        /// Public pbox identifier.
        #[arg(long)]
        box_id: String,
    },
}

#[derive(Debug, Args)]
struct CheckpointCommand {
    #[command(subcommand)]
    command: CheckpointSubcommand,
}

#[derive(Debug, Subcommand)]
enum CheckpointSubcommand {
    /// List checkpoints across boxes, or filter by box ID/current.
    List {
        /// Optional box filter. Omit to list checkpoints across all boxes.
        id: Option<String>,
    },
    /// Create a checkpoint of a pbox.
    Create {
        /// Public pbox identifier.
        id: String,
        /// Snapshot name.
        name: String,
        /// Optional checkpoint description.
        #[arg(long)]
        description: Option<String>,
    },
    /// Roll back a pbox to a checkpoint.
    Rollback {
        /// Public pbox identifier.
        id: String,
        /// Snapshot name.
        name: String,
        /// Start the container after a successful rollback.
        #[arg(long)]
        start: bool,
        /// Confirm the destructive operation.
        #[arg(long)]
        yes: bool,
    },
    /// Delete a checkpoint from a pbox.
    Delete {
        /// Public pbox identifier.
        id: String,
        /// Snapshot name.
        name: String,
        /// Confirm the destructive operation.
        #[arg(long)]
        yes: bool,
    },
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
struct SetupOutput {
    config_path: String,
    verified: bool,
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
    ipv6: Option<String>,
    name: Option<String>,
    recipes: Vec<PboxRecipeProvenance>,
    capabilities: Vec<String>,
}

#[derive(Debug, Serialize)]
struct BoxInfo {
    id: PboxId,
    vmid: u64,
    state: String,
    node: String,
    ip: Option<String>,
    ipv6: Option<String>,
    name: Option<String>,
    recipes: Vec<PboxRecipeProvenance>,
    capabilities: Vec<String>,
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
#[derive(Debug, Serialize)]
struct ForwardOutput {
    id: String,
    local: String,
    remote: String,
}

#[derive(Debug, Serialize)]
struct SnapshotActionOutput {
    id: PboxId,
    name: String,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    started: Option<bool>,
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
            let message = safe_terminal_text(&format!("{error:#}"));
            ui::stderr().error(&message);
            std::process::exit(1);
        }
    }
}

fn run() -> Result<RunOutcome> {
    let cli = ui::parse_cli();
    progress::set_verbose(cli.verbose);
    ui::configure(cli.color, cli.json);
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
        Command::Image(command) => {
            run_image(command.command, &store, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
        Command::Setup(command) => {
            run_setup(&store, command, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
        Command::New(command) => {
            run_new(&store, command, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
        Command::Repair { id } => {
            run_repair(&store, &id, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
        Command::Ssh(command) => run_ssh(&store, command, cli.json),
        Command::Exec(command) => run_exec(&store, command, cli.json),
        Command::Scp(command) => {
            run_scp(&store, command, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
        Command::Forward(command) => {
            run_forward(&store, command, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
        Command::Recipe(command) => {
            run_recipe(command.command, &store, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
        Command::Snapshot(command) => {
            snapshots::run(command, &store, cli.json, cli.color).map(|_| RunOutcome::Success)
        }
        Command::Checkpoint(command) => {
            run_snapshot(command.command, &store, cli.json, cli.color).map(|_| RunOutcome::Success)
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
        Command::Delete { id, yes, wait } => {
            run_delete(&store, &id, yes, wait, cli.json, cli.color).map(|_| RunOutcome::Success)
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
                ui::json_text(
                    &(serde_json::to_string_pretty(
                        &serde_json::json!({"key": key, "value": value}),
                    )?),
                );
            } else {
                ui::stdout().stdout_metadata(&key, &value);
            }
        }
        ConfigSubcommand::List => {
            let config = store.load_file().context("load pbox configuration")?;
            if json {
                ui::json_text(&(serde_json::to_string_pretty(&config.redacted())?));
            } else {
                ui::stdout().stdout_heading("Configuration");
                ui::stdout().stdout_fields(&config.redacted_pairs());
            }
        }
        ConfigSubcommand::Set { key, value } => {
            let value = read_config_value(&key, value)?;
            let mut config = store.load_file().context("load pbox configuration")?;
            config
                .set_value(&key, &value)
                .context("validate configuration value")?;
            store.save(&config).context("save pbox configuration")?;
            if json {
                ui::json_text(
                    &(serde_json::to_string_pretty(
                        &serde_json::json!({"key": key, "value": config.get_redacted(&key)}),
                    )?),
                );
            } else {
                ui::stdout().success(&format!(
                    "{key}: {}",
                    config
                        .get_redacted(&key)
                        .unwrap_or_else(|| "<unset>".to_owned())
                ));
            }
        }
        ConfigSubcommand::Unset { key } => {
            let mut config = store.load_file().context("load pbox configuration")?;
            config
                .unset_value(&key)
                .context("reset configuration value")?;
            store.save(&config).context("save pbox configuration")?;
            if json {
                ui::json_text(
                    &(serde_json::to_string_pretty(
                        &serde_json::json!({"key": key, "value": config.get_redacted(&key)}),
                    )?),
                );
            } else {
                ui::stdout().success(&format!(
                    "{key}: {}",
                    config
                        .get_redacted(&key)
                        .unwrap_or_else(|| "<unset>".to_owned())
                ));
            }
        }
    }
    Ok(())
}

fn run_image(
    command: ImageSubcommand,
    store: &ConfigStore,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    if let ImageSubcommand::Search(command) = command {
        let mut query = command.query.unwrap_or_default();
        let mut registry = command.registry;
        if images::is_registry_search(&query) {
            registry = query.trim_end_matches('/').to_owned();
            query.clear();
        }
        if query.trim().is_empty() {
            if json || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
                bail!(
                    "enter an image name: `pbox image search debian` (or `pbox image search --registry {registry} debian`)"
                );
            }
            ui::stderr().section("Search images");
            ui::stderr().metadata("registry", &registry);
            query = prompt_setup_text(ui::stderr(), "Search", None)?;
        }
        let result = images::search_images(&query, &registry, command.limit)?;
        if json {
            print_value(&result, true, color)?;
        } else {
            ui::image_search_results(&result);
        }
        return Ok(());
    }
    let config = load_config(store)?;
    let client = client_from_config(&config)?;
    match command {
        ImageSubcommand::Search(_) => unreachable!("image search handled above"),
        ImageSubcommand::Tags(command) => {
            if command.limit == 0 {
                bail!("--limit must be greater than zero");
            }
            let node = resolve_pve_node(&client, &config.pve.node, command.node.as_deref())?;
            let result = search_oci_repository(&client, &node, &command.repository, command.limit)?;
            if json {
                print_value(&result, true, color)?;
            } else {
                ui::stdout().stdout_heading(&result.repository);
                if result.tags.is_empty() {
                    ui::stdout().stdout_hint("No tags found.");
                } else {
                    ui::stdout().stdout_heading("Tags");
                    for tag in result.tags {
                        ui::stdout().stdout_metadata("tag", &tag);
                    }
                }
            }
        }
        ImageSubcommand::Pull(command) => {
            let configured_storage = command
                .storage
                .as_deref()
                .unwrap_or(config.pve.template_storage.as_str());
            let node = resolve_pve_node_with_storage(
                &client,
                &config.pve.node,
                command.node.as_deref(),
                configured_storage,
                "vztmpl",
                "OCI templates",
            )?;
            let storages = client
                .list_node_storages(&node)
                .with_context(|| format!("discover PVE storage on node {node}"))?;
            let storage =
                select_pve_storage(&storages, configured_storage, "vztmpl", "OCI templates")?
                    .to_owned();
            let image = oci_reference_for_image(&command.reference);
            let mut prepared = prepare_oci_template(&client, &node, &storage, &image)?;
            let downloaded = prepared.task.is_some();
            if let Some(task) = prepared.task.take() {
                wait_for_oci_template_task(
                    &client,
                    &node,
                    &storage,
                    &prepared.reference,
                    &prepared.volume,
                    task,
                )?;
            }
            let output = ImagePullOutput {
                reference: prepared.reference,
                node,
                storage,
                volume: prepared.volume,
                downloaded,
            };
            if json {
                print_value(&output, true, color)?;
            } else if output.downloaded {
                ui::stdout().success(&format!("pulled {}", output.reference));
                ui::stdout().stdout_metadata("volume", &output.volume);
            } else {
                ui::stdout().success(&format!("already present {}", output.reference));
                ui::stdout().stdout_metadata("volume", &output.volume);
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct SetupAnswers {
    pve_url: String,
    token_id: String,
    token_secret: Option<String>,
    pve_node: String,
    pve_storage: String,
    pve_template_storage: String,
    pve_bridge: String,
    tls_insecure: bool,
    vmid_pattern: String,
    agent_port: String,
    recipes_repository: String,
    recipes_reference: String,
}

#[derive(Debug, Clone)]
struct SetupChoice {
    value: String,
    description: String,
}

#[derive(Debug)]
struct SetupDiscovery {
    nodes: Vec<SetupChoice>,
}

#[derive(Debug)]
struct SetupResources {
    rootfs_storages: Vec<SetupChoice>,
    template_storages: Vec<SetupChoice>,
    bridges: Vec<SetupChoice>,
}

fn setup_connection_config(
    config: &Config,
    pve_url: &str,
    token_id: &str,
    token_secret: Option<&str>,
    tls_insecure: bool,
) -> Result<Config> {
    let mut updated = config.clone();
    updated
        .set_value("pve.url", pve_url)
        .context("validate PVE API URL")?;
    updated
        .set_value("pve.token_id", token_id)
        .context("validate PVE API token ID")?;
    if let Some(secret) = token_secret {
        updated
            .set_value("pve.token_secret", secret)
            .context("validate PVE API token secret")?;
    }
    if updated.pve.token_secret.is_none() {
        bail!("PVE API token secret is required");
    }
    updated
        .set_value("pve.tls_insecure", &tls_insecure.to_string())
        .context("validate TLS setting")?;
    updated
        .validate()
        .context("validate PVE connection configuration")?;
    Ok(updated)
}

fn discover_setup_nodes(client: &impl PveApi) -> Result<SetupDiscovery> {
    let mut nodes = client.list_nodes().context("discover PVE nodes")?;
    nodes.retain(|node| node.status.as_deref() == Some("online"));
    nodes.sort_by(|left, right| left.node.cmp(&right.node));
    if nodes.is_empty() {
        bail!("PVE returned no online nodes");
    }
    Ok(SetupDiscovery {
        nodes: nodes
            .into_iter()
            .map(|node| SetupChoice {
                value: node.node,
                description: "online".to_owned(),
            })
            .collect(),
    })
}

fn discover_setup_resources(client: &impl PveApi, nodes: &[String]) -> Result<SetupResources> {
    if nodes.is_empty() {
        bail!("PVE returned no nodes for resource discovery");
    }
    let mut rootfs_storages = None;
    let mut template_storages = None;
    let mut bridges = None;
    for node in nodes {
        let storages = client
            .list_node_storages(node)
            .with_context(|| format!("discover PVE storage on node {node}"))?;
        let node_rootfs_storages = storages
            .iter()
            .filter(|storage| storage_supports(storage, "rootdir"))
            .map(|storage| SetupChoice {
                value: storage.storage.clone(),
                description: setup_storage_description(storage),
            })
            .collect();
        intersect_setup_choices(&mut rootfs_storages, node_rootfs_storages);
        let node_template_storages = storages
            .iter()
            .filter(|storage| storage_supports(storage, "vztmpl"))
            .map(|storage| SetupChoice {
                value: storage.storage.clone(),
                description: setup_storage_description(storage),
            })
            .collect();
        intersect_setup_choices(&mut template_storages, node_template_storages);

        let networks = client
            .list_node_network_interfaces(node)
            .with_context(|| format!("discover PVE networks on node {node}"))?;
        let node_bridges = networks
            .into_iter()
            .filter(|network| {
                setup_network_is_active(network)
                    && matches!(
                        network.interface_type.as_deref(),
                        Some("bridge" | "OVSBridge")
                    )
            })
            .map(|network| {
                let description = setup_network_description(&network);
                SetupChoice {
                    value: network.iface,
                    description,
                }
            })
            .collect();
        intersect_setup_choices(&mut bridges, node_bridges);
    }

    Ok(SetupResources {
        rootfs_storages: rootfs_storages.unwrap_or_default(),
        template_storages: template_storages.unwrap_or_default(),
        bridges: bridges.unwrap_or_default(),
    })
}

fn intersect_setup_choices(current: &mut Option<Vec<SetupChoice>>, discovered: Vec<SetupChoice>) {
    if let Some(current) = current.as_mut() {
        current.retain(|choice| discovered.iter().any(|item| item.value == choice.value));
    } else {
        *current = Some(discovered);
    }
}

fn setup_network_is_active(network: &PveNetworkInterface) -> bool {
    match network.extra.get("active") {
        None => true,
        Some(serde_json::Value::Bool(active)) => *active,
        Some(serde_json::Value::Number(active)) => active.as_u64() != Some(0),
        Some(_) => true,
    }
}

fn setup_storage_description(storage: &PveStorage) -> String {
    let storage_type = storage
        .extra
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    format!(
        "type={storage_type}, content={}",
        storage.content.as_deref().unwrap_or("-")
    )
}

fn setup_network_description(network: &PveNetworkInterface) -> String {
    let interface_type = network.interface_type.as_deref().unwrap_or("unknown");
    match network.cidr.as_deref() {
        Some(cidr) => format!("type={interface_type}, cidr={cidr}"),
        None => format!("type={interface_type}"),
    }
}

fn setup_choice_default(configured: &str, choices: &[SetupChoice], allow_auto: bool) -> String {
    if allow_auto && configured == "auto" {
        return "auto".to_owned();
    }
    if configured != "auto" && choices.iter().any(|choice| choice.value == configured) {
        return configured.to_owned();
    }
    if allow_auto {
        return "auto".to_owned();
    }
    choices
        .first()
        .map(|choice| choice.value.clone())
        .unwrap_or_else(|| configured.to_owned())
}

fn prompt_setup_choice(
    style: CliStyle,
    config: &Config,
    key: &str,
    label: &str,
    configured: &str,
    choices: &[SetupChoice],
    allow_auto: bool,
) -> Result<String> {
    if choices.is_empty() {
        return prompt_setup_config_value(style, config, key, label, Some(configured));
    }
    let default = setup_choice_default(configured, choices, allow_auto);
    style.hint("Enter a number or name. Press Enter to accept the default.");
    loop {
        if allow_auto {
            ui::stderr().hint("a. auto (select automatically)");
        }
        for (index, choice) in choices.iter().enumerate() {
            ui::stderr().hint(&format!(
                "{}. {} ({})",
                index + 1,
                style.text(&choice.value),
                style.text(&choice.description)
            ));
        }
        let value = prompt_setup_text(style, label, Some(&default))?;
        if allow_auto && matches!(value.to_ascii_lowercase().as_str(), "a" | "auto") {
            return Ok("auto".to_owned());
        }
        if let Ok(index) = value.parse::<usize>()
            && (1..=choices.len()).contains(&index)
        {
            return Ok(choices[index - 1].value.clone());
        }
        if choices.iter().any(|choice| choice.value == value) {
            return Ok(value);
        }
        let auto_hint = if allow_auto { " or a for auto" } else { "" };
        style.error(&format!(
            "Invalid {label}: enter 1-{} or a choice name{auto_hint}.",
            choices.len()
        ));
    }
}

fn discover_setup_nodes_with_retry(
    style: CliStyle,
    client: &impl PveApi,
) -> Result<Option<SetupDiscovery>> {
    loop {
        style.progress("Discovering online PVE nodes...");
        match discover_setup_nodes(client) {
            Ok(discovery) => return Ok(Some(discovery)),
            Err(error) => {
                style.error(&format!("PVE node discovery failed: {error}"));
                if !prompt_setup_bool(style, "Retry PVE node discovery", true)? {
                    return Ok(None);
                }
            }
        }
    }
}

fn discover_setup_resources_with_retry(
    style: CliStyle,
    client: &impl PveApi,
    nodes: &[String],
    scope: &str,
) -> Result<Option<SetupResources>> {
    loop {
        style.progress(&format!(
            "Discovering storage and bridge options on {scope}..."
        ));
        match discover_setup_resources(client, nodes) {
            Ok(resources) => return Ok(Some(resources)),
            Err(error) => {
                style.error(&format!("PVE resource discovery failed: {error}"));
                if !prompt_setup_bool(style, "Retry PVE resource discovery", true)? {
                    return Ok(None);
                }
            }
        }
    }
}

fn apply_setup_values(config: &mut Config, answers: &SetupAnswers) -> Result<()> {
    let mut updated = setup_connection_config(
        config,
        &answers.pve_url,
        &answers.token_id,
        answers.token_secret.as_deref(),
        answers.tls_insecure,
    )?;
    updated
        .set_value("pve.node", &answers.pve_node)
        .context("validate PVE node")?;
    updated
        .set_value("pve.storage", &answers.pve_storage)
        .context("validate PVE storage")?;
    updated
        .set_value("pve.template-storage", &answers.pve_template_storage)
        .context("validate PVE template storage")?;
    updated
        .set_value("pve.bridge", &answers.pve_bridge)
        .context("validate PVE bridge")?;
    updated
        .set_value("pve.vmid-pattern", &answers.vmid_pattern)
        .context("validate VMID pattern")?;
    updated
        .set_value("agent.port", &answers.agent_port)
        .context("validate agent port")?;
    updated
        .set_value("recipes.repository", &answers.recipes_repository)
        .context("validate recipe repository")?;
    updated
        .set_value("recipes.ref", &answers.recipes_reference)
        .context("validate recipe reference")?;
    updated.validate().context("validate pbox configuration")?;
    *config = updated;
    Ok(())
}

fn agent_identity_changes(config: &Config, answers: &SetupAnswers) -> bool {
    let has_existing_identity = config.pve.token_id.is_some() || config.pve.token_secret.is_some();
    has_existing_identity
        && (config.pve.token_id.as_deref() != Some(answers.token_id.as_str())
            || answers.token_secret.is_some())
}

fn setup_pve_error_message(error: &PveError) -> String {
    match error {
        PveError::Client(_) => "could not build the PVE HTTP client".to_owned(),
        PveError::Request(_) => "could not reach the PVE API".to_owned(),
        PveError::UploadFile(_) => "could not read the PVE upload file".to_owned(),
        PveError::Decode(_) => "PVE returned an invalid response".to_owned(),
        PveError::Http { status, .. } => format!("PVE returned HTTP status {status}"),
        PveError::InvalidBaseUrl => "the PVE URL is invalid".to_owned(),
        PveError::InvalidPathSegment { .. } => "the PVE request path is invalid".to_owned(),
        PveError::InvalidSnapshotName { .. } => "the PVE snapshot name is invalid".to_owned(),
        PveError::Unsupported(message) => format!("PVE does not support this operation: {message}"),
    }
}

fn run_setup(
    store: &ConfigStore,
    command: SetupCommand,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    let mut config = store.load_file().context("load pbox configuration")?;
    let style = CliStyle::for_stderr(color, json);
    style.heading("pbox setup");
    style.hint("Connect pbox to Proxmox and set defaults for new guests.");
    let config_path = store.path().display().to_string();
    style.metadata("config", &config_path);
    style.hint("Press Enter to keep the shown default. Secret input is hidden.");

    style.section("PVE connection");
    let pve_url = prompt_setup_config_value(
        style,
        &config,
        "pve.url",
        "PVE API URL",
        config.pve.url.as_deref(),
    )?;
    let token_id = prompt_setup_config_value(
        style,
        &config,
        "pve.token_id",
        "PVE API token ID",
        config.pve.token_id.as_deref(),
    )?;
    let token_secret = prompt_setup_secret(style, config.pve.token_secret.is_some())?;

    style.section("Guest defaults");
    style.hint("Keep TLS verification enabled unless your PVE endpoint requires otherwise.");
    let tls_insecure = prompt_setup_bool(
        style,
        "Disable PVE TLS certificate verification (not recommended)",
        config.pve.tls_insecure,
    )?;
    let setup_client = if command.skip_verify {
        None
    } else {
        style.progress("Connecting to PVE...");
        let connection_config = setup_connection_config(
            &config,
            &pve_url,
            &token_id,
            token_secret.as_deref(),
            tls_insecure,
        )?;
        Some(client_from_config(&connection_config)?)
    };
    let setup_nodes = match setup_client.as_ref() {
        Some(client) => discover_setup_nodes_with_retry(style, client)?,
        None => None,
    };
    let vmid_pattern_default = config.vmid_pattern.to_string();
    let vmid_pattern = prompt_setup_config_value(
        style,
        &config,
        "pve.vmid-pattern",
        "PVE VMID pattern",
        Some(&vmid_pattern_default),
    )?;
    let agent_port_default = config.agent.port.to_string();
    style.section("PVE placement");
    let node_choices = setup_nodes
        .as_ref()
        .map(|discovery| discovery.nodes.as_slice())
        .unwrap_or(&[]);
    let pve_node = prompt_setup_choice(
        style,
        &config,
        "pve.node",
        "PVE node",
        &config.pve.node,
        node_choices,
        true,
    )?;
    let resource_nodes: Vec<String> = if pve_node == "auto" {
        node_choices
            .iter()
            .map(|choice| choice.value.clone())
            .collect()
    } else {
        vec![pve_node.clone()]
    };
    let resource_scope = if pve_node == "auto" {
        "all online PVE nodes".to_owned()
    } else {
        format!("PVE node {pve_node}")
    };
    let setup_resources = match setup_client.as_ref() {
        Some(client) if !resource_nodes.is_empty() => {
            discover_setup_resources_with_retry(style, client, &resource_nodes, &resource_scope)?
        }
        _ => None,
    };
    let rootfs_choices = setup_resources
        .as_ref()
        .map(|resources| resources.rootfs_storages.as_slice())
        .unwrap_or(&[]);
    let pve_storage = prompt_setup_choice(
        style,
        &config,
        "pve.storage",
        "Rootfs storage",
        &config.pve.storage,
        rootfs_choices,
        true,
    )?;
    let template_choices = setup_resources
        .as_ref()
        .map(|resources| resources.template_storages.as_slice())
        .unwrap_or(&[]);
    let pve_template_storage = prompt_setup_choice(
        style,
        &config,
        "pve.template-storage",
        "Template storage",
        &config.pve.template_storage,
        template_choices,
        true,
    )?;
    let bridge_choices = setup_resources
        .as_ref()
        .map(|resources| resources.bridges.as_slice())
        .unwrap_or(&[]);
    let pve_bridge = prompt_setup_choice(
        style,
        &config,
        "pve.bridge",
        "PVE bridge",
        &config.pve.bridge,
        bridge_choices,
        false,
    )?;
    let agent_port = prompt_setup_config_value(
        style,
        &config,
        "agent.port",
        "Guest agent port",
        Some(&agent_port_default),
    )?;

    style.section("Recipes");
    let recipes_repository = prompt_setup_config_value(
        style,
        &config,
        "recipes.repository",
        "Recipe repository",
        Some(config.recipes.repository.as_str()),
    )?;
    let recipes_reference = prompt_setup_config_value(
        style,
        &config,
        "recipes.ref",
        "Recipe repository ref",
        Some(config.recipes.reference.as_str()),
    )?;
    let answers = SetupAnswers {
        pve_url,
        token_id,
        token_secret,
        pve_node,
        pve_storage,
        pve_template_storage,
        pve_bridge,
        tls_insecure,
        vmid_pattern,
        agent_port,
        recipes_repository,
        recipes_reference,
    };

    style.section("Apply");
    if agent_identity_changes(&config, &answers) {
        style.warning("Changing the token ID or secret changes the agent trust root.");
        style.hint("Existing boxes can disconnect until their trust is repaired.");
        if !prompt_setup_bool(style, "Continue with token change", false)? {
            bail!("setup cancelled");
        }
    }
    apply_setup_values(&mut config, &answers)?;

    let verified = if command.skip_verify {
        false
    } else {
        style.progress("Verifying PVE connection...");
        let client = client_from_config(&config)?;
        client.list_cluster_resources().map_err(|error| {
            anyhow!(
                "PVE connection verification failed: {}; configuration was not saved",
                setup_pve_error_message(&error)
            )
        })?;
        true
    };
    store.save(&config).context("save pbox configuration")?;
    let output = SetupOutput {
        config_path: store.path().display().to_string(),
        verified,
    };
    if json {
        ui::json_text(&(serde_json::to_string_pretty(&output)?));
    } else {
        let output_style = CliStyle::for_stdout(color, false);
        ui::blank_stdout();
        output_style.stdout_status("ok", ANSI_GREEN, "Configuration saved");
        output_style.stdout_metadata("config", &output.config_path);
        if verified {
            output_style.stdout_status("ok", ANSI_GREEN, "PVE connection verified");
        } else {
            output_style.stdout_status(
                "!",
                ANSI_YELLOW,
                "PVE connection not checked (--skip-verify)",
            );
        }
        output_style.stdout_metadata("next", "pbox list");
    }
    Ok(())
}

fn prompt_setup_config_value(
    style: CliStyle,
    config: &Config,
    key: &str,
    label: &str,
    default: Option<&str>,
) -> Result<String> {
    loop {
        let value = prompt_setup_text(style, label, default)?;
        let mut candidate = config.clone();
        match candidate.set_value(key, &value) {
            Ok(()) => return Ok(value),
            Err(error) => {
                style.error(&format!(
                    "Invalid {label}: {}",
                    safe_terminal_text(&error.to_string())
                ));
                style.hint("Correct the value and try again.");
            }
        }
    }
}

fn prompt_setup_secret(style: CliStyle, existing: bool) -> Result<Option<String>> {
    if existing {
        style.hint("Leave blank to keep the current secret.");
    }
    loop {
        let value = read_secret_with_prompt(style, "PVE API token secret")?;
        if value.is_empty() {
            if existing {
                return Ok(None);
            }
            style.error("API token secret is required.");
            continue;
        }
        return Ok(Some(value));
    }
}

fn prompt_setup_bool(style: CliStyle, label: &str, default: bool) -> Result<bool> {
    let default_text = if default { "yes" } else { "no" };
    loop {
        let value = prompt_setup_text(style, label, Some(default_text))?;
        match parse_setup_bool(&value) {
            Ok(value) => return Ok(value),
            Err(error) => {
                style.error(&format!("Invalid {label}: {error}"));
                style.hint("Enter yes or no.");
            }
        }
    }
}

fn parse_setup_bool(value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" => Ok(true),
        "0" | "false" | "no" | "n" => Ok(false),
        _ => bail!("expected yes or no"),
    }
}

fn prompt_setup_text(style: CliStyle, label: &str, default: Option<&str>) -> Result<String> {
    style.prompt(label, default)?;
    let mut value = String::new();
    let read = io::stdin().read_line(&mut value).context("read input")?;
    if read == 0 {
        bail!("input ended before a value was entered");
    }
    let value = value.trim_end_matches(['\r', '\n']);
    if value.is_empty() {
        Ok(default.unwrap_or_default().to_owned())
    } else {
        Ok(value.to_owned())
    }
}

#[derive(Debug)]
struct RemotePath {
    box_id: String,
    path: String,
}

fn run_scp(store: &ConfigStore, command: ScpCommand, json: bool, color: ColorChoice) -> Result<()> {
    let source_remote = parse_remote_path(&command.source)?;
    let destination_remote = parse_remote_path(&command.destination)?;
    let (upload, box_id, local_path, remote_path) = match (source_remote, destination_remote) {
        (None, Some(destination)) => (true, destination.box_id, command.source, destination.path),
        (Some(source), None) => (false, source.box_id, command.destination, source.path),
        (None, None) => {
            return Err(anyhow!("scp requires one remote path in BOX_ID:/path form"));
        }
        (Some(_), Some(_)) => {
            return Err(anyhow!("scp does not support remote-to-remote copies"));
        }
    };

    let config = load_config(store)?;
    let endpoint_command = ExecCommand {
        id: box_id.clone(),
        endpoint: None,
        cwd: "/".to_owned(),
        user: "root".to_owned(),
        env: Vec::new(),
        argv: vec!["true".to_owned()],
    };
    let (resolved_box_id, endpoint) = resolve_agent_target(&config, &endpoint_command)?;
    let materials = agent_materials(&config, &resolved_box_id)?;
    let local = PathBuf::from(&local_path);
    let upload_data = if upload {
        let metadata = fs::metadata(&local)
            .with_context(|| format!("read local file metadata {}", local.display()))?;
        if !metadata.is_file() {
            bail!("scp source is not a regular file: {}", local.display());
        }
        if metadata.len() > MAX_FILE_TRANSFER_BYTES {
            bail!("scp source exceeds the 64 MiB limit: {}", local.display());
        }
        Some(fs::read(&local).with_context(|| format!("read local file {}", local.display()))?)
    } else {
        None
    };
    let upload_mode = if upload {
        Some(local_file_mode(&local)?)
    } else {
        None
    };
    let ca_pem = materials.ca.certificate_pem.clone();
    let client_identity = materials.client;
    let remote_path_for_rpc = remote_path.clone();
    let client_box_id = resolved_box_id.clone();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create async runtime for pbox-agent file transfer")?;
    let size = runtime.block_on(async move {
        let mut client = relay::connect_agent(
            &config,
            &endpoint,
            &client_box_id,
            &ca_pem,
            &client_identity,
        )
        .await
        .context("connect to pbox-agent")?;
        client
            .info()
            .await
            .context("validate pbox-agent identity")?;
        if let Some(data) = upload_data {
            let result = client
                .put_file(
                    remote_path_for_rpc.clone(),
                    data,
                    upload_mode.unwrap_or(0o600),
                    true,
                )
                .await
                .context("upload file through pbox-agent")?;
            Ok::<u64, anyhow::Error>(result.size)
        } else {
            let data = client
                .get_file(remote_path_for_rpc)
                .await
                .context("download file through pbox-agent")?;
            let size = data.len() as u64;
            write_download(&local, data)?;
            Ok(size)
        }
    })?;
    if json {
        ui::json_text(
            &(serde_json::to_string_pretty(&serde_json::json!({
                "box_id": resolved_box_id,
                "local": local_path,
                "remote": remote_path,
                "direction": if upload { "upload" } else { "download" },
                "bytes": size,
            }))?),
        );
    } else {
        let _ = color_enabled(color, json);
        ui::stdout().success(&format!(
            "{} {} {} {} ({} bytes)",
            if upload { "uploaded" } else { "downloaded" },
            local_path,
            if upload { "to" } else { "from" },
            remote_path,
            size
        ));
    }
    Ok(())
}

fn local_file_mode(path: &Path) -> Result<u32> {
    #[cfg(unix)]
    {
        fs::metadata(path)
            .with_context(|| format!("read local file permissions {}", path.display()))
            .map(|metadata| metadata.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(0o600)
    }
}

fn write_download(path: &Path, data: Vec<u8>) -> Result<()> {
    let existing_mode = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                bail!("refusing to overwrite symlink {}", path.display());
            }
            if !metadata.file_type().is_file() {
                bail!(
                    "download destination is not a regular file: {}",
                    path.display()
                );
            }
            #[cfg(unix)]
            {
                Some(metadata.permissions().mode() & 0o7777)
            }
            #[cfg(not(unix))]
            {
                Some(0)
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error).with_context(|| format!("inspect local file {}", path.display()));
        }
    };
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("read system clock for temporary download")?
        .as_nanos();
    let temporary = parent.join(format!(".pbox-download-{}-{stamp}", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("create temporary download {}", temporary.display()))?;
        file.write_all(&data)
            .with_context(|| format!("write temporary download {}", temporary.display()))?;
        #[cfg(unix)]
        fs::set_permissions(
            &temporary,
            fs::Permissions::from_mode(existing_mode.unwrap_or(0o600)),
        )
        .with_context(|| format!("set downloaded file permissions {}", path.display()))?;
        fs::rename(&temporary, path)
            .with_context(|| format!("replace local file {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn parse_remote_path(value: &str) -> Result<Option<RemotePath>> {
    let Some((box_id, path)) = value.split_once(':') else {
        return Ok(None);
    };
    let parsed: PboxId = box_id.parse().context("parse remote pbox identifier")?;
    if path.is_empty() {
        return Err(anyhow!("remote path cannot be empty"));
    }
    if path.contains('\0') {
        return Err(anyhow!("remote path cannot contain NUL bytes"));
    }
    Ok(Some(RemotePath {
        box_id: parsed.to_string(),
        path: path.to_owned(),
    }))
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
                ui::json_text(&(serde_json::to_string_pretty(&catalog)?));
            } else {
                ui::stdout().success(&format!(
                    "synced {} recipes from {} @ {}",
                    catalog.recipes.len(),
                    safe_terminal_text(&catalog.repository),
                    safe_terminal_text(&catalog.revision)
                ));
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
                ui::json_text(&(serde_json::to_string_pretty(result)?));
            } else {
                print_recipe_info(result, color_enabled(color, json));
            }
        }
        RecipeSubcommand::Apply { recipe, box_id } => {
            let config = load_config(store)?;
            let repository = recipe_repository(&config)?;
            let (_recipe_lock, catalog) =
                repository.prepare_for_apply(if config.recipes.auto_sync {
                    Some(parse_recipe_sync_ttl(&config.recipes.sync_ttl)?)
                } else {
                    None
                })?;
            let selected = catalog
                .recipes
                .iter()
                .find(|candidate| candidate.id == recipe)
                .ok_or_else(|| anyhow!("recipe not found: {recipe}"))?;
            let client = client_from_config(&config)?;
            let record = find_box(&client, &box_id)?;
            if record.state != "running" {
                bail!("box {} is not running", record.id);
            }
            if config.recipes.rollback_on_failure && config.recipes.snapshot_before_apply == "never"
            {
                ui::stderr()
                    .warning("rollback-on-failure is enabled but snapshot-before-apply is never");
            }
            let binary = std::env::current_exe().context("locate pbox executable")?;
            let planned_run = AnsibleRun {
                recipe: selected.id.clone(),
                box_id: box_id.clone(),
                repository: catalog.repository.clone(),
                revision: catalog.revision.clone(),
            };
            let snapshot = match create_recipe_snapshot(&client, &record, &config, &planned_run) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return finish_recipe_failure(
                        &client,
                        &record,
                        &planned_run,
                        &selected.metadata.capabilities,
                        error,
                        None,
                        false,
                    );
                }
            };
            let applied = match apply_recipe(
                store.path(),
                &binary,
                repository.cache_dir(),
                &catalog,
                selected,
                &box_id,
                json,
            ) {
                Ok(applied) => applied,
                Err(error) => {
                    return finish_recipe_failure(
                        &client,
                        &record,
                        &planned_run,
                        &selected.metadata.capabilities,
                        error,
                        snapshot.as_ref(),
                        config.recipes.rollback_on_failure,
                    );
                }
            };
            let ansible::RecipeApplyResult { run, cleanup_error } = applied;
            finish_recipe_success(
                &client,
                &record,
                &run,
                &selected.metadata.capabilities,
                snapshot.as_ref(),
                cleanup_error,
            )?;
            if json {
                ui::json_text(&(serde_json::to_string_pretty(&run)?));
            } else {
                ui::stdout().success(&format!(
                    "applied {} to {} @ {}",
                    safe_terminal_text(&run.recipe),
                    safe_terminal_text(&run.box_id),
                    safe_terminal_text(&run.revision)
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct RecipeSnapshot {
    name: String,
}

fn create_recipe_snapshot(
    client: &impl PveApi,
    record: &BoxRecord,
    config: &Config,
    run: &AnsibleRun,
) -> Result<Option<RecipeSnapshot>> {
    match config.recipes.snapshot_before_apply.as_str() {
        "never" => return Ok(None),
        "always" | "auto" => {}
        policy => bail!("invalid recipe snapshot policy: {policy}"),
    }
    let name = format!(
        "pbox-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("read system clock")?
            .as_nanos()
    );
    let request = LxcSnapshotRequest {
        snapname: name.clone(),
        description: Some(format!(
            "pbox before recipe {} @ {}",
            safe_terminal_text(&run.recipe),
            safe_terminal_text(&run.revision)
        )),
    };
    let task = match client.create_lxc_snapshot(&record.node, record.vmid, &request) {
        Ok(task) => task,
        Err(error)
            if config.recipes.snapshot_before_apply == "auto" && snapshot_unavailable(&error) =>
        {
            ui::stderr().warning(&format!(
                "snapshots are unavailable for {}; continuing without one",
                safe_terminal_text(record.id.as_str())
            ));
            return Ok(None);
        }
        Err(error) => {
            return Err(cleanup_failed_recipe_snapshot(
                client,
                record,
                &name,
                anyhow!(error).context("create recipe snapshot"),
            ));
        }
    };
    if let Err(error) = wait_for_task(client, &record.node, task) {
        return Err(cleanup_failed_recipe_snapshot(
            client,
            record,
            &name,
            anyhow!(error).context(format!("wait for recipe snapshot {name}")),
        ));
    }
    Ok(Some(RecipeSnapshot { name }))
}

fn cleanup_failed_recipe_snapshot(
    client: &impl PveApi,
    record: &BoxRecord,
    name: &str,
    primary_error: anyhow::Error,
) -> anyhow::Error {
    match delete_recipe_snapshot(
        client,
        record,
        &RecipeSnapshot {
            name: name.to_owned(),
        },
    ) {
        Ok(()) => primary_error.context(format!(
            "removed incomplete recipe snapshot {name} after snapshot operation failure"
        )),
        Err(cleanup_error) => attach_error(
            primary_error,
            &format!("recipe snapshot {name} may remain; cleanup failed"),
            cleanup_error,
        ),
    }
}

fn snapshot_unavailable(error: &PveError) -> bool {
    matches!(
        error,
        PveError::Http { status, .. } if matches!(status.as_u16(), 400 | 405 | 501)
    )
}

fn rollback_recipe_snapshot(
    client: &impl PveApi,
    record: &BoxRecord,
    snapshot: &RecipeSnapshot,
) -> Result<()> {
    let task = client
        .rollback_lxc_snapshot(&record.node, record.vmid, &snapshot.name, true)
        .with_context(|| format!("rollback recipe snapshot {}", snapshot.name))?;
    wait_for_task(client, &record.node, task)
        .with_context(|| format!("wait for recipe snapshot rollback {}", snapshot.name))
}

fn delete_recipe_snapshot(
    client: &impl PveApi,
    record: &BoxRecord,
    snapshot: &RecipeSnapshot,
) -> Result<()> {
    let task = client
        .delete_lxc_snapshot(&record.node, record.vmid, &snapshot.name)
        .with_context(|| format!("delete recipe snapshot {}", snapshot.name))?;
    wait_for_task(client, &record.node, task)
        .with_context(|| format!("wait for recipe snapshot deletion {}", snapshot.name))
}

fn finish_recipe_failure(
    client: &impl PveApi,
    record: &BoxRecord,
    planned_run: &AnsibleRun,
    capabilities: &[String],
    primary_error: anyhow::Error,
    snapshot: Option<&RecipeSnapshot>,
    rollback_on_failure: bool,
) -> Result<()> {
    let mut error = primary_error;
    if rollback_on_failure {
        match snapshot {
            Some(snapshot) => match rollback_recipe_snapshot(client, record, snapshot) {
                Ok(()) => {
                    if let Err(delete_error) = delete_recipe_snapshot(client, record, snapshot) {
                        error =
                            attach_error(error, "delete recovered recipe snapshot", delete_error);
                    }
                }
                Err(rollback_error) => {
                    error = attach_error(error, "rollback recipe snapshot", rollback_error);
                }
            },
            None => {
                error = error.context("rollback requested but no recipe snapshot was available");
            }
        }
    } else if let Some(snapshot) = snapshot {
        error = error.context(format!(
            "recipe snapshot {} was preserved for manual recovery",
            snapshot.name
        ));
    }
    if let Err(provenance_error) =
        record_recipe_provenance(client, record, planned_run, capabilities, "failed")
    {
        error = attach_error(error, "record failed recipe provenance", provenance_error);
    }
    Err(error)
}

fn finish_recipe_success(
    client: &impl PveApi,
    record: &BoxRecord,
    run: &AnsibleRun,
    capabilities: &[String],
    snapshot: Option<&RecipeSnapshot>,
    cleanup_error: Option<anyhow::Error>,
) -> Result<()> {
    let mut cleanup_error = cleanup_error;
    if let Err(error) = record_recipe_provenance(client, record, run, capabilities, "success") {
        let error = match cleanup_error {
            Some(cleanup_error) => attach_error(error, "recipe cleanup also failed", cleanup_error),
            None => error,
        };
        return Err(match snapshot {
            Some(snapshot) => error.context(format!(
                "recipe snapshot {} was preserved for manual recovery",
                snapshot.name
            )),
            None => error,
        });
    }
    if let Some(snapshot) = snapshot
        && let Err(error) = delete_recipe_snapshot(client, record, snapshot)
    {
        cleanup_error = merge_cleanup_error(cleanup_error, "remove recipe snapshot", error);
    }
    if let Some(error) = cleanup_error {
        return Err(error.context("recipe applied successfully but cleanup failed"));
    }
    Ok(())
}

fn attach_error(primary: anyhow::Error, label: &str, secondary: anyhow::Error) -> anyhow::Error {
    primary.context(format!("{label}: {secondary:#}"))
}

fn merge_cleanup_error(
    existing: Option<anyhow::Error>,
    label: &str,
    error: anyhow::Error,
) -> Option<anyhow::Error> {
    Some(match existing {
        Some(existing) => attach_error(existing, label, error),
        None => error.context(label.to_owned()),
    })
}

fn record_recipe_provenance(
    client: &impl PveApi,
    record: &BoxRecord,
    run: &AnsibleRun,
    capabilities: &[String],
    result: &str,
) -> Result<()> {
    let provenance = PboxRecipeProvenance {
        id: run.recipe.clone(),
        repository: run.repository.clone(),
        revision: run.revision.clone(),
        applied_at: Some(current_timestamp()?),
        result: Some(result.to_owned()),
    };
    let mut last_error = None;
    for _attempt in 0..3 {
        let config = match client.get_lxc_config(&record.node, record.vmid) {
            Ok(config) => config,
            Err(error) => {
                last_error = Some(format!("read metadata: {error}"));
                continue;
            }
        };
        let description = config.description.as_deref().unwrap_or("");
        let mut metadata = parse_metadata(description)
            .context("parse pbox metadata before recording recipe provenance")?
            .ok_or_else(|| anyhow!("box {} has no pbox metadata", record.id))?;
        if metadata.id != record.id || metadata.vmid != record.vmid {
            bail!("pbox metadata does not match box {}", record.id);
        }
        if let Some(existing) = metadata
            .recipes
            .iter_mut()
            .find(|existing| existing.id == provenance.id)
        {
            *existing = provenance.clone();
        } else {
            metadata.recipes.push(provenance.clone());
        }
        if result == "success" {
            for capability in capabilities {
                if !metadata.capabilities.contains(capability) {
                    metadata.capabilities.push(capability.clone());
                }
            }
        }
        metadata
            .recipes
            .sort_by(|left, right| left.id.cmp(&right.id));
        metadata.capabilities.sort();
        let description = preserve_metadata(description, &metadata)
            .context("preserve pbox metadata while recording recipe provenance")?;
        let request = LxcConfigUpdateRequest {
            digest: config.digest,
            description: Some(description),
            ..Default::default()
        };
        match client.update_lxc_config(&record.node, record.vmid, &request) {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error.to_string()),
        }
    }
    Err(anyhow!(
        "could not record recipe provenance for {} after configuration retries: {}",
        record.id,
        last_error.unwrap_or_else(|| "unknown PVE error".to_owned())
    ))
}

fn recipe_repository(config: &Config) -> Result<RecipeRepository> {
    RecipeRepository::new(
        Some(config.recipes.repository.as_str()),
        &config.recipes.reference,
    )
}

fn current_timestamp() -> Result<String> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .context("format recipe provenance timestamp")
}

fn parse_recipe_sync_ttl(value: &str) -> Result<Duration> {
    parse_duration(value).map_err(|reason| anyhow!("invalid recipe sync TTL {value:?}; {reason}"))
}

fn load_recipe_catalog(store: &ConfigStore) -> Result<RecipeCatalog> {
    let config = load_config(store)?;
    let repository = recipe_repository(&config)?;
    if config.recipes.auto_sync {
        repository.sync_if_stale(parse_recipe_sync_ttl(&config.recipes.sync_ttl)?)
    } else {
        repository.discover()
    }
}

fn run_forward(
    store: &ConfigStore,
    command: ForwardCommand,
    json: bool,
    _color: ColorChoice,
) -> Result<()> {
    validate_forward_arguments(&command)?;
    let config = load_config(store)?;
    let (box_id, endpoint) =
        resolve_agent_endpoint(&config, &command.id, command.endpoint.as_deref())?;
    let materials = agent_materials(&config, &box_id)?;
    let remote_host = command.remote_host;
    let remote_port = command.remote_port.unwrap_or(command.local_port);
    let listen = command.listen;
    let local_port = command.local_port;
    let ca_pem = materials.ca.certificate_pem;
    let client_identity = materials.client;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create async runtime for pbox-agent forwarding")?;
    runtime.block_on(async move {
        let listener = TcpListener::bind((listen.as_str(), local_port))
            .await
            .with_context(|| format!("bind local forwarding address {listen}:{local_port}"))?;
        let local_address = listener
            .local_addr()
            .context("read local forwarding address")?;
        let output = ForwardOutput {
            id: box_id.clone(),
            local: local_address.to_string(),
            remote: format!("{}:{}", remote_host, remote_port),
        };
        if json {
            ui::json_text(&(serde_json::to_string_pretty(&output)?));
        } else {
            ui::stdout().success(&format!("forwarding {} -> {} (press Ctrl-C to stop)",
                output.local, output.remote));
        }
        let connection_slots = Arc::new(Semaphore::new(MAX_FORWARD_CONNECTIONS));
        let shutdown = tokio::signal::ctrl_c();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                result = listener.accept() => {
                    let (socket, peer) = result.context("accept local forwarding connection")?;
                    let connection_permit = match connection_slots.clone().try_acquire_owned() {
                        Ok(permit) => permit,
                        Err(_) => {
                            ui::stderr().warning(&format!("rejecting {peer}: maximum of {MAX_FORWARD_CONNECTIONS} connections reached"));
                            continue;
                        }
                    };
                    let endpoint = endpoint.clone();
                    let box_id = box_id.clone();
                    let ca_pem = ca_pem.clone();
                    let client_identity = client_identity.clone();
                    let remote_host = remote_host.clone();
                    let config = config.clone();
                    tokio::spawn(async move {
                        let _connection_permit = connection_permit;
                        let result = async {
                            let mut client = relay::connect_agent(
                                &config,
                                &endpoint,
                                &box_id,
                                &ca_pem,
                                &client_identity,
                            )
                            .await
                            .context("connect to pbox-agent")?;
                            client
                                .info()
                                .await
                                .context("validate pbox-agent identity")?;
                            client
                                .forward_tcp(socket, remote_host, remote_port)
                                .await
                                .context("forward TCP connection")
                        }
                        .await;
                        if let Err(error) = result {
                            ui::stderr().error(&format!("{peer}: {error:#}"));
                        }
                    });
                }
                result = &mut shutdown => {
                    result.context("wait for Ctrl-C")?;
                    return Ok(());
                }
            }
        }
    })
}

fn validate_forward_arguments(command: &ForwardCommand) -> Result<()> {
    if command.local_port == 0 {
        bail!("local forwarding port cannot be zero");
    }
    if command.remote_port == Some(0) {
        bail!("remote forwarding port cannot be zero");
    }
    for (label, value) in [
        ("box id", command.id.as_str()),
        ("listen address", command.listen.as_str()),
        ("remote host", command.remote_host.as_str()),
    ] {
        if value.is_empty() {
            bail!("{label} cannot be empty");
        }
        if value.contains('\0') {
            bail!("{label} cannot contain NUL bytes");
        }
    }
    if let Some(endpoint) = command.endpoint.as_deref() {
        if endpoint.is_empty() {
            bail!("agent endpoint cannot be empty");
        }
        if endpoint.contains('\0') {
            bail!("agent endpoint cannot contain NUL bytes");
        }
    }
    Ok(())
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
        let mut client =
            relay::connect_agent(&config, &endpoint, &box_id, &ca_pem, &client_identity)
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
        ui::json_text(&(serde_json::to_string_pretty(&output)?));
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
fn run_ssh(store: &ConfigStore, command: SshCommand, json: bool) -> Result<RunOutcome> {
    if json {
        bail!("pbox ssh does not support --json");
    }
    validate_ssh_arguments(&command)?;
    let config = load_config(store)?;
    let (box_id, endpoint) =
        resolve_agent_endpoint(&config, &command.id, command.endpoint.as_deref())?;
    let box_name = client_from_config(&config)
        .and_then(|client| find_box(&client, &box_id))
        .ok()
        .and_then(|record| record.name)
        .unwrap_or_else(|| box_id.clone());
    let materials = agent_materials(&config, &box_id)?;
    let env = ssh_environment(&command.env, std::env::var("COLORTERM").ok())?;
    let argv = ssh_command_argv(&command.argv);
    let ca_pem = materials.ca.certificate_pem.clone();
    let client_identity = materials.client;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create async runtime for pbox-agent shell")?;
    let (mut client, info) = runtime.block_on(async move {
        let mut client =
            relay::connect_agent(&config, &endpoint, &box_id, &ca_pem, &client_identity)
                .await
                .context("connect to pbox-agent")?;
        let info = client
            .info()
            .await
            .context("validate pbox-agent identity")?;
        Ok::<_, anyhow::Error>((client, info))
    })?;
    if !info
        .capabilities
        .iter()
        .any(|capability| capability == "pty")
    {
        bail!("pbox-agent does not advertise PTY support; upgrade the guest agent");
    }
    if command.argv.is_empty() {
        let access = runtime.block_on(guest::check_client(&mut client, &command.user));
        ui::user_access(&info.box_id, &command.user, access);
        ui::stderr().progress(&format!(
            "Connected to {}. Type exit to disconnect.",
            info.box_id
        ));
    }
    let signals = runtime.block_on(install_terminal_signals())?;
    let terminal = TerminalModeGuard::enter()?;
    let title = ui::TerminalTitleGuard::enter(&box_name);
    let result = runtime.block_on(run_ssh_session_with_signals(
        &mut client,
        argv,
        command.cwd,
        env,
        command.user,
        (terminal.as_ref(), title.as_ref()),
        signals,
    ));
    drop(terminal);
    drop(title);
    let result = result?;
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
        .ok_or_else(|| anyhow!("environment entry must use KEY=VALUE form"))?;
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
fn ssh_environment(
    entries: &[String],
    color_term: Option<String>,
) -> Result<Vec<(String, String)>> {
    let mut env = entries
        .iter()
        .map(|entry| parse_env_entry(entry))
        .collect::<Result<Vec<_>>>()?;
    if !env.iter().any(|(key, _)| key == "TERM") {
        env.push(("TERM".to_owned(), "xterm-256color".to_owned()));
    }
    if let Some(color_term) =
        color_term.filter(|value| matches!(value.as_str(), "truecolor" | "24bit"))
        && !env.iter().any(|(key, _)| key == "COLORTERM")
    {
        env.push(("COLORTERM".to_owned(), color_term));
    }
    Ok(env)
}

fn ssh_command_argv(argv: &[String]) -> Vec<String> {
    if argv.is_empty() {
        vec!["/bin/sh".to_owned(), "-c".to_owned(),
            r#"shell=$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f7); exec "${shell:-/bin/sh}" -il"#.to_owned()]
    } else {
        argv.to_owned()
    }
}

fn validate_ssh_arguments(command: &SshCommand) -> Result<()> {
    for (label, value) in [
        ("box id", command.id.as_str()),
        ("working directory", command.cwd.as_str()),
        ("guest user", command.user.as_str()),
    ] {
        if value.contains('\0') {
            bail!("{label} cannot contain NUL bytes");
        }
    }
    if let Some(endpoint) = command.endpoint.as_deref() {
        if endpoint.is_empty() {
            bail!("agent endpoint cannot be empty");
        }
        if endpoint.contains('\0') {
            bail!("agent endpoint cannot contain NUL bytes");
        }
    }
    if command.argv.iter().any(|argument| argument.contains('\0')) {
        bail!("command arguments cannot contain NUL bytes");
    }
    for entry in &command.env {
        parse_env_entry(entry)?;
    }
    Ok(())
}
#[cfg(unix)]
struct TerminalSignals {
    interrupt: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
    quit: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(not(unix))]
struct TerminalSignals;

async fn install_terminal_signals() -> Result<TerminalSignals> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(TerminalSignals {
            interrupt: signal(SignalKind::interrupt()).context("register SIGINT handler")?,
            hangup: signal(SignalKind::hangup()).context("register SIGHUP handler")?,
            quit: signal(SignalKind::quit()).context("register SIGQUIT handler")?,
            terminate: signal(SignalKind::terminate()).context("register SIGTERM handler")?,
        })
    }
    #[cfg(not(unix))]
    {
        Ok(TerminalSignals)
    }
}

async fn run_ssh_session_with_signals(
    client: &mut AgentClient,
    argv: Vec<String>,
    cwd: String,
    env: Vec<(String, String)>,
    user: String,
    terminal: (Option<&TerminalModeGuard>, Option<&ui::TerminalTitleGuard>),
    signals: TerminalSignals,
) -> Result<ExecResult> {
    #[cfg(unix)]
    {
        let TerminalSignals {
            mut interrupt,
            mut hangup,
            mut quit,
            mut terminate,
        } = signals;
        tokio::select! {
            result = run_ssh_session(client, argv, cwd, env, user, terminal.1.map(|title| title.name.as_str())) => result,
            _ = interrupt.recv() => terminate_after_signal(terminal, 128 + 2),
            _ = hangup.recv() => terminate_after_signal(terminal, 128 + 1),
            _ = quit.recv() => terminate_after_signal(terminal, 128 + 3),
            _ = terminate.recv() => terminate_after_signal(terminal, 128 + 15),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (terminal, signals);
        run_ssh_session(
            client,
            argv,
            cwd,
            env,
            user,
            terminal.1.map(|title| title.name.as_str()),
        )
        .await
    }
}

#[cfg(unix)]
fn terminate_after_signal(
    terminal: (Option<&TerminalModeGuard>, Option<&ui::TerminalTitleGuard>),
    exit_code: i32,
) -> Result<ExecResult> {
    if let Some(title) = terminal.1 {
        title.restore();
    }
    if let Some(terminal) = terminal.0 {
        terminal.restore();
    }
    std::process::exit(exit_code);
}

async fn run_ssh_session(
    client: &mut AgentClient,
    argv: Vec<String>,
    cwd: String,
    env: Vec<(String, String)>,
    user: String,
    title: Option<&str>,
) -> Result<ExecResult> {
    let mut session = client
        .exec_pty_session(argv, cwd, env, user)
        .await
        .context("start pbox-agent PTY session")?;
    let input_sender = session.input.clone();
    let _input_thread = thread::spawn(move || pump_terminal_input(input_sender));
    let mut result = ExecResult::default();
    let mut titles = ui::TitlePrefix::new(title);
    while let Some(event) = session
        .output
        .message()
        .await
        .context("read pbox-agent PTY output")?
    {
        match event.event {
            Some(exec_event::Event::Stdout(data)) => {
                write_pty_output(titles.push(&data), false)
                    .await
                    .context("write pbox-agent PTY output")?;
            }
            Some(exec_event::Event::Stderr(data)) => {
                write_pty_output(titles.push(&data), true)
                    .await
                    .context("write pbox-agent PTY error output")?;
            }
            Some(exec_event::Event::Exit(exit)) => {
                result.code = exit.code;
                result.signal = exit.signal;
                result.exited = true;
                break;
            }
            None => bail!("pbox-agent PTY stream sent an empty event"),
        }
    }
    write_pty_output(titles.finish(), false).await?;
    if !result.exited {
        bail!("pbox-agent PTY stream ended without exit status");
    }
    match tokio::time::timeout(SSH_POST_EXIT_TIMEOUT, session.output.message()).await {
        Ok(Ok(None)) => {}
        Ok(Ok(Some(_))) => bail!("pbox-agent PTY stream sent data after exit"),
        Ok(Err(error)) => return Err(error).context("close pbox-agent PTY output"),
        Err(_) => bail!("pbox-agent PTY stream did not close after exit"),
    }
    Ok(result)
}

async fn write_pty_output(data: Vec<u8>, stderr: bool) -> Result<()> {
    let result = tokio::task::spawn_blocking(move || {
        if stderr {
            let mut output = io::stderr();
            output.write_all(&data)?;
            output.flush()
        } else {
            let mut output = io::stdout();
            output.write_all(&data)?;
            output.flush()
        }
    })
    .await
    .context("join PTY output writer")?;
    result.context("write PTY output")
}

fn pump_terminal_input(sender: tokio::sync::mpsc::Sender<ExecInput>) {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    let mut buffer = [0_u8; 8192];
    loop {
        let count = match stdin.read(&mut buffer) {
            Ok(count) => count,
            Err(error) => {
                ui::stderr().error(&format!("read terminal input: {error}"));
                break;
            }
        };
        if count == 0 {
            let _ = sender.blocking_send(ExecInput::Eof);
            break;
        }
        if sender
            .blocking_send(ExecInput::Data(buffer[..count].to_vec()))
            .is_err()
        {
            break;
        }
    }
}

#[cfg(unix)]
struct TerminalModeGuard {
    original: nix::sys::termios::Termios,
}

#[cfg(unix)]
impl TerminalModeGuard {
    fn enter() -> Result<Option<Self>> {
        let stdin = io::stdin();
        if !stdin.is_terminal() {
            return Ok(None);
        }
        let original = tcgetattr(&stdin).context("read terminal settings")?;
        let mut raw = original.clone();
        cfmakeraw(&mut raw);
        tcsetattr(&stdin, SetArg::TCSAFLUSH, &raw).context("enable raw terminal mode")?;
        Ok(Some(Self { original }))
    }

    fn restore(&self) {
        let stdin = io::stdin();
        if let Err(error) = tcsetattr(&stdin, SetArg::TCSAFLUSH, &self.original) {
            ui::stderr().error(&format!("restore terminal settings: {error}"));
        }
    }
}

#[cfg(unix)]
impl Drop for TerminalModeGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

#[cfg(not(unix))]
struct TerminalModeGuard;

#[cfg(not(unix))]
impl TerminalModeGuard {
    fn enter() -> Result<Option<Self>> {
        Ok(None)
    }
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

fn read_config_value(key: &str, value: Option<String>) -> Result<String> {
    if is_secret_config_key(key) {
        if value.is_some() {
            bail!("do not pass secret values as arguments; pipe the value on stdin instead");
        }
        return read_secret_from_stdin();
    }
    match value {
        Some(value) => Ok(value),
        None => read_value_from_stdin(),
    }
}

fn is_secret_config_key(key: &str) -> bool {
    matches!(
        key,
        "pve.token_secret" | "pve.token-secret" | "pve.api-token-secret"
    )
}

fn read_value_from_stdin() -> Result<String> {
    ui::stderr().prompt("Value", None)?;
    io::stderr().flush().context("flush configuration prompt")?;
    let mut value = String::new();
    io::stdin()
        .read_line(&mut value)
        .context("read configuration value")?;
    Ok(value.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(unix)]
const SECRET_PROMPT_SIGNALS: [Signal; 5] = [
    Signal::SIGHUP,
    Signal::SIGINT,
    Signal::SIGQUIT,
    Signal::SIGTERM,
    Signal::SIGTSTP,
];

#[cfg(unix)]
struct SignalDispositionGuard {
    original: Vec<(Signal, nix::libc::sigaction)>,
}

#[cfg(unix)]
impl SignalDispositionGuard {
    fn capture() -> Result<Self> {
        let mut original = Vec::with_capacity(SECRET_PROMPT_SIGNALS.len());
        for signal in SECRET_PROMPT_SIGNALS {
            let mut action = std::mem::MaybeUninit::<nix::libc::sigaction>::uninit();
            let result = unsafe {
                nix::libc::sigaction(
                    signal as nix::libc::c_int,
                    std::ptr::null(),
                    action.as_mut_ptr(),
                )
            };
            if result != 0 {
                return Err(nix::errno::Errno::last()).context("read signal disposition");
            }
            original.push((signal, unsafe { action.assume_init() }));
        }
        Ok(Self { original })
    }

    fn restore(&mut self) -> Result<()> {
        let mut first_error = None;
        for (signal, action) in &self.original {
            let result = unsafe {
                nix::libc::sigaction(
                    *signal as nix::libc::c_int,
                    action as *const nix::libc::sigaction,
                    std::ptr::null_mut(),
                )
            };
            if result != 0 && first_error.is_none() {
                first_error = Some(nix::errno::Errno::last());
            }
        }
        if let Some(error) = first_error {
            Err(error).context("restore signal dispositions")
        } else {
            self.original.clear();
            Ok(())
        }
    }
}

#[cfg(unix)]
impl Drop for SignalDispositionGuard {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(unix)]
struct SecretPromptSignals {
    interrupt: tokio::signal::unix::Signal,
    hangup: tokio::signal::unix::Signal,
    quit: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
    stop: tokio::signal::unix::Signal,
}

#[cfg(unix)]
async fn install_secret_prompt_signals() -> Result<SecretPromptSignals> {
    use tokio::signal::unix::{SignalKind, signal};

    Ok(SecretPromptSignals {
        interrupt: signal(SignalKind::interrupt()).context("register SIGINT handler")?,
        hangup: signal(SignalKind::hangup()).context("register SIGHUP handler")?,
        quit: signal(SignalKind::quit()).context("register SIGQUIT handler")?,
        terminate: signal(SignalKind::terminate()).context("register SIGTERM handler")?,
        stop: signal(SignalKind::from_raw(nix::libc::SIGTSTP))
            .context("register SIGTSTP handler")?,
    })
}

#[cfg(unix)]
struct TerminalEchoGuard {
    original: Option<nix::sys::termios::Termios>,
}

#[cfg(unix)]
impl TerminalEchoGuard {
    fn new(stdin: &io::Stdin) -> Result<Option<Self>> {
        let mut disabled = match tcgetattr(stdin) {
            Ok(terminal) => terminal,
            Err(nix::errno::Errno::ENOTTY) => return Ok(None),
            Err(error) => return Err(error).context("read terminal settings"),
        };
        let original = disabled.clone();
        disabled.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(stdin, SetArg::TCSANOW, &disabled).context("disable terminal echo")?;
        Ok(Some(Self {
            original: Some(original),
        }))
    }

    fn restore(&mut self, stdin: &io::Stdin) -> Result<()> {
        let Some(original) = self.original.as_ref() else {
            return Ok(());
        };
        tcsetattr(stdin, SetArg::TCSANOW, original).context("restore terminal echo")?;
        self.original = None;
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for TerminalEchoGuard {
    fn drop(&mut self) {
        if let Some(original) = self.original.as_ref() {
            let stdin = io::stdin();
            let _ = tcsetattr(&stdin, SetArg::TCSANOW, original);
        }
    }
}

fn read_secret_from_stdin() -> Result<String> {
    read_secret_with_prompt(ui::stderr(), "Secret")
}

fn read_secret_with_prompt(style: CliStyle, prompt: &str) -> Result<String> {
    #[cfg(unix)]
    {
        let mut signal_handlers = SignalDispositionGuard::capture()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("create async runtime for secret prompt")?;
        let result = runtime.block_on(read_secret_with_prompt_async(style, prompt));
        drop(runtime);
        let restore_result = signal_handlers.restore();
        match (result, restore_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(signal_error)) => {
                Err(anyhow!("{error}; additionally, {signal_error}"))
            }
        }
    }
    #[cfg(not(unix))]
    {
        style.prompt(prompt, None)?;
        let mut value = String::new();
        let read = io::stdin()
            .read_line(&mut value)
            .context("read configuration secret")?;
        if read == 0 {
            bail!("input ended while reading secret");
        }
        Ok(value.trim_end_matches(['\r', '\n']).to_owned())
    }
}

#[cfg(unix)]
async fn read_secret_with_prompt_async(style: CliStyle, prompt: &str) -> Result<String> {
    let signals = install_secret_prompt_signals().await?;
    let stdin = io::stdin();
    let read_stdin = io::stdin();
    let mut terminal = TerminalEchoGuard::new(&stdin)?;
    let had_terminal = terminal.is_some();
    style.prompt(prompt, None)?;
    let read_task = tokio::task::spawn_blocking(move || {
        let mut value = String::new();
        let read = read_stdin
            .lock()
            .read_line(&mut value)
            .context("read configuration secret")?;
        Ok::<_, anyhow::Error>((read, value))
    });
    let SecretPromptSignals {
        mut interrupt,
        mut hangup,
        mut quit,
        mut terminate,
        mut stop,
    } = signals;
    let read_result = tokio::select! {
        result = read_task => result.context("join secret input task"),
        _ = interrupt.recv() => terminate_secret_prompt_on_signal(&mut terminal, had_terminal, 128 + 2),
        _ = hangup.recv() => terminate_secret_prompt_on_signal(&mut terminal, had_terminal, 128 + 1),
        _ = quit.recv() => terminate_secret_prompt_on_signal(&mut terminal, had_terminal, 128 + 3),
        _ = terminate.recv() => terminate_secret_prompt_on_signal(&mut terminal, had_terminal, 128 + 15),
        _ = stop.recv() => terminate_secret_prompt_on_signal(&mut terminal, had_terminal, 128 + 20),
    };
    let restore_result = match terminal.as_mut() {
        Some(terminal) => terminal.restore(&stdin),
        None => Ok(()),
    };
    if had_terminal {
        ui::blank_stderr();
    }
    restore_result?;
    let (read, value) = read_result??;
    if read == 0 {
        bail!("input ended while reading secret");
    }
    Ok(value.trim_end_matches(['\r', '\n']).to_owned())
}

#[cfg(unix)]
fn terminate_secret_prompt_on_signal(
    terminal: &mut Option<TerminalEchoGuard>,
    had_terminal: bool,
    exit_code: i32,
) -> ! {
    let restore_result = match terminal.as_mut() {
        Some(terminal) => terminal.restore(&io::stdin()),
        None => Ok(()),
    };
    if had_terminal {
        ui::blank_stderr();
    }
    if let Err(error) = restore_result {
        ui::stderr().warning(&format!("could not restore terminal echo: {error}"));
    }
    std::process::exit(exit_code);
}
#[derive(Debug, Clone)]
struct ResolvedNew {
    node: String,
    ostemplate: String,
    rootfs: String,
    net0: String,
    name: Option<String>,
    memory: u64,
    swap: u64,
    cores: u64,
    unprivileged: bool,
    onboot: bool,
    stopped: bool,
}

fn non_empty_new_value(label: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{label} cannot be empty");
    }
    Ok(value.to_owned())
}

fn positive_new_value(label: &str, value: u64) -> Result<u64> {
    if value == 0 {
        bail!("{label} must be greater than zero");
    }
    Ok(value)
}

fn new_volume_override(label: &str, value: &str) -> Result<String> {
    let value = non_empty_new_value(label, value)?;
    let (storage, volume) = value
        .split_once(':')
        .ok_or_else(|| anyhow!("{label} must use STORAGE:VALUE syntax"))?;
    if storage.is_empty() || volume.is_empty() {
        bail!("{label} must use STORAGE:VALUE syntax");
    }
    Ok(value)
}

fn normalise_rootfs_size(value: &str) -> String {
    let value = value.trim();
    let Some(size) = value.strip_suffix('G') else {
        return value.to_owned();
    };
    if !size.is_empty()
        && size
            .chars()
            .all(|character| character.is_ascii_digit() || character == '.')
        && size.parse::<f64>().is_ok()
    {
        size.to_owned()
    } else {
        value.to_owned()
    }
}

fn new_rootfs_override(value: &str) -> Result<String> {
    let value = new_volume_override("--rootfs", value)?;
    let (storage, size) = value
        .split_once(':')
        .ok_or_else(|| anyhow!("--rootfs must use STORAGE:VALUE syntax"))?;
    Ok(format!("{storage}:{}", normalise_rootfs_size(size)))
}

fn rootfs_size_for_storage(storages: &[PveStorage], storage: &str, configured: &str) -> String {
    let is_directory = storages
        .iter()
        .find(|candidate| candidate.storage == storage)
        .and_then(|candidate| candidate.extra.get("type"))
        .and_then(serde_json::Value::as_str)
        == Some("dir");
    if is_directory {
        "0".to_owned()
    } else {
        normalise_rootfs_size(configured)
    }
}

fn storage_supports(storage: &PveStorage, content: &str) -> bool {
    storage.active == Some(1)
        && storage.enabled == Some(1)
        && storage.content.as_deref().is_some_and(|available| {
            available
                .split(',')
                .any(|candidate| candidate.trim() == content)
        })
}

fn select_pve_storage<'a>(
    storages: &'a [PveStorage],
    configured: &str,
    content: &str,
    role: &str,
) -> Result<&'a str> {
    if configured != "auto" {
        let storage = storages
            .iter()
            .find(|storage| storage.storage == configured)
            .ok_or_else(|| anyhow!("PVE storage '{configured}' was not found on this node"))?;
        if !storage_supports(storage, content) {
            bail!(
                "PVE storage '{configured}' cannot store {role}; set the matching pve storage setting"
            );
        }
        return Ok(&storage.storage);
    }

    let mut candidates: Vec<&PveStorage> = storages
        .iter()
        .filter(|storage| storage_supports(storage, content))
        .collect();
    candidates.sort_by(|left, right| left.storage.cmp(&right.storage));
    candidates
        .first()
        .map(|storage| storage.storage.as_str())
        .ok_or_else(|| anyhow!("no active PVE storage on this node supports {role}"))
}

fn template_matches(content: &PveStorageContent, image: &str) -> bool {
    if content.content.as_deref() != Some("vztmpl") {
        return false;
    }
    let Some(filename) = content.volid.rsplit('/').next() else {
        return false;
    };
    let image = image.trim().to_ascii_lowercase();
    if image.is_empty() {
        return false;
    }
    let filename = filename.to_ascii_lowercase();
    let stem = filename
        .strip_suffix(".tar.zst")
        .or_else(|| filename.strip_suffix(".tar.gz"))
        .or_else(|| filename.strip_suffix(".tar.xz"))
        .or_else(|| filename.strip_suffix(".tar.bz2"))
        .or_else(|| filename.strip_suffix(".tar"))
        .unwrap_or(&filename);
    let versionless = stem.split('_').next().unwrap_or(stem);
    let canonical = versionless
        .strip_suffix("-standard")
        .or_else(|| versionless.strip_suffix("-default"))
        .unwrap_or(versionless);
    // Require a recognised PVE flavour. A bare filename such as
    // `debian-13.tar.zst` must not impersonate the configured image alias.
    canonical != versionless && (canonical == image || versionless == image)
}

fn resolve_pve_node(
    client: &impl PveApi,
    configured: &str,
    requested: Option<&str>,
) -> Result<String> {
    let requested = requested.unwrap_or(configured);
    if requested != "auto" {
        return non_empty_new_value("PVE node", requested);
    }

    let mut nodes = client.list_nodes().context("discover PVE nodes")?;
    nodes.retain(|node| node.status.as_deref() == Some("online"));
    nodes.sort_by(|left, right| left.node.cmp(&right.node));
    nodes
        .first()
        .map(|node| node.node.clone())
        .ok_or_else(|| anyhow!("no online PVE node found; pass --node or set pve.node"))
}

fn resolve_pve_node_with_storage(
    client: &impl PveApi,
    configured: &str,
    requested: Option<&str>,
    storage: &str,
    content: &str,
    role: &str,
) -> Result<String> {
    let requested = requested.unwrap_or(configured);
    if requested != "auto" {
        return non_empty_new_value("PVE node", requested);
    }

    let mut nodes = client.list_nodes().context("discover PVE nodes")?;
    nodes.retain(|node| node.status.as_deref() == Some("online"));
    nodes.sort_by(|left, right| left.node.cmp(&right.node));
    for node in nodes {
        let storages = client
            .list_node_storages(&node.node)
            .with_context(|| format!("discover PVE storage on node {}", node.node))?;
        if storage_matches_requirement(&storages, storage, content) {
            return Ok(node.node);
        }
    }
    bail!(
        "no online PVE node has active storage for {role}; pass --node or adjust the storage setting"
    )
}

fn resolve_new_node(
    client: &impl PveApi,
    config: &Config,
    command: &NewCommand,
    prepared_ostemplate: Option<&str>,
) -> Result<String> {
    let requested = command.node.as_deref().unwrap_or(config.pve.node.as_str());
    if requested != "auto" {
        return non_empty_new_value("PVE node", requested);
    }

    let needs_rootfs_storage = command.rootfs.is_none();
    let needs_template_storage = command.ostemplate.is_none() && prepared_ostemplate.is_none();
    let mut nodes = client.list_nodes().context("discover PVE nodes")?;
    nodes.retain(|node| node.status.as_deref() == Some("online"));
    nodes.sort_by(|left, right| left.node.cmp(&right.node));
    if !needs_rootfs_storage && !needs_template_storage {
        return nodes
            .first()
            .map(|node| node.node.clone())
            .ok_or_else(|| anyhow!("no online PVE node found; pass --node or set pve.node"));
    }
    for node in nodes {
        let storages = client
            .list_node_storages(&node.node)
            .with_context(|| format!("discover PVE storage on node {}", node.node))?;
        let rootfs_available = !needs_rootfs_storage
            || storage_matches_requirement(&storages, &config.pve.storage, "rootdir");
        let template_available = !needs_template_storage
            || storage_matches_requirement(&storages, &config.pve.template_storage, "vztmpl");
        if rootfs_available && template_available {
            return Ok(node.node);
        }
    }
    bail!(
        "no online PVE node has the configured storage for this box; pass --node or adjust pve.storage and pve.template-storage"
    );
}

fn storage_matches_requirement(storages: &[PveStorage], configured: &str, content: &str) -> bool {
    storages.iter().any(|storage| {
        (configured == "auto" || storage.storage == configured)
            && storage_supports(storage, content)
    })
}
#[cfg(test)]
fn resolve_new_command(
    client: &impl PveApi,
    config: &Config,
    command: &NewCommand,
) -> Result<ResolvedNew> {
    resolve_new_command_with_template(client, config, command, None, None)
}

fn find_pve_template(
    client: &impl PveApi,
    node: &str,
    storage: &str,
    image: &str,
) -> Result<Option<String>> {
    let contents = client
        .list_storage_content(node, storage, "vztmpl")
        .with_context(|| format!("list PVE templates in storage {storage}"))?;
    let matches: Vec<&PveStorageContent> = contents
        .iter()
        .filter(|content| template_matches(content, image))
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [content] => Ok(Some(content.volid.clone())),
        _ => bail!(
            "image '{image}' matches multiple PVE templates in storage '{storage}'; use --ostemplate to select one"
        ),
    }
}

fn resolve_new_command_with_template(
    client: &impl PveApi,
    config: &Config,
    command: &NewCommand,
    prepared_ostemplate: Option<&str>,
    prepared_node: Option<&str>,
) -> Result<ResolvedNew> {
    let node = match prepared_node {
        Some(node) => non_empty_new_value("PVE node", node)?,
        None => resolve_new_node(client, config, command, prepared_ostemplate)?,
    };
    let needs_storages =
        command.rootfs.is_none() || (command.ostemplate.is_none() && prepared_ostemplate.is_none());
    let storages = if needs_storages {
        Some(
            client
                .list_node_storages(&node)
                .with_context(|| format!("discover PVE storage on node {node}"))?,
        )
    } else {
        None
    };

    let rootfs = match command.rootfs.as_deref() {
        Some(rootfs) => new_rootfs_override(rootfs)?,
        None => {
            let storages = storages
                .as_deref()
                .ok_or_else(|| anyhow!("internal error: rootfs storage discovery was skipped"))?;
            let storage = select_pve_storage(storages, &config.pve.storage, "rootdir", "rootfs")?;
            format!(
                "{storage}:{}",
                rootfs_size_for_storage(storages, storage, &config.pve.defaults.disk)
            )
        }
    };

    let ostemplate = match (command.ostemplate.as_deref(), prepared_ostemplate) {
        (Some(ostemplate), _) => new_volume_override("--ostemplate", ostemplate)?,
        (None, Some(ostemplate)) => ostemplate.to_owned(),
        (None, None) => {
            let image = non_empty_new_value(
                "--image",
                command
                    .image
                    .as_deref()
                    .unwrap_or(config.images.default.as_str()),
            )?;
            let storages = storages
                .as_deref()
                .ok_or_else(|| anyhow!("internal error: template storage discovery was skipped"))?;
            let storage = select_pve_storage(
                storages,
                &config.pve.template_storage,
                "vztmpl",
                "templates",
            )?;
            find_pve_template(client, &node, storage, &image)?.ok_or_else(|| {
                anyhow!(
                    "could not find image '{image}' in PVE storage '{storage}'; use --ostemplate or set images.default"
                )
            })?
        }
    };

    let net0 = match command.net0.as_deref() {
        Some(net0) => non_empty_new_value("--net0", net0)?,
        None => {
            let bridge = non_empty_new_value("pve.bridge", &config.pve.bridge)?;
            format!("name=eth0,bridge={bridge},ip=dhcp")
        }
    };
    let name = command
        .name
        .as_deref()
        .map(|name| non_empty_new_value("--name", name))
        .transpose()?;

    Ok(ResolvedNew {
        node,
        ostemplate,
        rootfs,
        net0,
        name,
        memory: positive_new_value(
            "--memory",
            command.memory.unwrap_or(config.pve.defaults.memory),
        )?,
        swap: command.swap.unwrap_or(config.pve.defaults.swap),
        cores: positive_new_value(
            "--cores",
            command.cores.unwrap_or(config.pve.defaults.cores),
        )?,
        unprivileged: config.pve.defaults.unprivileged,
        onboot: config.pve.defaults.onboot,
        stopped: command.stopped,
    })
}
const VMID_CREATE_ATTEMPTS: usize = 8;

fn create_lxc_with_retry(
    client: &impl PveApi,
    config: &Config,
    resolved: &ResolvedNew,
    id: &PboxId,
    hostname: &str,
    key: &BootstrapKey,
) -> Result<(u64, PveTaskResponse)> {
    let resources = client
        .list_cluster_resources()
        .context("list PVE resources for VMID allocation")?;
    let mut occupied: BTreeSet<u64> = resources
        .iter()
        .filter_map(|resource| resource.vmid)
        .collect();
    for attempt in 0..VMID_CREATE_ATTEMPTS {
        let vmid = config
            .vmid_pattern
            .allocate_lowest(occupied.iter())
            .context("allocate a free PVE VMID")?;
        let metadata = PboxMetadata::new(id.clone(), vmid).with_node(&resolved.node);
        let description = format!(
            "Managed by `pbox`.\n{}",
            encode_metadata(&metadata).context("encode pbox metadata")?
        );
        let request = LxcCreateRequest {
            ostemplate: Some(resolved.ostemplate.clone()),
            hostname: Some(hostname.to_owned()),
            memory: Some(resolved.memory),
            swap: Some(resolved.swap),
            cores: Some(resolved.cores),
            rootfs: Some(resolved.rootfs.clone()),
            net0: Some(resolved.net0.clone()),
            unprivileged: Some(resolved.unprivileged),
            onboot: Some(resolved.onboot),
            description: Some(description),
            ssh_public_keys: config
                .relay
                .url
                .is_none()
                .then(|| key.public_key().to_owned()),
            start: Some(true),
        };
        match client.create_lxc(&resolved.node, vmid, &request) {
            Ok(task) => return Ok((vmid, task)),
            Err(error) if is_vmid_conflict(&error) && attempt + 1 < VMID_CREATE_ATTEMPTS => {
                occupied.insert(vmid);
                let resources = client
                    .list_cluster_resources()
                    .context("refresh PVE resources after VMID allocation race")?;
                occupied.extend(resources.iter().filter_map(|resource| resource.vmid));
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("create box {id} with VMID {vmid} on {}", resolved.node)
                });
            }
        }
    }
    bail!("could not create box {id}: VMID allocation kept racing")
}

fn is_vmid_conflict(error: &PveError) -> bool {
    match error {
        PveError::Http { status, message } => {
            let message = message.to_ascii_lowercase();
            status.as_u16() == 409 || (message.contains("vmid") && message.contains("already"))
        }
        _ => false,
    }
}

fn prepare_new_template_with_progress(
    client: &impl PveApi,
    config: &Config,
    command: &NewCommand,
    progress: Option<CliStyle>,
) -> Result<(Option<String>, Option<String>)> {
    if command.ostemplate.is_some() {
        return Ok((None, None));
    }
    if let Some(style) = progress {
        style.progress("Resolving PVE node and image template...");
    }

    let image = non_empty_new_value(
        "--image",
        command
            .image
            .as_deref()
            .unwrap_or(config.images.default.as_str()),
    )?;
    let node = resolve_new_node(client, config, command, None)?;
    let storages = client
        .list_node_storages(&node)
        .with_context(|| format!("discover PVE storage on node {node}"))?;
    let storage = select_pve_storage(
        &storages,
        &config.pve.template_storage,
        "vztmpl",
        "templates",
    )?;
    if let Some(template) = find_pve_template(client, &node, storage, &image)? {
        if let Some(style) = progress {
            style.progress(&format!("Using existing PVE template {template}..."));
        }
        return Ok((Some(node), Some(template)));
    }

    let oci_image = oci_reference_for_image(&image);
    if let Some(style) = progress {
        style.progress(&format!("Preparing OCI image {oci_image} for PVE..."));
    }
    let mut prepared = prepare_oci_template(client, &node, storage, &oci_image)?;
    if let Some(task) = prepared.task.take() {
        wait_for_oci_template_task_with_progress(
            client,
            &node,
            storage,
            &prepared.reference,
            &prepared.volume,
            task,
            progress,
        )?;
    }
    Ok((Some(node), Some(prepared.volume)))
}

fn run_new(store: &ConfigStore, command: NewCommand, json: bool, color: ColorChoice) -> Result<()> {
    let progress = CliStyle::for_stderr(color, json);
    let config = load_config(store)?;
    if command.snapshot.is_some() {
        return snapshots::restore(&config, command, json, color);
    }
    if config.relay.url.is_some() {
        return relay::run_new(&config, command, json, color);
    }
    progress.progress("Starting box creation...");
    let agent_binary = resolve_agent_binary(&config)?;
    if !agent_binary.is_file() {
        return Err(anyhow!(
            "pbox-agent binary does not exist: {}",
            agent_binary.display()
        ));
    }
    progress.progress("Connecting to PVE...");
    let client = client_from_config(&config)?;
    let (prepared_node, prepared_ostemplate) =
        prepare_new_template_with_progress(&client, &config, &command, Some(progress))?;
    progress.progress("Resolving box resource defaults...");
    let resolved = resolve_new_command_with_template(
        &client,
        &config,
        &command,
        prepared_ostemplate.as_deref(),
        prepared_node.as_deref(),
    )?;
    progress.progress("Checking existing boxes...");
    let existing = discover_boxes(&client)?;
    let id = generate_unique_id(&existing)?;
    progress.progress(&format!(
        "Box {id} allocated; creating LXC on node {}...",
        resolved.node
    ));
    let id_text = id.to_string();
    let materials = agent_materials(&config, &id_text)?;
    let key = BootstrapKey::generate(&id_text).context("create temporary bootstrap SSH key")?;
    let mut operation = BootstrapOperation::new(
        &id_text,
        &resolved.node,
        config.agent.port,
        key.remote_stage(),
    );
    if let Err(error) = key.save_operation(&operation) {
        let _ = key.cleanup();
        return Err(error).context("persist bootstrap recovery operation");
    }
    let hostname = resolved
        .name
        .clone()
        .unwrap_or_else(|| format!("pbox-{}", id_text.trim_start_matches("pbx_")));
    let (vmid, task) =
        match create_lxc_with_retry(&client, &config, &resolved, &id, &hostname, &key) {
            Ok(result) => result,
            Err(error) => return Err(cleanup_uncreated_bootstrap(&key, error, &id_text)),
        };
    progress.progress(&format!("Waiting for PVE to create VMID {vmid}..."));
    operation.vmid = Some(vmid);
    operation.phase = "container-created".to_owned();
    save_bootstrap_operation(
        &key,
        &operation,
        "record created container in bootstrap recovery operation",
        &id_text,
    )?;
    if let Err(error) = wait_for_task_with_progress(
        &client,
        &resolved.node,
        task,
        Some(progress),
        "container creation",
    ) {
        operation.phase = "container-create-task".to_owned();
        let _ = key.save_operation(&operation);
        return Err(error)
            .context(format!(
                "PVE did not finish creating box {id} with VMID {vmid} on {}",
                resolved.node
            ))
            .context(format_bootstrap_repair_path(&key, &id_text));
    }
    operation.phase = "container-running".to_owned();
    save_bootstrap_operation(
        &key,
        &operation,
        "record running container in bootstrap recovery operation",
        &id_text,
    )?;

    progress.progress("Waiting for the container IPv4 address...");
    let ip = match wait_for_lxc_ip_with_progress(&client, &resolved.node, vmid, Some(progress)) {
        Ok(ip) => ip,
        Err(error) => {
            operation.phase = "ip-discovery".to_owned();
            let _ = key.save_operation(&operation);
            return Err(error)
                .context("wait for the new container IPv4 address")
                .context(format_bootstrap_repair_path(&key, &id_text));
        }
    };
    operation.ip = Some(ip);
    operation.phase = "ip-discovered".to_owned();
    save_bootstrap_operation(
        &key,
        &operation,
        "record container address in bootstrap recovery operation",
        &id_text,
    )?;
    operation.phase = "bootstrapping".to_owned();
    save_bootstrap_operation(&key, &operation, "record guest bootstrap start", &id_text)?;
    progress.progress(&format!("Bootstrapping pbox-agent over SSH at {ip}..."));
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
    if let Err(error) = bootstrap_box(&bootstrap_request) {
        return Err(error).context(format_bootstrap_repair_path(&key, &id_text));
    }
    progress.progress("Verifying authenticated pbox-agent...");
    operation.phase = "agent-ready".to_owned();
    save_bootstrap_operation(
        &key,
        &operation,
        "record authenticated guest agent readiness",
        &id_text,
    )?;
    let probe = AgentProbeRequest {
        box_id: &id_text,
        ip,
        port: config.agent.port,
        client_ca: &materials.ca,
        client_subject: &materials.client_subject,
    };
    progress.progress("Removing temporary bootstrap credentials...");
    cleanup_bootstrap_with_fallback(&probe, &key)
        .with_context(|| format_bootstrap_repair_path(&key, &id_text))?;
    operation.phase = "guest-cleaned".to_owned();
    save_bootstrap_operation(&key, &operation, "record guest bootstrap cleanup", &id_text)?;
    key.cleanup().with_context(|| {
        format!(
            "remove completed bootstrap credentials; {}",
            format_bootstrap_repair_path(&key, &id_text)
        )
    })?;

    if !json {
        let endpoint = format!("https://{ip}:{}", config.agent.port);
        ui::user_access(&id_text, "pbox", guest::check(&config, &id_text, &endpoint));
    }
    progress.progress("Box bootstrap complete.");
    let (state, output_ip) = if resolved.stopped {
        progress.progress(&format!("Stopping box {id}..."));
        let task = client
            .shutdown_lxc(&resolved.node, vmid)
            .with_context(|| format!("stop bootstrapped box {id}"))?;
        wait_for_task_with_progress(
            &client,
            &resolved.node,
            task,
            Some(progress),
            "box shutdown",
        )
        .with_context(|| format!("PVE did not finish stopping box {id}"))?;
        ("stopped".to_owned(), None)
    } else {
        ("running".to_owned(), Some(ip.to_string()))
    };
    let info = BoxInfo {
        id,
        vmid,
        state,
        node: resolved.node.clone(),
        ipv6: if resolved.stopped {
            None
        } else {
            discover_lxc_addresses(&client, &resolved.node, vmid)?.1
        },
        ip: output_ip,
        name: Some(hostname),
        recipes: Vec::new(),
        capabilities: Vec::new(),
    };
    print_box_info(&info, json, color)
}
fn run_repair(
    store: &ConfigStore,
    requested_id: &str,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    let id: PboxId = requested_id.parse().context("parse pbox id")?;
    let id_text = id.to_string();
    let config = load_config(store)?;
    if snapshots::repair(&config, requested_id, json, color)? {
        return Ok(());
    }
    let (key, mut operation) = BootstrapKey::find_pending(&id_text)?
        .ok_or_else(|| anyhow!("no interrupted bootstrap operation found for {id_text}"))?;
    if operation.relay {
        return relay::repair(&config, key, operation, json, color);
    }
    let client = client_from_config(&config)?;
    let record = if operation.vmid.is_some() {
        find_box(&client, &id_text)?
    } else {
        let boxes = discover_boxes(&client)?;
        match boxes
            .into_iter()
            .find(|record| record.id.to_string() == id_text)
        {
            Some(record) => record,
            None => {
                return Err(cleanup_uncreated_bootstrap(
                    &key,
                    anyhow!("no PVE container exists for bootstrap operation {id_text}"),
                    &id_text,
                ));
            }
        }
    };
    if let Some(recorded_vmid) = operation.vmid
        && (record.vmid != recorded_vmid || record.node != operation.node)
    {
        bail!(
            "bootstrap operation for {id_text} targets node {} VMID {}, but PVE metadata resolves it to node {} VMID {}",
            operation.node,
            recorded_vmid,
            record.node,
            record.vmid,
        );
    }
    if record.state != "running" {
        bail!(
            "box {id_text} is {}; start it before running `pbox repair {id_text}`",
            record.state
        );
    }
    let ip = wait_for_lxc_ip(&client, &record.node, record.vmid)
        .with_context(|| format!("discover IPv4 address for box {id_text}"))?;
    let materials = agent_materials(&config, &id_text)?;
    let probe = AgentProbeRequest {
        box_id: &id_text,
        ip,
        port: config.agent.port,
        client_ca: &materials.ca,
        client_subject: &materials.client_subject,
    };

    if operation.phase == "guest-cleaned" {
        wait_for_agent(&probe).context("verify the guest agent after bootstrap cleanup")?;
        cleanup_bootstrap_with_fallback(&probe, &key)
            .with_context(|| format_bootstrap_repair_path(&key, &id_text))?;
    } else {
        let agent_ready = operation.phase == "agent-ready" && wait_for_agent(&probe).is_ok();
        if agent_ready {
            cleanup_bootstrap_with_fallback(&probe, &key)
                .with_context(|| format_bootstrap_repair_path(&key, &id_text))?;
            operation.node = record.node.clone();
            operation.vmid = Some(record.vmid);
            operation.ip = Some(ip);
            operation.port = config.agent.port;
            operation.phase = "guest-cleaned".to_owned();
            save_bootstrap_operation(&key, &operation, "record guest bootstrap cleanup", &id_text)?;
        } else {
            let agent_binary = resolve_agent_binary(&config)?;
            if !agent_binary.is_file() {
                return Err(anyhow!(
                    "pbox-agent binary does not exist: {}",
                    agent_binary.display()
                ));
            }
            operation.node = record.node.clone();
            operation.vmid = Some(record.vmid);
            operation.ip = Some(ip);
            operation.port = config.agent.port;
            operation.phase = "repairing".to_owned();
            save_bootstrap_operation(&key, &operation, "record bootstrap repair state", &id_text)?;

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
            if let Err(error) = bootstrap_box(&bootstrap_request) {
                return Err(error).context(format_bootstrap_repair_path(&key, &id_text));
            }
            operation.phase = "agent-ready".to_owned();
            save_bootstrap_operation(
                &key,
                &operation,
                "record repaired guest agent readiness",
                &id_text,
            )?;
            cleanup_bootstrap_with_fallback(&probe, &key)
                .with_context(|| format_bootstrap_repair_path(&key, &id_text))?;
            operation.phase = "guest-cleaned".to_owned();
            save_bootstrap_operation(&key, &operation, "record guest bootstrap cleanup", &id_text)?;
        }
    }

    key.cleanup().with_context(|| {
        format!(
            "remove completed bootstrap credentials; {}",
            format_bootstrap_repair_path(&key, &id_text)
        )
    })?;

    let info = BoxInfo {
        id,
        vmid: record.vmid,
        state: record.state,
        node: record.node,
        ip: Some(ip.to_string()),
        ipv6: record.ipv6,
        name: record.name,
        recipes: record.recipes,
        capabilities: record.capabilities,
    };
    print_box_info(&info, json, color)
}

fn ensure_box_started(client: &impl PveApi, record: &BoxRecord) -> Result<()> {
    if client.get_lxc_state(&record.node, record.vmid)? != "running" {
        let task = client
            .start_lxc(&record.node, record.vmid)
            .with_context(|| format!("start box {}", record.id))?;
        wait_for_task(client, &record.node, task)
            .with_context(|| format!("PVE did not finish starting box {}", record.id))?;
    }
    Ok(())
}

fn run_start(
    store: &ConfigStore,
    requested_id: &str,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    let config = load_config(store)?;
    let client = client_from_config(&config)?;
    let record = find_box(&client, requested_id)?;
    ensure_box_started(&client, &record)?;
    if config.relay.url.is_some() {
        relay::wait_ready(&config, &record.id.to_string())?;
        if !json {
            ui::user_access(
                record.id.as_str(),
                "pbox",
                guest::check(&config, record.id.as_str(), relay::ENDPOINT),
            );
        }
        let current = find_box(&client, &record.id.to_string())?;
        return print_box_info(
            &BoxInfo {
                id: record.id,
                vmid: record.vmid,
                node: record.node,
                state: "running".to_owned(),
                ip: current.ip.map(|ip| ip.to_string()),
                ipv6: current.ipv6,
                name: record.name,
                recipes: record.recipes,
                capabilities: record.capabilities,
            },
            json,
            color,
        );
    }
    let ip = wait_for_lxc_ip(&client, &record.node, record.vmid)
        .with_context(|| format!("discover IPv4 address for box {}", record.id))?;
    let box_id = record.id.to_string();
    let materials = agent_materials(&config, &box_id)?;
    let probe = AgentProbeRequest {
        box_id: &box_id,
        ip,
        port: config.agent.port,
        client_ca: &materials.ca,
        client_subject: &materials.client_subject,
    };
    wait_for_agent(&probe)
        .with_context(|| format!("wait for authenticated pbox-agent in box {}", record.id))?;
    if !json {
        let endpoint = format!("https://{ip}:{}", config.agent.port);
        ui::user_access(&box_id, "pbox", guest::check(&config, &box_id, &endpoint));
    }
    let info = BoxInfo {
        id: record.id,
        vmid: record.vmid,
        state: "running".to_owned(),
        node: record.node,
        ip: Some(ip.to_string()),
        ipv6: record.ipv6,
        name: record.name,
        recipes: record.recipes,
        capabilities: record.capabilities,
    };
    print_box_info(&info, json, color)
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
        ipv6: if state == "running" {
            record.ipv6
        } else {
            None
        },
        name: record.name,
        recipes: record.recipes,
        capabilities: record.capabilities,
    };
    print_box_info(&info, json, color)
}
fn format_bootstrap_repair_path(key: &BootstrapKey, box_id: &str) -> String {
    format!(
        "bootstrap operation remains in {}; run `pbox repair {box_id}` to retry",
        key.operation_directory().display()
    )
}

fn save_bootstrap_operation(
    key: &BootstrapKey,
    operation: &BootstrapOperation,
    description: &str,
    box_id: &str,
) -> Result<()> {
    key.save_operation(operation).with_context(|| {
        format!(
            "{description}; {}",
            format_bootstrap_repair_path(key, box_id)
        )
    })
}

fn cleanup_uncreated_bootstrap(
    key: &BootstrapKey,
    error: anyhow::Error,
    box_id: &str,
) -> anyhow::Error {
    match key.cleanup() {
        Ok(()) => error.context(format!(
            "PVE did not create box {box_id}; removed bootstrap recovery state"
        )),
        Err(cleanup_error) => error.context(format!(
            "PVE did not create box {box_id}; could not remove bootstrap recovery state in {}: {cleanup_error}",
            key.operation_directory().display()
        )),
    }
}

fn cleanup_pending_bootstrap(client: &impl PveApi, box_id: &str) -> Result<()> {
    if let Some((key, mut operation)) = BootstrapKey::find_pending(box_id)? {
        relay::cleanup_template(client, &key, &mut operation)?;
        key.cleanup()
            .with_context(|| format!("remove bootstrap recovery state for deleted box {box_id}"))?;
    }
    Ok(())
}

fn is_current_box_reference(value: &str) -> bool {
    value.eq_ignore_ascii_case("current")
}

fn find_box_reference(client: &impl PveApi, requested_id: &str) -> Result<BoxRecord> {
    if !is_current_box_reference(requested_id) {
        return find_box(client, requested_id);
    }
    let boxes = discover_boxes(client)?;
    match boxes.len() {
        0 => bail!("current box is not defined; no pbox-managed containers were found"),
        1 => Ok(boxes
            .into_iter()
            .next()
            .expect("one current box must be present")),
        _ => {
            let available = boxes
                .iter()
                .map(|record| {
                    format!(
                        "{} ({} on node {}, VMID {})",
                        record.id,
                        record.name.as_deref().unwrap_or("unnamed"),
                        record.node,
                        record.vmid
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            bail!("current box is ambiguous; choose an explicit ID: {available}");
        }
    }
}

fn delete_box(client: &impl PveApi, record: &BoxRecord, style: CliStyle) -> Result<()> {
    let state = client
        .get_lxc_state(&record.node, record.vmid)
        .context("check box state before deletion")?;
    if state != "stopped" {
        style.progress(&format!("Stopping {}...", record.id));
        let task = client
            .shutdown_lxc(&record.node, record.vmid)
            .with_context(|| format!("request shutdown of {}; box was not deleted", record.id))?;
        wait_for_task(client, &record.node, task)
            .with_context(|| format!("shutdown failed; box was not deleted. Use `pbox stop {} --force` if needed, then retry deletion", record.id))?;
        anyhow::ensure!(
            client.get_lxc_state(&record.node, record.vmid)? == "stopped",
            "box {} is still running; it was not deleted",
            record.id
        );
    }
    style.progress(&format!("Deleting {}...", record.id));
    let task = client
        .delete_lxc(&record.node, record.vmid)
        .with_context(|| format!("delete box {}", record.id))?;
    wait_for_task(client, &record.node, task)
        .with_context(|| format!("PVE did not finish deleting box {}", record.id))
}

fn queue_delete(client: &impl PveApi, record: &BoxRecord) -> Result<PveTaskResponse> {
    client
        .force_delete_lxc(&record.node, record.vmid)
        .with_context(|| format!("queue deletion of box {}", record.id))
}

fn run_delete(
    store: &ConfigStore,
    requested_id: &str,
    confirmed: bool,
    wait: bool,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    let client = client_from_store(store)?;
    let record = find_box_reference(&client, requested_id)?;
    let style = CliStyle::for_stderr(color, json);
    // Failed bootstrap operations still own private templates and recovery files.
    // Finish synchronously in that case so cleanup only follows successful deletion.
    let recovery = BootstrapKey::find_pending(&record.id.to_string())?.is_some();
    let wait = wait || recovery;
    if !confirmed {
        if json || !io::stdin().is_terminal() || !io::stderr().is_terminal() {
            bail!(
                "to permanently delete {}, run `pbox rm {} --yes`",
                record.id,
                record.id
            );
        }
        style.section("Delete box");
        style.metadata("id", &record.id.to_string());
        style.metadata("name", record.name.as_deref().unwrap_or("unnamed"));
        style.metadata("state", &record.state);
        style.warning("This permanently deletes the box and its data.");
        if !wait {
            style.hint("Running boxes will be stopped immediately. Deletion continues in Proxmox.");
        }
        if !confirm_delete(&mut io::stdin().lock(), &mut io::stderr().lock())? {
            style.hint("Cancelled. No changes made.");
            return Ok(());
        }
    }
    if !wait {
        let task = queue_delete(&client, &record)?;
        if json {
            ui::json_text(&serde_json::to_string_pretty(&serde_json::json!({
                "id": record.id, "vmid": record.vmid, "node": record.node,
                "deleted": false, "queued": true, "upid": task.upid
            }))?);
        } else {
            let output = CliStyle::for_stdout(color, json);
            output.success(&format!("Deletion queued: {}", record.id));
            output.hint(&format!("Proxmox is stopping and deleting the box. Check node {} task history for the result.", record.node));
            if progress::verbose() {
                output.stdout_metadata("task", &task.upid);
            }
        }
        return Ok(());
    }
    if recovery && !json {
        style.hint("Waiting for deletion to finish so private bootstrap files can be cleaned up.");
    }
    delete_box(&client, &record, style)?;
    cleanup_pending_bootstrap(&client, &record.id.to_string())?;
    let output = DeleteOutput {
        id: record.id,
        vmid: record.vmid,
        node: record.node,
        deleted: true,
    };
    if json {
        ui::json_text(&(serde_json::to_string_pretty(&output)?));
    } else {
        let output_style = CliStyle::for_stdout(color, json);
        output_style.stdout_status("ok", ANSI_GREEN, &format!("Deleted box {}.", output.id));
    }
    Ok(())
}

fn run_snapshot(
    command: CheckpointSubcommand,
    store: &ConfigStore,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    match command {
        CheckpointSubcommand::List { id } => run_snapshot_list(store, id.as_deref(), json, color),
        CheckpointSubcommand::Create {
            id,
            name,
            description,
        } => run_snapshot_create(store, &id, &name, description.as_deref(), json, color),
        CheckpointSubcommand::Rollback {
            id,
            name,
            start,
            yes,
        } => run_snapshot_rollback(store, &id, &name, start, yes, json, color),
        CheckpointSubcommand::Delete { id, name, yes } => {
            run_snapshot_delete(store, &id, &name, yes, json, color)
        }
    }
}

fn run_snapshot_list(
    store: &ConfigStore,
    requested_id: Option<&str>,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    let client = client_from_store(store)?;
    let records = if let Some(id) = requested_id {
        vec![find_box_reference(&client, id)?]
    } else {
        discover_boxes(&client)?
    };
    let mut groups = Vec::new();
    for record in records {
        let snapshots = client
            .list_lxc_snapshots(&record.node, record.vmid)
            .with_context(|| format!("list snapshots for box {}", record.id))?;
        groups.push((record, snapshots));
    }
    if json {
        if requested_id.is_some() {
            ui::json_text(&serde_json::to_string_pretty(&groups[0].1)?);
        } else {
            let output: Vec<_> = groups
                .iter()
                .map(|(record, snapshots)| {
                    serde_json::json!({
                        "id": record.id, "name": record.name, "node": record.node,
                        "snapshots": snapshots
                    })
                })
                .collect();
            ui::json_text(&serde_json::to_string_pretty(&output)?);
        }
    } else {
        ui::snapshot_groups(&groups, color_enabled(color, json));
    }
    Ok(())
}

fn run_snapshot_create(
    store: &ConfigStore,
    requested_id: &str,
    name: &str,
    description: Option<&str>,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    validate_snapshot_arguments(name, description)?;
    let client = client_from_store(store)?;
    let record = find_box(&client, requested_id)?;
    create_box_snapshot(&client, &record, name, description)?;
    print_snapshot_action(
        &SnapshotActionOutput {
            id: record.id,
            name: name.to_owned(),
            action: "created".to_owned(),
            started: None,
        },
        json,
        color,
    )
}

fn run_snapshot_rollback(
    store: &ConfigStore,
    requested_id: &str,
    name: &str,
    start: bool,
    confirmed: bool,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    if !confirmed {
        return Err(anyhow!(
            "rolling back a snapshot changes guest state; repeat the command with --yes"
        ));
    }
    validate_snapshot_arguments(name, None)?;
    let client = client_from_store(store)?;
    let record = find_box(&client, requested_id)?;
    rollback_box_snapshot(&client, &record, name, start)?;
    print_snapshot_action(
        &SnapshotActionOutput {
            id: record.id,
            name: name.to_owned(),
            action: "rolled back".to_owned(),
            started: Some(start),
        },
        json,
        color,
    )
}

fn run_snapshot_delete(
    store: &ConfigStore,
    requested_id: &str,
    name: &str,
    confirmed: bool,
    json: bool,
    color: ColorChoice,
) -> Result<()> {
    if !confirmed {
        return Err(anyhow!(
            "deleting a snapshot is permanent; repeat the command with --yes"
        ));
    }
    validate_snapshot_arguments(name, None)?;
    let client = client_from_store(store)?;
    let record = find_box(&client, requested_id)?;
    delete_box_snapshot(&client, &record, name)?;
    print_snapshot_action(
        &SnapshotActionOutput {
            id: record.id,
            name: name.to_owned(),
            action: "deleted".to_owned(),
            started: None,
        },
        json,
        color,
    )
}

fn validate_snapshot_arguments(name: &str, description: Option<&str>) -> Result<()> {
    if name.contains('\0') {
        bail!("snapshot name cannot contain NUL bytes");
    }
    if description.is_some_and(|value| value.contains('\0')) {
        bail!("snapshot description cannot contain NUL bytes");
    }
    Ok(())
}

fn create_box_snapshot(
    client: &impl PveApi,
    record: &BoxRecord,
    name: &str,
    description: Option<&str>,
) -> Result<()> {
    let request = LxcSnapshotRequest {
        snapname: name.to_owned(),
        description: description.map(str::to_owned),
    };
    let task = client
        .create_lxc_snapshot(&record.node, record.vmid, &request)
        .with_context(|| format!("create snapshot {name} for box {}", record.id))?;
    wait_for_task(client, &record.node, task)
        .with_context(|| format!("PVE did not finish creating snapshot {name}"))?;
    Ok(())
}

fn rollback_box_snapshot(
    client: &impl PveApi,
    record: &BoxRecord,
    name: &str,
    start: bool,
) -> Result<()> {
    let task = client
        .rollback_lxc_snapshot(&record.node, record.vmid, name, start)
        .with_context(|| format!("roll back box {} to snapshot {name}", record.id))?;
    wait_for_task(client, &record.node, task)
        .with_context(|| format!("PVE did not finish rolling back snapshot {name}"))?;
    Ok(())
}

fn delete_box_snapshot(client: &impl PveApi, record: &BoxRecord, name: &str) -> Result<()> {
    let task = client
        .delete_lxc_snapshot(&record.node, record.vmid, name)
        .with_context(|| format!("delete snapshot {name} from box {}", record.id))?;
    wait_for_task(client, &record.node, task)
        .with_context(|| format!("PVE did not finish deleting snapshot {name}"))?;
    Ok(())
}

fn format_snapshot_time(timestamp: Option<u64>) -> String {
    let Some(timestamp) = timestamp else {
        return "-".to_owned();
    };
    let Ok(timestamp) = i64::try_from(timestamp) else {
        return timestamp.to_string();
    };
    OffsetDateTime::from_unix_timestamp(timestamp)
        .ok()
        .and_then(|time| time.format(&Rfc3339).ok())
        .unwrap_or_else(|| timestamp.to_string())
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

const PROGRESS_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
struct ProgressHeartbeat {
    stop: Arc<AtomicBool>,
    output_lock: Arc<Mutex<()>>,
    style: CliStyle,
}

fn spawn_progress_heartbeat(
    style: Option<CliStyle>,
    message: impl Into<String>,
) -> Option<ProgressHeartbeat> {
    let style = style?;
    let stop = Arc::new(AtomicBool::new(false));
    let output_lock = Arc::new(Mutex::new(()));
    let thread_stop = Arc::clone(&stop);
    let thread_output_lock = Arc::clone(&output_lock);
    let message = message.into();
    thread::spawn(move || {
        let started = Instant::now();
        while !thread_stop.load(Ordering::Relaxed) {
            thread::sleep(PROGRESS_HEARTBEAT_INTERVAL);
            if thread_stop.load(Ordering::Relaxed) {
                break;
            }
            let Ok(_guard) = thread_output_lock.lock() else {
                break;
            };
            if thread_stop.load(Ordering::Relaxed) {
                break;
            }
            style.progress_live(&format!(
                "{message} ({}s elapsed)",
                started.elapsed().as_secs()
            ));
        }
    });
    Some(ProgressHeartbeat {
        stop,
        output_lock,
        style,
    })
}

fn stop_progress_heartbeat(heartbeat: Option<ProgressHeartbeat>) {
    if let Some(heartbeat) = heartbeat {
        heartbeat.stop.store(true, Ordering::Relaxed);
        if let Ok(_guard) = heartbeat.output_lock.lock() {
            heartbeat.style.clear_progress_line();
        }
    }
}

fn wait_for_oci_template_task(
    client: &impl PveApi,
    node: &str,
    storage: &str,
    reference: &str,
    volume: &str,
    task: PveTaskResponse,
) -> Result<()> {
    wait_for_oci_template_task_with_progress(client, node, storage, reference, volume, task, None)
}

fn wait_for_oci_template_task_with_progress(
    client: &impl PveApi,
    node: &str,
    storage: &str,
    reference: &str,
    volume: &str,
    task: PveTaskResponse,
    progress: Option<CliStyle>,
) -> Result<()> {
    let description = format!("OCI image {reference}");
    if let Err(task_error) = wait_for_task_with_progress(client, node, task, progress, &description)
    {
        if oci_template_present(client, node, storage, volume)
            .with_context(|| format!("recheck OCI template {volume}"))?
        {
            return Ok(());
        }
        return Err(task_error).with_context(|| format!("wait for OCI image {reference}"));
    }
    Ok(())
}

fn wait_for_task(client: &impl PveApi, node: &str, task: PveTaskResponse) -> Result<()> {
    wait_for_task_with_progress(client, node, task, None, "PVE task")
}
const TASK_LOG_LIMIT: u64 = 40;
const TASK_LOG_MAX_CHARS: usize = 8 * 1024;

fn task_error_with_log(
    client: &impl PveApi,
    node: &str,
    upid: &str,
    message: String,
) -> anyhow::Error {
    let Ok(entries) = client.get_task_log(node, upid, 0, TASK_LOG_LIMIT) else {
        return anyhow!(message);
    };
    let mut log = String::new();
    let mut used = 0;
    for entry in entries {
        let line = safe_terminal_text(&entry.t);
        if line.is_empty() {
            continue;
        }
        for character in line.chars() {
            if used >= TASK_LOG_MAX_CHARS {
                break;
            }
            log.push(character);
            used += 1;
        }
        if used >= TASK_LOG_MAX_CHARS {
            break;
        }
        log.push('\n');
        used += 1;
    }
    if log.is_empty() {
        return anyhow!(message);
    }
    anyhow!("{message}\nPVE task log:\n{}", log.trim_end())
}

fn wait_for_task_with_progress(
    client: &impl PveApi,
    node: &str,
    task: PveTaskResponse,
    progress: Option<CliStyle>,
    description: &str,
) -> Result<()> {
    let heartbeat = spawn_progress_heartbeat(
        progress,
        format!("Waiting for {description} on PVE node {node}"),
    );
    progress::substep(description);
    let started = Instant::now();
    let mut log_cursor = 0;
    let mut read_failures = 0;
    let mut last_log = None::<Instant>;
    let result = loop {
        let status = match client.get_task_status(node, &task.upid) {
            Ok(status) => {
                read_failures = 0;
                status
            }
            Err(PveError::Http { status, .. })
                if matches!(status.as_u16(), 502 | 503 | 504 | 596) && read_failures < 3 =>
            {
                read_failures += 1;
                thread::sleep(Duration::from_secs(1));
                continue;
            }
            Err(error) => break Err(anyhow!("read PVE task status: {error}")),
        };
        if (progress::has_details() || progress::verbose())
            && (last_log.is_none_or(|time| time.elapsed() >= Duration::from_secs(1))
                || status.status == "stopped")
        {
            if let Ok(lines) = client.get_task_log(node, &task.upid, log_cursor, 100) {
                for line in lines {
                    log_cursor = log_cursor.max(line.n);
                    progress::log(&line.t);
                }
            }
            last_log = Some(Instant::now());
        }
        if status.status == "stopped" {
            if status.is_successful() {
                break Ok(());
            }
            break Err(task_error_with_log(
                client,
                node,
                &task.upid,
                format!(
                    "PVE task failed on node {node} (UPID {}): {}",
                    task.upid,
                    status
                        .exitstatus
                        .as_deref()
                        .unwrap_or("missing exit status")
                ),
            ));
        }
        if started.elapsed() >= PVE_TASK_TIMEOUT {
            break Err(task_error_with_log(
                client,
                node,
                &task.upid,
                format!(
                    "PVE task {} on node {node} did not finish within {} seconds",
                    task.upid,
                    PVE_TASK_TIMEOUT.as_secs(),
                ),
            ));
        }
        thread::sleep(PVE_TASK_POLL_INTERVAL);
    };
    stop_progress_heartbeat(heartbeat);
    progress::substep_done(description, started.elapsed().as_secs(), result.is_ok());
    result
}

fn run_list(store: &ConfigStore, json: bool, color: ColorChoice) -> Result<()> {
    let client = client_from_store(store)?;
    let records = discover_boxes(&client)?;
    if json {
        ui::json_text(&(serde_json::to_string_pretty(&records)?));
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
        ipv6: record.ipv6,
        name: record.name,
        recipes: record.recipes,
        capabilities: record.capabilities,
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
    if config.pve.tls_insecure {
        ui::stderr().warning("pve.tls_insecure disables PVE certificate verification and exposes the API token to a MITM");
    }
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
    resolve_agent_endpoint(config, &command.id, command.endpoint.as_deref())
}

fn resolve_agent_endpoint(
    config: &Config,
    requested_id: &str,
    endpoint: Option<&str>,
) -> Result<(String, String)> {
    let id: PboxId = requested_id.parse().context("parse pbox id")?;
    if let Some(endpoint) = endpoint {
        return Ok((id.to_string(), endpoint.to_owned()));
    }
    let client = client_from_config(config)?;
    let record = find_box(&client, &id.to_string())?;
    if client.get_lxc_state(&record.node, record.vmid)? != "running" {
        return Err(anyhow!("box {} is not running", record.id));
    }
    if config.relay.url.is_some() {
        relay::access(config, &id.to_string(), "client")?;
        return Ok((id.to_string(), relay::ENDPOINT.to_owned()));
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
    wait_for_lxc_ip_with_progress(client, node, vmid, None)
}

fn wait_for_lxc_ip_with_progress(
    client: &impl PveApi,
    node: &str,
    vmid: u64,
    progress: Option<CliStyle>,
) -> Result<Ipv4Addr> {
    let heartbeat = spawn_progress_heartbeat(
        progress,
        format!("Waiting for an IPv4 address for LXC {vmid} on node {node}"),
    );
    let started = Instant::now();
    let mut last_error = None;
    let result = loop {
        match discover_lxc_ip(client, node, vmid) {
            Ok(Some(address)) => {
                break address
                    .parse()
                    .with_context(|| format!("parse discovered IPv4 address {address}"));
            }
            Ok(None) => {}
            Err(error) => last_error = Some(error.to_string()),
        }
        if started.elapsed() >= LXC_IP_TIMEOUT {
            let reason = last_error.as_deref().unwrap_or(
                "PVE returned no usable IPv4 address; check DHCP or static network configuration",
            );
            break Err(anyhow!(
                "could not discover an IPv4 address within {} seconds: {reason}",
                LXC_IP_TIMEOUT.as_secs()
            ));
        }
        thread::sleep(LXC_IP_POLL_INTERVAL);
    };
    stop_progress_heartbeat(heartbeat);
    result
}

fn discover_lxc_ip(client: &impl PveApi, node: &str, vmid: u64) -> Result<Option<String>> {
    let interfaces = client
        .list_lxc_interfaces(node, vmid)
        .context("read runtime LXC interfaces")?;
    Ok(select_lxc_ipv4(&interfaces).map(|address| address.to_string()))
}

fn discover_lxc_addresses(
    client: &impl PveApi,
    node: &str,
    vmid: u64,
) -> Result<(Option<String>, Option<String>)> {
    let interfaces = client
        .list_lxc_interfaces(node, vmid)
        .context("read runtime LXC interfaces")?;
    Ok((
        select_lxc_ipv4(&interfaces).map(|ip| ip.to_string()),
        select_lxc_ipv6(&interfaces).map(|ip| ip.to_string()),
    ))
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
        let (ip, ipv6) = if state == "running" {
            discover_lxc_addresses(client, node, vmid).with_context(|| {
                format!(
                    "discover addresses for box {metadata_id}",
                    metadata_id = metadata.id
                )
            })?
        } else {
            (None, None)
        };
        let name = config.hostname.or(resource.name);
        records.push(BoxRecord {
            id: metadata.id,
            vmid,
            state,
            node: node.to_owned(),
            ip,
            ipv6,
            name,
            recipes: metadata.recipes,
            capabilities: metadata.capabilities,
        });
    }
    records.sort_by(|left, right| left.id.cmp(&right.id));
    for pair in records.windows(2) {
        if pair[0].id == pair[1].id {
            bail!(
                "duplicate pbox id {} in PVE metadata for node {} VMID {} and node {} VMID {}",
                pair[0].id,
                pair[0].node,
                pair[0].vmid,
                pair[1].node,
                pair[1].vmid,
            );
        }
    }
    Ok(records)
}

fn find_box(client: &impl PveApi, requested_id: &str) -> Result<BoxRecord> {
    let id: PboxId = requested_id.parse().context("parse pbox id")?;
    discover_boxes(client)?
        .into_iter()
        .find(|record| record.id == id)
        .ok_or_else(|| anyhow!("box '{id}' was not found"))
}

fn box_state_colour(state: &str) -> &'static str {
    match state {
        "running" => ANSI_GREEN,
        "stopped" => ANSI_YELLOW,
        _ => ANSI_RED,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ANSI_CYAN, ANSI_RESET, AnsibleRun, BootstrapKey, BoxRecord, Cli, CliStyle, Command,
        ForwardCommand, NewCommand, RecipeSnapshot, SetupAnswers, SetupChoice, SetupCommand,
        SetupOutput, SshCommand, agent_identity_changes, apply_setup_values, create_box_snapshot,
        create_lxc_with_retry, create_recipe_snapshot, delete_box_snapshot, delete_recipe_snapshot,
        exec_exit_code, finish_recipe_failure, finish_recipe_success, format_snapshot_time,
        parse_env_entry, parse_recipe_sync_ttl, parse_remote_path, parse_setup_bool,
        record_recipe_provenance, resolve_new_command, resolve_pve_node_with_storage,
        rollback_box_snapshot, safe_terminal_text, setup_choice_default, setup_pve_error_message,
        ssh_command_argv, template_matches, validate_forward_arguments,
        validate_snapshot_arguments, validate_ssh_arguments, wait_for_oci_template_task,
        write_download,
    };
    use anyhow::anyhow;
    use clap::Parser;
    use pbox_agent_client::ExecResult;
    use pbox_core::ui::ColorMode;
    use pbox_core::{
        ClusterResource, Config, LxcConfig, LxcConfigUpdateRequest, LxcCreateRequest, LxcInterface,
        LxcSnapshot, LxcSnapshotRequest, PboxId, PboxMetadata, PveApi, PveError,
        PveNetworkInterface, PveNode, PveStorage, PveStorageContent, PveTaskLog, PveTaskResponse,
        PveTaskStatus, encode_metadata, parse_metadata,
    };
    use std::cell::RefCell;
    use std::fs;
    use std::time::Duration;

    struct FakePve {
        current_state: RefCell<String>,
        config: RefCell<LxcConfig>,
        snapshots: RefCell<Vec<LxcSnapshot>>,
        nodes: RefCell<Vec<PveNode>>,
        storages: RefCell<Vec<PveStorage>>,
        storage_content: RefCell<Vec<PveStorageContent>>,
        resources: RefCell<Vec<ClusterResource>>,
        created: RefCell<Vec<(String, u64, LxcCreateRequest)>>,
        create_conflicts: RefCell<usize>,
        events: RefCell<Vec<String>>,
        oci_tags: RefCell<Vec<String>>,
        task_log: RefCell<Vec<PveTaskLog>>,
        fail_next_task: RefCell<bool>,
        fail_create: RefCell<bool>,
        fail_updates: RefCell<bool>,
    }
    impl FakePve {
        fn new(metadata: &PboxMetadata, note: &str) -> Self {
            Self {
                current_state: RefCell::new("stopped".to_owned()),
                config: RefCell::new(LxcConfig {
                    digest: Some("digest-1".to_owned()),
                    description: Some(format!("{note}\n\n{}", encode_metadata(metadata).unwrap())),
                    hostname: None,
                    cores: None,
                    memory: None,
                    swap: None,
                    rootfs: None,
                    unprivileged: None,
                    net0: None,
                    extra: serde_json::Map::new(),
                }),
                snapshots: RefCell::new(Vec::new()),
                nodes: RefCell::new(vec![PveNode {
                    node: "pve01".to_owned(),
                    status: Some("online".to_owned()),
                    extra: serde_json::Map::new(),
                }]),
                storages: RefCell::new(vec![PveStorage {
                    storage: "local".to_owned(),
                    content: Some("vztmpl,rootdir".to_owned()),
                    active: Some(1),
                    enabled: Some(1),
                    extra: serde_json::Map::new(),
                }]),
                storage_content: RefCell::new(vec![PveStorageContent {
                    volid: "local:vztmpl/debian-13-standard_13.0-1_amd64.tar.zst".to_owned(),
                    content: Some("vztmpl".to_owned()),
                    format: Some("tar.zst".to_owned()),
                    is_base: Some(1),
                    extra: serde_json::Map::new(),
                }]),
                resources: RefCell::new(Vec::new()),
                created: RefCell::new(Vec::new()),
                create_conflicts: RefCell::new(0),
                events: RefCell::new(Vec::new()),
                oci_tags: RefCell::new(Vec::new()),
                task_log: RefCell::new(Vec::new()),
                fail_create: RefCell::new(false),
                fail_next_task: RefCell::new(false),
                fail_updates: RefCell::new(false),
            }
        }

        fn fail_next_task(&self) {
            *self.fail_next_task.borrow_mut() = true;
        }
        fn set_task_log(&self, entries: Vec<PveTaskLog>) {
            *self.task_log.borrow_mut() = entries;
        }

        fn fail_create(&self) {
            *self.fail_create.borrow_mut() = true;
        }

        fn fail_updates(&self) {
            *self.fail_updates.borrow_mut() = true;
        }

        fn unsupported() -> PveError {
            PveError::InvalidPathSegment {
                field: "test".to_owned(),
            }
        }

        fn task(name: &str) -> PveTaskResponse {
            PveTaskResponse {
                upid: name.to_owned(),
            }
        }
    }

    impl PveApi for FakePve {
        fn list_cluster_resources(&self) -> Result<Vec<ClusterResource>, PveError> {
            Ok(self.resources.borrow().clone())
        }
        fn list_nodes(&self) -> Result<Vec<pbox_core::PveNode>, PveError> {
            Ok(self.nodes.borrow().clone())
        }

        fn list_node_storages(&self, _node: &str) -> Result<Vec<pbox_core::PveStorage>, PveError> {
            Ok(self.storages.borrow().clone())
        }

        fn list_node_network_interfaces(
            &self,
            _node: &str,
        ) -> Result<Vec<PveNetworkInterface>, PveError> {
            Ok(vec![PveNetworkInterface {
                iface: "vmbr0".to_owned(),
                interface_type: Some("bridge".to_owned()),
                cidr: Some("192.0.2.1/24".to_owned()),
                extra: serde_json::Map::new(),
            }])
        }

        fn list_storage_content(
            &self,
            _node: &str,
            _storage: &str,
            _content: &str,
        ) -> Result<Vec<pbox_core::PveStorageContent>, PveError> {
            Ok(self.storage_content.borrow().clone())
        }

        fn list_oci_repo_tags(&self, node: &str, reference: &str) -> Result<Vec<String>, PveError> {
            self.events
                .borrow_mut()
                .push(format!("oci-tags:{node}:{reference}"));
            Ok(self.oci_tags.borrow().clone())
        }

        fn pull_oci_registry(
            &self,
            node: &str,
            storage: &str,
            reference: &str,
            filename: &str,
        ) -> Result<PveTaskResponse, PveError> {
            self.events
                .borrow_mut()
                .push(format!("oci-pull:{node}:{storage}:{reference}:{filename}"));
            Ok(Self::task("oci-pull"))
        }

        fn upload_storage_template(
            &self,
            node: &str,
            storage: &str,
            filename: &str,
            _path: &std::path::Path,
        ) -> Result<PveTaskResponse, PveError> {
            self.events
                .borrow_mut()
                .push(format!("oci-upload:{node}:{storage}:{filename}"));
            Ok(Self::task("oci-upload"))
        }

        fn get_lxc_state(&self, _node: &str, _vmid: u64) -> Result<String, PveError> {
            Ok(self.current_state.borrow().clone())
        }

        fn get_lxc_config(&self, _node: &str, _vmid: u64) -> Result<LxcConfig, PveError> {
            Ok(self.config.borrow().clone())
        }

        fn list_lxc_interfaces(
            &self,
            _node: &str,
            _vmid: u64,
        ) -> Result<Vec<LxcInterface>, PveError> {
            Err(Self::unsupported())
        }

        fn list_lxc_snapshots(
            &self,
            _node: &str,
            _vmid: u64,
        ) -> Result<Vec<LxcSnapshot>, PveError> {
            Ok(self.snapshots.borrow().clone())
        }

        fn get_task_status(&self, _node: &str, upid: &str) -> Result<PveTaskStatus, PveError> {
            self.events.borrow_mut().push(format!("wait:{upid}"));
            let failed = self.fail_next_task.replace(false);
            Ok(PveTaskStatus {
                status: "stopped".to_owned(),
                exitstatus: Some(if failed {
                    "ERROR: test".to_owned()
                } else {
                    "OK".to_owned()
                }),
                upid: Some(upid.to_owned()),
                node: None,
                pid: None,
                starttime: None,
                type_: None,
            })
        }
        fn get_task_log(
            &self,
            _node: &str,
            _upid: &str,
            _start: u64,
            _limit: u64,
        ) -> Result<Vec<PveTaskLog>, PveError> {
            Ok(self.task_log.borrow().clone())
        }

        fn create_lxc(
            &self,
            node: &str,
            vmid: u64,
            request: &LxcCreateRequest,
        ) -> Result<PveTaskResponse, PveError> {
            self.created
                .borrow_mut()
                .push((node.to_owned(), vmid, request.clone()));
            let mut conflicts = self.create_conflicts.borrow_mut();
            if *conflicts > 0 {
                *conflicts -= 1;
                return Err(PveError::Http {
                    status: "409".parse().unwrap(),
                    message: "VMID already exists".to_owned(),
                });
            }
            Ok(Self::task("create"))
        }

        fn update_lxc_config(
            &self,
            _node: &str,
            _vmid: u64,
            request: &LxcConfigUpdateRequest,
        ) -> Result<(), PveError> {
            self.events.borrow_mut().push("update".to_owned());
            if *self.fail_updates.borrow() {
                return Err(Self::unsupported());
            }
            let mut config = self.config.borrow_mut();
            config.digest = request.digest.clone();
            if let Some(description) = &request.description {
                config.description = Some(description.clone());
            }
            Ok(())
        }

        fn start_lxc(&self, _node: &str, _vmid: u64) -> Result<PveTaskResponse, PveError> {
            self.events.borrow_mut().push("start".to_owned());
            *self.current_state.borrow_mut() = "running".to_owned();
            Ok(Self::task("start"))
        }

        fn shutdown_lxc(&self, _node: &str, _vmid: u64) -> Result<PveTaskResponse, PveError> {
            self.events.borrow_mut().push("shutdown".to_owned());
            *self.current_state.borrow_mut() = "stopped".to_owned();
            Ok(Self::task("shutdown"))
        }

        fn stop_lxc(&self, _node: &str, _vmid: u64) -> Result<PveTaskResponse, PveError> {
            Err(Self::unsupported())
        }

        fn clone_lxc(
            &self,
            _node: &str,
            _vmid: u64,
            request: &pbox_core::LxcCloneRequest,
        ) -> Result<PveTaskResponse, PveError> {
            self.events
                .borrow_mut()
                .push(format!("clone:{}:{}", request.newid, request.full));
            Ok(Self::task("clone"))
        }

        fn delete_lxc(&self, _node: &str, _vmid: u64) -> Result<PveTaskResponse, PveError> {
            self.events.borrow_mut().push("delete".to_owned());
            Ok(Self::task("delete"))
        }

        fn force_delete_lxc(&self, _node: &str, _vmid: u64) -> Result<PveTaskResponse, PveError> {
            self.events.borrow_mut().push("force-delete".to_owned());
            Ok(Self::task("force-delete"))
        }

        fn create_lxc_snapshot(
            &self,
            _node: &str,
            _vmid: u64,
            request: &LxcSnapshotRequest,
        ) -> Result<PveTaskResponse, PveError> {
            self.events
                .borrow_mut()
                .push(format!("create:{}", request.snapname));
            if *self.fail_create.borrow() {
                return Err(Self::unsupported());
            }
            self.snapshots.borrow_mut().push(LxcSnapshot {
                name: request.snapname.clone(),
                description: request.description.clone(),
                snaptime: Some(1_724_520_000),
                parent: None,
                extra: serde_json::Map::new(),
            });
            Ok(Self::task("snapshot-create"))
        }

        fn rollback_lxc_snapshot(
            &self,
            _node: &str,
            _vmid: u64,
            snapname: &str,
            start: bool,
        ) -> Result<PveTaskResponse, PveError> {
            self.events
                .borrow_mut()
                .push(format!("rollback:{snapname}:{start}"));
            Ok(Self::task("snapshot-rollback"))
        }

        fn delete_lxc_snapshot(
            &self,
            _node: &str,
            _vmid: u64,
            snapname: &str,
        ) -> Result<PveTaskResponse, PveError> {
            self.events.borrow_mut().push(format!("delete:{snapname}"));
            self.snapshots
                .borrow_mut()
                .retain(|snapshot| snapshot.name != snapname);
            Ok(Self::task("snapshot-delete"))
        }
    }

    #[test]
    fn immediate_restart_uses_node_state_even_when_cluster_cache_says_running() {
        let record = test_record();
        let fake = FakePve::new(&test_metadata(&record), "test");
        assert_eq!(record.state, "running");
        super::ensure_box_started(&fake, &record).unwrap();
        assert!(fake.events.borrow().iter().any(|event| event == "start"));
        fake.events.borrow_mut().clear();
        super::ensure_box_started(&fake, &record).unwrap();
        assert!(fake.events.borrow().is_empty());
    }

    #[test]
    fn delete_confirmation_defaults_to_cancel_and_accepts_only_yes() {
        for (input, expected) in [
            ("", false),
            ("\n", false),
            ("n\n", false),
            ("yes\n", true),
            (" Y \n", true),
            ("maybe\nno\n", false),
        ] {
            let mut output = Vec::new();
            assert_eq!(
                super::confirm_delete(&mut input.as_bytes(), &mut output).unwrap(),
                expected
            );
            assert!(String::from_utf8(output).unwrap().contains("[y/N]"));
        }
    }

    #[test]
    fn saved_environment_cloning_explicitly_copies_all_disks_and_waits() {
        let record = test_record();
        let fake = FakePve::new(&test_metadata(&record), "test");
        let vmid = super::snapshots::clone_full(
            &fake,
            &Config::default(),
            "pve01",
            9007,
            "pbox-test",
            |vmid| Ok(format!("copy {vmid}")),
            None,
        )
        .unwrap();
        assert_eq!(
            *fake.events.borrow(),
            [format!("clone:{vmid}:1"), "wait:clone".to_owned()]
        );
    }

    #[test]
    fn background_delete_returns_task_without_local_stop_or_wait() {
        let record = test_record();
        let fake = FakePve::new(&test_metadata(&record), "test");
        let task = super::queue_delete(&fake, &record).unwrap();
        assert!(!task.upid.is_empty());
        assert_eq!(*fake.events.borrow(), ["force-delete"]);
        let cli = Cli::try_parse_from(["pbox", "rm", "current", "--wait", "--yes"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Delete {
                wait: true,
                yes: true,
                ..
            }
        ));
    }

    #[test]
    fn delete_stops_running_guest_and_waits_before_destroying() {
        let record = test_record();
        let fake = FakePve::new(&test_metadata(&record), "test");
        *fake.current_state.borrow_mut() = "running".to_owned();
        super::delete_box(
            &fake,
            &record,
            super::CliStyle::for_stderr(super::ColorChoice::Never, true),
        )
        .unwrap();
        assert_eq!(
            *fake.events.borrow(),
            ["shutdown", "wait:shutdown", "delete", "wait:delete"]
        );
    }

    #[test]
    fn delete_does_not_destroy_guest_after_failed_shutdown() {
        let record = test_record();
        let fake = FakePve::new(&test_metadata(&record), "test");
        *fake.current_state.borrow_mut() = "running".to_owned();
        *fake.fail_next_task.borrow_mut() = true;
        assert!(
            super::delete_box(
                &fake,
                &record,
                super::CliStyle::for_stderr(super::ColorChoice::Never, true)
            )
            .is_err()
        );
        assert_eq!(*fake.events.borrow(), ["shutdown", "wait:shutdown"]);
    }

    #[test]
    fn delete_stopped_guest_skips_shutdown() {
        let record = test_record();
        let fake = FakePve::new(&test_metadata(&record), "test");
        super::delete_box(
            &fake,
            &record,
            super::CliStyle::for_stderr(super::ColorChoice::Never, true),
        )
        .unwrap();
        assert_eq!(*fake.events.borrow(), ["delete", "wait:delete"]);
    }

    fn test_record() -> BoxRecord {
        BoxRecord {
            id: PboxId::parse("pbx_t3yzd9y3").unwrap(),
            vmid: 9007,
            state: "running".to_owned(),
            node: "pve01".to_owned(),
            ip: None,
            ipv6: None,
            name: Some("test-box".to_owned()),
            recipes: Vec::new(),
            capabilities: Vec::new(),
        }
    }

    fn test_metadata(record: &BoxRecord) -> PboxMetadata {
        PboxMetadata::new(record.id.clone(), record.vmid).with_node(record.node.clone())
    }
    fn test_cluster_resource(vmid: u64) -> ClusterResource {
        ClusterResource {
            resource_type: "lxc".to_owned(),
            vmid: Some(vmid),
            node: Some("pve01".to_owned()),
            status: Some("stopped".to_owned()),
            name: Some(format!("pbox-{vmid}")),
            tags: None,
            uptime: None,
            mem: None,
            maxmem: None,
            disk: None,
            maxdisk: None,
            extra: serde_json::Map::new(),
        }
    }

    fn test_resolved_new() -> super::ResolvedNew {
        super::ResolvedNew {
            node: "pve01".to_owned(),
            ostemplate: "local:vztmpl/debian-13-standard_13.0-1_amd64.tar.zst".to_owned(),
            rootfs: "local:8G".to_owned(),
            net0: "name=eth0,bridge=vmbr0,ip=dhcp".to_owned(),
            name: Some("pbox-test".to_owned()),
            memory: 2048,
            swap: 128,
            cores: 4,
            unprivileged: true,
            onboot: true,
            stopped: false,
        }
    }

    #[test]
    fn setup_discovers_pve_nodes_storages_and_bridges() {
        let metadata = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007);
        let fake = FakePve::new(&metadata, "user note");
        let discovery = super::discover_setup_nodes(&fake).unwrap();
        let resources = super::discover_setup_resources(&fake, &["pve01".to_owned()]).unwrap();

        assert_eq!(discovery.nodes[0].value, "pve01");
        assert_eq!(resources.rootfs_storages[0].value, "local");
        assert_eq!(resources.template_storages[0].value, "local");
        assert_eq!(resources.bridges[0].value, "vmbr0");
        assert!(resources.bridges[0].description.contains("192.0.2.1/24"));
    }

    #[test]
    fn setup_choice_default_keeps_explicit_auto() {
        let choices = vec![
            SetupChoice {
                value: "pve01".to_owned(),
                description: "online".to_owned(),
            },
            SetupChoice {
                value: "pve02".to_owned(),
                description: "online".to_owned(),
            },
        ];

        assert_eq!(setup_choice_default("auto", &choices, true), "auto");
        assert_eq!(setup_choice_default("pve02", &choices, true), "pve02");
        assert_eq!(setup_choice_default("missing", &choices, true), "auto");
        assert_eq!(setup_choice_default("auto", &choices, false), "pve01");
    }

    #[test]
    fn task_failure_includes_safe_pve_log() {
        let metadata = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007);
        let fake = FakePve::new(&metadata, "user note");
        fake.set_task_log(vec![PveTaskLog {
            n: 0,
            t: "ERROR: no space\u{1b}[31m".to_owned(),
        }]);

        let error = super::task_error_with_log(
            &fake,
            "pve01",
            "UPID:pve01:1:2:3:create",
            "PVE task failed".to_owned(),
        );
        let rendered = format!("{error:#}");

        assert!(rendered.contains("PVE task log:"));
        assert!(rendered.contains("ERROR: no space"));
        assert!(!rendered.contains('\u{1b}'));
    }
    #[test]
    fn delete_command_accepts_rm_alias_and_current() {
        let cli = Cli::try_parse_from(["pbox", "rm", "current", "--yes"]).unwrap();
        let Command::Delete { id, yes, .. } = cli.command else {
            panic!("expected delete command");
        };

        assert_eq!(id, "current");
        assert!(yes);
    }

    #[test]
    fn current_box_reference_resolves_the_only_box() {
        let record = test_record();
        let metadata = test_metadata(&record);
        let fake = FakePve::new(&metadata, "user note");
        fake.resources
            .borrow_mut()
            .push(test_cluster_resource(record.vmid));

        let resolved = super::find_box_reference(&fake, "current").unwrap();

        assert_eq!(resolved.id, record.id);
        assert_eq!(resolved.vmid, record.vmid);
    }

    #[test]
    fn coloured_table_cells_pad_before_ansi_codes() {
        let style = CliStyle::from_enabled(true);
        let cell = super::format_box_cell(style, "pve", 16, ANSI_CYAN);

        assert_eq!(cell, format!("{ANSI_CYAN}{:<16}{ANSI_RESET}", "pve"));
    }

    #[test]
    fn new_command_accepts_zero_arguments() {
        let cli = Cli::try_parse_from(["pbox", "new"]).unwrap();
        let Command::New(command) = cli.command else {
            panic!("expected new command");
        };
        assert!(command.node.is_none());
        assert!(command.image.is_none());
        assert!(command.ostemplate.is_none());
        assert!(command.rootfs.is_none());
        assert!(command.net0.is_none());
    }

    #[test]
    fn new_resolution_uses_cluster_and_config_defaults() {
        let metadata = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007);
        let fake = FakePve::new(&metadata, "user note");
        let resolved = resolve_new_command(
            &fake,
            &Config::default(),
            &NewCommand {
                snapshot: None,
                node: None,
                image: None,
                ostemplate: None,
                rootfs: None,
                net0: None,
                name: None,
                memory: None,
                swap: None,
                cores: None,
                stopped: false,
            },
        )
        .unwrap();

        assert_eq!(resolved.node, "pve01");
        assert_eq!(
            resolved.ostemplate,
            "local:vztmpl/debian-13-standard_13.0-1_amd64.tar.zst"
        );
        assert_eq!(resolved.rootfs, "local:8");
        assert_eq!(resolved.net0, "name=eth0,bridge=vmbr0,ip=dhcp");
        assert_eq!(resolved.memory, 1024);
        assert_eq!(resolved.swap, 256);
        assert_eq!(resolved.cores, 2);
        assert!(resolved.unprivileged);
    }

    #[test]
    fn directory_storage_uses_zero_rootfs_size() {
        let metadata = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007);
        let fake = FakePve::new(&metadata, "user note");
        fake.storages.borrow_mut()[0]
            .extra
            .insert("type".to_owned(), serde_json::json!("dir"));

        let resolved = resolve_new_command(
            &fake,
            &Config::default(),
            &NewCommand {
                snapshot: None,
                node: None,
                image: None,
                ostemplate: None,
                rootfs: None,
                net0: None,
                name: None,
                memory: None,
                swap: None,
                cores: None,
                stopped: false,
            },
        )
        .unwrap();

        assert_eq!(resolved.rootfs, "local:0");
    }
    #[test]
    fn image_pull_node_selection_requires_template_storage() {
        let metadata = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007);
        let fake = FakePve::new(&metadata, "user note");
        fake.storages.borrow_mut()[0].content = Some("rootdir".to_owned());

        let error =
            resolve_pve_node_with_storage(&fake, "auto", None, "local", "vztmpl", "OCI templates")
                .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("no online PVE node has active storage")
        );
    }
    #[test]
    fn new_resolution_rejects_nodes_without_required_storage() {
        let metadata = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007);
        let fake = FakePve::new(&metadata, "user note");
        fake.storages.borrow_mut()[0].content = Some("rootdir".to_owned());
        let error = resolve_new_command(
            &fake,
            &Config::default(),
            &NewCommand {
                snapshot: None,
                node: None,
                image: None,
                ostemplate: None,
                rootfs: None,
                net0: None,
                name: None,
                memory: None,
                swap: None,
                cores: None,
                stopped: false,
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("no online PVE node has the configured storage")
        );
    }
    #[test]
    fn template_matching_accepts_all_pve_template_compressions() {
        let metadata = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007);
        let fake = FakePve::new(&metadata, "user note");
        let mut content = fake.storage_content.borrow()[0].clone();

        for extension in ["tar", "tar.gz", "tar.xz", "tar.zst", "tar.bz2"] {
            content.volid = format!("local:vztmpl/debian-13-standard_13.0-1_amd64.{extension}");
            assert!(template_matches(&content, "debian-13"));
        }
    }

    #[test]
    fn template_matching_requires_an_exact_pve_image_alias() {
        let metadata = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007);
        let fake = FakePve::new(&metadata, "user note");
        let content = fake.storage_content.borrow()[0].clone();

        assert!(template_matches(&content, "debian-13"));

        let mut untyped = content.clone();
        untyped.content = None;
        assert!(!template_matches(&untyped, "debian-13"));

        let mut unrelated = content;
        unrelated.volid = "local:vztmpl/debian-13-backdoor_1.0_amd64.tar.zst".to_owned();
        assert!(!template_matches(&unrelated, "debian-13"));
        let mut bare = unrelated.clone();
        bare.volid = "local:vztmpl/debian-13.tar.zst".to_owned();
        assert!(!template_matches(&bare, "debian-13"));
    }

    #[test]
    fn new_image_falls_back_to_oci_when_pve_template_is_missing() {
        let metadata = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007);
        let fake = FakePve::new(&metadata, "user note");
        fake.storage_content.borrow_mut().clear();

        let (node, template) = super::prepare_new_template_with_progress(
            &fake,
            &Config::default(),
            &NewCommand {
                snapshot: None,
                node: None,
                image: Some("debian-13".to_owned()),
                ostemplate: None,
                rootfs: None,
                net0: None,
                name: None,
                memory: None,
                swap: None,
                cores: None,
                stopped: false,
            },
            None,
        )
        .unwrap();

        assert_eq!(node.as_deref(), Some("pve01"));
        assert!(
            template
                .as_deref()
                .is_some_and(|value| value.starts_with("local:vztmpl/pbox-oci-"))
        );
        assert!(
            fake.events
                .borrow()
                .iter()
                .any(|event| event.contains("oci-pull:pve01:local:docker.io/library/debian:13"))
        );
    }

    #[test]
    fn local_oci_template_upload_uses_pve_upload_task() {
        let metadata = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007);
        let fake = FakePve::new(&metadata, "user note");
        let archive =
            std::env::temp_dir().join(format!("pbox-upload-test-{}.tar.zst", std::process::id()));
        fs::write(&archive, b"test archive").unwrap();

        let template = crate::images::upload_local_oci_template(
            &fake,
            "pve01",
            "local",
            "docker.io/library/debian:13",
            "pbox-oci-test",
            &archive,
        )
        .unwrap();
        let _ = fs::remove_file(&archive);

        assert_eq!(template.volume, "local:vztmpl/pbox-oci-test.tar.zst");
        assert_eq!(template.task.unwrap().upid, "oci-upload");
        assert!(
            fake.events
                .borrow()
                .iter()
                .any(|event| event == "oci-upload:pve01:local:pbox-oci-test.tar.zst")
        );
    }

    #[test]
    fn new_resolution_preserves_explicit_overrides() {
        let metadata = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007);
        let fake = FakePve::new(&metadata, "user note");

        let resolved = resolve_new_command(
            &fake,
            &Config::default(),
            &NewCommand {
                snapshot: None,
                node: Some("pve01".to_owned()),
                image: None,
                ostemplate: Some("local:vztmpl/custom.tar.zst".to_owned()),
                rootfs: Some("local-zfs:16G".to_owned()),
                net0: Some("name=eth0,bridge=vmbr9".to_owned()),
                name: Some("custom-name".to_owned()),
                memory: Some(2048),
                swap: Some(0),
                cores: Some(4),
                stopped: true,
            },
        )
        .unwrap();

        assert_eq!(resolved.node, "pve01");
        assert_eq!(resolved.ostemplate, "local:vztmpl/custom.tar.zst");
        assert_eq!(resolved.rootfs, "local-zfs:16");
        assert_eq!(resolved.net0, "name=eth0,bridge=vmbr9");
        assert_eq!(resolved.name.as_deref(), Some("custom-name"));
        assert_eq!(resolved.memory, 2048);
        assert_eq!(resolved.swap, 0);
        assert_eq!(resolved.cores, 4);
        assert!(resolved.stopped);
    }
    #[test]
    fn new_resolution_rejects_ambiguous_image_alias() {
        let metadata = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007);
        let fake = FakePve::new(&metadata, "user note");
        let mut second = fake.storage_content.borrow()[0].clone();
        second.volid = "local:vztmpl/debian-13-standard_13.0-2_amd64.tar.zst".to_owned();
        fake.storage_content.borrow_mut().push(second);

        let error = resolve_new_command(
            &fake,
            &Config::default(),
            &NewCommand {
                snapshot: None,
                node: None,
                image: None,
                ostemplate: None,
                rootfs: None,
                net0: None,
                name: None,
                memory: None,
                swap: None,
                cores: None,
                stopped: false,
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("matches multiple PVE templates"));
    }

    #[test]
    fn new_resolution_rejects_zero_memory() {
        let metadata = PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007);
        let fake = FakePve::new(&metadata, "user note");
        let error = resolve_new_command(
            &fake,
            &Config::default(),
            &NewCommand {
                snapshot: None,
                node: None,
                image: None,
                ostemplate: None,
                rootfs: None,
                net0: None,
                name: None,
                memory: Some(0),
                swap: None,
                cores: None,
                stopped: false,
            },
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("--memory must be greater than zero")
        );
    }
    #[test]
    fn create_lxc_allocates_gap_and_propagates_resolved_values() {
        let fake = FakePve::new(
            &PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007),
            "user note",
        );
        *fake.resources.borrow_mut() = vec![
            test_cluster_resource(9000),
            test_cluster_resource(9001),
            test_cluster_resource(9003),
        ];
        let key = BootstrapKey::generate("pbx_create_success").unwrap();
        let id = PboxId::parse("pbx_t3yzd9y3").unwrap();
        let resolved = test_resolved_new();

        let (vmid, task) =
            create_lxc_with_retry(&fake, &Config::default(), &resolved, &id, "pbox-test", &key)
                .unwrap();

        assert_eq!(vmid, 9002);
        assert_eq!(task.upid, "create");
        {
            let created = fake.created.borrow();
            assert_eq!(created.len(), 1);
            let (node, created_vmid, request) = &created[0];
            assert_eq!(node, "pve01");
            assert_eq!(*created_vmid, 9002);
            assert_eq!(
                request.ostemplate.as_deref(),
                Some(resolved.ostemplate.as_str())
            );
            assert_eq!(request.rootfs.as_deref(), Some(resolved.rootfs.as_str()));
            assert_eq!(request.net0.as_deref(), Some(resolved.net0.as_str()));
            assert_eq!(request.memory, Some(2048));
            assert_eq!(request.swap, Some(128));
            assert_eq!(request.cores, Some(4));
            assert_eq!(request.unprivileged, Some(true));
            assert_eq!(request.onboot, Some(true));
            assert_eq!(request.start, Some(true));
            let metadata = parse_metadata(request.description.as_deref().unwrap())
                .unwrap()
                .unwrap();
            assert_eq!(metadata.id, id);
            assert_eq!(metadata.vmid, 9002);
            assert_eq!(metadata.node.as_deref(), Some("pve01"));
        }
        key.cleanup().unwrap();
    }

    #[test]
    fn create_lxc_retries_after_a_vmid_conflict() {
        let fake = FakePve::new(
            &PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007),
            "user note",
        );
        *fake.resources.borrow_mut() = vec![
            test_cluster_resource(9000),
            test_cluster_resource(9001),
            test_cluster_resource(9003),
        ];
        *fake.create_conflicts.borrow_mut() = 1;
        let key = BootstrapKey::generate("pbx_create_conflict").unwrap();
        let id = PboxId::parse("pbx_t3yzd9y3").unwrap();

        let (vmid, _) = create_lxc_with_retry(
            &fake,
            &Config::default(),
            &test_resolved_new(),
            &id,
            "pbox-test",
            &key,
        )
        .unwrap();

        assert_eq!(vmid, 9004);
        let created = fake.created.borrow();
        assert_eq!(
            created.iter().map(|entry| entry.1).collect::<Vec<_>>(),
            [9002, 9004]
        );
        key.cleanup().unwrap();
    }

    #[test]
    fn snapshot_lifecycle_uses_pve_tasks_and_authoritative_state() {
        let record = test_record();
        let metadata = test_metadata(&record);
        let fake = FakePve::new(&metadata, "user note");

        create_box_snapshot(&fake, &record, "checkpoint", Some("before change")).unwrap();
        let snapshots = fake.list_lxc_snapshots(&record.node, record.vmid).unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].name, "checkpoint");
        assert_eq!(snapshots[0].description.as_deref(), Some("before change"));

        rollback_box_snapshot(&fake, &record, "checkpoint", true).unwrap();
        delete_box_snapshot(&fake, &record, "checkpoint").unwrap();
        assert!(
            fake.list_lxc_snapshots(&record.node, record.vmid)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            &*fake.events.borrow(),
            &[
                "create:checkpoint".to_owned(),
                "wait:snapshot-create".to_owned(),
                "rollback:checkpoint:true".to_owned(),
                "wait:snapshot-rollback".to_owned(),
                "delete:checkpoint".to_owned(),
                "wait:snapshot-delete".to_owned(),
            ]
        );
    }

    #[test]
    fn snapshot_inputs_reject_nul_without_contacting_pve() {
        assert!(validate_snapshot_arguments("checkpoint\0", None).is_err());
        assert!(validate_snapshot_arguments("checkpoint", Some("bad\0description")).is_err());
        assert!(validate_snapshot_arguments("checkpoint", Some("safe description")).is_ok());
    }

    #[test]
    fn terminal_text_replaces_control_sequences() {
        assert_eq!(
            safe_terminal_text("pve error\u{1b}[31m\nnext"),
            "pve error [31m next"
        );
    }

    #[test]
    fn snapshot_times_format_as_rfc3339_or_dash() {
        assert_eq!(format_snapshot_time(None), "-");
        assert_eq!(
            format_snapshot_time(Some(1_724_520_000)),
            "2024-08-24T17:20:00Z"
        );
    }
    #[test]
    fn provenance_failure_preserves_snapshot_for_manual_recovery() {
        let record = test_record();
        let metadata = test_metadata(&record);
        let fake = FakePve::new(&metadata, "user note");
        fake.fail_updates();
        let run = AnsibleRun {
            recipe: "desktop/xfce".to_owned(),
            box_id: record.id.to_string(),
            repository: "https://example.test/recipes.git".to_owned(),
            revision: "abc123".to_owned(),
        };
        let snapshot = RecipeSnapshot {
            name: "pbox-recipe-test".to_owned(),
        };

        let error = finish_recipe_success(
            &fake,
            &record,
            &run,
            &["desktop".to_owned()],
            Some(&snapshot),
            None,
        )
        .unwrap_err();

        assert!(format!("{error:#}").contains("preserved for manual recovery"));
        assert_eq!(
            &*fake.events.borrow(),
            &[
                "update".to_owned(),
                "update".to_owned(),
                "update".to_owned()
            ]
        );
    }

    #[test]
    fn failed_snapshot_request_attempts_cleanup_before_reporting() {
        let record = test_record();
        let metadata = test_metadata(&record);
        let fake = FakePve::new(&metadata, "user note");
        fake.fail_create();
        let run = AnsibleRun {
            recipe: "desktop/xfce".to_owned(),
            box_id: record.id.to_string(),
            repository: "https://example.test/recipes.git".to_owned(),
            revision: "abc123".to_owned(),
        };

        let error = create_recipe_snapshot(&fake, &record, &Config::default(), &run).unwrap_err();

        assert!(format!("{error:#}").contains("create recipe snapshot"));
        let events = fake.events.borrow();
        assert_eq!(events.len(), 3);
        assert!(events[0].starts_with("create:pbox-"));
        let snapshot_name = events[0].trim_start_matches("create:");
        assert_eq!(events[1], format!("delete:{snapshot_name}"));
        assert_eq!(events[2], "wait:snapshot-delete");
    }

    #[test]
    fn failed_snapshot_task_attempts_cleanup_before_reporting() {
        let record = test_record();
        let metadata = test_metadata(&record);
        let fake = FakePve::new(&metadata, "user note");
        fake.fail_next_task();
        let run = AnsibleRun {
            recipe: "desktop/xfce".to_owned(),
            box_id: record.id.to_string(),
            repository: "https://example.test/recipes.git".to_owned(),
            revision: "abc123".to_owned(),
        };

        let error = create_recipe_snapshot(&fake, &record, &Config::default(), &run).unwrap_err();

        assert!(format!("{error:#}").contains("PVE task failed"));
        let events = fake.events.borrow();
        assert_eq!(events.len(), 4);
        assert!(events[0].starts_with("create:pbox-"));
        let snapshot_name = events[0].trim_start_matches("create:");
        assert_eq!(events[2], format!("delete:{snapshot_name}"));
        assert_eq!(
            &events[1..],
            [
                "wait:snapshot-create".to_owned(),
                format!("delete:{snapshot_name}"),
                "wait:snapshot-delete".to_owned(),
            ]
        );
    }

    #[test]
    fn snapshot_policy_controls_creation_and_cleanup() {
        let record = test_record();
        let metadata = test_metadata(&record);
        let fake = FakePve::new(&metadata, "user note");
        let run = AnsibleRun {
            recipe: "desktop/xfce".to_owned(),
            box_id: record.id.to_string(),
            repository: "https://example.test/recipes.git".to_owned(),
            revision: "abc123".to_owned(),
        };
        let mut config = Config::default();
        config.recipes.snapshot_before_apply = "never".to_owned();
        assert!(
            create_recipe_snapshot(&fake, &record, &config, &run)
                .unwrap()
                .is_none()
        );
        assert!(fake.events.borrow().is_empty());

        config.recipes.snapshot_before_apply = "always".to_owned();
        let snapshot = create_recipe_snapshot(&fake, &record, &config, &run)
            .unwrap()
            .expect("snapshot");
        assert!(snapshot.name.starts_with("pbox-"));
        delete_recipe_snapshot(&fake, &record, &snapshot).unwrap();
        assert_eq!(
            &*fake.events.borrow(),
            &[
                format!("create:{}", snapshot.name),
                "wait:snapshot-create".to_owned(),
                format!("delete:{}", snapshot.name),
                "wait:snapshot-delete".to_owned(),
            ]
        );
    }

    #[test]
    fn successful_provenance_preserves_user_text_and_records_capabilities() {
        let record = test_record();
        let metadata = test_metadata(&record);
        let fake = FakePve::new(&metadata, "user note");
        let run = AnsibleRun {
            recipe: "desktop/xfce".to_owned(),
            box_id: record.id.to_string(),
            repository: "https://example.test/recipes.git".to_owned(),
            revision: "abc123".to_owned(),
        };

        record_recipe_provenance(&fake, &record, &run, &["desktop".to_owned()], "success").unwrap();

        let description = fake.config.borrow().description.clone().unwrap();
        assert!(description.starts_with("user note"));
        let metadata = parse_metadata(&description).unwrap().unwrap();
        assert_eq!(metadata.capabilities, vec!["desktop".to_owned()]);
        assert_eq!(metadata.recipes.len(), 1);
        assert_eq!(metadata.recipes[0].result.as_deref(), Some("success"));
    }

    #[test]
    fn failed_recipe_rolls_back_deletes_snapshot_and_records_failure() {
        let record = test_record();
        let metadata = test_metadata(&record);
        let fake = FakePve::new(&metadata, "user note");
        let run = AnsibleRun {
            recipe: "desktop/xfce".to_owned(),
            box_id: record.id.to_string(),
            repository: "https://example.test/recipes.git".to_owned(),
            revision: "abc123".to_owned(),
        };
        let snapshot = RecipeSnapshot {
            name: "pbox-recipe-test".to_owned(),
        };

        let error = finish_recipe_failure(
            &fake,
            &record,
            &run,
            &["desktop".to_owned()],
            anyhow!("recipe failed"),
            Some(&snapshot),
            true,
        )
        .unwrap_err();

        assert!(error.to_string().contains("recipe failed"));
        assert_eq!(
            &*fake.events.borrow(),
            &[
                "rollback:pbox-recipe-test:true".to_owned(),
                "wait:snapshot-rollback".to_owned(),
                "delete:pbox-recipe-test".to_owned(),
                "wait:snapshot-delete".to_owned(),
                "update".to_owned(),
            ]
        );
        let description = fake.config.borrow().description.clone().unwrap();
        let metadata = parse_metadata(&description).unwrap().unwrap();
        assert!(metadata.capabilities.is_empty());
        assert_eq!(metadata.recipes[0].result.as_deref(), Some("failed"));
    }

    #[test]
    fn failed_recipe_without_snapshot_reports_missing_recovery() {
        let record = test_record();
        let metadata = test_metadata(&record);
        let fake = FakePve::new(&metadata, "user note");
        let run = AnsibleRun {
            recipe: "docker".to_owned(),
            box_id: record.id.to_string(),
            repository: "https://example.test/recipes.git".to_owned(),
            revision: "abc123".to_owned(),
        };

        let error = finish_recipe_failure(
            &fake,
            &record,
            &run,
            &[],
            anyhow!("recipe failed"),
            None,
            true,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("rollback requested but no recipe snapshot was available")
        );
        assert_eq!(&*fake.events.borrow(), &["update".to_owned()]);
    }

    #[test]
    fn failed_recipe_without_rollback_preserves_snapshot_for_recovery() {
        let record = test_record();
        let metadata = test_metadata(&record);
        let fake = FakePve::new(&metadata, "user note");
        let run = AnsibleRun {
            recipe: "docker".to_owned(),
            box_id: record.id.to_string(),
            repository: "https://example.test/recipes.git".to_owned(),
            revision: "abc123".to_owned(),
        };
        let snapshot = RecipeSnapshot {
            name: "pbox-recipe-test".to_owned(),
        };

        let error = finish_recipe_failure(
            &fake,
            &record,
            &run,
            &[],
            anyhow!("recipe failed"),
            Some(&snapshot),
            false,
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("recipe snapshot pbox-recipe-test was preserved")
        );
        assert_eq!(&*fake.events.borrow(), &["update".to_owned()]);
    }

    #[test]
    fn colour_is_disabled_for_json_and_no_color() {
        assert!(!ColorMode::Always.enabled(true, false, true));
        assert!(!ColorMode::Auto.enabled(true, true, false));
    }

    #[test]
    fn setup_style_sanitises_and_paints_text() {
        let plain = CliStyle::from_enabled(false);
        let coloured = CliStyle::from_enabled(true);

        assert_eq!(plain.paint(ANSI_CYAN, "unsafe\ntext"), "unsafe text");
        assert_eq!(
            coloured.paint(ANSI_CYAN, "unsafe\ntext"),
            "\x1b[36munsafe text\x1b[0m"
        );
    }
    #[test]
    fn forward_arguments_reject_zero_ports_and_nul_values() {
        let valid = ForwardCommand {
            id: "pbx_t3yzd9y3".to_owned(),
            local_port: 3000,
            listen: "127.0.0.1".to_owned(),
            endpoint: Some("https://127.0.0.1:7443".to_owned()),
            remote_host: "127.0.0.1".to_owned(),
            remote_port: None,
        };
        assert!(validate_forward_arguments(&valid).is_ok());

        let mut zero_local = valid;
        zero_local.local_port = 0;
        assert!(validate_forward_arguments(&zero_local).is_err());

        let mut nul_host = zero_local;
        nul_host.local_port = 3000;
        nul_host.remote_host.push('\0');
        assert!(validate_forward_arguments(&nul_host).is_err());
    }

    #[test]
    fn parse_env_entry_splits_on_first_equals() {
        assert_eq!(
            parse_env_entry("GREETING=hello=world").unwrap(),
            ("GREETING".to_owned(), "hello=world".to_owned())
        );
    }

    #[test]
    fn parse_env_entry_rejects_invalid_entries_without_echoing_input() {
        let error = parse_env_entry("TOP_SECRET_VALUE").unwrap_err().to_string();
        assert!(!error.contains("TOP_SECRET_VALUE"));
        assert!(error.contains("KEY=VALUE"));
        assert!(parse_env_entry("=missing-name").is_err());
        assert!(parse_env_entry("BAD\0VALUE=x").is_err());
    }

    #[test]
    fn parse_remote_path_accepts_pbox_paths_and_local_paths() {
        assert!(parse_remote_path("/tmp/file").unwrap().is_none());
        let remote = parse_remote_path("pbx_t3yzd9y3:/var/tmp/file")
            .unwrap()
            .expect("remote path");
        assert_eq!(remote.box_id, "pbx_t3yzd9y3");
        assert_eq!(remote.path, "/var/tmp/file");
    }

    #[test]
    fn parse_remote_path_rejects_invalid_remote_paths() {
        assert!(parse_remote_path("pbx_t3yzd9y3:").is_err());
        assert!(parse_remote_path("not-a-box:/tmp/file").is_err());
        assert!(parse_remote_path("pbx_t3yzd9y3:/tmp\0file").is_err());
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

    #[test]
    fn recipe_sync_ttl_parses_units() {
        assert_eq!(
            parse_recipe_sync_ttl("15m").unwrap(),
            Duration::from_secs(900)
        );
        assert_eq!(
            parse_recipe_sync_ttl("2h").unwrap(),
            Duration::from_secs(7_200)
        );
        assert_eq!(
            parse_recipe_sync_ttl("3d").unwrap(),
            Duration::from_secs(259_200)
        );
        assert_eq!(
            parse_recipe_sync_ttl("30").unwrap(),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn recipe_sync_ttl_rejects_invalid_values() {
        assert!(parse_recipe_sync_ttl("").is_err());
        assert!(parse_recipe_sync_ttl("15x").is_err());
        assert!(parse_recipe_sync_ttl("xm").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn download_rejects_existing_symlinks() {
        use std::os::unix::fs::symlink;

        let suffix = format!("pbox-download-test-{}", std::process::id());
        let target = std::env::temp_dir().join(format!("{suffix}-target"));
        let link = std::env::temp_dir().join(format!("{suffix}-link"));
        fs::write(&target, b"original").unwrap();
        symlink(&target, &link).unwrap();

        let result = write_download(&link, b"replacement".to_vec());

        assert!(result.is_err());
        assert_eq!(fs::read(&target).unwrap(), b"original");
        fs::remove_file(&link).unwrap();
        fs::remove_file(&target).unwrap();
    }
    #[test]
    fn ssh_advertises_a_portable_terminal_and_preserves_overrides() {
        let env = super::ssh_environment(&[], Some("truecolor".to_owned())).unwrap();
        assert!(env.contains(&("TERM".to_owned(), "xterm-256color".to_owned())));
        assert!(env.contains(&("COLORTERM".to_owned(), "truecolor".to_owned())));
        let env = super::ssh_environment(
            &["TERM=vt100".to_owned(), "COLORTERM=custom".to_owned()],
            Some("truecolor".to_owned()),
        )
        .unwrap();
        assert_eq!(
            env,
            [
                ("TERM".to_owned(), "vt100".to_owned()),
                ("COLORTERM".to_owned(), "custom".to_owned())
            ]
        );
        let env = super::ssh_environment(&[], Some("unknown".to_owned())).unwrap();
        assert!(!env.iter().any(|(key, _)| key == "COLORTERM"));
    }

    #[test]
    fn ssh_defaults_to_a_login_shell() {
        assert_eq!(
            ssh_command_argv(&[]),
            vec!["/bin/sh".to_owned(), "-c".to_owned(),
            r#"shell=$(getent passwd "$(id -u)" 2>/dev/null | cut -d: -f7); exec "${shell:-/bin/sh}" -il"#.to_owned()]
        );
    }

    #[test]
    fn ssh_rejects_nul_bytes_in_command_arguments() {
        let command = SshCommand {
            id: "pbx_t3yzd9y3".to_owned(),
            endpoint: Some("https://127.0.0.1:7443".to_owned()),
            cwd: "/home/pbox".to_owned(),
            user: "pbox".to_owned(),
            env: Vec::new(),
            argv: vec!["/bin/sh\0".to_owned()],
        };

        let error = validate_ssh_arguments(&command).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("command arguments cannot contain NUL bytes")
        );
    }
    #[test]
    fn setup_alias_accepts_onboard_and_skip_verify() {
        let cli = Cli::try_parse_from(["pbox", "onboard", "--skip-verify"]).unwrap();

        assert!(matches!(
            cli.command,
            Command::Setup(SetupCommand { skip_verify: true })
        ));
    }

    #[test]
    fn setup_answers_update_required_configuration_without_exposing_secrets() {
        let mut config = Config::default();
        let answers = SetupAnswers {
            pve_url: "https://pve.example".to_owned(),
            token_id: "user@pam!pbox".to_owned(),
            token_secret: Some("secret-value".to_owned()),
            pve_node: "pve01".to_owned(),
            pve_storage: "local-zfs".to_owned(),
            pve_template_storage: "local".to_owned(),
            pve_bridge: "vmbr0".to_owned(),
            tls_insecure: false,
            vmid_pattern: "95xx".to_owned(),
            agent_port: "7444".to_owned(),
            recipes_repository: "https://example.test/recipes.git".to_owned(),
            recipes_reference: "main".to_owned(),
        };

        apply_setup_values(&mut config, &answers).unwrap();

        assert_eq!(config.pve.url.as_deref(), Some("https://pve.example"));
        assert_eq!(config.pve.token_id.as_deref(), Some("user@pam!pbox"));
        assert_eq!(
            config
                .pve
                .token_secret
                .as_ref()
                .map(|secret| secret.expose()),
            Some("secret-value")
        );
        assert_eq!(config.pve.node, "pve01");
        assert_eq!(config.pve.storage, "local-zfs");
        assert_eq!(config.pve.template_storage, "local");
        assert_eq!(config.pve.bridge, "vmbr0");
        assert_eq!(config.vmid_pattern.to_string(), "95xx");
        assert_eq!(config.agent.port, 7444);
        let redacted = serde_json::to_string(&config.redacted()).unwrap();
        assert!(!redacted.contains("secret-value"));
    }
    #[test]
    fn setup_keeps_existing_secret_when_prompt_is_blank() {
        let mut config = Config::default();
        config
            .set_value("pve.token_secret", "existing-secret")
            .unwrap();
        let answers = SetupAnswers {
            pve_url: "https://pve.example".to_owned(),
            token_id: "user@pam!pbox".to_owned(),
            token_secret: None,
            pve_node: "auto".to_owned(),
            pve_storage: "auto".to_owned(),
            pve_template_storage: "local".to_owned(),
            pve_bridge: "vmbr0".to_owned(),
            tls_insecure: false,
            vmid_pattern: "9xxx".to_owned(),
            agent_port: "7443".to_owned(),
            recipes_repository: Config::default().recipes.repository,
            recipes_reference: "main".to_owned(),
        };

        apply_setup_values(&mut config, &answers).unwrap();

        assert_eq!(
            config
                .pve
                .token_secret
                .as_ref()
                .map(|secret| secret.expose()),
            Some("existing-secret")
        );
    }

    #[test]
    fn setup_rejects_invalid_answers_without_partial_mutation() {
        let mut config = Config::default();
        config.set_value("pve.url", "https://old.example").unwrap();
        let original = config.clone();
        let answers = SetupAnswers {
            pve_url: "http://not-https.example".to_owned(),
            token_id: "user@pam!pbox".to_owned(),
            token_secret: Some("secret-value".to_owned()),
            pve_node: "pve01".to_owned(),
            pve_storage: "local".to_owned(),
            pve_template_storage: "local".to_owned(),
            pve_bridge: "vmbr0".to_owned(),
            tls_insecure: false,
            vmid_pattern: "95xx".to_owned(),
            agent_port: "7444".to_owned(),
            recipes_repository: "https://example.test/recipes.git".to_owned(),
            recipes_reference: "main".to_owned(),
        };

        assert!(apply_setup_values(&mut config, &answers).is_err());
        assert_eq!(config, original);
    }

    #[test]
    fn setup_detects_agent_identity_rotation() {
        let mut config = Config::default();
        config.set_value("pve.token_id", "old@pam!pbox").unwrap();
        config.set_value("pve.token_secret", "old-secret").unwrap();
        let unchanged = SetupAnswers {
            pve_url: "https://pve.example".to_owned(),
            token_id: "old@pam!pbox".to_owned(),
            token_secret: None,
            pve_node: "auto".to_owned(),
            pve_storage: "auto".to_owned(),
            pve_template_storage: "local".to_owned(),
            pve_bridge: "vmbr0".to_owned(),
            tls_insecure: false,
            vmid_pattern: "9xxx".to_owned(),
            agent_port: "7443".to_owned(),
            recipes_repository: Config::default().recipes.repository,
            recipes_reference: "main".to_owned(),
        };
        assert!(!agent_identity_changes(&config, &unchanged));

        let mut rotated = unchanged.clone();
        rotated.token_id = "new@pam!pbox".to_owned();
        assert!(agent_identity_changes(&config, &rotated));
        rotated.token_id = "old@pam!pbox".to_owned();
        rotated.token_secret = Some("new-secret".to_owned());
        assert!(agent_identity_changes(&config, &rotated));
    }

    #[test]
    fn setup_boolean_parser_accepts_yes_and_no() {
        assert!(parse_setup_bool("yes").unwrap());
        assert!(!parse_setup_bool(" FALSE ").unwrap());
        assert!(parse_setup_bool("maybe").is_err());
    }

    #[test]
    fn setup_output_contains_only_safe_status_fields() {
        let output = SetupOutput {
            config_path: "/tmp/pbox/config.toml".to_owned(),
            verified: true,
        };
        let json = serde_json::to_string(&output).unwrap();

        assert!(json.contains("config_path"));
        assert!(json.contains("verified"));
        assert!(!json.contains("secret"));
    }
    #[test]
    fn setup_verification_errors_use_safe_messages() {
        let message = setup_pve_error_message(&PveError::InvalidBaseUrl);

        assert_eq!(message, "the PVE URL is invalid");
        assert!(!message.contains("secret"));
    }
    #[test]
    fn discovery_commands_accept_missing_filters() {
        let cli = Cli::try_parse_from(["pbox", "checkpoint", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Checkpoint(super::CheckpointCommand {
                command: super::CheckpointSubcommand::List { id: None }
            })
        ));
        let cli = Cli::try_parse_from(["pbox", "checkpoint", "list", "current"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Checkpoint(super::CheckpointCommand {
                command: super::CheckpointSubcommand::List { id: Some(_) }
            })
        ));
        assert!(Cli::try_parse_from(["pbox", "image", "search"]).is_ok());
        assert!(Cli::try_parse_from(["pbox", "image", "tags", "debian"]).is_ok());
    }

    #[test]
    fn image_commands_parse_with_expected_defaults() {
        let cli = Cli::try_parse_from([
            "pbox",
            "image",
            "search",
            "ghcr.io/example/base",
            "--limit",
            "10",
        ])
        .unwrap();
        let Command::Image(command) = cli.command else {
            panic!("expected image command");
        };
        let super::ImageSubcommand::Search(command) = command.command else {
            panic!("expected image search command");
        };
        assert_eq!(command.query.as_deref(), Some("ghcr.io/example/base"));
        assert_eq!(command.limit, 10);
        assert_eq!(command.registry, "docker.io");

        let cli = Cli::try_parse_from(["pbox", "image", "pull", "ghcr.io/example/base"]).unwrap();
        let Command::Image(command) = cli.command else {
            panic!("expected image command");
        };
        let super::ImageSubcommand::Pull(command) = command.command else {
            panic!("expected image pull command");
        };
        assert_eq!(command.reference, "ghcr.io/example/base");
        assert!(command.storage.is_none());
    }

    #[test]
    fn oci_search_sorts_deduplicates_and_limits_tags() {
        let fake = FakePve::new(
            &PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007),
            "user note",
        );
        *fake.oci_tags.borrow_mut() = vec![
            "z".to_owned(),
            "a".to_owned(),
            "z".to_owned(),
            "m".to_owned(),
        ];
        let result =
            super::search_oci_repository(&fake, "pve01", "ghcr.io/example/base", 2).unwrap();

        assert_eq!(result.repository, "ghcr.io/example/base");
        assert_eq!(result.tags, ["a", "m"]);
        assert_eq!(
            fake.events.borrow().as_slice(),
            ["oci-tags:pve01:ghcr.io/example/base"]
        );
    }

    #[test]
    fn oci_template_pull_is_idempotent_and_rejects_digests() {
        let fake = FakePve::new(
            &PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007),
            "user note",
        );
        let first =
            super::prepare_oci_template(&fake, "pve01", "local", "ghcr.io/example/base:latest")
                .unwrap();
        assert!(first.task.is_some());
        assert_eq!(first.volume, format!("local:vztmpl/{}.tar", first.filename));
        assert!(fake
            .events
            .borrow()
            .iter()
            .any(|event| event.starts_with("oci-pull:pve01:local:ghcr.io/example/base:latest:")));

        fake.storage_content.borrow_mut().push(PveStorageContent {
            volid: first.volume.clone(),
            content: Some("vztmpl".to_owned()),
            format: Some("tar".to_owned()),
            is_base: None,
            extra: serde_json::Map::new(),
        });
        let second =
            super::prepare_oci_template(&fake, "pve01", "local", "ghcr.io/example/base:latest")
                .unwrap();
        assert!(second.task.is_none());
        assert_eq!(second.volume, first.volume);

        let error = super::prepare_oci_template(
            &fake,
            "pve01",
            "local",
            "ghcr.io/example/base@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("requires a tagged image reference")
        );
    }

    #[test]
    fn failed_pull_task_is_accepted_when_template_exists() {
        let fake = FakePve::new(
            &PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007),
            "user note",
        );
        let prepared =
            super::prepare_oci_template(&fake, "pve01", "local", "ghcr.io/example/base:latest")
                .unwrap();
        let task = prepared.task.clone().unwrap();
        fake.storage_content.borrow_mut().push(PveStorageContent {
            volid: prepared.volume.clone(),
            content: Some("vztmpl".to_owned()),
            format: Some("tar".to_owned()),
            is_base: None,
            extra: serde_json::Map::new(),
        });
        fake.fail_next_task();

        wait_for_oci_template_task(
            &fake,
            "pve01",
            "local",
            &prepared.reference,
            &prepared.volume,
            task,
        )
        .unwrap();
    }
    #[test]
    fn prepared_template_is_preserved_and_explicit_template_wins() {
        let fake = FakePve::new(
            &PboxMetadata::new(PboxId::parse("pbx_t3yzd9y3").unwrap(), 9007),
            "user note",
        );
        let command = NewCommand {
            snapshot: None,
            node: Some("pve01".to_owned()),
            image: Some("ghcr.io/example/base:latest".to_owned()),
            ostemplate: None,
            rootfs: Some("local:8G".to_owned()),
            net0: None,
            name: None,
            memory: None,
            swap: None,
            cores: None,
            stopped: false,
        };
        let resolved = super::resolve_new_command_with_template(
            &fake,
            &Config::default(),
            &command,
            Some("local:vztmpl/pbox-oci.tar"),
            Some("pve01"),
        )
        .unwrap();
        assert_eq!(resolved.ostemplate, "local:vztmpl/pbox-oci.tar");

        let mut explicit = command;
        explicit.ostemplate = Some("local:vztmpl/custom.tar".to_owned());
        let resolved = super::resolve_new_command_with_template(
            &fake,
            &Config::default(),
            &explicit,
            Some("local:vztmpl/pbox-oci.tar"),
            Some("pve01"),
        )
        .unwrap();
        assert_eq!(resolved.ostemplate, "local:vztmpl/custom.tar");
    }
}
