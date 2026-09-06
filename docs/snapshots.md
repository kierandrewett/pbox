# Saved environments and checkpoints

[Quick start](../README.md#quick-start) · [Storage setup](configuration.md#placement-and-resources)

| | Snapshot | Checkpoint |
| --- | --- | --- |
| Use it to | Create new boxes with the same installed tools and files | Undo changes to one box |
| Stored as | Independent Proxmox LXC template | Proxmox rollback point on the box |
| Identifier | `psn_` ID or chosen name | Checkpoint name plus box ID |
| Survives deleting the source box | Yes | No |
| Needs | Storage supporting full container copies/templates | Storage supporting native snapshots |

## Save an environment

```sh
pbox snapshot create current --name tools-ready
pbox snapshot list
pbox snapshot info tools-ready
```

Save work first: capture stops the source box while making a full disk copy,
then restores its previous running/stopped state. Running processes and RAM
are not saved.

Capture shows five timed stages and live PVE task logs. Some storage backends
report transfer totals only after copying finishes; elapsed time continues to
update while waiting. Use `--verbose` to retain the logs. Redirected output gets
a progress line every five seconds, and `--json` keeps the result machine-readable.

The snapshot uses a separate Proxmox VMID and disk space. It contains the box's
files, including application configuration and credentials stored there.
Host bind mounts and device passthrough are rejected because they cannot become
independent copies.

Names accept 1–63 letters, digits, hyphens or underscores and cannot start with
`psn_`. Use the snapshot ID if a name is ambiguous.

## Create a box from a snapshot

```sh
pbox new --snapshot tools-ready
```

The new box is a full copy with its own box ID, agent credentials, machine ID
and SSH host keys. Deleting the original box or snapshot leaves existing copies
intact.

| Setting | Restore behaviour |
| --- | --- |
| Node | Same PVE node as the snapshot |
| Disk size | Inherited; resizing during restore is not supported |
| Primary network | DHCP on the configured bridge by default |
| Extra interfaces | Preserve bridge/VLAN settings with fresh MACs and dynamic addresses |
| `--stopped` | Complete initialisation, then leave the new box stopped |

Snapshots remain tied to the pbox authentication context used to capture them.
They are not a portable replacement for publishing an OCI image.

## Delete a saved environment

```sh
pbox snapshot rm tools-ready
```

Only the saved template is removed. Existing full copies keep their data.

## Checkpoints

```sh
pbox checkpoint create current before-change
pbox checkpoint list current
pbox checkpoint rollback current before-change --start
pbox checkpoint delete current before-change
```

Rollback restores the box's saved disk state and prompts for confirmation.
`--start` starts it afterwards. Checkpoint names are positional arguments;
`--name` is used for independent snapshot creation.

Native snapshot support depends on the storage backend. See
[Proxmox storage](https://pve.proxmox.com/pve-docs/chapter-pvesm.html).
Recipe rollback uses this native mechanism; its
[policy is configurable](recipes.md#rollback-and-storage).

## Recover an interrupted operation

| Operation | Recovery |
| --- | --- |
| Creating a box from a snapshot | `pbox repair BOX` resumes initialisation |
| Capturing a source box | After ensuring capture is no longer running, use `pbox snapshot repair-source BOX` |
| Incomplete saved copy | Inspect `pbox snapshot list` / `info`, then remove the failed copy if no longer needed |

Source repair removes preparation files and restores the recorded prior state.
Incomplete copies stay visible so they can be inspected and removed.

<details>
<summary>How restored identities work</summary>

The source agent is prevented from connecting as the old box. The systemd service
or workspace supervisor runs a bootstrap agent to give the clone a fresh
identity, then removes bootstrap credentials.
Relay restores use separate routes for each clone; direct restores need a route
to the guest. Restore may boot more than once while installing the new identity.

</details>
