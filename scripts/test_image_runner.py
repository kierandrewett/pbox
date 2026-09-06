"""Resource-ownership regressions for the Docker matrix runner (no Docker needed)."""
import argparse
import importlib.util
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("image_runner", Path(__file__).with_name("test-images.py"))
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class CleanupTests(unittest.TestCase):
    def setUp(self):
        self.output = tempfile.TemporaryDirectory()
        self.addCleanup(self.output.cleanup)
        self.suite = runner.Suite(argparse.Namespace(output=self.output.name))
        self.addCleanup(lambda: __import__("shutil").rmtree(self.suite.fixture, ignore_errors=True))
        self.calls = []
        self.owners = {}

        def docker(*args, **_kwargs):
            self.calls.append(args)
            name = args[-1]
            if "inspect" in args:
                if name not in self.owners:
                    return subprocess.CompletedProcess(args, 1, "error: no such object")
                return subprocess.CompletedProcess(args, 0, self.owners[name])
            if "rm" in args:
                self.owners.pop(name, None)
            return subprocess.CompletedProcess(args, 0, "")

        self.suite.docker = docker

    def test_removes_owned_resources_in_dependency_order_and_is_repeatable(self):
        self.suite.containers = ["guest"]
        self.suite.images = ["prepared"]
        self.suite.pulled = ["base"]
        self.suite.pulled_ids = {"base": "sha256:base"}
        self.owners = {"guest": self.suite.run_id, "prepared": self.suite.run_id,
                       "base": "sha256:base", "unrelated": "keep"}
        self.suite.cleanup()
        removals = [call[-1] for call in self.calls if "rm" in call]
        self.assertEqual(removals, ["guest", "prepared", "base"])
        self.assertEqual(self.owners, {"unrelated": "keep"})
        self.suite.cleanup()
        self.assertEqual(self.suite.cleanup_errors, [])

    def test_preserves_retagged_images_and_foreign_containers(self):
        self.suite.containers = ["guest"]
        self.suite.pulled = ["base"]
        self.suite.pulled_ids = {"base": "sha256:old"}
        self.owners = {"guest": "different-run", "base": "sha256:new"}
        self.suite.cleanup()
        self.assertFalse(any("rm" in call for call in self.calls))
        self.assertEqual(len(self.suite.cleanup_errors), 2)

    def test_cleanup_continues_after_an_engine_failure(self):
        self.suite.containers = ["remaining", "broken"]
        self.owners = {"remaining": self.suite.run_id, "broken": self.suite.run_id}
        docker = self.suite.docker

        def fail_one(*args, **kwargs):
            if args[-1] == "broken":
                raise RuntimeError("engine timeout")
            return docker(*args, **kwargs)

        self.suite.docker = fail_one
        self.suite.cleanup()
        self.assertNotIn("remaining", self.owners)
        self.assertEqual(self.suite.cleanup_errors, ["engine timeout"])


if __name__ == "__main__":
    unittest.main()
