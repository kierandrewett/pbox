# Choosing an image

[Quick start](../README.md#quick-start) · [Recipes](recipes.md)

## Image references

```sh
pbox new --image debian:13
pbox new --image docker.io/library/debian:13
pbox new --image ghcr.io/your-account/your-image:tag
```

Pbox prepares OCI container images with Podman, then runs the filesystem as a
Proxmox LXC. Image architecture, guest tools and startup support must match the
selected creation path.

For a private registry, authenticate on the machine running pbox first:

```sh
podman login ghcr.io
```

See [Podman login](https://docs.podman.io/en/stable/markdown/podman-login.1.html).
Pbox uses local registry credentials to prepare the image.

## Direct and relay creation

| Path | Startup requirements |
| --- | --- |
| Direct | Uses distro init preparation and guest SSH bootstrap; image checks require systemd, OpenRC or runit plus the prepared guest tools |
| Relay | Uses the workspace bootstrap described below; needs PVE support for the LXC `entrypoint` parameter and an outbound route to the relay |

Relay creation accepts `--image`; it cannot personalise an arbitrary existing
PVE `--ostemplate` through the API. Saved environments use the separate
[`--snapshot` path](snapshots.md).

## Relay workspace behaviour

| Image setting | Behaviour |
| --- | --- |
| Systemd present | Bootstrap configures networking and hands control to real systemd |
| No systemd | A small supervisor runs the agent, network client and image application |
| `USER` is root or unset | Interactive sessions use a generated `pbox` account with passwordless sudo |
| Non-root `USER` | Interactive sessions retain that user |
| `ENV` and `WORKDIR` | Used as defaults for shell and command sessions |
| Bare shell `CMD` | Opened when connecting a shell |
| Other `ENTRYPOINT` / `CMD` | Runs as a background application under its configured user; its exit stops the container |

For the generated account, the default `/` working directory becomes
`/home/pbox`. Override session defaults explicitly when needed:

```sh
pbox ssh current --user root
pbox exec current --cwd /tmp -- pwd
```

The workspace bootstrap installs missing networking and sudo dependencies using
apt, dnf, pacman, apk or zypper. Arch-based images receive a full package upgrade
when preparation installs packages. Package-manager support does not guarantee
that every image or recipe is compatible.

## Agent compatibility

The local `pbox-agent` binary is copied into the image and must execute there
before preparation installs packages.

| Guest | Agent build |
| --- | --- |
| Matching glibc distribution and architecture | A compatible native agent build |
| Alpine / musl | A matching musl agent build |
| Mixed glibc and musl images | Static musl agent for that architecture |

The contributor [agent build instructions](development.md#build-the-agent)
produce a portable musl binary. Set `agent.binary` to use it.

## Diagnose a failed image

| Failure stage | What to check |
| --- | --- |
| Pull | Image name/tag, registry access and `podman login` |
| Local preparation | Package repositories, agent architecture/libc and reported missing tools |
| PVE creation | Storage availability, permissions and supported container options |
| Waiting for agent | Guest startup, addressing, DNS and direct/relay reachability |

Use `pbox --verbose new --image IMAGE` for full preparation output. A successful
local preparation does not prove that the LXC can boot or reach the relay.
If a box was created, inspect it in PVE and use `pbox repair BOX` after correcting
the cause.

For image-maintainer testing, see [the image test matrix](development.md#image-tests).
