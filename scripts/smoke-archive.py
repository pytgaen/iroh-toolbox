"""Verify and exercise packaged executables outside the development checkout."""

import argparse
import hashlib
import http.server
import json
import os
import queue
import shlex
import socket
import subprocess
import tarfile
import tempfile
import threading
import time
import urllib.request
import zipfile
from contextlib import ExitStack
from pathlib import Path


def unpack(archive, destination):
    checksum_file = archive.with_name(archive.name + ".sha256")
    checksum, name = checksum_file.read_text().strip().split(maxsplit=1)
    if name != archive.name or hashlib.sha256(archive.read_bytes()).hexdigest() != checksum:
        raise ValueError("archive checksum mismatch")
    executable = "iroh-toolbox.exe" if archive.suffix == ".zip" else "iroh-toolbox"
    expected = {executable, "LICENSE.txt", "NOTICE.txt", "QUICKSTART.txt", "BUILD.json"}
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as source:
            if set(source.namelist()) != expected or len(source.namelist()) != len(expected):
                raise ValueError("unexpected archive contents")
            for name in expected:
                (destination / name).write_bytes(source.read(name))
    else:
        with tarfile.open(archive) as source:
            members = source.getmembers()
            if {m.name for m in members} != expected or len(members) != len(expected) or not all(m.isfile() for m in members):
                raise ValueError("unexpected archive contents")
            for name in expected:
                (destination / name).write_bytes(source.extractfile(name).read())
    binary = destination / executable
    binary.chmod(0o755)
    build = json.loads((destination / "BUILD.json").read_text())
    if hashlib.sha256(binary.read_bytes()).hexdigest() != build["binary_sha256"]:
        raise ValueError("executable checksum mismatch")
    return binary, build


P = 2**255 - 19
D = -121665 * pow(121666, -1, P) % P
BASE_Y = 4 * pow(5, -1, P) % P


def _add(a, b):
    (x1, y1), (x2, y2) = a, b
    t = D * x1 * x2 * y1 * y2 % P
    return ((x1 * y2 + x2 * y1) * pow(1 + t, -1, P) % P,
            (y1 * y2 + x1 * x2) * pow(1 - t, -1, P) % P)


def _base_point():
    y = BASE_Y
    x = pow((y * y - 1) * pow(D * y * y + 1, -1, P) % P, (P + 3) // 8, P)
    if (x * x - (y * y - 1) * pow(D * y * y + 1, -1, P)) % P:
        x = x * pow(2, (P - 1) // 4, P) % P
    return (x if x % 2 == 0 else P - x, y)


def machine_key(seed):
    """z-base-32 Ed25519 public key of a machine seed (RFC 8032), as peers.toml stores it."""
    digest = bytearray(hashlib.sha512(seed).digest()[:32])
    digest[0] &= 248
    digest[31] = (digest[31] & 127) | 64
    scalar, point, result = int.from_bytes(digest, "little"), _base_point(), (0, 1)
    while scalar:
        if scalar & 1:
            result = _add(result, point)
        point, scalar = _add(point, point), scalar >> 1
    x, y = result
    key = (y | (x & 1) << 255).to_bytes(32, "little")
    bits = "".join(f"{byte:08b}" for byte in key).ljust(260, "0")
    alphabet = "ybndrfg8ejkmcpqxot1uwisza345h769"
    return "".join(alphabet[int(bits[i:i + 5], 2)] for i in range(0, 260, 5))


def identity(binary, home, env, name):
    """Creates a machine in `home`; returns what another machine's peer book needs."""
    env = dict(env, IROH_TOOLBOX_HOME=str(home))
    for args in (["id"], ["id", "rename", name]):
        subprocess.run([str(binary), *args], env=env, check=True, capture_output=True)
    values = {}
    for line in (home / "identity").read_text().splitlines():
        key, _, value = line.partition(" = ")
        values[key] = value.strip('"')
    return {"name": name, "machine": machine_key(bytes.fromhex(values["machine_seed"])), "salt": values["salt"]}


def pair(home, local_name, other):
    """Adds `other` to the peer book of `home`, as `pair` leaves it."""
    with open(home / "peers.toml", "a", encoding="utf-8") as book:
        book.write(f'[peers.{local_name}]\nmachine = "{other["machine"]}"\nsalt = "{other["salt"]}"\n'
                   f'announced = "{other["name"]}"\npaired = "2026-11-01T10:12:00Z"\n')
    if os.name != "nt":
        (home / "peers.toml").chmod(0o600)


def start_relay(root, env):
    """Starts the official iroh-relay named by IROH_TEST_RELAY_BINARY on a loopback port."""
    relay = os.environ.get("IROH_TEST_RELAY_BINARY")
    if not relay or not Path(relay).is_file():
        raise RuntimeError("IROH_TEST_RELAY_BINARY must point to the official iroh-relay "
                           "(mise run test:relay:install); the archive smoke check needs a local relay")
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
    config = root / "relay.toml"
    config.write_text(f'http_bind_addr = "127.0.0.1:{port}"\nenable_quic_addr_discovery = false\n'
                      'enable_metrics = false\n', encoding="utf-8")
    process = subprocess.Popen([relay, "--dev", "--config-path", str(config)], env=dict(env, RUST_LOG="off"),
                               stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        try:
            socket.create_connection(("127.0.0.1", port), timeout=0.5).close()
            return process, f"http://127.0.0.1:{port}"
        except OSError:
            if process.poll() is not None:
                break
            time.sleep(0.1)
    process.kill()
    raise RuntimeError("local relay did not start")


class Peer:
    def __init__(self, binary, work, env, *args):
        self.process = subprocess.Popen([str(binary), *args], cwd=work, env=env,
                                        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, text=True, encoding="utf-8")
        self.lines = [queue.Queue(), queue.Queue()]
        self.logs = [[], []]
        self.readers = []
        for index, stream in enumerate([self.process.stdout, self.process.stderr]):
            def read(index=index, stream=stream):
                for line in stream:
                    self.logs[index].append(line)
                    self.lines[index].put(line.strip())
            thread = threading.Thread(target=read, daemon=True)
            thread.start()
            self.readers.append(thread)

    def line(self, index=0):
        try:
            return self.lines[index].get(timeout=20)
        except queue.Empty:
            raise RuntimeError("no expected output: " + "".join(self.logs[1])) from None

    def wait(self, success=True):
        code = self.process.wait(timeout=30)
        for reader in self.readers:
            reader.join(timeout=2)
        if (code == 0) != success:
            raise RuntimeError("unexpected exit: " + "".join(self.logs[1]))
        return "".join(self.logs[0]), "".join(self.logs[1])

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=5)
        for reader in self.readers:
            reader.join(timeout=2)


def smoke(archive):
    with tempfile.TemporaryDirectory(prefix="iroh-archive-") as temporary, ExitStack() as cleanup:
        root = Path(temporary)
        binary, build = unpack(archive, root)
        env = {key: value for key, value in os.environ.items()
               if not key.startswith("IROH_") and not key.lower().endswith("_proxy")}
        env["NO_PROXY"] = "*"
        # The extracted executable must work without Rust/Python or the checkout
        # on its PATH. Python remains only the parent process driving the test.
        env["PATH"] = str(root)
        actual = subprocess.check_output([str(binary), "--version"], cwd=root, env=env, text=True).strip()
        if actual != build["version_output"]:
            raise RuntimeError("packaged version mismatch")
        work_a, work_b = root / "a", root / "b"
        for work in (work_a, work_b):
            (work / "home").mkdir(parents=True)
        source = work_a / "my-folder"
        (source / "nested").mkdir(parents=True)
        (source / "nested" / "hello.txt").write_bytes(b"hello from A\n")
        (source / "zero").write_bytes(b"")
        # Two machines on one host: each has its own identity and peer book.
        relay, relay_url = start_relay(root, env)
        cleanup.callback(relay.wait, 5)
        cleanup.callback(relay.kill)
        envs = {work: dict(env, IROH_TOOLBOX_HOME=str(work / "home")) for work in (work_a, work_b)}

        def peer(work, *args):
            result = Peer(binary, work, envs[work], "--relay", relay_url, *args)
            cleanup.callback(result.close)
            return result

        def send(output="./received"):
            sender = peer(work_a, "send", "./my-folder", "--no-verify")
            code = sender.line()
            if len(code.split("-")) != 6:
                raise RuntimeError("send did not print a six-word code")
            for _ in range(20):
                suggestion = sender.line(1)
                if suggestion.startswith("iroh-toolbox "):
                    break
            else:
                raise RuntimeError("no remote command suggestion")
            # The suggestion leaves the code out of the process list: it is typed
            # at a prompt. Without a terminal, B passes it as `code:WORDS`.
            command = shlex.split(suggestion)
            if command != ["iroh-toolbox", "--relay", relay_url + "/", "receive", "code:",
                           "--no-verify", "--output", "./received"]:
                raise RuntimeError("unexpected remote command: " + suggestion)
            command[4] = f"code:{code}"
            command[-1] = output
            receiver = Peer(binary, work_b, envs[work_b], *command[1:])
            cleanup.callback(receiver.close)
            return sender, receiver

        sender, receiver = send()
        stdout, stderr = receiver.wait()
        if stdout or "BLAKE3 verified" not in stderr:
            raise RuntimeError("missing verified receiver result")
        sender.wait()
        for name in ["nested/hello.txt", "zero"]:
            if (source / name).read_bytes() != (work_b / "received/my-folder" / name).read_bytes():
                raise RuntimeError("received content mismatch")
        sender, receiver = send()
        _, stderr = receiver.wait(success=False)
        if "destination already exists" not in stderr or "BLAKE3 verified" in stderr:
            raise RuntimeError("overwrite refusal did not identify the collision")
        if (work_b / "received/my-folder/nested/hello.txt").read_bytes() != b"hello from A\n":
            raise RuntimeError("collision changed existing content")
        sender, receiver = send("./received-again")
        receiver.wait()
        sender.wait()
        if (work_b / "received-again/my-folder/nested/hello.txt").read_bytes() != b"hello from A\n":
            raise RuntimeError("retry with a new output directory failed")

        # Pair A and B the way `pair` leaves their books (it needs a terminal for the
        # confirmation number); A's tunnel server then admits B with --allow b.
        machine_a = identity(binary, work_a / "home", env, "a")
        machine_b = identity(binary, work_b / "home", env, "b")
        pair(work_a / "home", "b", machine_b)
        pair(work_b / "home", "a", machine_a)

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self):
                data = b"hello from A\n"
                self.send_response(200)
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def log_message(self, *_args):
                pass

        service = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cleanup.callback(service.server_close)
        thread = threading.Thread(target=service.serve_forever, daemon=True)
        thread.start()
        cleanup.callback(service.shutdown)
        server = peer(work_a, "tunnel", "tcp", "serve", "--to", f"127.0.0.1:{service.server_port}",
                      "--slot", "web", "--allow", "b")
        for _ in range(20):
            suggestion = server.line(1)
            if suggestion.startswith("iroh-toolbox "):
                break
        else:
            raise RuntimeError("no remote command suggestion")
        if shlex.split(suggestion)[-4:-2] != ["connect", "peer:a/web"]:
            raise RuntimeError("unexpected tunnel command: " + suggestion)
        client = peer(work_b, "tunnel", "tcp", "connect", "peer:a/web", "--bind", "127.0.0.1:0")
        bound = client.line(1).removeprefix("TCP listening on ")
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        for _ in range(2):
            with opener.open(f"http://{bound}/hello.txt", timeout=15) as response:
                if response.read() != b"hello from A\n":
                    raise RuntimeError("HTTP response mismatch")
        print(f"{archive.name}: SHA-256, code-paired folder, empty file, collision refusal and two HTTP requests to a paired machine passed ({build['target']})")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archives", nargs="*", type=Path)
    args = parser.parse_args()
    archives = args.archives or sorted(Path("dist").glob("*.tar.gz")) + sorted(Path("dist").glob("*.zip"))
    if not archives:
        parser.error("no archives; run mise run dist first")
    for archive in archives:
        smoke(archive.resolve(strict=True))


if __name__ == "__main__":
    main()
