# pbox implementation plan

## Goal

Deliver a usable pbox CLI in small increments while keeping PVE as the source
of truth and keeping guest access separate from infrastructure control.

## First vertical slice

1. Rust workspace and CLI shell.
2. Typed configuration, redaction, and XDG paths.
3. Public ID and VMID-pattern allocation logic.
4. PVE metadata parsing and preservation.
5. Typed PVE REST client with task polling primitives.
6. Human and JSON renderers for read-only commands.

This slice is testable without a live Proxmox cluster. It must not claim that
LXC creation or guest access works yet.

## Dependency order

```text
config + domain models
        |
        +--> PVE client + metadata
        |
        +--> UI renderers + read-only CLI
        |
        +--> crypto + protocol
                 |
                 +--> agent client + transport
                              |
                              +--> lifecycle bootstrap
                                         |
                                         +--> recipes + Ansible
```

## Product invariants

- PVE VMIDs are never the public box identifier.
- No local database is the source of truth for boxes.
- Secrets never appear in metadata, normal output, or agent messages.
- Normal output and JSON output share the same domain result.
- Failed provisioning preserves enough PVE state for repair.
