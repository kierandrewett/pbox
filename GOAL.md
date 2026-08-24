# pbox goal

Build a Linux-only developer sandbox CLI backed by Proxmox VE LXC containers.

The first shippable milestone proves the control-plane contract without a hosted
service or local box registry:

- `pbox config` stores validated local configuration and redacts secrets.
- `pbox id` creates and validates stable public IDs such as `pbx_t3yzd9y3`.
- VMID patterns such as `9xxx` allocate the lowest free candidate safely.
- PVE metadata preserves user notes while storing machine-readable ownership.
- A typed PVE client can list resources, inspect containers, and handle tasks.
- `pbox list`, `pbox info`, and JSON output use the same domain data.

The next vertical slice adds real LXC lifecycle operations, then guest access
through the authenticated agent. PVE remains the authoritative box registry.
