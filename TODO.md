# Follow-up work

PVE visibility authentication:

- [x] Issue per-box client credentials after a live PVE visibility check.
- [x] Connect the CLI without a local relay key and provide guest enrolment.
- [x] Test denial, expiry, cross-box isolation, and authenticated shell access.
- [x] Deploy and migrate ornadb-dev through verified host access.

Use [GitHub issues](https://github.com/kierandrewett/pbox/issues) to track work.

Current documented limitations:

- Snapshot restore does not support cross-node placement or disk resizing.
- The relay workspace path does not support an explicit static IPv6 gateway.
- Desktop recipes require a compatible VNC backend; Wayland-only sessions need their own recipe.

See the [documentation index](docs/README.md) for current behaviour.
