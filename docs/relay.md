# Relay setup

[Quick start](../README.md#quick-start) · [Networking](configuration.md#networking)

A relay lets pbox reach boxes that have no inbound route from the machine running
the CLI. Both ends connect to the relay. Proxmox API access is still required
separately.

## Choose an address

| Deployment | Address example | Requirements |
| --- | --- | --- |
| HTTPS reverse proxy | `https://relay.example.com` | DNS points to the proxy; TCP 443 is reachable; proxy forwards WebSocket upgrades to the relay |
| Encrypted private network | `http://192.0.2.10:8080` | Both ends can reach that address through the protected network |
| Private IPv6 | `http://[fd00::10]:8080` | IPv6 routing from both ends and a listener/proxy accepting IPv6 |

These are examples, not addresses to copy unchanged. The default service listens
on IPv4 port 8080. Use HTTPS for access over untrusted networks.

## Connect to an existing relay

Obtain the service URL and its key file from the operator. On the machine running
pbox:

```sh
pbox config set relay.url https://relay.example.com
pbox config set relay.key-file /absolute/path/to/relay.key
pbox relay check
```

Use the existing key. Generating a different one will fail authentication.

## Set up a new relay

### 1. Generate the key

On the machine running pbox:

```sh
pbox relay keygen
```

This writes a key with mode `600` and saves its path in pbox configuration.
Use the reported path in the deployment steps below. The same key is needed by
the relay service and pbox; box credentials are generated automatically.

### 2. Install the relay service

Choose one deployment below. Source builds currently require
[Rust](https://rust-lang.org/tools/install/), Git and a C toolchain.

<details>
<summary><strong>Docker Compose</strong></summary>

On the relay host, install [Docker Compose](https://docs.docker.com/compose/install/)
and download the deployment files:

```sh
git clone https://github.com/kierandrewett/pbox.git
cd pbox
```

Transfer the key from step 1 to this host. Keep it outside the checkout by
overriding the supplied Compose file's secret path. From the checkout, run
these commands as root, replacing `/path/to/transferred/relay.key`:

```sh
install -d -m 700 /etc/pbox-relay
install -m 644 /path/to/transferred/relay.key /etc/pbox-relay/relay.key
cat > /etc/pbox-relay/compose.override.yml <<'YAML'
secrets:
  relay-key:
    file: /etc/pbox-relay/relay.key
YAML
docker compose -f deploy/relay/compose.yml \
  -f /etc/pbox-relay/compose.override.yml up -d --build
curl --fail http://127.0.0.1:8080/healthz
```

Expected response: `ok`. The container runs as UID 10001, so the bind-mounted
key must be readable by that user. The enclosing directory has mode `700`
to restrict host access. Use both `-f` arguments for later Compose commands.

The supplied [Compose file](../deploy/relay/compose.yml) publishes TCP 8080.
For HTTPS, configure a reverse proxy; a [Caddy example](../deploy/relay/Caddyfile)
is included. If the proxy runs in a separate container, both containers need a
shared network for the `pbox-relay:8080` address to resolve.

</details>

<details>
<summary><strong>Systemd, including an unprivileged Proxmox LXC</strong></summary>

Use a Linux host or LXC with systemd and network access. On that host, build the
relay from source:

```sh
git clone https://github.com/kierandrewett/pbox.git
cd pbox
cargo build --locked --release -p pbox-relay
```

Transfer the key from step 1 to this host. Install the binary, key and
[service unit](../deploy/relay/pbox-relay.service) as root:

```sh
install -m 755 target/release/pbox-relay /usr/local/bin/pbox-relay
install -d -m 700 /etc/pbox-relay
install -m 600 /path/to/transferred/relay.key /etc/pbox-relay/relay.key
install -m 644 deploy/relay/pbox-relay.service /etc/systemd/system/pbox-relay.service
systemctl daemon-reload
systemctl enable --now pbox-relay
curl --fail http://127.0.0.1:8080/healthz
```

Expected response: `ok`. Systemd gives the key to the service through its
credentials directory. The service uses a dynamic user. Put an HTTPS proxy in
front of it or use an encrypted private network.

</details>

### 3. Configure pbox and verify

After the service and its public/private address are ready, on the machine
running pbox:

```sh
pbox config set relay.url https://relay.example.com
pbox relay check
pbox new --image debian:13
pbox exec current -- true
pbox ssh current
```

Use a box ID instead of `current` when several boxes exist. Configure the relay
before creating boxes; setting a URL does not install relay settings into
existing guests.

## Verify the connection

| Check | Meaning |
| --- | --- |
| `curl --fail http://127.0.0.1:8080/healthz` on the relay host | The local relay HTTP service responds |
| `pbox relay check` | This client reaches the relay and its key matches |
| Successful box creation and `pbox exec BOX -- true` | The guest connects through the relay and authenticated command execution works |

The key check needs a relay version with the `/v1/check` endpoint. It does not
test WebSocket forwarding or connectivity from a guest.

| Failure | Check |
| --- | --- |
| Connection refused / timeout | Service state, listener, firewall and route |
| TLS failure | Hostname, reverse-proxy certificate and local CA trust |
| HTTP 401 | Client and service are using the same key |
| HTTP 404 during credential check | Relay version and proxy routing for `/v1/check/` |
| Health passes but guest connection fails | Guest DNS/outbound route and proxy WebSocket support |

## Sessions and recovery

Agents reconnect after a relay restart. An interrupted shell must be reopened
with `pbox ssh BOX`; commands are not replayed.

```sh
pbox repair BOX
```

Use repair after correcting a failed creation's network or service problem.
It resumes bootstrap and cleanup where possible. To discard the box instead,
use `pbox rm BOX`.

Independent copies should use [pbox snapshots](snapshots.md); restoring a raw
copy with the same agent identity can conflict with the original box.

The relay forwards the encrypted CLI-to-agent connection. It can see connection
metadata but cannot read shell or file contents. The URL's HTTPS layer protects
relay credentials in transit.

**Keep the service key when upgrading.** Replacing it invalidates existing
relay credentials. Deploy the updated binary/container and restart the service;
`pbox update` updates only the CLI.
