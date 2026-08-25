# pbox goal

Build a Linux-only developer sandbox CLI backed by Proxmox VE LXC containers.

## Current state

The repository has passed the first control-plane milestone and now contains
these implemented slices:

- validated local configuration with secret redaction;
- stable public IDs such as `pbx_t3yzd9y3`;
- VMID pattern allocation;
- PVE metadata parsing and preservation;
- a typed PVE REST client with task polling;
- human and JSON output for box data;
- LXC creation, lifecycle commands, and guest-agent bootstrap;
- agent-backed shell, command execution, file transfer, and TCP forwarding;
- Ansible recipe discovery and application;
- PVE snapshot operations;
- OCI registry search, image pulls, and template selection for `pbox new`.

These paths still need a live PVE cluster and guest network for end-to-end
verification. Unit tests and fake-PVE tests do not prove a live deployment.

## Remaining product work

The full product specification still includes work that is not in the current
CLI:

- `current` shell resolution and the complete Box-style command aliases;
- box forking with controlled agent identity re-keying;
- desktop recipe transport and the `pbox desktop` command;
- PVE console relay fallback when direct guest access is unavailable;
- agent release packaging, upgrades, and trust rotation commands;
- recipe repository content, release packaging, and operator documentation;
- production end-to-end tests against a disposable PVE environment.

PVE remains the authoritative box registry. The local client must not require a
database to rediscover boxes.
