use anyhow::{Context, Result, anyhow};
use clap::{Args, Parser, Subcommand, ValueEnum};
use pbox_core::{ConfigStore, PboxId, PveApi, PveClient, PveClientConfig, parse_metadata};
use serde::Serialize;
use std::io::{self, IsTerminal};
use std::path::PathBuf;

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
    /// List pbox-managed containers discovered from PVE metadata.
    List,
    /// Show one pbox discovered from PVE metadata.
    Info { id: String },
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
    state: String,
    node: String,
    name: Option<String>,
}

#[derive(Debug, Serialize)]
struct BoxInfo {
    id: PboxId,
    state: String,
    node: String,
    name: Option<String>,
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
        Command::List => run_list(&store, cli.json, cli.color),
        Command::Info { id } => run_info(&store, &id, cli.json, cli.color),
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
    let id: PboxId = requested_id.parse().context("parse pbox id")?;
    let client = client_from_store(store)?;
    let records = discover_boxes(&client)?;
    let record = records
        .into_iter()
        .find(|record| record.id == id)
        .ok_or_else(|| anyhow!("box '{id}' was not found"))?;
    let info = BoxInfo {
        id: record.id,
        state: record.state,
        node: record.node,
        name: record.name,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&info)?);
    } else {
        let _ = color_enabled(color, json);
        println!("box");
        println!("  id:     {}", info.id);
        println!("  state:  {}", info.state);
        println!("  node:   {}", info.node);
        println!("  name:   {}", info.name.as_deref().unwrap_or("-"));
    }
    Ok(())
}

fn client_from_store(store: &ConfigStore) -> Result<PveClient> {
    let config = store
        .load(&Default::default())
        .context("load pbox configuration")?;
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
        let name = config.hostname.or(resource.name);
        records.push(BoxRecord {
            id: metadata.id,
            state: resource.status.unwrap_or_else(|| "unknown".to_owned()),
            node: node.to_owned(),
            name,
        });
    }
    records.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(records)
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
