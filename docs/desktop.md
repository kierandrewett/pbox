# Desktop sessions

Apply an Ansible desktop recipe, then run `pbox desktop BOX` to open a native
VNC viewer window. Install TigerVNC's `vncviewer` locally, or pass
`--viewer EXECUTABLE` for a TigerVNC-compatible viewer.
With several desktops installed, select one with `--session xfce` (or mate/lxqt).
`--no-viewer` prints the local endpoint and keeps forwarding until Ctrl-C.
`--json` implies no viewer and emits one endpoint receipt while the tunnel stays open.

Desktop recipes install a compatible VNC backend and register session argv in
`/etc/pbox-desktops/NAME.json`. The shared Ansible role installs
`/usr/local/bin/pbox-desktop`. The CLI executes that launcher through the existing
authenticated agent. The launcher returns `{ "session": "xfce", "port": 5910 }`
after startup. Other recipe backends can implement the same launcher protocol.

The initial shared backend uses TigerVNC with X11 desktop sessions on Debian and
Ubuntu. XFCE, MATE and LXQt recipes use it. Wayland-only desktops require a
compatible backend recipe; installing their packages alone is insufficient.

The launcher supervises VNC and the desktop independently of agent exec and viewer
connections. Closing the viewer ends the tunnel but preserves applications.
Logging out ends that session; opening it again starts another. Reboot ends all
sessions. Multiple registered desktops receive stable distinct displays.
Logs are in `/home/pbox/.local/state/pbox-desktop/NAME/desktop.log`.

VNC and the local tunnel bind loopback only. VNC has no additional password:
remote access uses agent mutual TLS (also through the relay), while processes
inside the guest or on the controller can access their respective loopback ports.
This follows pbox's single-user developer sandbox trust model.
