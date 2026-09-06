# Saved environments and checkpoints

A pbox **snapshot** is an independent PVE LXC template with a `psn_` ID and a
human-readable name. Its source box is provenance, not an ownership relationship.
Both capture and restore explicitly request **full clones**, never linked clones.
The resource consumes a VMID and storage in PVE; no filesystem archive is kept on
the workstation. Deleting a source, template, or restored box does not delete the
other full copies.

A **checkpoint** is PVE's native per-container rollback point. Existing checkpoint
operations remain available under `pbox checkpoint`; their API behaviour and JSON
results are retained. Checkpoints disappear with their box.

## Capture

`pbox snapshot create current --name llm-ready` prepares a bootstrap service through
the authenticated guest agent, gracefully shuts down the source, full-clones it,
and converts the stopped copy to a template through the PVE API. The source's
previous running/stopped state is restored and its preparation files are removed.
This interrupts running processes; it is a saved disk environment, not suspended RAM.
Host bind mounts and device passthrough cannot become independent copies and are
rejected. The chosen storage must support PVE container templates/full copies.

The snapshot list includes incomplete copies so a failed operation remains visible.
Snapshot removal targets only its recorded PVE resource. A failed task is never
reported as a completed snapshot.

## Restore and bootstrap identity

`pbox new --snapshot llm-ready` full-clones the template on its PVE node. Disk sizes
are inherited; resource and network overrides use normal PVE configuration fields.
Primary networking defaults to DHCP on the configured bridge. Extra interfaces keep
their bridge/VLAN settings but get fresh MACs and dynamic addressing instead of
copying static addresses and gateways.

The clone initially uses a generated hostname. The source agent's systemd condition
prevents it from connecting under the old box identity. A separate bootstrap agent
then receives fresh credentials and the normal agent unit. Its first boot also
regenerates machine ID and SSH host keys, and restores the source root-directory
permissions (some PVE template storage backends change those permissions). A second
boot makes the new machine ID effective for PID 1. A requested custom hostname is
applied at that point. `--stopped` leaves the personalised box stopped.

Relay bootstrap routes are `BOOTSTRAP_ID~NEW_BOX_ID`. A snapshot receives an
agent-only seed scoped to its bootstrap ID. It can authenticate bootstrap routes
within that namespace, but cannot authenticate clients, normal box routes, or
another snapshot's bootstrap routes. Each clone has its own route; simultaneous
restores cannot consume each other's connections. Mutual TLS remains in place.
Bootstrap credentials and files are removed from the restored box after handoff.
Snapshots, like their underlying images, are trusted root-level guest content.
They remain tied to the pbox authentication context in which they were captured.

The updated relay is required for these bootstrap routes. Direct mode instead
uses the clone's reachable guest address and the same TLS bootstrap identity;
it still requires a network route to the guest. Neither mode needs PVE host SSH,
console injection, hook scripts, or a privileged management container.

## Recovery

Pending clone metadata is written into PVE before the clone task runs, including
requested resource/network overrides and final running/stopped state.
`pbox repair BOX_ID` resumes personalisation after interruption; it can also finish
an already completed credential handoff. The source snapshot need not still exist.

An exclusive preparation directory prevents overlapping captures of one source.
If the controller is killed while capturing, stop that operation first, then use
`pbox snapshot repair-source BOX_ID` to remove its preparation files and restore
its recorded prior running/stopped state. Normal success and handled failures
restore that state automatically.

Names can collide across controllers; use the `psn_` ID when a name is ambiguous.
Cross-node restores and disk resizing are not implemented by `new --snapshot`.
