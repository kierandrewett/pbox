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

With a relay configured, creation shows a single updating status line, then the
available IPv4/IPv6 addresses and the exact command to connect. Use
`pbox new --verbose` for image preparation details. Preparing an OCI image still
installs the prerequisites needed to boot it as an LXC guest.

`pbox info` and `pbox list` show both address families when assigned; loopback and
link-local IPv6 addresses are omitted. JSON retains `ip` for IPv4 and adds `ipv6`.
`pbox ssh` opens the guest user's configured login shell.

`pbox rm BOX_ID` asks for confirmation, shuts down a running box, then deletes it.
Press Enter to cancel, or use `--yes` to skip the prompt in scripts. If shutdown
fails, deletion stops; forced shutdown remains an explicit `pbox stop BOX_ID --force`.
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
