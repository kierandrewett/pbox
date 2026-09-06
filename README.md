# pbox

Create disposable Proxmox workspaces for development and LLM tools.

Choose an image, create a box, install the tools you need, and connect from
your terminal or desktop.

## Install

Install the published CLI with [cargo-binstall](https://github.com/cargo-bins/cargo-binstall):

```sh
cargo binstall pbox
```

Or install it directly with Cargo:

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

Pbox runs on a Linux machine and creates Linux containers on Proxmox.
Podman prepares the image locally; PVE provides the CPU, memory, disk and
network for the running box.

### 1. Check the setup

You need:

| Component | What it is used for |
| --- | --- |
| Machine running pbox | Runs the `pbox` CLI and [Podman](https://podman.io/docs/installation) |
| [Podman](https://podman.io/docs/installation) | Pulls and prepares OCI images |
| [Proxmox VE](https://www.proxmox.com/en/proxmox-virtual-environment/overview) API token | Lets pbox create and manage LXCs |
| [PVE bridge](https://pve.proxmox.com/wiki/Network_Configuration) | Connects the LXC to a network |
| [PVE storage](https://pve.proxmox.com/pve-docs/pvesm.1.html) | Holds the LXC root filesystem |

The machine running pbox needs a route to the guest network for direct connections.
Use a [relay](docs/relay.md) when it does not.

| Network path | Choose this when | Configure |
| --- | --- | --- |
| **Direct** | The machine running pbox can reach the guest IP | Nothing extra |
| **Relay** | The guest network is private or unreachable | `relay.url` and `relay.key-file` |

> **Common gap:** the PVE API address and the guest network are separate paths.
> A working `pbox setup` proves API access; it does not prove that the machine
> running pbox can reach a guest IP.

### 2. Install pbox

```sh
cargo binstall pbox
```

<details>
<summary>Without cargo-binstall</summary>

```sh
cargo install pbox --locked
```

This requires a [Rust installation](https://www.rust-lang.org/tools/install/).

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
| `pve.bridge` | PVE bridge attached to the required network |
| `pve.storage` | LXC disk storage |
| `pve.template_storage` | Temporary image/template storage |

#### Proxmox references

These are the relevant parts of the Proxmox documentation:

- [API tokens and permissions](https://pve.proxmox.com/pve-docs/pve-admin-guide.pdf)
  — token creation, privileges and token scope.
- [`pveum` reference](https://pve.proxmox.com/pve-docs/pveum.1.html) — the token
  ID format is `USER@REALM!TOKEN_NAME`; pbox stores that value as `pve.token_id`.
- [Network configuration](https://pve.proxmox.com/wiki/Network_Configuration) —
  bridges, routed networks, NAT and VLANs.
- [`pct` reference](https://pve.proxmox.com/pve-docs/pct.1.html) — container
  network settings such as `bridge`, `ip`, `ip6` and `gw`.
- [`pvesm` reference](https://pve.proxmox.com/pve-docs/pvesm.1.html) — storage
  types and the `rootdir` and `vztmpl` content types pbox uses.

The API URL normally has this form:

```text
https://PROXMOX_HOST:8006
```

The token secret is separate from the token ID. Keep both private; `pbox config
list` redacts the secret.

### 4. Add a relay when needed

Use a relay when the box cannot be reached directly from the machine running
pbox. The box makes an outbound connection to the relay, so that machine does not
need a route to the guest network.

```sh
pbox relay keygen
pbox config set relay.url https://pbox.example.com
pbox relay check
```

Copy the generated key to the relay host using a secure channel and configure
the relay service with that same file. Keep the key outside your repositories;
never copy it into a box. `pbox relay check` verifies health and authentication.
See [relay.md](docs/relay.md) for deployment.

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
desktop in a native window on the machine running pbox:

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

Use a relay when the machine running pbox cannot route directly to a box. The box opens
an outbound connection to the relay, and the CLI uses the same relay to reach
it. Pbox still needs separate access to the Proxmox API.

The relay forwards the encrypted agent connection. It does not read shell,
file-transfer or port-forwarding data. Each box uses a credential scoped to
that box.

The relay master key belongs on the relay host and on the machine that manages
it. Keep it outside repositories and never copy it into a box. See
[relay.md](docs/relay.md) for deployment, HTTPS and recovery instructions.

## License

MPL-2.0
