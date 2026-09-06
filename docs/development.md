# Contributing to pbox

[User guide](../README.md) · [CLI design system](cli-design.md)

## Checkout and tools

```sh
git clone --recurse-submodules https://github.com/kierandrewett/pbox.git
cd pbox
```

The `recipes/` submodule is the separate
[pbox-recipes repository](https://github.com/kierandrewett/pbox-recipes).
For an existing checkout, run `git submodule update --init --recursive`.
Recipe changes are committed and pushed there before updating the parent gitlink.

Install Rust (see `rust-version` in [Cargo.toml](../Cargo.toml)), a C toolchain,
[`protoc`](https://protobuf.dev/installation/), Python 3 and
[`just`](https://github.com/casey/just).

## Build the CLI

```sh
cargo build --locked -p pbox
```

The package is named `pbox`; its directory is `crates/pbox-cli`.

## Build the agent

For a native binary:

```sh
cargo build --locked --release -p pbox-agent
```

For an x86-64 static musl binary, install a musl C toolchain and the Rust target:

```sh
rustup target add x86_64-unknown-linux-musl
CC_x86_64_unknown_linux_musl=musl-gcc just build-agent
```

This writes `target/release/pbox-agent`. Use it explicitly with an installed CLI:

```sh
pbox config set agent.binary "$PWD/target/release/pbox-agent"
```

`just build` builds the native release CLI and musl agent. Select the target
and compiler for other architectures; the examples above are x86-64.

## Checks

```sh
cargo fmt --all -- --check
just check
just test
```

`just check` runs Clippy for all workspace targets. `just test` runs Rust tests
and the Python script tests. CLI presentation changes must also be inspected in
terminal, redirected and JSON modes; follow [the design system](cli-design.md).

## Image tests

Requires Docker, the build tools above and enough space for disposable images.

```sh
just test-images --workspace --images debian,fedora,cachyos,alpine
just test-images --workspace --images cachyos --skip-build
```

The suite exercises production preparation, authenticated agent RPC, image
metadata and restart behaviour in ordinary containers. It does not boot systemd
or prove Proxmox LXC boot and network access.

Test resources have a unique run label. Cleanup removes only owned containers
and eligible images. Logs and resource journals remain under
`test-results/images/`, or the selected `--output` directory.

After a hard interruption, check that the run is no longer active, then:

```sh
python3 scripts/test-images.py --cleanup-only test-results/images/resources-RUN_ID.json
```

Without `--workspace`, the suite exercises distro-init preparation used by
direct creation and legacy templates.

Live validation needs a disposable PVE environment. Check creation, guest
addressing and DNS, shell/exec/files, relay connectivity where used, stop/start,
and snapshot restore with a distinct identity. Local matrix success alone is
not evidence that those paths work.

## Repository map

| Path | Purpose |
| --- | --- |
| `crates/pbox-cli` | Commands, orchestration and terminal UI |
| `crates/pbox-core` | Configuration, IDs, metadata and PVE API client |
| `crates/pbox-agent` | Guest execution, PTYs, files, forwarding and workspace startup |
| `crates/pbox-agent-client` | Authenticated agent client |
| `crates/pbox-relay` | Relay server and WebSocket transport |
| `crates/pbox-crypto` | Guest/client certificate material |
| `proto/` | Active gRPC contract, compiled by `crates/pbox-proto/build.rs` using protoc |
| `deploy/relay/` | Compose, systemd and reverse-proxy examples |

## Releases

[The release workflow](../.github/workflows/release.yml) runs for `v*` tags.
It publishes workspace crates in dependency order, then builds separate CLI and
agent archives. It requires `CARGO_REGISTRY_TOKEN` in repository secrets.

Review both package and binary jobs before announcing a release. The current
binary job targets GNU/Linux; `just build-agent` targets musl. Do not describe
GNU agent archives as portable to Alpine.
