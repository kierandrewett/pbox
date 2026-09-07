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

## Agent computer control

One-shot commands work without a local display, viewer, or TTY. They start or
reconnect to the selected desktop using the same launcher as the viewer, then
exit after the operation. An existing viewer stays attached. No additional guest
packages or agent update are needed for the supplied TigerVNC recipes.

```sh
pbox desktop screenshot current --output screen.png
pbox desktop click current 400 300
pbox desktop type current 'Hello from an agent'
pbox desktop send current --key Ctrl+A --key Backspace
printf 'multiple\nlines\n' | pbox desktop type current --stdin
pbox desktop move current 600 400
pbox desktop click current 600 400 --button right
pbox desktop click current 600 400 --count 2
pbox desktop drag current 200 200 600 400 --duration-ms 800
pbox desktop scroll current 600 400 down --steps 5
```

Every control command accepts `--session NAME` and `--endpoint URL` after its
subcommand. Coordinates are absolute, zero-based pixels in the full screenshot,
with `(0, 0)` at the top left. Out-of-bounds points fail before input is sent.
If displaying a resized screenshot, convert coordinates back to its original
width and height. Drag holds the chosen button along a straight path and releases
it at the destination. Scroll supports `up`, `down`, `left`, and `right`.

`type` sends literal Unicode text as key events (up to 64 KiB of UTF-8); rendering
and input-method support depend on the guest application. `send` accepts repeated
`--key` arguments in order: characters, Enter, Tab, Escape, Backspace, Delete,
Insert, Home, End, PageUp, PageDown, arrows, Space, Plus, Minus, F1–F12, and
combinations with Ctrl, Alt, Shift, or Super. For example, `Ctrl+Alt+Delete` is
one chord; `--key Tab --key Enter` is two sequential presses. Each chord releases
its keys before the next one. Use `--` before positional text beginning with `-`.

### Screenshots and JSON

`screenshot` (alias `read`) saves `desktop.png` in the current directory by
default. `--output PATH` selects a local file and replaces it if it exists.
`--output -` writes only PNG bytes to redirected stdout; it cannot be combined
with `--json` or written directly to a terminal.

```sh
pbox --json desktop screenshot current
pbox --json desktop screenshot current --output screen.png
pbox --json desktop click current 400 300
pbox desktop screenshot current --output - > screen.png
```

JSON screenshots contain `id`, `session`, `action`, `width`, `height`, and
`format: "png"`. Without `--output`, `data_base64` contains the image; with a file
path, `output` contains that path instead. Input receipts contain `id`, `session`,
`action`, `width`, `height`, and `sent: true`.

A successful input receipt means the input was written and a subsequent VNC
screen round trip completed. It does **not** mean the application finished its
work. Take another screenshot to observe the result. Commands never retry input;
a connection failure or the 30-second control timeout can mean partial delivery.
Inspect the screen before deciding whether to repeat an action. Avoid simultaneous
input from multiple agents or a human viewer, as they share the same desktop.

Control currently supports RFB 3.8 with the supplied loopback
`SecurityTypes=None` backend, accessed through pbox authentication. Unsupported
VNC security/versions fail explicitly. Screens are limited to 16,777,216 pixels;
these commands do not resize the desktop.
