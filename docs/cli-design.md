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
- Guest output from `exec`, one-off `ssh`, and transferred files are data streams.
  Preserve their bytes. Persistent interactive SSH also passes live terminal
  controls through, apart from the documented title prefixing.
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
  Box deletion confirmation explains the immediate stop; `--wait` uses graceful shutdown.
  Snapshot deletion also queues by default; `--wait` shows timed progress and task
  logs until Proxmox confirms completion. The queued receipt identifies the node
  and VMID to find in Proxmox's Tasks tab.
- Clap owns help layout and usage errors, with palette tokens supplied by `ui.rs`.
- `completions SHELL` emits an unchanged shell script on stdout, even with
  `--json` or forced colour. Generating the script needs no PVE configuration or connection. Live argument
  completion reads box and saved-environment names from pboxd's local snapshot;
  a missing snapshot starts pboxd in the background and returns immediately.
  Guest session completion has a short bounded wait and silently returns no
  session names when the guest is unavailable. Recipe IDs come from the local
  recipe cache without contacting Git. Box IDs and unique names are
  interchangeable; ambiguous names never select a resource. Desktop session
  completion uses the supported session names and remains available without a
  guest query.
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
| `new`, `repair`, `start`, `restart`, `stop` | Progress, box details, success, next command |
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

`info` reads the box's current Proxmox configuration and shows CPU cores,
allocated memory, swap, root disk size and storage. Memory uses binary units;
disk size uses binary units. A zero-size directory volume is shown as `No quota`,
and filesystem usage is labelled shared rather than attributed to that box. Missing values are explicit, and zero
swap remains zero. JSON adds a `resources` object with `cores`, `memory_mib`,
`swap_mib`, `disk_size` (original PVE notation), `storage`, `filesystem_size_bytes`
and `filesystem_used_bytes`; existing fields keep their meanings.

`list` reads pboxd's persistent inventory snapshot without PVE requests or pings.
The daemon refreshes independently and restarts on demand after exiting. Caches
are scoped to configuration. A missing initial cache reports background loading;
an old cache is displayed immediately with its age on human stderr. Queued local
deletions overlay the cached row immediately, before the next PVE refresh.

## Terminal sessions

`ssh BOX` resumes the `main` shell. `ssh BOX:NAME` and its `attach` alias select another
terminal. `--session NAME` also selects another
terminal; `session list` shows all boxes, with an optional `BOX` filter.
`session close BOX NAME` ends one terminal. Connection
output names the session and explains `Ctrl-]` (detach) and `exit` (end shell).
A new attachment moves the terminal from its old connection. Named sessions
require the agent's `terminal-sessions` capability; never silently fall back to
a disposable PTY. One-off `ssh BOX -- COMMAND` retains its previous behaviour.
SSH always allocates a guest PTY. Missing, empty or `dumb` inherited `TERM`
values become `xterm-256color`, including under harnesses. Explicit `--env TERM=...`
overrides are preserved.

The agent restores screen contents and input modes on reattachment without
replaying terminal queries or clipboard writes. Live guest output remains a data
stream. Detach and connection failures reset local keyboard, mouse and paste
modes and restore the terminal title and termios. Session lists use stdout;
Lists group sessions by box name, ID and state. Each session shows attachment
state, user, starting directory and command on one row, with columns fitted to
the terminal width. Empty boxes remain visible.
JSON returns a flat array of session records with `box_id` and `box_name`.
Stopped boxes and agents without session support have no sessions.
Unreachable boxes produce warnings on stderr, retain other results, and return
exit status 1. Human output marks the list incomplete. Closing requires confirmation or
`--yes` and reports success only after the PTY process is reaped. Completion
queries for session names are read-only and have the same two-second limit as
box completion.

`session start/read/send` work without a local TTY. Start acknowledges an actual
process spawn; duplicate names fail. Read emits decoded screen text without
headings or ANSI; JSON adds screen dimensions and zero-based cursor coordinates.
Send acknowledges accepted input, never retries it, and preserves attachments.
Text precedes named keys; paste follows the application's bracketed-paste mode.
These commands require `session-control`; an older agent gets explicit upgrade
guidance. Active sessions are not restarted to satisfy that requirement.

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

Session targets accept `BOX:NAME` for start, read, send and close, as well as
`BOX NAME`. Conflicting forms fail. Close confirmation identifies the box and
session, attachment state, user, starting directory and command. It explains
that running processes end and warns when an attachment will be disconnected.
JSON close receipts add `box_id` while retaining `name` and `closed`.

Terminal cleanup resets input modes without erasing screen contents or the
current line. It leaves the alternate screen only when guest output entered it
and has not left it. Cleanup tracks these modes by observing guest output.

A separate terminal supervisor owns persistent PTYs on systemd and pbox minimal
init guests. The agent proxies session RPCs over a private local socket. Agent
updates preserve those processes; an interrupted attachment can reconnect.
Legacy agent-owned sessions still defer updates until closed. A running supervisor
is updated only when it has no sessions. Box shutdown ends all sessions.

## Guest updates and viewing

`agent update BOX` works without a local TTY and returns an explicit `current`,
`updated`, `blocked` or `unavailable` status. JSON includes box ID, version,
blocking sessions and sessions ended by the operation. Blocked and unavailable
results have exit status 1. SSH and session control share the same update path;
listing, completion and read-only viewing never update a guest.

Legacy sessions block updates by default. `--kill-sessions` permits ending them
after showing their names, attachment state, users, directories and commands.
The confirmation warns about stopped programs and unsaved work. `--yes` requires
`--kill-sessions` and skips that prompt for automation. There is no implicit
kill or SSH prerequisite, and user input is never retried after an update.

Interactive SSH gives the guest the full physical terminal dimensions. Pass live
screen, cursor, mouse and keyboard controls through; do not draw a status bar,
rewrite scroll margins or reconstruct cursor positions. Terminal emulation is
only for snapshots and the read-only viewer, never for decorating a live stream.
Keep plain URLs and OSC 8 hyperlinks intact. Text selection and link activation
belong to the host terminal; pbox must not capture their mouse events.
Native scrollback belongs to the host terminal. No pbox-owned alternate screen
or screen clear is used on entry or exit. Show the session and detach key in the
connection message and keep the box name in the terminal title.

The supervisor retains up to 10,000 ordinary full-screen scrollback lines for
`session read --history`. This is a headless read API, not an interactive scroll
implementation. Applications using their own screen or partial scroll regions
may not leave retained lines. Older supervisors report history as unavailable.
The plain read prints retained lines before the current screen; JSON adds
`history` and `history_supported` only when requested.

`ssh` and `attach` accept `--read-only`. Read decoded snapshots without attachment,
input or resize RPCs. Ctrl+C leaves the viewer. Render inline updates and let the
host terminal keep scrollback. Viewing does not update or resize the guest.

The read-only viewer status row shows CPU, memory and root-disk percentages from
Proxmox on wide terminals. Poll every five seconds off the terminal loop, with a three-second
request timeout. Missing, failed or stale readings display `—`; narrow views keep
session controls instead. Do not repaint unchanged readings and disturb an idle
cursor. Preserve guest cursor visibility, blinking and shape, including replay.

Session listings show the foreground process chain and current directory when
the supervisor supports them. Sample `/proc` only for inspection. Keep original
`argv` and `cwd` in JSON; add `pid`, `foreground_pid`, `current_cwd` and `processes`
with parent IDs. Unknown process details fall back to the original command.

Use a dim cyan session label, dim control hints and small Unicode symbols beside
`⚙ CPU`, `🧠 RAM` and `💾 DISK` labels. Normal readings are green, >=80% amber, >=95% red;
percentages remain visible without colour. Keep the terminal's default background
and avoid reverse video or icon-font dependencies. Account for double-width
Unicode symbols when fitting the row.

## Desktop computer control

`desktop screenshot/type/send/move/click/drag/scroll` are one-shot operations.
`desktop_control_result` renders their success and screen dimensions; screenshots
also show the local PNG path. All external labels and paths are terminal-safe.
JSON emits a single receipt without human stdout. Screenshot JSON embeds base64
PNG unless an output file is requested. `screenshot --output -` is a binary data
stream, refuses a terminal, and conflicts with JSON. Input receipts acknowledge
a completed VNC round trip, not application completion. Errors never retry input.
The original `desktop BOX` viewer/tunnel command and its JSON schema are retained.
