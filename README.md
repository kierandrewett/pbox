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

Direct access needs a route from the workstation to the guest. For private guest
subnets, deploy the optional relay in Docker or a Proxmox LXC and configure its
hostname or reachable private IP. See [relay setup](docs/relay.md).

Pbox needs Linux, a compatible guest agent binary, and Podman plus zstd for local
OCI image preparation. Image authors choose their own LLM tools. No Tailscale
installation is required when using a reachable relay.

Run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`
to check the workspace. See `GOAL.md`, `PLAN.md`, and `TODO.md` for scope and remaining work.
