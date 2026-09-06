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
        "completions",
        "config",
        "relay",
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
        "ls",
        "ps",
        "info",
        "start",
        "stop",
        "rm",
        "delete",
        "config get",
        "config list",
        "config set",
        "config unset",
        "relay keygen",
        "relay check",
        "image search",
        "image tags",
        "image pull",
        "recipe sync",
        "recipe list",
        "recipe search",
        "recipe info",
        "recipe apply",
        "snapshot list",
        "snapshot create",
        "snapshot info",
        "snapshot repair-source",
        "checkpoint",
        "checkpoint list",
        "checkpoint create",
        "checkpoint rollback",
        "checkpoint delete",
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
fn completions_are_offline_unstyled_scripts_for_each_shell() {
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = pbox(
            &[
                "--config",
                "/nonexistent/pbox-test.toml",
                "completions",
                shell,
            ],
            false,
        );
        assert!(output.status.success(), "{shell}: {:?}", output.stderr);
        assert!(output.stderr.is_empty());
        let script = String::from_utf8(output.stdout.clone()).unwrap();
        assert!(
            script.contains("PBOX_COMPLETE"),
            "{shell} missing live completion registration"
        );
        assert!(!output.stdout.contains(&27));
        let forced = pbox(&["--json", "--color=always", "completions", shell], false);
        assert!(forced.status.success());
        assert_eq!(output.stdout, forced.stdout);
        assert!(forced.stderr.is_empty());
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

#[test]
fn image_discovery_does_not_require_pve_configuration() {
    let output = pbox(
        &[
            "--config",
            "/nonexistent/pbox-test.toml",
            "image",
            "search",
            "debian",
            "--limit",
            "0",
        ],
        true,
    );
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(!output.status.success());
    assert!(
        error.contains("--limit must be between 1 and 100"),
        "{error}"
    );
    let output = pbox(&["image", "search", "docker.io"], true);
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(error.contains("pbox image search debian"), "{error}");
    assert!(!error.contains("skopeo"));
}
