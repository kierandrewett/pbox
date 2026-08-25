# pbox implementation plan

## Goal

Deliver a usable pbox CLI in small increments while keeping PVE as the source
of truth and keeping guest access separate from infrastructure control.

## Completed slices

1. Rust workspace and CLI shell.
2. Typed configuration, redaction, and XDG paths.
3. Public ID and VMID-pattern allocation logic.
4. PVE metadata parsing and preservation.
5. Typed PVE REST client with task polling primitives.
6. Human and JSON renderers for read-only commands.
7. LXC creation, lifecycle operations, and guest-agent bootstrap.
8. Authenticated agent operations for shell, exec, files, and forwarding.
9. Ansible recipe discovery and application.
10. PVE snapshot operations.
11. OCI registry search, image pulls, and template selection.

All completed slices have local unit or fake-PVE coverage. Live PVE and guest
network verification remains outstanding.

## Next dependency order

```text
current-box shell state + command aliases
        |
        +--> fork and agent identity re-keying
        |
        +--> desktop recipe transport
        |
        +--> PVE console relay fallback
        |
        +--> release packaging, upgrades, and end-to-end verification
```

## Product invariants

- PVE VMIDs are never the public box identifier.
- No local database is the source of truth for boxes.
- Secrets never appear in metadata, normal output, or agent messages.
- Normal output and JSON output share the same domain result.
- Failed provisioning preserves enough PVE state for repair.
