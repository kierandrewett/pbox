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
- Keep existing JSON result schemas stable. Background deletion has an explicit
  queued receipt (`deleted: false`, `queued: true`, `upid`); `--wait` retains the
  completed-deletion result. Never insert a heading or success message
  into JSON stdout. `json_text` accepts already-serialised machine output only.
- Interactive SSH saves the host terminal title, sets the box name, and
  restores the title on disconnect using the terminal title stack. Guest OSC 0/1/2 title
  changes receive a `box-name · ` prefix, including across transport chunks. Terminals without title-stack support may not restore it.
- Guest output from `exec` and `ssh`, and transferred files, are data streams. Preserve their bytes; do not style or sanitise them, except
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
  uses `[y/N]`; cancellation is a hint, not a runtime error. Background deletion
  says `Deletion queued`, never `Deleted`, and identifies where to check its result.
  The confirmation explains the immediate stop; `--wait` uses graceful shutdown.
- Clap owns help layout and usage errors, with palette tokens supplied by `ui.rs`.
- `completions SHELL` emits an unchanged shell script on stdout, even with
  `--json` or forced colour. Generating the script needs no PVE configuration or connection. Live argument
  completion silently queries the selected PVE configuration, with a two-second
  waiting limit, and reads recipe IDs from the local recipe cache without
  contacting Git. Box IDs and unique names are interchangeable; ambiguous names
  never select a resource. Desktop session completion uses the supported session
  names and remains available without a guest query.
- Image compatibility failures name the failed requirement before PVE creation.
  Agent startup timeouts keep the box and show a shared diagnostic section on
  stderr with PVE console checks and the repair command.

## Command coverage

When a desktop launcher is absent, `desktop` reports `No desktop installed on BOX`
and shows the Ansible install command followed by the retry command on stderr.
Transport failures and broken installed launchers retain their own errors.
JSON mode leaves stdout empty on failure and emits unstyled guidance on stderr.

| Commands | Shared presentation |
| --- | --- |
| `setup`, `config`, `relay keygen/check` | Headings, metadata, prompts, hints, success, errors |
| `update` | Installer command and completion status |
| `image search/tags/pull` | Headings, metadata, success, verbose diagnostics |
| `new`, `repair`, `start`, `stop` | Progress, box details, success, next command |
| `rm` / `delete` | Section, metadata, warning, prompt, progress, success |
| `list`, `info`, `id` | Tables, metadata, resource titles |
| `recipe` | Tables, metadata, success, warnings |
| `desktop` | Session and VNC endpoint metadata; JSON receipt, then tunnel until disconnect |
| `snapshot`, `checkpoint` | Tables, success, errors; snapshot capture uses timed stages and live task logs |
| `completions` | Unchanged generated script |
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

Image search displays image names and descriptions with next-command hints; tag
listing is a separate command. Missing interactive search terms use the shared
prompt, while redirected input gets a concrete usage hint. Snapshot listing needs
no box ID and lists independent saved environments by name and psn_ ID. Source
box IDs are provenance only. Per-box PVE rollback points are called checkpoints.

Image preparation errors use a short error heading, image and creation-status
metadata, then separate `What failed` and `Next steps` sections. Diagnostic text
wraps onto aligned lines. Creation distinguishes local image preparation from
upload to a named PVE node/storage, and reports known compressed-layer, rootfs
and upload sizes without implying that cached bytes were downloaded.
Deletion confirmation includes image provenance (and snapshot provenance when
available); older boxes without recorded provenance show `Not recorded`.
`ls` and `ps` are visible aliases of `list` and share its output.
`list` shows cached agent status in `PING`; a PVE-running container with no
agent response is shown as `disconnected`. A small per-user `pboxd` process
performs the probes and is restarted automatically by the next CLI invocation.

`list` shows `deleting` when PVE reports an active destroy task or the
`destroyed` configuration lock. Agent pings are skipped for that state. Cached
agent status must not replace a fresh PVE lifecycle state such as stopped or deleting.

The box list includes IMAGE from recorded OCI provenance, with `-` for older
boxes lacking it. Columns fit their content and use two spaces between columns,
including optional IPv6. JSON includes the optional `image` field when known.

`list` reads pboxd's persistent inventory snapshot without PVE requests or pings.
The daemon refreshes independently and restarts on demand after exiting. Caches
are scoped to configuration. A missing initial cache reports background loading;
an old cache is displayed immediately with its age on human stderr. Queued local
deletions overlay the cached row immediately, before the next PVE refresh.

## Snapshot progress

Snapshot capture shows five numbered stages: prepare the source, stop it, copy
its disks, finalise the saved environment, and restore the source. The copy
stage explicitly says the source is stopped. Each completed stage retains its
elapsed time. Recovery after a failure gets its own stage; a failed copy must
never receive a success marker when recovery starts.

Snapshot capture and box creation use the same progress renderer. Terminals
show an elapsed-time spinner, substeps and three recent PVE log lines. Plain
output retains phase transitions and logs, with a heartbeat every five seconds.
Verbose output retains full logs without cursor movement. JSON stdout keeps its
existing schema and normal JSON mode does not emit human progress.

Copy percentages and transfer statistics come from PVE logs when available.
Some storage backends report totals only when finished; do not invent a
percentage or ETA. Log parsing and log retrieval failures cannot determine
whether a PVE task succeeded. The final snapshot result reports the source's
restored running/stopped state.

## Recipe progress

Recipe application shows the recipe and target, then timed preparation and apply
stages on stderr. Recipe stages reuse the box-creation display: terminals expand
to show the active Ansible task, six recent completed tasks, and a three-line
Ansible log tail, then collapse when the stage succeeds. Task completion follows
Ansible results; skipped tasks are explicitly labelled. Plain output retains
task progress, outcomes, and logs as separate lines. Only successful stages receive a completion marker. Failures show a concise reason with
a private saved log path. Successful runs remove their temporary logs. `--verbose`
streams raw Ansible output to stderr; JSON stdout retains only the result schema.
Ansible task banners, result dictionaries, source excerpts, and recaps are hidden
in the default view. Failed tasks are never marked successful.

Recipe task progress and log messages come from a versioned aggregate Ansible
callback, independently of console formatting. Raw stdout/stderr are preserved
in logs and verbose output. Unknown or malformed events are ignored; process exit
status determines success. Callback logs respect `no_log`. Modules may buffer
their output until task completion. `python3 scripts/test-ansible-progress.py`
checks identical events with different stdout callbacks, failures, skips and redaction.
