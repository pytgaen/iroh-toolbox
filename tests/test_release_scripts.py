"""Release metadata must reject stale binaries and unfinished release notes."""

import importlib.util
import io
import json
import os
import shutil
import subprocess
import sys
import tarfile
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("package_release", ROOT / "scripts/package-release.py")
package = importlib.util.module_from_spec(spec)
spec.loader.exec_module(package)
spec = importlib.util.spec_from_file_location("smoke_archive", ROOT / "scripts/smoke-archive.py")
smoke_archive = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke_archive)
spec = importlib.util.spec_from_file_location("smoke_install", ROOT / "scripts/smoke-install.py")
smoke_install = importlib.util.module_from_spec(spec)
spec.loader.exec_module(smoke_install)


class PackagingTests(unittest.TestCase):
    def package(self, *, binary_commit="abcdef0", dirty=False, label="v0.1.0"):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Cargo.toml").write_text('[package]\nversion = "0.1.0"\n')
            (root / "LICENSE.txt").write_text("Apache-2.0")
            (root / "NOTICE").write_text("Notice")
            guide = root / "docs/distribution/QUICKSTART.txt"
            guide.parent.mkdir(parents=True)
            guide.write_text("Offline guide")
            binary = root / "binary"
            binary.write_bytes(b"executable fixture")

            def output(args, **kwargs):
                if args[0] == "rustc":
                    return "rustc test\nhost: x86_64-unknown-linux-gnu\n"
                if args[0] == "git":
                    if args[1] == "status":
                        return b" M src/main.rs\n" if dirty else b""
                    return "abcdef0" if "--short=7" in args else "abcdef0" + "1" * 33
                if args[-1] == "-V":
                    return "iroh-toolbox v0.1.0\n"
                return ("iroh-toolbox v0.1.0\n"
                        f"  Commit    {binary_commit}\n"
                        "  Platform  x86_64-unknown-linux-gnu\n")

            argv = ["package-release", "--binary", str(binary), "--label", label]
            with patch.object(package, "ROOT", root), patch.object(sys, "argv", argv), \
                    patch.object(package.subprocess, "check_output", side_effect=output), \
                    redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                package.main()
            with tarfile.open(next((root / "dist").glob("*.tar.gz"))) as archive:
                return json.load(archive.extractfile("BUILD.json"))

    def test_rejects_binary_from_another_commit(self):
        with self.assertRaises(SystemExit) as error:
            self.package(binary_commit="deadbee")
        self.assertEqual(error.exception.code, 2)

    def test_rejects_dirty_stable_archive(self):
        with self.assertRaises(SystemExit) as error:
            self.package(dirty=True)
        self.assertEqual(error.exception.code, 2)

    def test_dev_archive_records_dirty_checkout(self):
        build = self.package(dirty=True, label="v0.1.0-dev")
        self.assertTrue(build["dirty"])
        self.assertTrue(build["commit"].startswith("abcdef0"))

    def test_clean_stable_archive_records_commit(self):
        self.assertFalse(self.package()["dirty"])


class ReleaseNotesTests(unittest.TestCase):
    def notes(self, heading, *, draft=False, version="0.1.0", public=True):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Cargo.toml").write_text('[package]\nversion = "0.1.0"\n')
            (root / "CHANGELOG.md").write_text("# Changelog\n\nNo releases have been published yet.\n")
            if public:
                (root / "CHANGELOG.md").write_text(f"# Changelog\n\n## [{version}] — {heading}\n\n- Feature.\n")
            (root / "notes").mkdir()
            (root / f"notes/changelog-{version}.md").write_text(
                f"## [{version}] — {heading}\n\n- Draft feature.\n")
            args = [sys.executable, str(ROOT / "scripts/release-notes.py"), f"v{version}"]
            if draft:
                args.append("--draft")
            return subprocess.run(args,
                                  cwd=root, capture_output=True, text=True)

    def test_requires_finished_release_notes(self):
        self.assertEqual(self.notes("Unreleased").returncode, 2)
        self.assertEqual(self.notes("2026-99-99").returncode, 2)
        self.assertEqual(self.notes("YYYY-MM-DD").returncode, 2)
        result = self.notes("2026-10-27")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "- Feature.\n")

    def test_preview_uses_draft_without_requiring_version_bump(self):
        result = self.notes("YYYY-MM-DD", draft=True, version="0.2.0", public=False)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "- Draft feature.\n")
        self.assertEqual(self.notes("2026-99-99", draft=True).returncode, 2)

    def test_stable_release_cannot_fall_back_to_draft(self):
        result = self.notes("2026-10-27", public=False)
        self.assertEqual(result.returncode, 2)
        self.assertIn("CHANGELOG.md", result.stderr)

    def test_stable_release_requires_matching_manifest(self):
        self.assertEqual(self.notes("2026-10-27", version="0.2.0").returncode, 2)

    def test_draft_is_required_for_preview(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "Cargo.toml").write_text('[package]\nversion = "0.1.0"\n')
            result = subprocess.run(
                [sys.executable, str(ROOT / "scripts/release-notes.py"), "v0.1.0", "--draft"],
                cwd=root, capture_output=True, text=True)
            self.assertEqual(result.returncode, 2)
            self.assertIn("missing notes/changelog-0.1.0.md", result.stderr)


class ArchiveSmokeTests(unittest.TestCase):
    """The archive smoke check pairs machines by writing their peer books."""

    ALPHABET = "ybndrfg8ejkmcpqxot1uwisza345h769"

    def test_machine_key_is_the_z_base_32_ed25519_public_key(self):
        # RFC 8032 section 7.1, test 1.
        seed = bytes.fromhex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
        expected = "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
        key = smoke_archive.machine_key(seed)
        self.assertEqual(len(key), 52)
        bits = "".join(f"{self.ALPHABET.index(char):05b}" for char in key)[:256]
        self.assertEqual(int(bits, 2).to_bytes(32, "big").hex(), expected)

    def test_pair_appends_a_private_peer_book_entry(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            other = {"name": "b", "machine": "m" * 52, "salt": "salt"}
            smoke_archive.pair(home, "laptop", other)
            smoke_archive.pair(home, "phone", other)
            book = (home / "peers.toml").read_text()
            self.assertIn('[peers.laptop]\nmachine = "' + "m" * 52 + '"', book)
            self.assertIn("[peers.phone]", book)
            self.assertNotIn("access", book)
            if os.name != "nt":
                self.assertEqual((home / "peers.toml").stat().st_mode & 0o777, 0o600)

    def test_relay_binary_is_required(self):
        with tempfile.TemporaryDirectory() as temporary, \
                patch.dict(os.environ, {"IROH_TEST_RELAY_BINARY": ""}):
            with self.assertRaisesRegex(RuntimeError, "IROH_TEST_RELAY_BINARY"):
                smoke_archive.start_relay(Path(temporary), {})


@unittest.skipIf(os.name == "nt", "install.sh targets Linux")
class InstallScriptTests(unittest.TestCase):
    """install.sh against a local server shaped like GitHub release downloads."""

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "home"
        self.home.mkdir()
        self.release = self.root / "release"
        self.release.mkdir()
        executable = self.root / "iroh-toolbox"
        executable.write_text('#!/bin/sh\necho "iroh-toolbox v0.1.0"\n')
        self.archive = self.root / "archive.tar.gz"
        with tarfile.open(self.archive, "w:gz") as archive:
            entry = archive.gettarinfo(executable, "iroh-toolbox")
            entry.mode = 0o755
            with executable.open("rb") as content:
                archive.addfile(entry, content)
        self.published = "iroh-toolbox-v0.1.0-x86_64-unknown-linux-musl.tar.gz"
        smoke_install.publish(self.release, "v0.1.0", [(self.archive, self.published)])

    def install(self, *args, platform=("Linux", "x86_64"), tag="v0.1.0", path=None, base_url=None):
        tools = self.root / "tools"
        tools.mkdir(exist_ok=True)
        uname = tools / "uname"
        uname.write_text(f'#!/bin/sh\ncase "$1" in -s) echo {platform[0]} ;; -m) echo {platform[1]} ;; esac\n')
        uname.chmod(0o755)
        with smoke_install.release_server(self.release, tag) as url:
            env = {"HOME": str(self.home), "PATH": f"{tools}:{path or os.environ['PATH']}",
                   "IROH_TOOLBOX_INSTALL_BASE_URL": base_url or url}
            return subprocess.run(["sh", str(ROOT / "scripts/install.sh"), *args], env=env,
                                  capture_output=True, text=True, timeout=60)

    def test_installs_the_latest_release_after_checking_its_checksum(self):
        result = self.install()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("version    v0.1.0 (latest)", result.stdout)
        self.assertIn("checksum   verified (iroh-toolbox-v0.1.0-sha256sums.txt)", result.stdout)
        self.assertIn('export PATH="$HOME/.local/bin:$PATH"', result.stdout)
        installed = self.home / ".local/bin/iroh-toolbox"
        self.assertTrue(os.access(installed, os.X_OK))
        self.assertIn(f"  installed  {installed}\niroh-toolbox v0.1.0\n", result.stdout)

    def test_version_and_directory_options_and_replacement(self):
        bin_dir = self.root / "custom bin"
        first = self.install("--version", "0.1.0", "--bin-dir", str(bin_dir))
        self.assertEqual(first.returncode, 0, first.stderr)
        self.assertNotIn("(latest)", first.stdout)
        again = self.install("--version", "v0.1.0", "--bin-dir", str(bin_dir))
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertIn("replaced   iroh-toolbox v0.1.0", again.stdout)
        self.assertTrue((bin_dir / "iroh-toolbox").is_file())

    def test_refuses_a_tampered_archive_and_keeps_the_installation(self):
        self.assertEqual(self.install().returncode, 0)
        installed = self.home / ".local/bin/iroh-toolbox"
        before = installed.read_bytes()
        (self.release / self.published).write_bytes(self.archive.read_bytes() + b"tampered")
        result = self.install()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertEqual(installed.read_bytes(), before)

    def test_explains_missing_releases_and_platforms(self):
        cases = [
            (dict(args=["--version", "v9.9.9"]), "release v9.9.9 not found"),
            (dict(args=["--version", "latest"]), "invalid version"),
            (dict(platform=("Linux", "aarch64")), "no release archive for Linux aarch64"),
            (dict(platform=("Darwin", "arm64")), "no release archive for macOS (Darwin arm64)"),
            (dict(platform=("MINGW64_NT-10.0", "x86_64")), "use install.ps1"),
            (dict(tag=None), "no published release found"),
        ]
        for options, message in cases:
            with self.subTest(message):
                result = self.install(*options.pop("args", []), **options)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(message, result.stderr)
        self.assertFalse((self.home / ".local/bin/iroh-toolbox").exists())

    def test_refuses_plain_http_outside_loopback(self):
        for base_url in ["http://example.com/releases", "http://localhost:1@example.com/releases",
                         "ftp://127.0.0.1/releases"]:
            with self.subTest(base_url):
                result = self.install(base_url=base_url)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("IROH_TOOLBOX_INSTALL_BASE_URL must use https", result.stderr)
                self.assertNotIn("iroh-toolbox installer", result.stdout)
        self.assertFalse((self.home / ".local/bin/iroh-toolbox").exists())

    @unittest.skipUnless(shutil.which("wget"), "wget is not installed")
    def test_wget_replaces_a_missing_curl(self):
        tools = self.root / "no-curl"
        tools.mkdir()
        for tool in ["sh", "mktemp", "rm", "awk", "cut", "sed", "tail", "tar", "gzip", "mkdir", "cp",
                     "chmod", "mv", "cat", "wget", "sha256sum", "shasum"]:
            if found := shutil.which(tool):
                (tools / tool).symlink_to(found)
        result = self.install(path=str(tools))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("version    v0.1.0 (latest)", result.stdout)
