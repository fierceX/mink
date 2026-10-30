"""Pin the web-embedding build script: failure propagation and input tracking.

The build script under test is copied from `crates/mink-server/build.rs` into a
throwaway Cargo project, so this exercises the real Cargo rerun rules (not a
string assertion). The fake `npm` records every invocation and can fail, which
is how the tests tell an incremental no-op from a rebuild.
"""

import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
BUILD_RS = REPO / "crates" / "mink-server" / "build.rs"

FAKE_NPM = r'''#!/usr/bin/env python3
import os
import pathlib
import sys

state = pathlib.Path(os.environ["FAKE_NPM_STATE"])
state.mkdir(parents=True, exist_ok=True)
runs_file = state / "runs"
runs = int(runs_file.read_text() or "0") if runs_file.exists() else 0
runs += 1
runs_file.write_text(str(runs))
if os.environ.get("FAKE_NPM_FAIL") == "1":
    sys.stderr.write("fake npm: refusing to build\n")
    sys.exit(3)
if os.environ.get("FAKE_NPM_WRITE", "1") == "1":
    dist = pathlib.Path.cwd() / "dist"
    (dist / "assets").mkdir(parents=True, exist_ok=True)
    (dist / "index.html").write_text(
        '<div id="root"></div>\n'
        '<script type="module" src="./assets/app.js"></script>\n'
        '<link rel="stylesheet" href="./assets/style.css">\n',
        encoding="utf-8",
    )
    (dist / "assets" / "app.js").write_text(f"console.log({runs})\n", encoding="utf-8")
    (dist / "assets" / "style.css").write_text(".x{color:red}\n", encoding="utf-8")
'''


def write(path: Path, text: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


class EmbedProject:
    """A throwaway cargo project shaped like the repo for `build.rs`."""

    def __init__(self, root: Path) -> None:
        self.root = root
        self.state = root / "state"
        self.bin = root / "bin"
        self.web = root / "apps" / "web"
        self.crate = root / "crates" / "mink-server"

    @classmethod
    def create(cls) -> "EmbedProject":
        root = Path(tempfile.mkdtemp(prefix="mink-web-embed-"))
        project = cls(root)
        write(root / "Cargo.toml", '[workspace]\nmembers = ["crates/mink-server"]\nresolver = "2"\n')
        write(
            project.crate / "Cargo.toml",
            '[package]\nname = "webembed-fixture"\nversion = "0.1.0"\nedition = "2021"\nbuild = "build.rs"\n',
        )
        write(project.crate / "src" / "main.rs", "fn main() {}\n")
        shutil.copyfile(BUILD_RS, project.crate / "build.rs")
        # Inputs the Vite build reads, plus the baseline dist the fake npm writes.
        write(project.web / "src" / "main.ts", "export const x = 1\n")
        write(project.web / "index.html", '<div id="root"></div>\n')
        write(project.web / "package.json", '{"scripts": {"build": "vite build"}}\n')
        write(project.web / "package-lock.json", '{"lockfileVersion": 3}\n')
        write(project.web / "vite.config.ts", "export default {}\n")
        write(project.web / "tsconfig.json", "{}\n")
        # The build script also declares the git provenance inputs; create them
        # so this test isolates the web-input rerun rules.
        write(root / ".git" / "HEAD", "ref: refs/heads/main\n")
        write(root / ".git" / "refs" / "heads" / "main", "0" * 40 + "\n")
        write(root / ".git" / "packed-refs", "# pack-refs with: peeled fully-peeled sorted\n")
        project.bin.mkdir(parents=True)
        npm = project.bin / "npm"
        npm.write_text(FAKE_NPM, encoding="utf-8")
        npm.chmod(0o755)
        return project

    def destroy(self) -> None:
        # A test may leave mode-000 entries behind; restore before deleting.
        for dirpath, dirnames, filenames in os.walk(self.root):
            for name in dirnames + filenames:
                path = Path(dirpath) / name
                try:
                    if path.is_symlink():
                        continue
                    path.chmod(0o755)
                except OSError:
                    pass
        shutil.rmtree(self.root, ignore_errors=True)

    def env(self, **overrides: str) -> dict[str, str]:
        env = os.environ.copy()
        env["PATH"] = f"{self.bin}{os.pathsep}{env.get('PATH', '')}"
        env["FAKE_NPM_STATE"] = str(self.state)
        env["CARGO_TARGET_DIR"] = str(self.root / "target")
        env.pop("MINK_SERVER_ALLOW_EMPTY_WEB", None)
        env.pop("FAKE_NPM_FAIL", None)
        env.pop("FAKE_NPM_WRITE", None)
        env.update(overrides)
        return env

    def build(self, **overrides: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["cargo", "build", "--offline", "-p", "webembed-fixture"],
            cwd=self.root,
            env=self.env(**overrides),
            capture_output=True,
            text=True,
            timeout=300,
        )

    def npm_runs(self) -> int:
        path = self.state / "runs"
        return int(path.read_text()) if path.exists() else 0

    def out_dir(self) -> Path:
        dirs = sorted((self.root / "target" / "debug" / "build").glob("webembed-fixture-*/out"))
        self_ = [d for d in dirs if (d / "assets.rs").exists()]
        assert self_, f"no build output under {self.root / 'target'}"
        return self_[-1]

    def manifest(self) -> str:
        return (self.out_dir() / "assets.rs").read_text(encoding="utf-8")

    def manifest_if_any(self) -> str | None:
        try:
            path = self.out_dir() / "assets.rs"
        except AssertionError:
            return None
        return path.read_text(encoding="utf-8") if path.exists() else None

    def touch_input(self, relative: str, text: str) -> None:
        path = self.web / relative
        write(path, text)
        # Cargo compares mtimes; make sure the write is strictly newer.
        stamp = path.stat().st_mtime_ns + 1_000_000
        os.utime(path, ns=(stamp, stamp))


class WebEmbedBuild(unittest.TestCase):
    def setUp(self) -> None:
        self.project = EmbedProject.create()

    def tearDown(self) -> None:
        self.project.destroy()

    def test_publishes_a_complete_manifest_and_a_verified_index(self) -> None:
        result = self.project.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        manifest = self.project.manifest()
        self.assertIn('"index.html"', manifest)
        self.assertIn('"assets/app.js"', manifest)
        self.assertIn('"assets/style.css"', manifest)
        published = self.project.out_dir() / "assets"
        self.assertTrue((published / "assets" / "app.js").is_file())
        self.assertEqual(
            (published / "index.html").read_bytes(),
            (self.project.web / "dist" / "index.html").read_bytes(),
        )
        self.assertFalse((self.project.out_dir() / "assets.staging").exists())
        self.assertEqual(self.project.npm_runs(), 1)

    def test_unchanged_inputs_do_not_rerun_the_build(self) -> None:
        self.assertEqual(self.project.build().returncode, 0)
        first = self.project.manifest()
        result = self.project.build()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.project.npm_runs(), 1, "an unchanged build reran npm")
        self.assertEqual(self.project.manifest(), first, "the manifest must be stable")

    def test_build_configuration_and_lockfile_are_tracked_inputs(self) -> None:
        for relative in ("vite.config.ts", "package-lock.json", "index.html"):
            before = self.project.npm_runs()
            self.project.touch_input(relative, f"// changed {relative}\n")
            result = self.project.build()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(
                self.project.npm_runs(),
                before + 1,
                f"changing {relative} must rebuild the frontend",
            )
        before = self.project.npm_runs()
        source = self.project.web / "src" / "main.ts"
        write(source, "export const x = 2\n")
        stamp = source.stat().st_mtime_ns + 1_000_000
        os.utime(source, ns=(stamp, stamp))
        self.assertEqual(self.project.build().returncode, 0)
        self.assertEqual(self.project.npm_runs(), before + 1)

    def test_npm_failure_aborts_by_default(self) -> None:
        result = self.project.build(FAKE_NPM_FAIL="1")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("explicit headless builds", result.stderr)
        self.assertIsNone(self.project.manifest_if_any())

    def test_opt_in_empty_build_drops_stale_assets_and_is_deterministic(self) -> None:
        self.assertEqual(self.project.build().returncode, 0)
        out = self.project.out_dir()
        stale = out / "assets" / "stale.txt"
        stale.write_text("old build", encoding="utf-8")
        self.assertIn("index.html", self.project.manifest())

        result = self.project.build(FAKE_NPM_FAIL="1", MINK_SERVER_ALLOW_EMPTY_WEB="1")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("include_bytes", self.project.manifest())
        self.assertFalse(stale.exists(), "a stale dist must not survive an empty build")
        self.assertFalse((out / "assets").exists())

    def test_missing_index_fails_without_the_opt_in(self) -> None:
        (self.project.web / "dist" / "assets").mkdir(parents=True)
        (self.project.web / "dist" / "assets" / "app.js").write_text("x\n", encoding="utf-8")
        result = self.project.build(FAKE_NPM_WRITE="0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("dist/index.html", result.stderr)
        self.assertIsNone(self.project.manifest_if_any())

    def test_broken_symlink_in_dist_fails(self) -> None:
        dist = self.project.web / "dist"
        (dist / "assets").mkdir(parents=True)
        (dist / "index.html").write_text("<div></div>\n", encoding="utf-8")
        (dist / "assets" / "missing.js").symlink_to(dist / "does-not-exist.js")
        result = self.project.build(FAKE_NPM_WRITE="0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("broken symlink", result.stderr)

    def test_directory_cycle_in_dist_fails(self) -> None:
        dist = self.project.web / "dist"
        (dist / "assets").mkdir(parents=True)
        (dist / "index.html").write_text("<div></div>\n", encoding="utf-8")
        (dist / "loop").symlink_to(dist)
        result = self.project.build(FAKE_NPM_WRITE="0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("directory cycle", result.stderr)

    @unittest.skipUnless(sys.platform != "win32", "POSIX permissions only")
    def test_unreadable_asset_fails(self) -> None:
        dist = self.project.web / "dist"
        (dist / "assets").mkdir(parents=True)
        (dist / "index.html").write_text("<div></div>\n", encoding="utf-8")
        secret = dist / "assets" / "secret.js"
        secret.write_text("secret\n", encoding="utf-8")
        secret.chmod(0o000)
        try:
            secret.read_text(encoding="utf-8")
            privileged = True
        except OSError:
            privileged = False
        if privileged:
            self.skipTest("the test runner can read mode 000")
        result = self.project.build(FAKE_NPM_WRITE="0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("secret.js", result.stderr)

    def test_rebuilt_assets_replace_the_published_bytes(self) -> None:
        self.assertEqual(self.project.build().returncode, 0)
        first = self.project.manifest()
        app = self.project.out_dir() / "assets" / "assets" / "app.js"
        first_bytes = app.read_bytes()
        self.project.touch_input("vite.config.ts", "export default {mode: 2}\n")
        self.assertEqual(self.project.build().returncode, 0)
        second = self.project.manifest()
        self.assertEqual(first, second, "the same asset set keeps a stable manifest")
        self.assertNotEqual(first_bytes, app.read_bytes(), "a rebuild must publish the new bytes")
        # Every include path resolves to the current published tree.
        for line in second.splitlines():
            if "include_bytes!" not in line:
                continue
            path = json.loads(line.split("include_bytes!(", 1)[1].split(")", 1)[0])
            self.assertTrue(Path(path).is_file(), path)


if __name__ == "__main__":
    unittest.main()
