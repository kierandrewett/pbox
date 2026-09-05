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

With a relay configured, creation keeps completed steps above the active spinner
and names the resolved image. The result shows that image, available IPv4/IPv6
addresses, and the exact command to connect. Use
`pbox new --verbose` to retain full logs. While a phase runs, recent substeps and
live image/PVE logs expand beneath it; they collapse into its completed row. Preparing an OCI image still
installs the prerequisites needed to boot it as an LXC guest.

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
