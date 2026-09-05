use std::process::{Command, Output};

fn pbox(args: &[&str], no_color: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_pbox"));
    command
        .args(args)
        .env_remove("NO_COLOR")
        .env("TERM", "xterm-256color");
    if no_color {
        command.env("NO_COLOR", "1");
    }
    command.output().unwrap()
}

#[test]
fn help_for_every_command_uses_the_shared_palette() {
    for command in [
        "id",
        "config",
        "image",
        "setup",
        "new",
        "repair",
        "ssh",
        "exec",
        "scp",
        "forward",
        "recipe",
        "snapshot",
        "list",
        "info",
        "start",
        "stop",
        "rm",
        "delete",
        "config get",
        "config list",
        "config set",
        "config unset",
        "image search",
        "image pull",
        "recipe sync",
        "recipe list",
        "recipe search",
        "recipe info",
        "recipe apply",
        "snapshot list",
        "snapshot create",
        "snapshot rollback",
        "snapshot delete",
    ] {
        let mut args = vec!["--color=always"];
        args.extend(command.split_whitespace());
        args.push("--help");
        let output = pbox(&args, false);
        assert!(output.status.success(), "{command}: {:?}", output.stderr);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.contains("\x1b[1m\x1b[36m") || text.contains("\x1b[1;36m"),
            "{command}: {text:?}"
        );
        assert!(text.contains("Usage:"));
    }
}

#[test]
fn colour_controls_apply_to_help_results_and_errors() {
    for args in [
        vec!["id"],
        vec!["--help"],
        vec!["config", "get", "invalid.key"],
        vec!["invalid-command"],
    ] {
        let mut coloured = vec!["--color=always"];
        coloured.extend(&args);
        let output = pbox(&coloured, false);
        assert!(output.stdout.contains(&27) || output.stderr.contains(&27));
        let output = pbox(&coloured, true);
        assert!(!output.stdout.contains(&27) && !output.stderr.contains(&27));
        let mut plain = vec!["--color=never"];
        plain.extend(&args);
        let output = pbox(&plain, false);
        assert!(!output.stdout.contains(&27) && !output.stderr.contains(&27));
        let output = pbox(&args, false);
        assert!(!output.stdout.contains(&27) && !output.stderr.contains(&27));
    }
}

#[test]
fn json_is_unstyled_even_when_colour_is_forced() {
    let output = pbox(&["--color=always", "--json", "id"], false);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(value["id"].as_str().unwrap().starts_with("pbx_"));
    assert!(!output.stdout.contains(&27));
}
