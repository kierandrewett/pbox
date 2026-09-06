#!/usr/bin/env python3
"""Test production preparation and agent RPC in ordinary, unprivileged Docker containers."""
import argparse
import concurrent.futures
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
LABEL = "dev.pbox.image-test"


class Suite:
    def __init__(self, args):
        self.args = args
        self.run_id = uuid.uuid4().hex[:12]
        self.containers = []
        self.images = []
        self.pulled = []
        self.lock = threading.RLock()
        self.pulled_ids = {}
        self.output = Path(args.output).resolve()
        self.output.mkdir(parents=True, exist_ok=True)
        self.fixture = Path(tempfile.mkdtemp(prefix="pbox-image-test-"))
        self.journal = self.output / f"resources-{self.run_id}.json"
        self.results = []
        self.cleanup_errors = []
        # Never inherit the user's real PVE credentials/configuration into RPC tests.
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("PBOX_")}

    def save_resources(self):
        with self.lock:
            data = dict(run_id=self.run_id, containers=self.containers, images=self.images,
                        pulled=self.pulled, pulled_ids=self.pulled_ids, fixture=str(self.fixture))
            temporary = self.journal.with_suffix(".tmp")
            temporary.write_text(json.dumps(data, indent=2) + "\n")
            temporary.replace(self.journal)

    def command(self, argv, log=None, timeout=900, check=True, env=None):
        result = subprocess.run(argv, cwd=ROOT, env=env or self.env,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                text=True, errors="replace", timeout=timeout)
        if log:
            with log.open("a") as stream:
                stream.write("$ " + " ".join(map(str, argv)) + "\n" + result.stdout)
        if check and result.returncode:
            raise RuntimeError(f"{' '.join(map(str, argv[:3]))} failed ({result.returncode}):\n{result.stdout[-3000:]}")
        return result

    def docker(self, *argv, **kwargs):
        return self.command(["docker", *map(str, argv)], **kwargs)

    def prepare_fixture(self):
        log = self.output / "build.log"
        if not self.args.skip_build:
            self.command(["cargo", "build", "--release", "-p", "pbox-cli", "-p", "pbox-agent"], log=log, timeout=1800)
        env = dict(self.env, PBOX_TEST_FIXTURE=str(self.fixture),
                   PBOX_TEST_AGENT=str((ROOT / "target/release/pbox-agent").resolve()))
        self.command(["cargo", "test", "-p", "pbox-cli", "export_docker_image_fixture", "--", "--ignored", "--exact", "images::tests::export_docker_image_fixture"], log=log, env=env)
        if not (self.fixture / "prepare.sh").exists():
            raise RuntimeError("production fixture exporter did not run")

    def pull(self, rows):
        for image in dict.fromkeys(row["image"] for row in rows):
            if self.docker("image", "inspect", image, check=False).returncode == 0:
                continue
            # Track before pulling: interrupted pulls may still create the tag.
            self.pulled.append(image)
            self.save_resources()
            print(f"Pulling {image}", flush=True)
            self.docker("pull", image, log=self.output / "pull.log")
            self.pulled_ids[image] = self.docker("image", "inspect", "--format", "{{.Id}}", image).stdout.strip()
            self.save_resources()

    def container(self, name):
        with self.lock:
            self.containers.append(name)
            self.save_resources()
        return name

    def rpc(self, endpoint, script, log, timeout=15):
        return self.command([str(ROOT / "target/release/pbox"), "--config", str(self.fixture / "config.toml"),
                             "exec", "--endpoint", endpoint, "pbx_test1234", "--", "/bin/sh", "-ec", script],
                            log=log, timeout=timeout, check=False)

    def wait_agent(self, endpoint, log):
        deadline = time.monotonic() + self.args.boot_timeout
        last = ""
        while time.monotonic() < deadline:
            try:
                result = self.rpc(endpoint, "test $(whoami) = pbox; sudo -n true; infocmp xterm-256color >/dev/null; printf agent-ready", log)
                last = result.stdout
                if result.returncode == 0 and "agent-ready" in result.stdout:
                    return
            except subprocess.TimeoutExpired:
                last = "RPC timed out"
            time.sleep(1)
        raise RuntimeError(f"agent did not become ready: {last[-1500:]}")

    def guardrails(self, boot, log):
        self.docker("cp", self.fixture / "preflight.sh", f"{boot}:/tmp/pbox-preflight.sh")
        self.docker("cp", self.fixture / "user.sh", f"{boot}:/tmp/pbox-user.sh")
        # Reproduce an image that can execute the agent but would block before
        # launching it at first boot. Restore policy before other guardrails.
        self.docker("exec", boot, "rm", "/etc/systemd/system/systemd-firstboot.service")
        interactive = self.docker("exec", boot, "/bin/sh", "/tmp/pbox-preflight.sh", check=False, log=log)
        self.docker("exec", boot, "ln", "-s", "/dev/null", "/etc/systemd/system/systemd-firstboot.service")
        if interactive.returncode == 0 or "Interactive first-boot setup" not in interactive.stdout:
            raise RuntimeError("interactive first-boot wizard was not rejected")
        # Exercise failure diagnostics using the production preflight, not mocked checks.
        self.docker("exec", boot, "/bin/sh", "-ec",
                    "mv /usr/local/bin/pbox-agent /usr/local/bin/pbox-agent.saved; "
                    "printf '#!/bin/sh\\nexit 42\\n' > /usr/local/bin/pbox-agent; chmod 755 /usr/local/bin/pbox-agent", log=log)
        broken = self.docker("exec", boot, "/bin/sh", "/tmp/pbox-preflight.sh", check=False, log=log)
        self.docker("exec", boot, "mv", "/usr/local/bin/pbox-agent.saved", "/usr/local/bin/pbox-agent")
        if broken.returncode == 0 or "pbox-agent cannot run" not in broken.stdout:
            raise RuntimeError("broken agent was not rejected with an actionable diagnostic")
        self.docker("exec", boot, "/bin/sh", "-ec",
                    "mv /etc/systemd/system/pbox-agent.service /tmp/pbox-agent.service; "
                    "ln -s /dev/null /etc/systemd/system/pbox-agent.service", log=log)
        masked = self.docker("exec", boot, "/bin/sh", "/tmp/pbox-preflight.sh", check=False, log=log)
        if masked.returncode == 0 or "Image compatibility check failed:" not in masked.stdout:
            raise RuntimeError("masked agent service was not rejected")
        self.docker("exec", boot, "/bin/sh", "-ec",
                    "printf '%s\\n' 'pbox ALL=(ALL:ALL) PASSWD: ALL' > /etc/sudoers.d/90-pbox; "
                    "/bin/sh -e /tmp/pbox-user.sh; grep -q 'PASSWD: ALL' /etc/sudoers.d/90-pbox; "
                    "! su -s /bin/sh pbox -c 'sudo -n true'", log=log)

    def test_image(self, row):
        name = row["name"]
        log = self.output / f"{name}.log"
        started = time.monotonic()
        result = dict(row, status="failed")
        boot = None
        try:
            result["image_id"] = self.docker("image", "inspect", "--format", "{{.Id}}", row["image"]).stdout.strip()
            prep = self.container(f"pbox-test-{self.run_id}-{name}-prepare")
            self.docker("create", "--name", prep, "--label", f"{LABEL}={self.run_id}", "--user", "0", "--workdir", "/",
                        "--entrypoint", "/bin/sh", row["image"], "-c", (self.fixture / "prepare.sh").read_text(), log=log)
            self.docker("cp", str(self.fixture / "payload") + "/.", f"{prep}:/", log=log)
            prepared = self.docker("start", "--attach", prep, log=log, check=False)
            exit_code = self.docker("inspect", "--format", "{{.State.ExitCode}}", prep).stdout.strip()
            if "reject" in row:
                if exit_code == "0" or row["reject"] not in prepared.stdout:
                    raise RuntimeError(f"expected rejection {row['reject']!r}; got {prepared.stdout[-2000:]}")
                result.update(status="passed", checks=["expected compatibility rejection"])
                return result
            if exit_code != "0" or prepared.returncode:
                raise RuntimeError(f"preparation failed: {prepared.stdout[-3000:]}")
            manifest = self.fixture / f"{name}-ostype"
            self.docker("cp", f"{prep}:/etc/pbox-image-ostype", manifest)
            if manifest.read_text().strip() != row["ostype"]:
                raise RuntimeError("wrong PVE OS type")
            # Test first-boot preset policy offline; never boot systemd on this host.
            empty = self.fixture / f"{name}-machine-id"
            empty.touch()
            self.docker("cp", empty, f"{prep}:/etc/machine-id")
            image = f"pbox-image-test:{self.run_id}-{name}"
            with self.lock:
                self.images.append(image)
                self.save_resources()
            self.docker("commit", prep, image, log=log)
            boot = self.container(f"pbox-test-{self.run_id}-{name}-boot")
            self.docker("run", "--detach", "--name", boot, "--label", f"{LABEL}={self.run_id}",
                        "--publish", "127.0.0.1::7443", "--entrypoint", "/usr/local/bin/pbox-agent", image,
                        "--listen", "0.0.0.0:7443", "--box-id", "pbx_test1234",
                        "--certificate", "/etc/pbox/server.pem", "--private-key", "/etc/pbox/server-key.pem",
                        "--client-ca", "/etc/pbox/client-ca.pem", log=log)
            address = self.docker("port", boot, "7443/tcp").stdout.strip()
            endpoint = "https://" + address
            self.wait_agent(endpoint, log)
            self.docker("exec", boot, "systemctl", "--root=/", "preset-all", log=log)
            self.docker("exec", boot, "systemctl", "--root=/", "is-enabled", "pbox-agent.service", log=log)
            self.docker("restart", "--time", "5", boot, log=log)
            # Docker may allocate a new host port on restart.
            endpoint = "https://" + self.docker("port", boot, "7443/tcp").stdout.strip()
            self.wait_agent(endpoint, log)
            self.guardrails(boot, log)
            result.update(status="passed", checks=["broken agent rejection", "masked service rejection", "existing user policy preserved", "production preparation", "PVE OS type", "offline first-boot preset policy", "authenticated agent RPC", "pbox user", "passwordless sudo", "xterm-256color", "restart and reconnect"])
        except Exception as error:
            result["error"] = str(error)
            if boot:
                self.docker("logs", boot, log=log, check=False)
        finally:
            result["seconds"] = round(time.monotonic() - started, 1)
            print(f"{result['status'].upper():6} {name} ({result['seconds']}s) — {log}", flush=True)
        return result

    def inspect(self, kind, reference, template):
        argv = ["image", "inspect"] if kind == "image" else ["inspect"]
        result = self.docker(*argv, "--format", template, reference, check=False, timeout=30)
        if result.returncode:
            if "no such" in result.stdout.lower() or "not found" in result.stdout.lower():
                return None
            raise RuntimeError(result.stdout.strip() or f"Cannot inspect {reference}")
        return result.stdout.strip()

    def cleanup(self):
        resources = [("container", name) for name in reversed(self.containers)]
        resources += [("image", name) for name in list(reversed(self.images)) + list(reversed(self.pulled))]
        for kind, name in resources:
            try:
                if kind == "image" and name in self.pulled:
                    actual = self.inspect(kind, name, "{{.Id}}")
                    expected = self.pulled_ids.get(name)
                else:
                    actual = self.inspect(kind, name, '{{index .Config.Labels "dev.pbox.image-test"}}')
                    expected = self.run_id
                if actual is None:
                    continue
                if actual != expected:
                    raise RuntimeError(f"Ownership changed or cannot be confirmed: {name}")
                argv = ["image", "rm", name] if kind == "image" else ["rm", "--force", "--volumes", name]
                self.docker(*argv, timeout=60)
            except Exception as error:
                self.cleanup_errors.append(str(error))
        import shutil
        if self.fixture.parent == Path(tempfile.gettempdir()) and self.fixture.name.startswith("pbox-image-test-"):
            shutil.rmtree(self.fixture, ignore_errors=True)
        self.save_resources()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cleanup-only", type=Path, help="Recover cleanup from a resources JSON file after a hard interruption")
    parser.add_argument("--images", help="Comma-separated matrix names; default: all")
    parser.add_argument("--jobs", type=int, default=2)
    parser.add_argument("--boot-timeout", type=int, default=90)
    parser.add_argument("--skip-build", action="store_true", help="Use existing release binaries (fixture exporter still compiles)")
    parser.add_argument("--output", default="test-results/images")
    args = parser.parse_args()
    if args.jobs < 1:
        parser.error("--jobs must be positive")
    rows = json.loads((ROOT / "tests/images/matrix.json").read_text())
    if args.images:
        selected = set(args.images.split(","))
        if selected - {row["name"] for row in rows}:
            parser.error("unknown image name")
        rows = [row for row in rows if row["name"] in selected]
    suite = Suite(args)
    if args.cleanup_only:
        import shutil
        shutil.rmtree(suite.fixture)
        data = json.loads(args.cleanup_only.read_text())
        suite.run_id = data["run_id"]
        suite.containers = data["containers"]
        suite.images = data["images"]
        suite.pulled = data["pulled"]
        suite.pulled_ids = data["pulled_ids"]
        suite.fixture = Path(data["fixture"])
        suite.journal = args.cleanup_only
        suite.cleanup()
        for error in suite.cleanup_errors:
            print(error, file=sys.stderr)
        return int(bool(suite.cleanup_errors))
    suite.save_resources()
    def interrupted(signum, _frame):
        raise KeyboardInterrupt(f"signal {signum}")
    signal.signal(signal.SIGTERM, interrupted)
    failure = None
    try:
        suite.docker("info")
        suite.prepare_fixture()
        suite.pull(rows)
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as pool:
            for result in pool.map(suite.test_image, rows):
                suite.results.append(result)
    except (Exception, KeyboardInterrupt) as error:
        failure = str(error)
        print(f"Suite failed: {error}", file=sys.stderr)
    finally:
        suite.cleanup()
        report = dict(run_id=suite.run_id, results=suite.results, error=failure, cleanup_errors=suite.cleanup_errors)
        (suite.output / "results.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"Cleanup: {'FAILED' if suite.cleanup_errors else 'complete'}. Results: {suite.output / 'results.json'}")
    return int(bool(failure or suite.cleanup_errors or any(row["status"] != "passed" for row in suite.results)))


if __name__ == "__main__":
    sys.exit(main())
