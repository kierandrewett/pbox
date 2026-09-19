# PVE visibility access

For enrolled boxes, visibility in PVE grants shell access, including the guest's
root user. No `VM.Console` permission or separate user access list is required.
The relay queries `/api2/json/cluster/resources?type=vm` with the caller's API
token for every credential request. Token permissions are the effective PVE
permissions, including privilege separation.

The relay operator maintains a JSON file with the trusted PVE origin and bindings:

```json
{
  "pve_url": "https://pve.example.com",
  "boxes": { "pbx_12345678": 9002 }
}
```

Run the relay with `--access-file /path/to/access.json` and
`--authority-key-file /path/to/authority.key` in addition to its existing relay
key file. Generate the authority key on the relay host under `umask 077`:

```sh
openssl rand -hex -out /path/to/authority.key 32
```

Keep it separate from the
legacy relay key, which existing operator clients can hold. Never distribute the
authority key to clients or guests. The caller cannot select the PVE origin or
change the bindings. When a
VM is deleted or its ID is reused, remove its binding before reuse and enrol the
new box with a new pbox ID.

On the trusted relay host, export guest identities with:

```sh
pbox-relay --key-file /path/to/relay.key --access-file /path/to/access.json \
    --authority-key-file /path/to/authority.key \
    --export-guest pbx_12345678 --guest-directory /path/to/new-directory
```

The output directory must not exist. Back up `/etc/pbox/server.pem`,
`/etc/pbox/server-key.pem`, and `/etc/pbox/client-ca.pem` in the guest. Transfer the
three exported files through verified host access, preserve private file
permissions, and restart only `pbox-agent.service`. Keep `/etc/pbox/relay.json`.
The existing agent supports the new trust files without an agent binary update.

On each client, configure its own PVE API token and the trusted HTTPS relay, then:

```sh
pbox config unset relay.key-file
pbox ssh BOX
```

The relay is trusted to receive the caller's PVE token and issue access. It does
not send that token to the guest or store it. HTTPS certificate verification is
required; redirects are rejected. Responses with private credentials use
`Cache-Control: no-store`. Client certificates and relay tokens last five minutes.
Each box has a separate certificate authority derived from the server-only
authority key. This key and the signing keys stay on the relay host. The legacy
relay key alone cannot generate credentials trusted by an enrolled guest.
Terminal traffic retains
mutual TLS through the relay.

Revoking PVE visibility prevents new credentials immediately. Already-issued
credentials remain usable for at most five minutes. Existing terminal connections
are not disconnected when the token expires. To end an active connection, close
the terminal or restart the guest network agent.

Migration is explicit. Legacy clients with a configured relay key continue to use
their old trust root, and cannot access a migrated guest. Restore the three guest
backup files and the client's relay key setting to roll back. Box provisioning
and snapshot bootstrap still use the legacy operator workflow; enrol each new box
before using PVE visibility access. Direct connections without a relay are unchanged.

## Deployment verification, 19 September 2026

The relay at `https://pbox.drewett.dev` enrols `ornadb-dev` (`pbx_6wjhtv6h`,
VMID 9002) against `https://pve.drewett.dev`. Its binding file is
`/config/apps/pbox-relay/access.json` on `bsociety`. Other existing boxes are not
yet enrolled.

The installed local CLI connected through `pbox ssh ornadb-dev` without a
configured relay key file and ran commands as the guest's `pbox` and `root` users.
The legacy client configuration failed TLS verification against the new guest.
Live
issuer requests without credentials returned 401; an invalid PVE token returned
403. Automated tests cover hidden VMs, revoked credentials, expiry, cross-box
isolation, and upstream failures. No live PVE permissions were changed for tests.
The signing key is `/config/apps/pbox-relay/authority.key` on the relay host.
It was generated on that host and was not copied to the client or guest.

The deployment used `cd /srv && ./up.sh pbox-relay`. Relay backups are in
`/config/apps/pbox-relay/migration-20260919/`: `compose.yml` and
`pbox-relay.previous`. Guest trust backups are in `/etc/pbox/backup-20260919/`.
Only `pbox-agent.service` was restarted; the existing terminal-host process
remained running. The old local relay key is retained for operator provisioning
and rollback, but is no longer configured for shell connections.
