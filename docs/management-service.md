# Management service

This design was superseded by the relay architecture.

Pbox uses the Proxmox API for lifecycle operations and `pbox-agent` for guest
access. When the machine running pbox cannot reach a guest directly, the guest
and CLI connect through the authenticated relay.

See [relay access](relay.md) for the current design and deployment instructions.
