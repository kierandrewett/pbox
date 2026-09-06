# Follow-up work

Use [GitHub issues](https://github.com/kierandrewett/pbox/issues) to track work.

Current documented limitations:

- Publish the CLI and agent packages and verify installation from a clean environment.
- Align release agent archives with the portable musl build.
- Snapshot restore does not support cross-node placement or disk resizing.
- The relay workspace path does not support an explicit static IPv6 gateway.
- Desktop recipes require a compatible VNC backend; Wayland-only sessions need their own recipe.

See the [documentation index](docs/README.md) for current behaviour.
