//! Live completion queries are read-only, silent and bounded independently of PVE timeouts.
use super::{
    Cli, ConfigStore, client_from_config, discover_boxes, load_config, recipes, snapshots,
};
use clap::CommandFactory;
use clap_complete::{ArgValueCompleter, CompleteEnv, CompletionCandidate};
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

pub(crate) const ENV: &str = "PBOX_COMPLETE";

pub(crate) fn dispatch() -> bool {
    if std::env::var_os(ENV).is_none_or(|value| value.is_empty() || value == "0") {
        return false;
    }
    // A failed completion must not print an error or fall through to command execution.
    let _ = CompleteEnv::with_factory(command)
        .var(ENV)
        .try_complete(std::env::args_os(), std::env::current_dir().ok().as_deref());
    true
}

fn command() -> clap::Command {
    add_completers(
        Cli::command(),
        ArgValueCompleter::new(boxes),
        ArgValueCompleter::new(saved_environments),
        ArgValueCompleter::new(recipe_ids),
    )
}

fn add_completers(
    command: clap::Command,
    boxes: ArgValueCompleter,
    saved: ArgValueCompleter,
    recipes: ArgValueCompleter,
) -> clap::Command {
    add_scoped_completers(command, boxes, saved, recipes, false)
}

fn add_scoped_completers(
    command: clap::Command,
    boxes: ArgValueCompleter,
    saved: ArgValueCompleter,
    recipes: ArgValueCompleter,
    desktop: bool,
) -> clap::Command {
    let desktop = desktop || command.get_name() == "desktop";
    let existing_session = matches!(command.get_name(), "close" | "read" | "send");
    let box_source = matches!(command.get_name(), "create" | "repair-source");
    let mut command = command.mut_args(|arg| match arg.get_id().as_str() {
        "id" | "box_id" => arg.add(boxes.clone()),
        "source" if box_source => arg.add(boxes.clone()),
        "snapshot" => arg.add(saved.clone()),
        "recipe" => arg.add(recipes.clone()),
        "session" if desktop => arg.add(ArgValueCompleter::new(desktop_sessions)),
        "session" => arg.add(ArgValueCompleter::new(terminal_sessions)),
        "name" if existing_session => arg.add(ArgValueCompleter::new(terminal_sessions)),
        "keys" => arg.add(ArgValueCompleter::new(terminal_keys)),
        _ => arg,
    });
    for child in command.get_subcommands_mut() {
        *child = add_scoped_completers(
            child.clone(),
            boxes.clone(),
            saved.clone(),
            recipes.clone(),
            desktop,
        );
    }
    command
}

fn config_path(args: impl IntoIterator<Item = OsString>) -> Option<PathBuf> {
    // Completion transport passes `-- pbox ...`; only inspect the user's words.
    let mut args = args.into_iter().skip_while(|arg| arg != "--").skip(2);
    let mut path = None;
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if arg == "--config" {
            path = args.next().map(PathBuf::from);
        } else if let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix("--config=")) {
            path = Some(PathBuf::from(value));
        }
    }
    path.or_else(|| std::env::var_os("PBOX_CONFIG_FILE").map(PathBuf::from))
}

fn boxes(current: &OsStr) -> Vec<CompletionCandidate> {
    if std::env::args_os().any(|word| word == "ssh" || word == "attach" || word == "session")
        && let Some((target, prefix)) = current.to_str().and_then(|word| word.split_once(':'))
    {
        return query_terminal_names(target.to_owned())
            .into_iter()
            .filter(|name| name.starts_with(prefix))
            .map(|name| CompletionCandidate::new(format!("{target}:{name}")))
            .collect();
    }
    query(current, false)
}

fn saved_environments(current: &OsStr) -> Vec<CompletionCandidate> {
    query(current, true)
}

fn recipe_ids(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    let store = ConfigStore::new(
        config_path(std::env::args_os()).unwrap_or_else(pbox_core::config::default_config_path),
    );
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result: anyhow::Result<Vec<(String, String)>> = (|| {
            let config = load_config(&store)?;
            let repository = recipes::RecipeRepository::new(
                Some(&config.recipes.repository),
                &config.recipes.reference,
            )?;
            Ok(repository
                .cached_catalog()?
                .recipes
                .into_iter()
                .map(|recipe| (recipe.id, recipe.metadata.description.unwrap_or_default()))
                .collect())
        })();
        let _ = sender.send(result.unwrap_or_default());
    });
    candidates(
        receiver
            .recv_timeout(Duration::from_millis(250))
            .unwrap_or_default(),
        prefix,
        false,
    )
}

fn desktop_sessions(current: &OsStr) -> Vec<CompletionCandidate> {
    candidates(
        [
            ("xfce", "Xfce"),
            ("mate", "MATE"),
            ("lxqt", "LXQt"),
            ("kde", "KDE Plasma"),
            ("gnome", "GNOME"),
            ("cinnamon", "Cinnamon"),
            ("sway", "Sway"),
            ("i3", "i3"),
        ]
        .into_iter()
        .map(|(id, description)| (id.to_owned(), description.to_owned()))
        .collect(),
        current.to_str().unwrap_or_default(),
        false,
    )
}

#[cfg(test)]
fn no_completions(_: &OsStr) -> Vec<CompletionCandidate> {
    Vec::new()
}

fn terminal_keys(current: &OsStr) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy().to_ascii_lowercase();
    [
        "Enter",
        "Tab",
        "Escape",
        "Backspace",
        "Space",
        "Up",
        "Down",
        "Left",
        "Right",
        "Home",
        "End",
        "PageUp",
        "PageDown",
        "Insert",
        "Delete",
        "Ctrl+C",
        "Ctrl+D",
        "Ctrl+L",
        "Ctrl+U",
        "Ctrl+W",
        "Shift+Tab",
        "Alt+Enter",
        "F1",
        "F2",
        "F3",
        "F4",
        "F5",
        "F6",
        "F7",
        "F8",
        "F9",
        "F10",
        "F11",
        "F12",
    ]
    .into_iter()
    .filter(|key| key.to_ascii_lowercase().starts_with(&prefix))
    .map(CompletionCandidate::new)
    .collect()
}

fn terminal_sessions(current: &OsStr) -> Vec<CompletionCandidate> {
    let words: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let target = words
        .iter()
        .position(|word| word == "ssh" || word == "attach")
        .and_then(|i| words.get(i + 1))
        .or_else(|| {
            words
                .iter()
                .position(|word| word == "session")
                .and_then(|i| words.get(i + 2))
        })
        .filter(|id| !id.starts_with('-'))
        .cloned();
    let Some(target) = target else {
        return Vec::new();
    };
    query_terminal_names(target)
        .into_iter()
        .filter(|name| name.starts_with(current.to_str().unwrap_or("")))
        .map(CompletionCandidate::new)
        .collect()
}

fn query_terminal_names(target: String) -> Vec<String> {
    let store = ConfigStore::new(
        config_path(std::env::args_os()).unwrap_or_else(pbox_core::config::default_config_path),
    );
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result: anyhow::Result<Vec<String>> = (|| {
            let config = load_config(&store)?;
            let (box_id, endpoint) = super::resolve_agent_endpoint(&config, &target, None)?;
            let materials = super::agent_materials(&config, &box_id)?;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    let mut client = super::relay::connect_agent(
                        &config,
                        &endpoint,
                        &box_id,
                        &materials.ca.certificate_pem,
                        &materials.client,
                    )
                    .await?;
                    Ok(client
                        .list_sessions()
                        .await?
                        .into_iter()
                        .map(|session| session.name)
                        .collect())
                })
        })();
        let _ = sender.send(result.unwrap_or_default());
    });
    receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default()
}

fn query(current: &OsStr, saved: bool) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    let store = ConfigStore::new(
        config_path(std::env::args_os()).unwrap_or_else(pbox_core::config::default_config_path),
    );
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result: anyhow::Result<Vec<(String, String)>> = (|| {
            let config = load_config(&store)?;
            let client = client_from_config(&config)?;
            if saved {
                Ok(snapshots::inventory(&client)?
                    .into_iter()
                    .map(|saved| (saved.id, saved.name))
                    .collect())
            } else {
                Ok(discover_boxes(&client)?
                    .into_iter()
                    .map(|record| (record.id.to_string(), record.name.unwrap_or_default()))
                    .collect())
            }
        })();
        let _ = sender.send(result.unwrap_or_default());
    });
    let records = receiver
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or_default();
    candidates(records, prefix, !saved)
}

fn candidates(
    records: Vec<(String, String)>,
    prefix: &str,
    allow_current: bool,
) -> Vec<CompletionCandidate> {
    let mut values = Vec::new();
    if allow_current && records.len() == 1 {
        values.push(("current".to_owned(), records[0].0.clone()));
    }
    for (id, name) in &records {
        values.push((id.clone(), name.clone()));
        if !name.is_empty() && records.iter().filter(|(_, other)| other == name).count() == 1 {
            values.push((name.clone(), id.clone()));
        }
    }
    values.sort();
    values.dedup_by(|a, b| a.0 == b.0);
    values
        .into_iter()
        .filter(|(value, _)| value.starts_with(prefix) && !value.chars().any(char::is_control))
        .map(|(value, description)| {
            CompletionCandidate::new(value)
                .help(Some(super::safe_terminal_text(&description).into()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn live_arguments_follow_command_positions_aliases_and_global_flags() {
        for words in [
            vec!["pbox", "rm", "pbox-f"],
            vec!["pbox", "--json", "ssh", "pbox-f"],
            vec!["pbox", "attach", "pbox-f"],
            vec!["pbox", "checkpoint", "create", "pbox-f"],
            vec!["pbox", "snapshot", "create", "pbox-f"],
            vec!["pbox", "recipe", "apply", "tools", "--box-id", "pbox-f"],
            vec!["pbox", "new", "--snapshot", "pbox-f"],
            vec!["pbox", "snapshot", "rm", "pbox-f"],
        ] {
            let fixture = || {
                ArgValueCompleter::new(|prefix: &OsStr| {
                    candidates(
                        vec![("pbx_12345678".into(), "pbox-fedora".into())],
                        prefix.to_str().unwrap(),
                        false,
                    )
                })
            };
            let mut command = add_completers(
                Cli::command(),
                fixture(),
                fixture(),
                ArgValueCompleter::new(|prefix: &OsStr| {
                    candidates(
                        vec![("browser/helium".into(), "Helium".into())],
                        prefix.to_str().unwrap(),
                        false,
                    )
                }),
            );
            let index = words.len() - 1;
            let result = clap_complete::engine::complete(
                &mut command,
                words.iter().map(OsString::from).collect(),
                index,
                None,
            )
            .unwrap();
            assert!(
                result
                    .iter()
                    .any(|value| value.get_value() == "pbox-fedora"),
                "{words:?}: {result:?}"
            );
        }
    }

    #[test]
    fn exact_names_resolve_and_ambiguous_names_never_select_a_box() {
        fn record(id: &str, name: &str) -> super::super::BoxRecord {
            super::super::BoxRecord {
                image: None,
                id: id.parse().unwrap(),
                vmid: 9000,
                state: "running".into(),
                node: "pve".into(),
                ip: None,
                ipv6: None,
                name: Some(name.into()),
                recipes: Vec::new(),
                capabilities: Vec::new(),
                ping: None,
            }
        }
        let selected = super::super::select_box_reference(
            vec![record("pbx_12345678", "pbox-fedora")],
            "pbox-fedora",
        )
        .unwrap();
        assert_eq!(selected.id.to_string(), "pbx_12345678");
        assert!(
            super::super::select_box_reference(
                vec![
                    record("pbx_12345678", "same"),
                    record("pbx_abcdefgh", "same")
                ],
                "same"
            )
            .unwrap_err()
            .to_string()
            .contains("ambiguous")
        );
        assert!(
            super::super::select_box_reference(
                vec![record("pbx_12345678", "pbox-fedora")],
                "pbox-f"
            )
            .is_err()
        );
    }

    #[test]
    fn box_candidates_match_names_and_ids_and_omit_ambiguous_names() {
        let records = vec![
            ("pbx_12345678".into(), "pbox-fedora".into()),
            ("pbx_abcdefgh".into(), "pbox-debian".into()),
        ];
        let found = candidates(records.clone(), "pbox-f", true);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].get_value(), "pbox-fedora");
        assert_eq!(candidates(records.clone(), "pbx_", true).len(), 2);
        assert!(candidates(records, "current", true).is_empty());
        let duplicates = vec![
            ("pbx_12345678".into(), "same".into()),
            ("pbx_abcdefgh".into(), "same".into()),
        ];
        assert!(candidates(duplicates, "same", true).is_empty());
    }

    #[test]
    fn completion_config_follows_explicit_flags_before_guest_separator() {
        let args = [
            "pbox",
            "--",
            "pbox",
            "--config=first",
            "rm",
            "--config",
            "second",
            "--",
            "--config=guest",
        ];
        assert_eq!(
            config_path(args.map(OsString::from)),
            Some(PathBuf::from("second"))
        );
    }

    #[test]
    fn recipe_ids_complete_for_recipe_commands() {
        let mut command = add_completers(
            Cli::command(),
            ArgValueCompleter::new(no_completions),
            ArgValueCompleter::new(no_completions),
            ArgValueCompleter::new(|prefix: &OsStr| {
                candidates(
                    vec![("browser/helium".into(), "Helium".into())],
                    prefix.to_str().unwrap(),
                    false,
                )
            }),
        );
        for words in [
            vec!["pbox", "recipe", "apply", "browser/h"],
            vec!["pbox", "recipe", "info", "browser/h"],
        ] {
            let index = words.len() - 1;
            let result = clap_complete::engine::complete(
                &mut command,
                words.iter().map(OsString::from).collect(),
                index,
                None,
            )
            .unwrap();
            assert!(
                result
                    .iter()
                    .any(|value| value.get_value() == "browser/helium"),
                "{words:?}: {result:?}"
            );
        }
    }
}
