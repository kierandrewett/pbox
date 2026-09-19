#!/usr/bin/env python3
"""Check local detach against stalled discovery and TLS, without a VM or secrets."""
import argparse
import os
import pathlib
import pty
import select
import socket
import subprocess
import tempfile
import termios
import threading
import time


def check(cli, discovery, key, term, read_only=False):
    with socket.socket() as server, tempfile.TemporaryDirectory(prefix="pbox-local-control-") as directory:
        server.bind(("127.0.0.1", 0))
        server.listen()
        server.settimeout(5)
        port = server.getsockname()[1]
        connected = threading.Event()
        stop = threading.Event()

        def stall():
            try:
                peer, _ = server.accept()
                with peer:
                    connected.set()
                    stop.wait(10)
            except OSError:
                pass

        worker = threading.Thread(target=stall, daemon=True)
        worker.start()
        config = pathlib.Path(directory) / "config.toml"
        config.write_text(f'[pve]\nurl = "https://127.0.0.1:{port}"\ntoken_id = "test@pam!test"\ntoken_secret = "test-only-secret"\n')
        args = [cli, "--config", str(config), "--color", "never", "ssh", "pbx_12345678"]
        if not discovery:
            args += ["--endpoint", f"https://127.0.0.1:{port}"]
        if read_only:
            args += ["--read-only"]
        master, slave = pty.openpty()
        original = termios.tcgetattr(slave)
        process = subprocess.Popen(args, stdin=slave, stdout=slave, stderr=slave,
                                   env=dict(os.environ, TERM=term, PBOX_NO_UPDATE_CHECK="1"))
        output = bytearray()
        try:
            assert connected.wait(5), "test server did not receive a connection"
            started = time.monotonic()
            os.write(master, key)
            while process.poll() is None and time.monotonic() - started < 1.5:
                if select.select([master], [], [], .02)[0]:
                    output.extend(os.read(master, 65536))
            assert process.poll() == 0, f"detach waited for network: {output!r}"
            while select.select([master], [], [], 0)[0]:
                output.extend(os.read(master, 65536))
            assert termios.tcgetattr(slave) == original, "local terminal settings were not restored"
            assert b"Connecting" in output and b"Detached" in output, output
            assert b"pbox: Connected" not in output, "false connected state"
            if term == "dumb":
                assert b"\x1b]2;" not in output, "title controls on a dumb terminal"
            print(f"PASS: {'discovery' if discovery else 'TLS'} {key!r} TERM={term}, detach {time.monotonic()-started:.3f}s; termios restored")
        finally:
            stop.set()
            if process.poll() is None:
                process.kill()
            process.wait(timeout=5)
            os.close(master)
            os.close(slave)
            worker.join(timeout=1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pbox", default="target/debug/pbox")
    args = parser.parse_args()
    cli = str(pathlib.Path(args.pbox).resolve())
    for discovery in [True, False]:
        for key in [b"\x1d", b"\x1b[93;5u"]:
            for term in ["xterm-256color", "dumb"]:
                check(cli, discovery, key, term)
        check(cli, discovery, b"\x1d", "xterm-256color", read_only=True)
        check(cli, discovery, b"\x03", "xterm-256color", read_only=True)


if __name__ == "__main__":
    main()
