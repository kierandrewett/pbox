# Desktop sessions

[Quick start](../README.md#quick-start) · [Recipes](recipes.md)

## Open a desktop

Install [TigerVNC viewer](https://tigervnc.org/) on the machine running pbox,
then install a desktop in the box:

```sh
pbox recipe apply --box-id current desktop/xfce
pbox desktop current
```

Pbox opens a native viewer window through an authenticated tunnel. The local
machine needs a graphical session; shell-only environments can use
`--no-viewer` to keep a tunnel open for another local viewer.

The supplied desktop recipes support Debian, Ubuntu, Arch Linux and CachyOS.
They require the `pbox` account in the box. An image with a custom non-root
user may need that account created before applying a desktop recipe.

## Choose a session

| Recipe | Session |
| --- | --- |
| `desktop/xfce` | `xfce` |
| `desktop/mate` | `mate` |
| `desktop/lxqt` | `lxqt` |

With one desktop installed, the session is selected automatically. With several:

```sh
pbox desktop current --session mate
```

These recipes use X11 sessions with TigerVNC. A Wayland-only desktop needs a
recipe with a suitable backend; installing its packages alone is not enough.

## Reconnect or use another viewer

| Action | Result |
| --- | --- |
| Close the viewer | Applications keep running in the box |
| Run `pbox desktop BOX` again | Reconnect to the session |
| Log out inside the desktop | End that session |
| Stop or reboot the box | End all desktop sessions |

```sh
pbox desktop current --viewer /path/to/tigervnc-compatible-viewer
pbox desktop current --no-viewer
```

`--no-viewer` prints the local endpoint and keeps the tunnel open until Ctrl-C.
`--json` also skips the viewer and emits one JSON endpoint record; the command
continues running.

## Troubleshooting

| Problem | Next step |
| --- | --- |
| Viewer executable missing | Install TigerVNC viewer locally or pass `--viewer` |
| No desktop installed | Apply a desktop recipe successfully before retrying |
| Several desktops installed | Pass `--session NAME` |
| Recipe rejects the guest | Check the supported distributions and required account above |
| Session exits or shows a blank screen | Read `/home/pbox/.local/state/pbox-desktop/NAME/desktop.log` inside the box |

To inspect a log:

```sh
pbox exec current -- tail -n 80 /home/pbox/.local/state/pbox-desktop/xfce/desktop.log
```

VNC listens on guest loopback, and the local tunnel also binds loopback. The
supplied backend uses no extra VNC password: remote access is authenticated by
pbox. Other processes on either machine can access that machine's loopback port.

## Custom desktop recipes

Recipes register session commands in `/etc/pbox-desktops/NAME.json` and install
`/usr/local/bin/pbox-desktop`. The launcher must start or reconnect to a
persistent session, then emit JSON such as:

```json
{"session": "xfce", "port": 5910}
```

The CLI forwards that guest port and opens the viewer. See the
[shared desktop role](https://github.com/kierandrewett/pbox-recipes/tree/main/roles/desktop-vnc)
for an implementation.
