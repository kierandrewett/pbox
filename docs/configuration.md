# Configuration

[Quick start](../README.md#quick-start) · [Relay setup](relay.md)

## API token

In the Proxmox web interface, open **Datacenter → Permissions → API Tokens**,
choose **Add**, select a user and give the token a name. Save the token secret:
Proxmox only displays it once.

See Proxmox's [API token guide](https://pve.proxmox.com/pve-docs/chapter-pveum.html#pveum_tokens)
and [permission management](https://pve.proxmox.com/pve-docs/chapter-pveum.html#pveum_permission_management).

| Field in pbox | What to enter |
| --- | --- |
| `pve.url` | HTTPS API address, such as `https://pve.example.com:8006`; use the actual address and port for your endpoint |
| `pve.token_id` | Full ID: `USER@REALM!TOKEN_NAME`, for example `pbox@pve!cli` |
| `pve.token_secret` | Separate token value, without the ID or `PVEAPIToken=` prefix |

With **Privilege Separation** enabled, both the user and the token need
permissions. A token cannot exceed its user's permissions. Creating a token
does not grant it permission to create containers.

Pbox needs to discover nodes, storage and networks; allocate and configure
containers; upload templates; and perform the lifecycle operations you use.
Snapshot commands also need clone, template or snapshot permissions. Consult
the [API viewer](https://pve.proxmox.com/pve-docs/api-viewer/) for the permissions
on each endpoint. Successful discovery alone does not test all these operations.

Run `pbox setup` to enter the secret through a hidden prompt. For scripts,
`PBOX_PVE_TOKEN_ID` and `PBOX_PVE_TOKEN_SECRET` can supply credentials.
Quote token IDs containing `!` in shell commands:

```sh
pbox config set pve.token_id 'pbox@pve!cli'
```

Pbox derives guest trust from the PVE token ID and secret. Changing them can
disconnect existing boxes; the setup wizard warns before saving that change.

## Placement and resources

```sh
pbox setup
pbox config list
```

| CLI config key | Meaning |
| --- | --- |
| `pve.node` | Node name, or `auto` for automatic selection |
| `pve.storage` | Container root filesystem storage, or `auto` |
| `pve.template-storage` | Uploaded container template storage |
| `pve.bridge` | Bridge attached to the desired guest network |
| `agent.binary` | Absolute path to a compatible local agent binary |
| `agent.port` | Agent listener port; default `7443` |

These are keys for `pbox config set`, not necessarily the TOML field names.
Use `pbox config list` to see all accepted keys and current saved values;
secrets are redacted.

Storage must be available on the selected node. Container disks use
`rootdir` content; uploaded templates use `vztmpl`. One storage pool need
not support both. See [Proxmox storage](https://pve.proxmox.com/pve-docs/chapter-pvesm.html).

## Networking

Proxmox API access and guest access are separate:

```text
pbox ───────────────► Proxmox API    create, stop, delete
pbox ───────────────► guest agent    shell, files, desktop (direct)

pbox ──► relay ◄───── guest agent    shell, files, desktop (relay)
```

| Configuration | Requirements |
| --- | --- |
| Direct, on a LAN or routed network | Pbox can reach guest SSH during initial bootstrap and the configured agent port afterwards |
| Direct, over a VPN | The VPN routes guest addresses and permits those ports |
| Relay, with a private or NAT guest network | Both pbox and guests can reach the relay; guests need working outbound routing and DNS |
| Isolated guest network | Supply the necessary routes and DNS before attempting package installation or relay access |

A bridge connects interfaces; it does not automatically provide a DHCP server,
DNS or internet access. See Proxmox's [bridge, routed and NAT examples](https://pve.proxmox.com/wiki/Network_Configuration).

New boxes use DHCP by default. For an existing network without DHCP, supply
a free address and its gateway through `--net0`. **Replace these example values**:

```sh
pbox new --image debian:13 --net0 'name=eth0,bridge=vmbr1,ip=192.0.2.20/24,gw=192.0.2.1'
```

See the [container network parameters](https://pve.proxmox.com/pve-docs/pct.1.html)
for the Proxmox format. The relay workspace path currently rejects an explicit
static IPv6 gateway; DHCPv6/SLAAC are supported.

## Configuration files and TLS

The default file is `~/.config/pbox/config.toml`, respecting
`XDG_CONFIG_HOME`. Select another file with `--config PATH` or
`PBOX_CONFIG_FILE`.

Operational commands load the file and apply supported environment overrides.
`pbox config list` displays the saved file, so an environment override can
make a command behave differently from that display.

PVE connections use HTTPS. For a private certificate authority, configure trust
on the machine running pbox; see [Proxmox certificates](https://pve.proxmox.com/pve-docs/chapter-sysadmin.html#sysadmin_certificate_management).
The setup wizard also offers `pve.tls_insecure`, which disables certificate
verification.

## What each check proves

| Check | Proves |
| --- | --- |
| `podman info` | Local Podman is usable |
| `ansible-playbook --version` | Ansible is installed locally |
| `pbox setup` without `--skip-verify` | PVE API authentication and discovery work |
| `pbox relay check` | Relay HTTP health and matching key from this machine |
| Successful `pbox new` | Provisioning and guest-agent readiness completed for that box |
| `pbox exec BOX -- true` | An authenticated command can run in that box |

A relay check does not test guest DNS, outbound access or the WebSocket tunnel.
Box creation and guest commands exercise those paths.
