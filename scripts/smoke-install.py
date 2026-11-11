"""Install a packaged archive with the release install scripts from a local release server."""

import argparse
import hashlib
import http.server
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
from contextlib import contextmanager
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


@contextmanager
def release_server(directory, tag):
    """Serves `directory` like GitHub release downloads; yields the releases URL.

    `/latest` redirects to the tag page, or to the release list when `tag` is
    None, as GitHub does before a first release.
    """

    class Handler(http.server.BaseHTTPRequestHandler):
        def do_HEAD(self):
            self.respond(head=True)

        def do_GET(self):
            self.respond(head=False)

        def respond(self, head):
            base = f"http://{self.headers['Host']}/releases"
            path = self.path.split("?", 1)[0]
            if path == "/releases/latest":
                self.send_response(302)
                self.send_header("Location", f"{base}/tag/{tag}" if tag else base)
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            body = None
            if path == "/releases" or (tag and path == f"/releases/tag/{tag}"):
                body = b"release page"
            elif tag and path.startswith(f"/releases/download/{tag}/"):
                file = Path(directory) / path.rsplit("/", 1)[1]
                if file.is_file():
                    body = file.read_bytes()
            if body is None:
                self.send_error(404)
                return
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if not head:
                self.wfile.write(body)

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_address[1]}/releases"
    finally:
        server.shutdown()
        server.server_close()


def publish(directory, tag, archives):
    """Copies archives, install scripts and the checksum manifest into `directory`."""
    names = []
    for source, name in archives:
        shutil.copyfile(source, Path(directory) / name)
        names.append(name)
    for script in ("install.sh", "install.ps1"):
        shutil.copyfile(ROOT / "scripts" / script, Path(directory) / script)
    lines = [f"{hashlib.sha256((Path(directory) / name).read_bytes()).hexdigest()}  {name}\n" for name in names]
    (Path(directory) / f"iroh-toolbox-{tag}-sha256sums.txt").write_text("".join(lines), newline="\n")


def run(command, env, expect_success=True):
    result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=300)
    output = result.stdout + result.stderr
    if (result.returncode == 0) != expect_success:
        raise SystemExit(f"unexpected exit {result.returncode} for {command}:\n{output}")
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", nargs="?", type=Path,
                        help="archive to install (default: the only archive in dist/)")
    parser.add_argument("--tag", help="release tag (default: from the archive name)")
    parser.add_argument("--target", help="serve the archive as this target (default: from its name; "
                                         "a host GNU/Linux development archive stands in for musl)")
    parser.add_argument("--shell", default="powershell" if os.name == "nt" else "sh",
                        help="sh, powershell or pwsh")
    parser.add_argument("--modify-path", action="store_true",
                        help="on Windows, let the installer edit the user PATH (CI runners only)")
    args = parser.parse_args()

    if args.archive is None:
        found = sorted(Path("dist").glob("iroh-toolbox-*.tar.gz")) + sorted(Path("dist").glob("iroh-toolbox-*.zip"))
        if len(found) != 1:
            parser.error("pass the archive to install; run mise run dist first" if not found
                         else f"several archives in dist/: {', '.join(path.name for path in found)}")
        args.archive = found[0]
    archive = args.archive.resolve(strict=True)
    parsed = re.fullmatch(r"iroh-toolbox-(v.+?)-((?:x86_64|aarch64)-.+)(\.tar\.gz|\.zip)", archive.name)
    if not parsed:
        parser.error("expected an iroh-toolbox-<label>-<target> archive")
    tag = args.tag or parsed[1]
    target = args.target or parsed[2].replace("x86_64-unknown-linux-gnu", "x86_64-unknown-linux-musl")
    suffix = parsed[3]
    windows = suffix == ".zip"
    name = f"iroh-toolbox-{tag}-{target}{suffix}"

    with tempfile.TemporaryDirectory() as temporary:
        release = Path(temporary) / "release"
        release.mkdir()
        publish(release, tag, [(archive, name)])
        bin_dir = Path(temporary) / "bin dir"
        with release_server(release, tag) as url:
            env = {**os.environ, "IROH_TOOLBOX_INSTALL_BASE_URL": url}
            if args.shell == "sh":
                fetch = f"curl -fsSL {url}/download/{tag}/install.sh"
                output = run(["sh", "-c", f'{fetch} | sh -s -- --bin-dir "$1"', "sh", str(bin_dir)], env)
                executable = bin_dir / "iroh-toolbox"
                again = run(["sh", "-c", f'{fetch} | sh -s -- --version "$1" --bin-dir "$2"',
                             "sh", tag, str(bin_dir)], env)
            else:
                script = f"{url}/download/{tag}/install.ps1"
                env["IROH_TOOLBOX_INSTALL_DIR"] = str(bin_dir)
                shell = [args.shell, "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"]
                if args.modify_path:
                    output = run(shell + [f"irm {script} | iex"], env)
                else:
                    output = run(shell + [f"& ([scriptblock]::Create((irm {script}))) -NoModifyPath"], env)
                executable = bin_dir / "iroh-toolbox.exe"
                again = run(shell + [f"& ([scriptblock]::Create((irm {script}))) -Version {tag} -NoModifyPath"], env)
            if "(latest)" not in output or "checksum   verified" not in output or not executable.is_file():
                raise SystemExit(f"latest installation did not complete:\n{output}")
            if "replaced" not in again:
                raise SystemExit(f"reinstallation did not report the replaced version:\n{again}")
            if args.modify_path and windows:
                user_path = run([args.shell, "-NoProfile", "-Command",
                                 "[Environment]::GetEnvironmentVariable('Path', 'User')"], env)
                # The runner's TEMP may use 8.3 short names (RUNNER~1) while
                # install.ps1 records the long path: compare directories, not strings.
                entries = [entry.strip() for entry in user_path.split(";")]
                if not any(entry and os.path.isdir(entry) and os.path.samefile(entry, bin_dir)
                           for entry in entries):
                    raise SystemExit(f"user PATH was not updated: {user_path}")

            # A changed archive is refused and leaves the installation untouched.
            installed = executable.read_bytes()
            (release / name).write_bytes((release / name).read_bytes() + b"tampered")
            if args.shell == "sh":
                refused = run(["sh", "-c", f'{fetch} | sh -s -- --bin-dir "$1"', "sh", str(bin_dir)], env,
                              expect_success=False)
            else:
                refused = run(shell + [f"& ([scriptblock]::Create((irm {script}))) -NoModifyPath"], env,
                              expect_success=False)
            if "checksum mismatch" not in refused or executable.read_bytes() != installed:
                raise SystemExit(f"a tampered archive was not refused:\n{refused}")
    print(output, end="")
    print(f"install scripts verified with {name} through {args.shell}")


if __name__ == "__main__":
    sys.exit(main())
