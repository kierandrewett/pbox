# Architecture and validation

| Responsibility | Component |
| --- | --- |
| Authoritative inventory and lifecycle | Proxmox VE API |
| Commands and orchestration | pbox CLI |
| Local inventory refresh | pboxd cache, rebuilt from PVE |
| Guest access | pbox-agent with mutual TLS |
| Access without a direct guest route | Authenticated connection relay |
| Software installation | Ansible recipes |
| Reusable environments | Full PVE copies with fresh restored identities |

PVE VMIDs and public pbox IDs are distinct. Local caches are not authoritative.
Interrupted provisioning must retain enough state for repair.

Validate changes with the [contributor checks](docs/development.md#checks).
Image tests and live PVE tests cover different boundaries; see
[image validation](docs/development.md#image-tests).
