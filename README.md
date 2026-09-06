# pbox

Disposable Proxmox LXC workspaces for development and LLM tools.

Create a box from an OCI image, install tools with Ansible recipes, and connect
with a local terminal or desktop window.

## Install

The normal install uses a published Rust package:

```sh
cargo binstall pbox
```

If cargo-binstall is not installed, compile pbox with Cargo:

```sh
cargo install pbox --locked
```

Install shell completion after installing pbox:

```sh
# zsh
source <(pbox completions zsh)

# bash
source <(pbox completions bash)

# fish
mkdir -p ~/.config/fish/completions
pbox completions fish > ~/.config/fish/completions/pbox.fish
```

Update an installed pbox:

```sh
pbox update
```

`pbox update` uses cargo-binstall when it is available and otherwise uses
`cargo install`.

## Quick start

Configure the Proxmox API once:

```sh
pbox setup
```

Create and open a box:

```sh
pbox new --image debian:13
pbox list
pbox ssh current
```

`current` works when exactly one pbox exists. Use a box ID or name when
there is more than one.

## Boxes

```sh
pbox list
pbox info BOX
pbox start BOX
pbox stop BOX
pbox rm BOX
```

`pbox new` accepts an OCI image reference or a saved snapshot:

```sh
pbox new --image docker.io/library/debian:13
pbox new --snapshot llm-ready
```

Use `pbox image search` and `pbox image tags` to find images. The image
keeps its own user, shell, environment and working directory. pbox adds the
guest agent and prepares the network during creation.

## Recipes

Recipes are Ansible playbooks stored in
[pbox-recipes](https://github.com/kierandrewett/pbox-recipes).

```sh
pbox recipe list
pbox recipe apply --box-id current desktop/xfce
pbox recipe apply --box-id current dev/base language/rust
```

Recipes install software inside a box. They can add desktops, browsers, IDEs,
programming languages and coding agents. Multiple recipes in one command share
one preparation step and run in the order given.

## Desktops

Install a desktop recipe, then open it in a native VNC window:

```sh
pbox recipe apply --box-id current desktop/xfce
pbox desktop current
```

Desktop recipes install and configure a VNC server when needed. See
[desktop.md](docs/desktop.md).

## Saved environments

Snapshots are independent PVE templates:

```sh
pbox snapshot create current --name llm-ready
pbox snapshot list
pbox snapshot rm llm-ready
```

Checkpoints are short-lived rollback points attached to one box:

```sh
pbox checkpoint list BOX
pbox checkpoint create BOX --name before-change
```

See [snapshots.md](docs/snapshots.md) for lifecycle and recovery details.

## Relay

Use the relay when the workstation cannot route to the private guest network.
See [relay.md](docs/relay.md) for deployment and configuration.

## Development

Build the CLI and guest agent:

```sh
just build
```

Run the checks:

```sh
just check
just test
```

The CLI is a Linux application. The guest agent is built as a portable musl
binary for supported Linux images. See
[image-compatibility.md](docs/image-compatibility.md) for image requirements
and the local image test matrix.

## License

MIT
