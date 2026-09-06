# pbox

Disposable Proxmox LXC workspaces for development and LLM tools. Bring your own
OCI image, create a box through the PVE API, then open a shell with `pbox ssh`.

```sh
cargo build --locked --release --workspace
install -m 755 target/release/pbox target/release/pbox-agent "$HOME/.cargo/bin/"
pbox setup
pbox new --image debian:13
pbox ssh BOX_ID
```

Enable shell completion for commands, flags, live box IDs/names and snapshot IDs/names:

```sh
# Zsh: add after compinit in ~/.zshrc
source <(pbox completions zsh)

# Bash: add to ~/.bashrc
source <(pbox completions bash)

# Fish: install once, regenerate after updating pbox
mkdir -p ~/.config/fish/completions
pbox completions fish > ~/.config/fish/completions/pbox.fish
```

Zsh must initialise completion with `autoload -Uz compinit; compinit` before
sourcing the script. `pbox completions` also supports `powershell` and `elvish`.
Script generation is offline and always writes a plain shell script, including with
`--json`. Pressing Tab on a box or snapshot argument queries the configured PVE
instance. Queries honour `--config` and `PBOX_CONFIG_FILE`, stop waiting after two
seconds and stay silent if PVE is unavailable. Unique box names work wherever a
box ID is accepted (except the `BOX_ID:path` syntax used by `scp`). Ambiguous names
require an explicit ID. Re-source the completion script after upgrading from static
completion: `source <(pbox completions zsh)`.

With a relay configured, creation keeps completed steps above the active spinner
and names the resolved image. The result shows that image, available IPv4/IPv6
addresses, and the exact command to connect. Use
`pbox new --verbose` to retain full logs. While a phase runs, recent substeps and
live image/PVE logs expand beneath it; they collapse into its completed row. Preparing an OCI image still
installs the prerequisites needed to boot it as an LXC guest.

Before uploading a prepared relay image, pbox checks systemd, required tools,
the agent's executable compatibility and its service preset. Failed checks stop
creation with the image name and instructions for fixing the Dockerfile.
These checks do not prove that guest networking or container permissions will
work after boot. If the agent cannot connect, pbox keeps the box and shows how
to inspect its service logs and networking in the PVE console, then retry with
`pbox repair BOX_ID`. Existing image-user restrictions are reported after connection.

Find images with `pbox image search debian` (Docker Hub by default), or use
`--registry REGISTRY` / a qualified query to search another registry. Bare
`pbox image search` prompts for a name; `pbox image search docker.io` prompts
within that registry. Search uses local Podman and does not require PVE setup.
Registries must support search; for a known image, list versions with
`pbox image tags docker.io/library/debian`.

Snapshots are independent saved environments stored as PVE container templates:

```sh
pbox snapshot create current --name llm-ready
pbox snapshot list
pbox new --snapshot llm-ready
pbox snapshot rm llm-ready
```

Saving briefly shuts down the source for a full disk copy, then restores its prior
running/stopped state. The template remains in PVE after the source is deleted.
Restoring makes another full copy; deleting the template does not delete those
boxes. Each new box receives fresh pbox credentials, machine ID, and SSH host keys.
Snapshot names are labels; `psn_` identifiers are unambiguous resource identities.

The source's filesystem and managed disks are saved, not running processes.
Host bind mounts and devices cannot be independent copies and are rejected.
Restore currently runs on the template's node and preserves disk sizes; CPU,
memory, hostname, and network can be overridden. Networking defaults to fresh DHCP
on the configured bridge rather than copying a static source address.

Snapshots stay in the same pbox authentication context. Relay deployments need
the updated pbox-relay for scoped bootstrap connections; no local archive or
privileged management server is involved. If provisioning is interrupted,
`pbox repair BOX_ID` resumes the new box. If a source capture was interrupted,
`pbox snapshot repair-source BOX_ID` removes its preparation files; only use it
once that capture is no longer active.

Existing per-container rollback points are available through `pbox checkpoint`.
They remain attached to their source and are deleted with it. They are not
converted automatically into independent snapshots. See [snapshot lifecycle and recovery](docs/snapshots.md).

`pbox info` and `pbox list` show both address families when assigned; loopback and
link-local IPv6 addresses are omitted. JSON retains `ip` for IPv4 and adds `ipv6`.
`pbox ssh` opens the guest user's configured login shell. Interactive terminal
titles are prefixed with `box-name · ` as the shell or apps update them. The previous
host title is restored on disconnect in terminals that support the title stack.

`pbox rm BOX_ID` asks for confirmation, then queues an immediate stop-and-delete
in Proxmox and returns. Check the node's task history for completion or failures;
`--json` returns the task UPID with `queued: true` and `deleted: false`.
Use `--wait` for graceful shutdown and confirmed deletion; shutdown failures stop
that operation. Press Enter to cancel, or use `--yes` to skip the prompt in scripts.
Boxes with unfinished bootstrap recovery state still wait so private templates
and recovery credentials can be cleaned up after successful deletion.
`current` selects the box only when exactly one pbox-managed container exists.

Direct access needs a route from the workstation to the guest. For private guest
subnets, deploy the optional relay in Docker or a Proxmox LXC and configure its
hostname or reachable private IP. See [relay setup](docs/relay.md).

Pbox needs Linux, a compatible guest agent binary, and Podman plus zstd for local
OCI image preparation. Image authors choose their own LLM tools. No Tailscale
installation is required when using a reachable relay.

All commands follow the [CLI design system](docs/cli-design.md), enforced by
shared rendering components and Clippy checks. `just check` runs strict Clippy;
`just test` runs all workspace tests.

Run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`
to check the workspace. See `GOAL.md`, `PLAN.md`, and `TODO.md` for scope and remaining work.

Accounts created by pbox have passwordless sudo inside the guest; password login
remains locked. If your image already defines `pbox`, its sudo policy is preserved.
Creation, start, and interactive SSH warn when passwordless root access is unavailable
and show `pbox ssh BOX_ID --user root` to install tools or adjust the policy.
Image authors can grant access with a root-owned, mode `0440` file in
`/etc/sudoers.d/90-pbox` containing `pbox ALL=(ALL:ALL) NOPASSWD: ALL`.

Interactive sessions default to `TERM=xterm-256color`; image preparation installs
its base terminfo entries. Use `pbox ssh BOX_ID --env TERM=...` to override it.
The CLI forwards `COLORTERM=truecolor` or `24bit` when advertised by the local
terminal. These defaults apply to PTYs, not ordinary `pbox exec` commands.

### Local image compatibility tests

Run `just test-images` to prepare the distro matrix in local Docker and exercise
agent access, terminal support, preset policy and compatibility failures. Tests
use ordinary containers and clean up their containers and newly downloaded images.
See [image compatibility and testing](docs/image-compatibility.md) for prerequisites,
coverage, individual image selection and interrupted-run cleanup.
