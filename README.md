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

When a newer published version is available, pbox prints a hint after a normal
command. It checks at most once a day and ignores network failures. Set
`PBOX_NO_UPDATE_CHECK=1` to disable the check.

## Quick start

Pbox runs on your workstation and creates Linux containers on Proxmox.
Podman prepares the image locally; PVE provides the CPU, memory, disk and
network for the running box.

### 1. Check the setup

You need:

| Component | What it is used for |
| --- | --- |
| Linux workstation | Runs the `pbox` CLI and Podman |
| Podman | Pulls and prepares OCI images |
| Proxmox API token | Lets pbox create and manage LXCs |
| PVE bridge | Connects the LXC to a network |
| PVE storage | Holds the LXC root filesystem |

Your workstation needs a route to the guest network for direct connections.
Use a [relay](docs/relay.md) when it does not.

| Network path | Choose this when | Configure |
| --- | --- | --- |
| **Direct** | The workstation can reach the guest IP | Nothing extra |
| **Relay** | The guest network is private or unreachable | `relay.url` and `relay.key-file` |

> **Common gap:** the PVE API address and the guest network are separate paths.
> A working `pbox setup` proves API access; it does not prove that your laptop
> can reach a guest IP.

### 2. Install pbox

```sh
cargo binstall pbox
```

<details>
<summary>Without cargo-binstall</summary>

```sh
cargo install pbox --locked
```

</details>

### 3. Configure Proxmox

Run the wizard. It discovers nodes, storage and bridges, then verifies the
configuration it saves:

```sh
pbox setup
pbox config list       # secrets are redacted
```

The values normally map like this:

| Pbox setting | Proxmox concept |
| --- | --- |
| `pve.url` | `https://HOST:8006` |
| `pve.token_id` / `pve.token_secret` | API token credentials |
| `pve.node` | Target PVE node |
| `pve.bridge` | Linux bridge, often `vmbr0` |
| `pve.storage` | LXC disk storage |
| `pve.template_storage` | Temporary image/template storage |

### 4. Add a relay when needed

The relay is an outbound path from the guest to a reachable host. It does not
replace Proxmox API access, and it does not make the guest directly routable.

```sh
pbox relay keygen
pbox config set relay.url https://pbox.example.com
pbox relay check
```

Copy the generated key to the relay host using a secure channel and configure
the relay service with that same file. `pbox relay check` verifies both health
and scoped-key authentication. See [relay.md](docs/relay.md) for deployment.

### 5. Create and connect

```sh
pbox new --image debian:13
pbox list
pbox ssh current
```

Pbox prepares the image, creates the LXC, installs `pbox-agent`, and waits for
the agent to become ready. `current` means the only box; otherwise use its ID:

```sh
pbox ssh pbx_d7ky95gz
```

### 6. Add tools or a desktop

Recipes are Ansible playbooks. Apply several in one command when they belong to
the same setup:

```sh
pbox recipe apply --box-id current dev/base language/rust
```

Desktop recipes install a VNC server when required. `pbox desktop` opens the
desktop in a native window on the workstation:

```sh
pbox recipe apply --box-id current desktop/xfce
pbox desktop current
```

<details>
<summary>If something fails</summary>

| Symptom | Check |
| --- | --- |
| `pbox setup` cannot connect | PVE URL, token permissions and API reachability |
| Box exists but has no IP | PVE bridge, DHCP and guest network access |
| Agent is not ready | `pbox info BOX`; retry with `pbox repair BOX` |
| Direct SSH cannot reach the box | Configure and check the relay |
| Recipe output is too short | Add `--verbose` for full Ansible output |
| Desktop opens but is blank | Confirm the desktop recipe completed, then retry `pbox desktop BOX` |

</details>

Run `pbox --help` or `pbox COMMAND --help` for the complete command list.

## Examples

List boxes:

```text
$ pbox list
ID            STATE    PING  NODE  IPV4          NAME           IMAGE
pbx_d7ky95gz  running  ok    pve   172.30.0.134  pbox-d7ky95gz  docker.io/cachyos/cachyos:latest
```

Inspect a box:

```text
$ pbox info current
box pbx_d7ky95gz
  vmid         9000
  state        running
  node         pve
  ipv4         172.30.0.134
  name         pbox-d7ky95gz
  recipes      agent/codex, browser/helium, desktop/xfce, dev/base, language/rust
```

Apply recipes:

```text
$ pbox recipe apply --box-id current dev/base language/rust
! Snapshots are unavailable on this storage; applying the recipe without a snapshot.
dev/base, language/rust → pbx_d7ky95gz
ok Preparing guest (1s)
> Applying recipes
> Install Rust with rustup
ok Install Rust with rustup (8s)
```

Open a shell:

```text
$ pbox ssh current
> Connected to pbx_d7ky95gz. Type exit to disconnect.
[pbox@pbox-d7ky95gz ~]$ rustup --version
rustup 1.28.2
[pbox@pbox-d7ky95gz ~]$
```

The exact task lines and timings depend on the image and the recipes already
installed. Use `--verbose` to stream the complete Ansible output.

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

The relay lets pbox work when the workstation cannot route directly to a
private guest network. The guest opens an outbound WebSocket connection to the
relay. The CLI connects to the same relay, and the relay joins the two
authenticated connections.

The relay carries the existing encrypted agent connection. It does not receive
guest shell, file or forwarding contents. It does handle connection metadata
and can interrupt a connection. Each box has its own scoped relay credential.

Configure a relay on the workstation:

```sh
pbox relay keygen
pbox config set relay.url https://pbox.example.com
pbox relay check
pbox new --image debian:13
pbox ssh current
```

Use a private relay address when both the workstation and guests can reach it:

```sh
pbox config set relay.url http://100.120.0.10:8080
```

Use HTTPS or an encrypted private network. The relay key is an operator secret;
keep it outside the repository and do not copy it into guests. See
[relay.md](docs/relay.md) for Docker, Proxmox LXC, reverse proxy and recovery
instructions.

## Development

Clone the recipes repository as a submodule:

```sh
git clone https://github.com/kierandrewett/pbox.git
cd pbox
git submodule update --init --recursive
```

The recipes are available in `recipes/`. The standalone repository remains the
source of truth for recipe releases.

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

MPL-2.0
