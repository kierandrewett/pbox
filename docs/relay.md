# Relay access

The relay lets a workstation reach a guest without an inbound route to that guest.
The agent opens an outbound WebSocket. `pbox ssh`, `exec`, `scp`, and `forward` use
that connection. The PVE API still handles container creation and lifecycle.

The same relay runs in Docker or as a systemd service in an unprivileged Proxmox
LXC. It needs no PVE credentials, host mounts, Docker socket, TUN device, or host
privileges. Both the workstation and the guest must reach its address.

## Address and encryption

Use `https://pbox.example.com` behind a reverse proxy for internet access. Caddy
forwards WebSocket upgrades automatically. Point the hostname at the proxy, allow
TCP 443 to the proxy, and route it to the relay's TCP port 8080. A hostname alone
does not make a private container reachable.

Use `http://100.x.y.z:8080` if both callers can reach that IP through an existing
Tailscale connection or subnet route. IPv6 literals also work, for example
`http://[fd00::10]:8080`. HTTP is suitable only on an already protected network:
agent session contents have their own encryption, but relay bearer credentials
need HTTPS or an encrypted network to protect them in transit.

The relay carries the existing TLS 1.3 connection between CLI and agent. Each end
checks certificates from the configured pbox context. HTTPS certificates at the
reverse proxy do not replace this check. The relay cannot read shell or file
contents, but it can see connection metadata and interrupt connections.

## Docker

From the repository root:

```sh
mkdir -p deploy/relay
(umask 077; openssl rand -hex 32 > deploy/relay/relay.key)
# Compose bind-mounted secrets need to be readable by the container user.
# Keep this directory private if the key file is made readable.
chmod 700 deploy/relay
chmod 644 deploy/relay/relay.key
docker compose -f deploy/relay/compose.yml up -d --build
curl --fail http://127.0.0.1:8080/healthz
```

The image builds `pbox-relay` from this checkout. It runs as UID 10001, with a
read-only filesystem and no capabilities. The example publishes port 8080. If a
reverse proxy shares its Docker network, remove the published port and connect
the proxy to `pbox-relay:8080` instead. See `deploy/relay/Caddyfile`.

Do not commit `relay.key`. Keep a backup of the master key. Changing it invalidates
all derived relay credentials, so existing guests need new relay configuration.
The first version uses one operator key per relay, not separate user accounts.

## Proxmox LXC

Create an ordinary unprivileged Debian 13 LXC with network access. Docker and
nesting are not required. Build the binary on a compatible Linux system:

```sh
cargo build --locked --release -p pbox-relay
```

Copy the binary and `deploy/relay/pbox-relay.service` into the LXC using your usual
initialisation or file-transfer process. Then run these commands **inside it**:

```sh
apt-get update && apt-get install -y --no-install-recommends openssl curl
install -m 755 pbox-relay /usr/local/bin/pbox-relay
install -d -m 700 /etc/pbox-relay
(umask 077; openssl rand -hex 32 > /etc/pbox-relay/relay.key)
install -m 644 pbox-relay.service /etc/systemd/system/pbox-relay.service
systemctl daemon-reload
systemctl enable --now pbox-relay
curl --fail http://127.0.0.1:8080/healthz
```

Systemd supplies the key through its credentials directory. The service uses a
dynamic unprivileged user. Make its IP reachable, or put an HTTPS reverse proxy
in front of it. Installing a relay in LXC does not require changes to the PVE host.

## Configure pbox

Copy the relay master key securely to the workstation, then:

```sh
chmod 600 "$HOME/.config/pbox/relay.key"
pbox config set relay.key-file "$HOME/.config/pbox/relay.key"
pbox config set relay.url https://pbox.example.com
pbox new --image ghcr.io/your-account/your-image:latest
pbox ssh BOX_ID
```

For a private registry, use `podman login REGISTRY` on the workstation first.
Pbox prepares the OCI image locally with Podman, so registry credentials remain
on the workstation. The image author chooses the development and LLM tools.
Pbox adds the system services and its agent, with fresh credentials for each box.
No relay master key or PVE API token is installed in a guest workspace.

Relay creation accepts OCI images through `--image`, including the `debian-13`
default alias. It cannot personalise an existing PVE `--ostemplate` through the
API, so it rejects that combination before creating resources. Use a glibc-based
systemd image compatible with your agent binary; Debian 13 is the validated base.
An ordinary OCI ENTRYPOINT is not the LXC boot command. LXC boots systemd, and image
authors should configure background services as systemd units. OCI runtime
USER, WORKDIR and ENV metadata are not currently restored by filesystem export.

## Lifecycle and recovery

Pbox installs the agent and credentials before uploading a unique temporary
root filesystem. PVE extracts it and starts the guest. Pbox deletes the temporary
PVE template and local archive, then checks the agent through the relay. It never
uses host SSH or PVE console commands to bootstrap a relay guest.

The guest listens on loopback and connects outbound. Stop/start preserves its
identity. Deleting the guest removes that running agent; box IDs are not reused
intentionally. Restoring a copy with the same identity can conflict with its
original. Independent clone/rekey support is not implemented yet.

The agent reconnects after a relay restart. Existing shells fail when their
connection is lost; pbox does not replay commands. Run `pbox ssh` again. Existing
agent session time limits still apply, including its one-hour command limit.

Interrupted creation records recovery state locally. Run `pbox repair BOX_ID`.
A running PVE task must finish before its private template can be deleted. If no
guest was created, repair removes the temporary template and local credentials.
If a guest exists, repair checks its agent before completing cleanup. You can also
use `pbox delete BOX_ID --yes` to delete a failed guest and its recorded template.

The relay allows 1024 waiting/active agent connections by default. Each agent
allows up to 32 connections, including its waiting connection. `--max-connections`
sets the relay limit. `/healthz` checks the relay process, not guest readiness.

## Implementation references

- WebSocket client API: https://docs.rs/tokio-tungstenite/0.29.0/tokio_tungstenite/fn.connect_async.html
- PVE storage API: https://github.com/proxmox/pve-storage/blob/master/src/PVE/API2/Storage/Content.pm
- Image payload copying: https://docs.podman.io/en/latest/markdown/podman-cp.1.html

## Validation on 2026-09-05

- A real Debian 13 guest was created through the PVE API on a private subnet.
  Its agent bound only to loopback and connected through the HTTPS relay.
- Concurrent execution, a terminal session, and a 2 MiB binary file round trip passed.
- Port forwarding reached a second relay running under the supplied systemd unit
  inside the unprivileged LXC. An authenticated command passed through that relay.
  The workstation reached this private LXC relay through the tested port forward.
- Relay restart and guest stop/start both restored access. The test guest and
  temporary template were deleted afterwards.
- The provided Dockerfile built successfully. Its image served its health endpoint
  as UID 10001 with a read-only filesystem and all capabilities removed.
- All 159 workspace tests, strict Clippy checks and formatting checks passed.
  Similarity review found existing API/test repetition, with no new relay duplication
  that needed a separate abstraction.

The bsociety deployment uses `https://pbox.drewett.dev`. Its Compose service runs
on the host's intranet Docker network behind the existing Caddy proxy.
