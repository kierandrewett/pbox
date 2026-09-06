# pbox

Linux development environments on Proxmox. Create a box from a container image,
install tools with Ansible recipes, and open a shell or desktop.

[Quick start](#quick-start) · [Configuration](docs/configuration.md) ·
[Relay setup](docs/relay.md) · [Recipes](docs/recipes.md) ·
[Help](#troubleshooting)

## Quick start

Run the commands below on the Linux machine where you want to use pbox.
A **box** is a Linux container running on Proxmox VE (PVE).

### 1. Check the requirements

| Component | Needed for |
| --- | --- |
| [Proxmox VE](https://www.proxmox.com/en/proxmox-virtual-environment/overview) | Running boxes on a standalone node or cluster |
| [Proxmox API token](https://pve.proxmox.com/pve-docs/chapter-pveum.html#pveum_tokens) | Giving pbox access to the nodes, containers and storage it manages |
| [Network bridge](https://pve.proxmox.com/wiki/Network_Configuration) | Connecting boxes to a network with addressing, DNS and access to package repositories |
| [Storage](https://pve.proxmox.com/pve-docs/chapter-pvesm.html) | Container disks (`rootdir`) and uploaded templates (`vztmpl`); these can use different storage pools |
| [Podman](https://podman.io/docs/installation) and [Git](https://git-scm.com/downloads) | Preparing images locally and downloading recipes |
| [Ansible](https://docs.ansible.com/projects/ansible/latest/installation_guide/intro_installation.html) | Applying recipes; `ansible-playbook` must be available locally |
| [TigerVNC viewer](https://tigervnc.org/) | Opening a desktop; optional for shell-only use |

The default box network uses DHCP. A bridge alone does not supply DHCP or DNS.
See [network configuration](docs/configuration.md#networking) for static addresses
and direct or relay access.

### 2. Install

The intended package install uses [cargo-binstall](https://github.com/cargo-bins/cargo-binstall)
or Cargo:

```sh
cargo binstall pbox
# Or compile with Cargo:
cargo install pbox --locked
```

> [!NOTE]
> The CLI and agent are not yet published on crates.io. Use the source install
> below until a release is available.

<details open>
<summary><strong>Install from source</strong></summary>

Install [Rust](https://rust-lang.org/tools/install/), a C toolchain and
[`protoc`](https://protobuf.dev/installation/), then:

```sh
git clone https://github.com/kierandrewett/pbox.git
cd pbox
cargo install --locked --path crates/pbox-cli
cargo install --locked --path crates/pbox-agent
```

Keep `~/.cargo/bin` on your `PATH`. The agent must run on the box's CPU
architecture and Linux distribution. This native build suits matching glibc
guests; for Alpine or a portable agent, see [agent builds](docs/development.md#build-the-agent).

</details>

**Both binaries are needed.** Pbox copies `pbox-agent` into boxes automatically.
It looks beside the CLI binary, or uses an explicit path:

```sh
pbox config set agent.binary /absolute/path/to/pbox-agent
```

### 3. Connect to Proxmox

[Create an API token](docs/configuration.md#api-token), then run:

```sh
pbox setup
```

The wizard asks for credentials and discovers nodes, storage and bridges.

| Value | Expected format | Example |
| --- | --- | --- |
| API URL | HTTPS address of the PVE API | `https://pve.example.com:8006` |
| Token ID | `USER@REALM!TOKEN_NAME` | `pbox@pve!cli` |
| Token secret | The separate value shown when creating the token | Paste into the hidden prompt |
| Node | A PVE node name, or `auto` | Select from the wizard |
| Rootfs storage | Storage supporting container disks | Select from the wizard |
| Template storage | Storage supporting container templates | May differ from rootfs storage |
| Bridge | The network to attach new boxes to | Select the bridge for your setup |

**Expected result:** `Configuration saved` and `PVE connection verified`.
This checks API access; creating a box also needs allocation and storage
permissions. [Permissions and configuration details](docs/configuration.md).

### 4. Choose the connection path

| Your setup | Connection |
| --- | --- |
| Pbox can reach guest addresses on a LAN, routed network or VPN | **Direct.** Guest SSH is used during initial setup; later sessions use the agent. |
| Pbox cannot reach guest addresses, but both pbox and the boxes can reach a relay | **Relay.** Follow [relay setup](docs/relay.md) before creating a box. |
| Boxes have no outbound route or working DNS | Configure the guest network first; a relay still needs to be reachable. |

> [!IMPORTANT]
> Reaching the Proxmox web interface does not imply that pbox can reach a box.
> A relay provides guest access; pbox still connects to the Proxmox API separately.

For an **existing relay**, obtain its URL and key file from whoever operates it:

```sh
pbox config set relay.url https://relay.example.com
pbox config set relay.key-file /absolute/path/to/relay.key
pbox relay check
```

For a **new relay**, [generate a key and deploy the service](docs/relay.md#set-up-a-new-relay),
then run the check. Generating a key alone does not start a relay.

### 5. Create a box and open a shell

```sh
pbox new --image debian:13
pbox list
pbox ssh current
```

Pbox waits for the guest agent before completing creation. Type `exit` to
disconnect; the box keeps running.

`current` selects the only box. With several boxes, use a `pbx_` ID or unique
name from `pbox list`. Shell access uses pbox's agent; you do not need to
configure a separate SSH login.

<details>
<summary>Example terminal output</summary>

Illustrative output; IDs, addresses and installed tools will differ.

```text
$ pbox list
ID            STATE    PING  NODE  IPV4          NAME           IMAGE
pbx_d7ky95gz  running  ok    pve   172.30.0.134  pbox-d7ky95gz  docker.io/library/debian:13

$ pbox ssh current
> Connected to pbx_d7ky95gz. Type exit to disconnect.
[pbox@pbox-d7ky95gz ~]$
```

</details>

### 6. Install tools

```sh
pbox recipe list
pbox recipe apply --box-id current dev/base language/rust
```

Recipes run in the order given. Browse
[pbox-recipes](https://github.com/kierandrewett/pbox-recipes) for languages,
browsers, IDEs and coding agents. [Recipe logs and recovery](docs/recipes.md).

For a desktop, install a desktop recipe and open the viewer:

```sh
pbox recipe apply --box-id current desktop/xfce
pbox desktop current
```

This opens a local TigerVNC window. Closing it leaves applications running.
[Desktop requirements and sessions](docs/desktop.md).

## Everyday commands

Replace `BOX` with an ID, unique name or `current`.

| Task | Command |
| --- | --- |
| List boxes | `pbox list` |
| Inspect a box | `pbox info BOX` |
| Run a command | `pbox exec BOX -- uname -a` |
| Copy a file into a box | `pbox scp ./file.txt BOX:/tmp/file.txt` |
| Reach an app on port 3000 | `pbox forward BOX 3000` |
| Stop / start | `pbox stop BOX` / `pbox start BOX` |
| Delete | `pbox rm BOX` |
| Find images | `pbox image search debian` |
| List image tags | `pbox image tags debian` |

`pbox list` uses a background inventory cache; changes can take a refresh to
appear. Use `pbox COMMAND --help` for options and `--json` for structured
results where supported.

### Save an environment

| | Snapshot | Checkpoint |
| --- | --- | --- |
| Purpose | Create new boxes from a saved environment | Roll back the same box |
| Lifetime | Independent of the source box | Deleted with the box |
| Storage | Full copy in Proxmox | Requires native snapshot support |

```sh
pbox snapshot create current --name tools-ready
pbox new --snapshot tools-ready

pbox checkpoint create current before-change
```

Snapshot capture stops the source while copying; save work first.
[Capture, restore and recovery](docs/snapshots.md).

### Shell completion

<details>
<summary>Zsh, Bash and Fish</summary>

For Zsh, add to `~/.zshrc`:

```sh
source <(pbox completions zsh)
```

For Bash, add to `~/.bashrc`:

```sh
source <(pbox completions bash)
```

For Fish, run once:

```sh
mkdir -p ~/.config/fish/completions
pbox completions fish > ~/.config/fish/completions/pbox.fish
```

</details>

### Updates

`pbox update` installs the latest published CLI through cargo-binstall or Cargo.
Until packages are published, repeat the source-install steps after updating
the checkout.

Pbox checks for published updates before selected commands and caches successful
checks for a day. Network failures are silent. Set `PBOX_NO_UPDATE_CHECK=1` to
disable checks. The CLI update does not update the local agent binary or relay;
`pbox ssh` compares the guest agent with the local binary before connecting.

## Troubleshooting

| Problem | Next step |
| --- | --- |
| `pbox-agent binary was not found` | Install the agent or set `agent.binary`; see [installation](#2-install). |
| PVE rejects credentials or permissions | Check the full token ID, secret and [user/token permissions](docs/configuration.md#api-token). |
| Box has no address | Check bridge, DHCP or static addressing in [networking](docs/configuration.md#networking). |
| Creation stops while waiting for the agent | Inspect the box in PVE, correct the reported problem, then run `pbox repair BOX`. |
| Relay check fails | Check the URL, running service and matching key using the [relay guide](docs/relay.md#verify-the-connection). |
| Recipe fails | Read the saved log path or rerun with `--verbose`; see [recipes](docs/recipes.md). |
| Desktop will not start | Check local viewer and guest session requirements in [desktop help](docs/desktop.md#troubleshooting). |

[Report an issue](https://github.com/kierandrewett/pbox/issues) with the command,
pbox version or source commit, guest image and relevant error output. Remove
credentials from logs before sharing them.

## License

[Mozilla Public License 2.0](LICENSE).
