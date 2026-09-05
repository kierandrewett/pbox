# CLI design system

All pbox command presentation belongs in `crates/pbox-cli/src/ui.rs`.
Command handlers select a semantic component; they do not print text or add ANSI
escape sequences themselves. This applies to every command and subcommand.

## Components

| Component | Appearance | Use |
| --- | --- | --- |
| Heading | Bold cyan | Section titles, resource titles, table headers |
| Metadata | Two-space indent, dim label, plain value | Box details, paths, addresses, configuration |
| Prompt | Cyan `?`, bold label, dim default | Setup, configuration input, deletion confirmation |
| Progress | Cyan `>`, bold message | An operation in progress |
| Success | Green `ok`, bold message | Completed operation |
| Warning | Amber `!`, bold message | Destructive consequences or degraded operation |
| Error | Red `x`, bold message | Runtime failure, on stderr |
| Hint | Two-space indent, dim text | Defaults, cancellation, empty results, verbose diagnostics |
| Next command | Two-space indent, dim `$`, cyan command | A command the user can copy |
| Table | Cyan header and identifiers, padded cells | Boxes, recipes, snapshots |

Markers, labels, spacing, and defaults remain visible without colour. Do not use
colour alone to communicate state. External text passes through `safe_terminal_text`
before human display. Calculate table padding before applying colour.

## Output rules

- Results and tables use stdout. Prompts, progress, warnings, and errors use stderr.
- `--color auto` enables colour only for the relevant terminal stream.
- `--color always` forces colour. `--color never` disables it.
- `NO_COLOR` and `--json` disable colour, including command help and usage errors.
- JSON result schemas stay unchanged. Never insert a heading or success message
  into JSON stdout. `json_text` accepts already-serialised machine output only.
- Interactive SSH saves the host terminal title, sets the box name, and
  restores the title on disconnect using the terminal title stack. Guest OSC 0/1/2 title
  changes receive a `box-name · ` prefix, including across transport chunks. Terminals without title-stack support may not restore it.
- Guest output from `exec` and `ssh`, transferred files, and Ansible process output
  are data streams. Preserve their bytes; do not style or sanitise them, except
  for the interactive SSH title prefix described above.
- Creation keeps each completed step with a green `ok` and its elapsed time, then
  shows a new spinner for the active step on a capable terminal. Failed steps are
  never marked complete. Redirected output
  and `TERM=dumb` use separate phase lines. The active phase expands to show recent
  substeps and a bounded live log tail, then collapses on completion. `--verbose`
  keeps the full preparation logs.
- Image preparation names the resolved OCI reference, including its registry and tag
  or digest. The creation result repeats it in an `image` metadata row.
- Normal output describes the user's operation. Put implementation details in
  verbose diagnostics or explicit resource details.
- Confirmation uses the shared prompt component with a safe default. Deletion
  uses `[y/N]`; cancellation is a hint, not a runtime error.
- Clap owns help layout and usage errors, with palette tokens supplied by `ui.rs`.

## Command coverage

| Commands | Shared presentation |
| --- | --- |
| `setup`, `config` | Headings, metadata, prompts, hints, success, errors |
| `image search/pull` | Headings, metadata, success, verbose diagnostics |
| `new`, `repair`, `start`, `stop` | Progress, box details, success, next command |
| `rm` / `delete` | Section, metadata, warning, prompt, progress, success |
| `list`, `info`, `id` | Tables, metadata, resource titles |
| `recipe` | Tables, metadata, success, warnings |
| `snapshot` | Tables, success, errors |
| `ssh`, `exec`, `scp`, `forward` | Connection/transfer status and errors; unchanged guest data |
| All help | Shared Clap palette |

## Enforcement and validation

The CLI crate denies Clippy's `print_stdout` and `print_stderr` lints. Only `ui.rs`
allows them. Do not add local exceptions in command handlers or new modules.
`just check` runs strict Clippy across all targets. `just test` tests the workspace,
including design-system and command-output regression tests.

For presentation changes, inspect a real terminal and redirected output. Verify
coloured, plain, and JSON modes. Add new components to `ui.rs` and this document
before using them. Keep command-specific layouts in the renderer. Do not add
another style helper to a command module.
