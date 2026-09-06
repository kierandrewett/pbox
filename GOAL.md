# Project scope

Pbox provides Linux development environments on Proxmox LXC: create a box,
install tools, open a shell or desktop, and save an environment for reuse.

The CLI manages infrastructure through the Proxmox API. The guest agent handles
shells, commands, files and forwarding, directly or through the relay. Recipes
are Ansible playbooks.

See the [user guide](README.md) for supported workflows and
[contributor guide](docs/development.md) for implementation and validation.
