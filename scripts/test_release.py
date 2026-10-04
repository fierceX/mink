import importlib.util
import io
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from urllib.error import HTTPError, URLError

ROOT = Path(__file__).resolve().parent.parent


def module(name):
    spec = importlib.util.spec_from_file_location(name, ROOT / f"scripts/{name}.py")
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


release = module("check-release")
publisher = module("publish-core")


class PublishTests(unittest.TestCase):
    def response(self, version="0.6.7", crate="mink-core", yanked=False):
        return io.BytesIO(json.dumps({"version": {"num": version, "crate": crate, "yanked": yanked}}).encode())

    def test_exact_existing_version_skips_publish(self):
        with patch.object(publisher, "urlopen", return_value=self.response()), patch.object(publisher.subprocess, "run") as run:
            publisher.publish("0.6.7", ROOT)
            run.assert_not_called()

    def test_missing_version_publishes_and_propagates_failure(self):
        absent = HTTPError("fixture", 404, "not found", {}, io.BytesIO())
        with patch.object(publisher, "urlopen", side_effect=absent), patch.object(publisher.subprocess, "run") as run:
            publisher.publish("0.6.7", ROOT)
            run.assert_called_once_with(["cargo", "publish", "-p", "mink-core", "--registry", "crates-io", "--locked"], cwd=ROOT, check=True)
        with patch.object(publisher, "urlopen", side_effect=absent), patch.object(publisher.subprocess, "run", side_effect=subprocess.CalledProcessError(43, "cargo")):
            with self.assertRaises(subprocess.CalledProcessError):
                publisher.publish("0.6.7", ROOT)

    def test_registry_errors_do_not_publish_or_claim_success(self):
        for error in [HTTPError("fixture", 403, "denied", {}, io.BytesIO()), HTTPError("fixture", 500, "failure", {}, io.BytesIO()), URLError("offline")]:
            with self.subTest(error=error), patch.object(publisher, "urlopen", side_effect=error), patch.object(publisher.subprocess, "run") as run:
                with self.assertRaises((HTTPError, URLError)):
                    publisher.publish("0.6.7", ROOT)
                run.assert_not_called()

    def test_wrong_identity_yanked_or_malformed_response_fails_closed(self):
        for response in [self.response(version="0.6.6"), self.response(crate="other"), self.response(yanked=True), io.BytesIO(b'{}')]:
            with patch.object(publisher, "urlopen", return_value=response):
                with self.assertRaises((ValueError, KeyError)):
                    publisher.already_published("0.6.7")


class VersionTests(unittest.TestCase):
    def test_optimized_python_still_rejects_the_wrong_tag(self):
        result = subprocess.run([sys.executable, "-O", str(ROOT / "scripts/check-release.py"), "--tag", "v99.0.0"], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match", result.stderr)

    def test_repository_and_tag_match(self):
        version = release.check(ROOT)
        self.assertEqual(release.check(ROOT, f"v{version}"), version)
        with self.assertRaises(ValueError):
            release.check(ROOT, "v99.0.0")

    def test_channel_and_lock_drift_are_rejected(self):
        files = ["Cargo.toml", "Cargo.lock", "pyproject.toml", "README.md", "CHANGELOG.md", "docs/index.html", "docs/integration/rust.md"]
        files += [f"crates/{p}/Cargo.toml" for p in ["mink-core", "mink-cli", "mink-server"]]
        files += ["crates/mink-core/README.md", "crates/mink-server/web/package.json", "crates/mink-server/web/package-lock.json"]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for file in files:
                target = root / file
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / file, target)
            version = release.check(root)
            for file in ["pyproject.toml", "Cargo.lock", "crates/mink-cli/Cargo.toml", "crates/mink-server/web/package-lock.json", "docs/index.html", "docs/integration/rust.md"]:
                path = root / file
                original = path.read_text()
                path.write_text(original.replace(version, "99.0.0"))
                with self.subTest(file=file), self.assertRaises(ValueError):
                    release.check(root)
                path.write_text(original)


if __name__ == "__main__":
    unittest.main()
