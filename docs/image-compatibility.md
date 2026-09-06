# OCI workspaces and image tests

New relay-backed OCI boxes start `pbox-agent --workspace-init` as LXC PID 1.
PVE's supported [`entrypoint` parameter](https://github.com/proxmox/pve-container/blob/master/src/PVE/LXC/Config.pm)
selects this bootstrap, with `ostype=unmanaged`. If the image contains systemd,
the agent writes its networking and agent units, disables interactive first-boot
setup and competing network managers, then uses `exec()` to replace itself with
systemd as PID 1. There is no intermediate reboot. Configuration is repeatable;
individual files are replaced atomically and handoff occurs only after setup
succeeds. Systemd starts the agent as a normal service, so `systemctl`, journald
and ordinary systemd services work. pbox waits for that agent before connecting.

Images without systemd, including minimal Alpine, retain the lightweight
supervisor. pbox does not pretend to implement systemd's API on those images.
The PVE installation must support the entrypoint API parameter. No host Docker
or host shell integration is needed.

The supervisor reaps child processes, restarts the agent and DHCP client, and
stops its children when PVE shuts down the container. It brings up loopback and
uses dhcpcd without udev or systemd-resolved. DHCPv4, DHCPv6 and SLAAC are enabled;
available global addresses remain visible in pbox. Static IPv4 is supported.
An explicit static IPv6 gateway is currently rejected with an actionable error.
Networks must provide working DNS and a route to the configured relay.

OCI `ENV` and `WORKDIR` become the defaults for `pbox exec` and `pbox ssh`.
Images with no `USER`, or root as `USER`, open as `pbox` with `/home/pbox` and
passwordless sudo. A non-root image `USER` is retained. The default `/` working
directory becomes `/home/pbox` for the generated account; `--user root` explicitly
opens a root session.
Explicit command options override them. A bare shell `CMD`, such as CachyOS's
`/usr/bin/bash`, is opened on SSH connection. Other `ENTRYPOINT` + `CMD` values
run once as a background application; its exit stops the container. SSH opens
the image user's shell independently. Images must supply the shell and any
application tools they need. The image application still runs under its configured OCI user.

Preparation installs dhcpcd and sudo if missing, using apt-get, dnf, pacman, apk or
zypper, retaining image repositories and signature verification. Arch/CachyOS
uses a full pacman upgrade. The agent must execute in the image before packages
are installed. The standard `just build` produces a static musl guest agent for Alpine and
glibc distributions. It requires the Rust musl target and a musl C compiler;
for example, set `CC_x86_64_unknown_linux_musl=musl-gcc`. The CLI stays native.
Custom agent builds configured through `agent.binary` must still match the
guest architecture and libc. Package manager support alone is not a
promise that every image works.

## Local tests

```sh
just test-images --workspace --images debian,fedora,cachyos,alpine
just test-images --workspace --images cachyos --skip-build
```

The suite exports production preparation and disposable credentials, then uses
ordinary Docker containers to check authenticated agent RPC, image metadata and
restart/reconnection. It never uses privileged mode, host namespaces, host
mounts, devices or systemd boot. Local Docker tests do not prove LXC boot or relay
networking; those require live PVE verification.

Containers and derived images carry a unique run label. Existing base images
are retained; newly pulled images are removed if their IDs still match. Cleanup
runs on completion, Ctrl-C and SIGTERM, without a global prune. Logs and image IDs
remain in `test-results/images/` (or `--output`). After a hard interruption:

```sh
python3 scripts/test-images.py --cleanup-only test-results/images/resources-RUN_ID.json
```

Without `--workspace`, the suite tests the older distro-init preparation path,
including offline service presets and passwordless sudo. Existing boxes and
legacy templates continue to use that path. Its earlier full eight-image matrix
passed on 6 September 2026, including expected Alpine rejection.

## Why CachyOS previously stalled

The official [CachyOS Dockerfile](https://github.com/CachyOS/docker/blob/master/Dockerfile)
builds an Arch-based shell image, with Bash as its command. Booting that rootfs
through systemd invoked an interactive `systemd-firstboot` wizard and held the
agent behind `sysinit.target`. The previous runtime masks that wizard. The bootstrap now masks that prompt before handing over to systemd; images
without systemd use the lightweight supervisor. Shell sessions still go through
the authenticated agent.

## Initial workspace verification with the glibc build, 6 September 2026

Debian 13, Fedora 44 and CachyOS passed production preparation, authenticated
RPC, restart/reconnection and numeric user/group, environment and working-directory
checks in local Docker. Alpine rejected the glibc agent before package installation.
A separate CachyOS test verified non-root `LD_LIBRARY_PATH` preservation.
All runs completed ownership-checked cleanup.

Live PVE verification covered a fresh CachyOS creation without manual repair,
Bash PTY with xterm-256color, image root user, DNS, agent crash/restart, full
stop/start, and independent snapshot save/restore. The clone connected under
its own ID and IPv4 address. Temporary guests and the snapshot were removed.
This network had no IPv6 router, so routed IPv6 connectivity was not verified.

## Alpine and development-account verification, 6 September 2026

The static musl agent passed the Docker matrix on Alpine 3.24.1, Debian 13 and
CachyOS. Root-default images opened as pbox in /home/pbox with passwordless sudo;
numeric non-root image users and environment overrides still passed. The musl
build handles libc's ioctl argument type and the supervisor supplies a complete
system PATH so Alpine's /sbin/dhcpcd starts under PVE's minimal environment.

A fresh Alpine LXC completed provisioning, relay connection, DNS, interactive
BusyBox shell with xterm-256color, home-directory writes, passwordless sudo and
stop/start persistence. The disposable guest and Docker resources were removed.

## PID 1 handoff verification

A fresh CachyOS guest booted through the agent into real systemd. Both
`pbox-agent.service` and `pbox-network.service` were active; `systemd-run --wait`
executed a transient service successfully. The pbox user, home and passwordless
sudo survived a stop/start cycle. Systemd configuration is tested offline in a
temporary directory; local Docker tests never boot systemd on the desktop host.
