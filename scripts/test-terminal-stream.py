#!/usr/bin/env python3
"""Live terminal regression: python3 scripts/test-terminal-stream.py BOX [--pbox PATH].

Requires a running box with Python. Creates and closes only its own named session.
No terminal emulator dependency: the contract is to preserve guest control bytes.
"""

import argparse
import fcntl
import os
import pathlib
import pty
import select
import signal
import struct
import subprocess
import termios
import time
import uuid


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("box")
    parser.add_argument("--pbox", default="pbox")
    args = parser.parse_args()
    cli = str(pathlib.Path(args.pbox).resolve()) if "/" in args.pbox else args.pbox
    target = f"{args.box}:stream-check-{uuid.uuid4().hex[:10]}"
    env = dict(os.environ, TERM="xterm-256color", PBOX_NO_UPDATE_CHECK="1")
    chunks = [
        b"\r\n\x1b[?2026h\x1b[2;1HREPEAT:a",
        b"\x1b[12b",
        b":END\x1b[4;1H",
        "👨‍👩‍👧‍👦 e\u0301 界".encode(),
        b"\r\nhttps://example.com/plain-link\r\n",
        b"\x1b]8;id=regression;https://example.com/terminal\x1b\\",
        b"selectable hyperlink\x1b]8;;\x1b\\",
        b"\x1b[s\x1b[6;1Hsaved cursor\x1b[u",
        b"\x1b[6 q\x1b[?12l\x1b[?25l\x1b[?25h",
        b"\x1b[1;10r\x1b[r\x1b[?2026l\r\n",
    ]
    keys = b"\x1b[<64;3;5M\x1b[200~paste\x1b[201~"
    # Newlines avoid shell quoting and exec-of-source in the guest fixture.
    fixture = (
        "import os,tty,time\n"
        "tty.setraw(0)\n"
        f"chunks={chunks!r}\n"
        "os.write(1,b'STREAM_BEGIN')\n"
        "for chunk in chunks:\n"
        " for byte in chunk:\n"
        "  os.write(1,bytes([byte]));time.sleep(.002)\n"
        "os.write(1,b'STREAM_END')\n"
        "data=b''\n"
        f"while len(data)<{len(keys)}: data+=os.read(0,{len(keys)}-len(data))\n"
        "size=os.get_terminal_size(0)\n"
        "os.write(1,('\\r\\nINPUT:'+data.hex()+' SIZE:%dx%d'%(size.lines,size.columns)).encode())\n"
    )
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 100, 0, 0))
    process = subprocess.Popen(
        [cli, "ssh", target, "--", "python3", "-c", fixture],
        stdin=slave, stdout=slave, stderr=slave, env=env,
    )
    os.close(slave)
    output = bytearray()

    def until(marker):
        deadline = time.monotonic() + 30
        while marker not in output:
            if time.monotonic() > deadline:
                raise AssertionError(f"Timed out waiting for {marker!r}: {output[-1500:]!r}")
            if select.select([master], [], [], .1)[0]:
                try:
                    data = os.read(master, 65536)
                except OSError as error:
                    raise AssertionError(f"Terminal closed: {output[-1500:]!r}") from error
                if not data:
                    raise AssertionError(f"Terminal closed: {output[-1500:]!r}")
                output.extend(data)

    try:
        until(b"STREAM_END")
        start = output.index(b"STREAM_BEGIN")
        end = output.index(b"STREAM_END", start) + len(b"STREAM_END")
        assert output[start:end] == b"STREAM_BEGIN" + b"".join(chunks) + b"STREAM_END", "pbox changed the guest terminal stream"
        fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
        os.kill(process.pid, signal.SIGWINCH)
        os.write(master, keys)
        until(b"INPUT:" + keys.hex().encode() + b" SIZE:40x120")
        process.wait(timeout=10)
        assert process.returncode == 0, process.returncode
        print("PASS: unchanged terminal controls, Unicode, mouse/paste input and full-size resize")
    finally:
        if process.poll() is None:
            process.terminate()
            process.wait(timeout=5)
        os.close(master)
        subprocess.run(
            [cli, "session", "close", target, "--yes"],
            env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=20,
        )


if __name__ == "__main__":
    main()
