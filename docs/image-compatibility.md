# OCI preparation and local image tests

Run `just test-images` to test the image matrix with local Docker. This builds
pbox and the agent, exports the **production** preparation scripts and payload
with disposable credentials, and tests each image. Python 3, Rust and a running
local Linux Docker engine are required. No PVE or relay configuration is used.

```sh
just test-images
just test-images --images arch,cachyos --jobs 2
just test-images --images debian --skip-build
```

The suite uses ordinary Docker containers and Docker's default isolation. It does
not use privileged mode, host devices, host networking, host PID namespaces,
host mounts or a Docker socket mount. It runs the agent directly; **it does not
boot systemd as PID 1**. Privileged systemd guests are unsuitable for running these
tests on a desktop host.

The matrix is in `tests/images/matrix.json`. Each positive case checks:

- Actual package preparation and the detected PVE OS type.
- Agent binary execution and xterm-256color terminfo.
- Offline systemd preset application, including vendor first-boot policy.
- Authenticated agent RPC as the generated pbox user and passwordless sudo.
- Agent container restart and reconnection.
- Clear rejection of a broken agent binary and a masked agent service.
- Preservation of an existing pbox user's sudo policy.

Alpine is an expected rejection: the current guest integration uses glibc and
systemd, whereas Alpine uses musl and OpenRC. Package manager failures, unsupported
images and incompatible agent binaries stop preparation before any PVE upload.

## Cleanup and evidence

Containers and committed images have a unique run label. A base image already
present locally is reused and retained. Images pulled by the suite are removed
at the end, after its containers and derived images. There is no global prune.
Ownership is checked before deletion, including the ID of a newly pulled image;
changed tags are retained and reported. Ctrl-C and SIGTERM enter cleanup.

Logs and `results.json` are retained under `test-results/images/`, or `--output`.
The resource journal is written before container creation. After a hard kill or
host interruption, recover cleanup with:

```sh
python3 scripts/test-images.py --cleanup-only test-results/images/resources-RUN_ID.json
```

A successful Docker run proves preparation and agent compatibility with those
specific image IDs and the tested agent build. It does **not** prove LXC boot,
PVE's generated network configuration, DHCP, IPv6 routing or relay reachability.
Those require a separate PVE integration test. Rolling tags can change; the
report records the tested image ID. The agent's minimum glibc version depends on
its build toolchain; older distro releases may require a compatible agent build
via `agent.binary` even when their package manager is supported.

## Distribution choices and research

Research checked 6 September 2026. Detection reads `/etc/os-release`, falling
back to `/usr/lib/os-release`, matches `ID` first and then `ID_LIKE`. Metadata is
parsed as data and never sourced or evaluated. This follows the
[systemd os-release contract](https://www.freedesktop.org/software/systemd/man/latest/os-release.html).

| Family | Preparation | Networking prerequisite | PVE type |
| --- | --- | --- | --- |
| Debian | apt-get | ifupdown and ISC DHCP client | debian |
| Ubuntu | apt-get | systemd networkd | ubuntu |
| Fedora | dnf | systemd-networkd | fedora |
| Rocky / Alma / RHEL | dnf, microdnf or yum | NetworkManager | centos |
| Arch / CachyOS | pacman full upgrade | systemd networkd | archlinux |
| openSUSE | zypper | wicked | opensuse |

The implementation preserves configured repositories and signature checking.
Arch uses `pacman -Syu`, avoiding partial upgrades; see the
[official pacman manual](https://man.archlinux.org/man/pacman.8).
CachyOS supplies its own repositories and keyring in its
[official Dockerfile](https://github.com/CachyOS/docker/blob/master/Dockerfile).
Missing or stale image repository keys are errors, not a reason to disable trust.

PVE's [OS detection](https://github.com/proxmox/pve-container/blob/master/src/PVE/LXC/Setup.pm)
uses supported distribution plugins and a fixed alias table, rather than general
ID_LIKE matching. pbox supplies the detected family through the standard `ostype`
create parameter; it does not rewrite os-release. PVE remains responsible for
writing guest networking. Requirements come from its
[Debian](https://github.com/proxmox/pve-container/blob/master/src/PVE/LXC/Setup/Debian.pm),
[Ubuntu](https://github.com/proxmox/pve-container/blob/master/src/PVE/LXC/Setup/Ubuntu.pm),
[Fedora](https://github.com/proxmox/pve-container/blob/master/src/PVE/LXC/Setup/Fedora.pm),
[CentOS](https://github.com/proxmox/pve-container/blob/master/src/PVE/LXC/Setup/CentOS.pm),
[Arch](https://github.com/proxmox/pve-container/blob/master/src/PVE/LXC/Setup/ArchLinux.pm)
and [SUSE](https://github.com/proxmox/pve-container/blob/master/src/PVE/LXC/Setup/SUSE.pm)
implementations. In particular, a standalone dhclient executable is not required
for networkd or NetworkManager.

Package families are not a promise that every release or minimal image works.
Repository contents, PVE version support, agent ABI and actual preflight results
all matter. The matrix defines the concrete releases tested locally.

## Verified matrix

Local Docker run on 6 September 2026: all eight cases passed; cleanup reported no
errors. The agent ran directly in ordinary containers, with offline systemd checks.

| Image | Result |
| --- | --- |
| `docker.io/library/debian:13` | Preparation, agent RPC, restart and guardrails passed |
| `docker.io/library/ubuntu:24.04` | Preparation, agent RPC, restart and guardrails passed |
| `docker.io/library/fedora:44` | Preparation, agent RPC, restart and guardrails passed |
| `docker.io/library/archlinux:base` | Preparation, agent RPC, restart and guardrails passed |
| `docker.io/cachyos/cachyos:latest` | Preparation, agent RPC, restart and guardrails passed |
| `docker.io/rockylinux/rockylinux:10` | Preparation, agent RPC, restart and guardrails passed |
| `docker.io/opensuse/tumbleweed:latest` | Preparation, agent RPC, restart and guardrails passed |
| `docker.io/library/alpine:latest` | Expected rejection passed |
