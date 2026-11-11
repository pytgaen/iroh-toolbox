"""Unix PTY check of the machine key passphrase.

The passphrase is typed on the controlling terminal with echo off, even when standard input is a
pipe; it is asked by commands that load a protected identity, refused without a controlling
terminal, and the terminal is restored after a wrong passphrase, Ctrl+C or a fatal signal. A
tunnel client asks it once, before it announces readiness, however many local connections follow."""
import fcntl
import hashlib
import json
import os
import pty
import select
import signal
import socket
import subprocess
import sys
import tempfile
import termios
import time
import tomllib

binary = sys.argv[1]
Z32 = "ybndrfg8ejkmcpqxot1uwisza345h769"
PROMPT = b"Passphrase for the machine key: "


def echo_on(fd):
    return bool(termios.tcgetattr(fd)[3] & termios.ECHO)


def canonical(fd):
    return bool(termios.tcgetattr(fd)[3] & termios.ICANON)


def drain(fd, seconds):
    seen = b""
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        if select.select([fd], [], [], 0.05)[0]:
            try:
                chunk = os.read(fd, 4096)
            except OSError:
                break
            if not chunk:
                break
            seen += chunk
    return seen


def wait_for(fd, needle, process, seen=b""):
    """Reads the terminal until `needle` appears; returns everything read."""
    until = time.monotonic() + 30
    while needle not in seen:
        assert time.monotonic() < until and process.poll() is None, seen
        seen += drain(fd, 0.1)
    return seen


def environment(home):
    env = {k: v for k, v in os.environ.items() if not k.startswith("IROH_")}
    env["IROH_TOOLBOX_HOME"] = home
    return env


def spawn(home, args, slave, mode):
    """Starts the binary. `mode`: "tty" (the terminal is stdin and controlling), "pipe" (stdin is a
    pipe, the terminal is still controlling) or "none" (a session without a controlling terminal)."""
    stdin = {"tty": slave, "pipe": subprocess.PIPE, "none": subprocess.DEVNULL}[mode]

    def controlling():
        os.setsid()
        fcntl.ioctl(slave, termios.TIOCSCTTY, 0)

    extra = {"preexec_fn": controlling} if mode != "none" else {"start_new_session": True}
    return subprocess.Popen(
        [binary, *args], stdin=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        env=environment(home), **extra,
    )


def run(home, args, typed, mode="tty"):
    """Runs the binary; `typed` is a list of (prompt, text) answered in order on the terminal.
    Returns (status, stdout, stderr)."""
    master, slave = pty.openpty()
    process = spawn(home, args, slave, mode)
    try:
        shown = b""
        for prompt, text in typed:
            shown = wait_for(master, prompt, process, shown)
            time.sleep(0.2)
            assert not echo_on(slave), "echo must be off while the passphrase is typed"
            os.write(master, text)
            echoed = drain(master, 0.4)
            assert b"hunter2" not in echoed and b"wrong-one" not in echoed, "the passphrase was echoed"
            shown += echoed
        out, rest = process.communicate(timeout=60)
        assert echo_on(slave) and canonical(slave), f"terminal not restored after {args}"
        return process.returncode, out.decode(), rest.decode()
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
        os.close(slave)


def z32(data):
    bits = "".join(f"{x:08b}" for x in data)
    bits += "0" * (-len(bits) % 5)
    return "".join(Z32[int(bits[i:i + 5], 2)] for i in range(0, len(bits), 5))


# Ed25519 public key of a seed (RFC 8032), to name a machine in a peer book without a dependency.
P = 2**255 - 19
D = -121665 * pow(121666, P - 2, P) % P


def add(a, b):
    x = (a[1] - a[0]) * (b[1] - b[0]) % P
    y = (a[1] + a[0]) * (b[1] + b[0]) % P
    c = 2 * a[3] * b[3] * D % P
    d = 2 * a[2] * b[2] % P
    e, f, g, h = y - x, d - c, d + c, y + x
    return (e * f % P, g * h % P, f * g % P, e * h % P)


def public_key(seed):
    gy = 4 * pow(5, P - 2, P) % P
    x2 = (gy * gy - 1) * pow(D * gy * gy + 1, P - 2, P) % P
    gx = pow(x2, (P + 3) // 8, P)
    if (gx * gx - x2) % P:
        gx = gx * pow(2, (P - 1) // 4, P) % P
    if gx & 1:
        gx = P - gx
    point, result = (gx, gy, 1, gx * gy % P), (0, 1, 1, 0)
    scalar = int.from_bytes(hashlib.sha512(seed).digest()[:32], "little")
    scalar = (scalar & ((1 << 254) - 8)) | (1 << 254)
    while scalar:
        if scalar & 1:
            result = add(result, point)
        point = add(point, point)
        scalar >>= 1
    zinv = pow(result[2], P - 2, P)
    x, y = result[0] * zinv % P, result[1] * zinv % P
    return z32((y | ((x & 1) << 255)).to_bytes(32, "little"))


def machine(home):
    with open(os.path.join(home, "identity"), "rb") as file:
        identity = tomllib.load(file)
    return public_key(bytes.fromhex(identity["machine_seed"])), identity["salt"]


def pair(home, local_name, key, salt):
    path = os.path.join(home, "peers.toml")
    with open(path, "a") as file:
        file.write(
            f'[peers.{local_name}]\nmachine = "{key}"\nsalt = "{salt}"\n'
            f'announced = "{local_name}"\npaired = "2026-11-01T10:12:00Z"\n'
        )
    os.chmod(path, 0o600)


def free_port():
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


def read_stderr(process, needle, seen=b"", seconds=30):
    until = time.monotonic() + seconds
    while needle not in seen:
        assert time.monotonic() < until and process.poll() is None, seen.decode(errors="replace")
        if select.select([process.stderr], [], [], 0.1)[0]:
            seen += os.read(process.stderr.fileno(), 4096)
    return seen


def protect(home):
    status, _, err = run(
        home, ["id", "protect"],
        [(b"New passphrase", b"hunter2\r"), (b"Repeat the passphrase", b"hunter2\r")],
    )
    assert status == 0, err


def check_basics():
    with tempfile.TemporaryDirectory() as home:
        identity = os.path.join(home, "identity")
        # A new identity is unprotected.
        status, out, _ = run(home, ["id", "--json"], [])
        assert status == 0 and json.loads(out)["protected"] is False, out
        machine_id = json.loads(out)["machine"]
        assert "version = 1" in open(identity).read()

        # Protect: the passphrase is typed twice.
        protect(home)
        text = open(identity).read()
        assert "version = 2" in text and "machine_seed" not in text, text

        # A command that loads the identity asks the passphrase once and shows it is protected.
        status, out, err = run(home, ["id", "--json"], [(PROMPT, b"hunter2\r")])
        assert status == 0, err
        shown = json.loads(out)
        assert shown["protected"] is True and shown["machine"] == machine_id, out

        # Standard input may be a pipe (`tar c . | iroh-toolbox pipe serve`): the passphrase is
        # read from the controlling terminal, not from the pipe.
        status, out, err = run(home, ["id", "--json"], [(PROMPT, b"hunter2\r")], mode="pipe")
        assert status == 0, err
        assert json.loads(out)["machine"] == machine_id, out

        # A wrong passphrase gives one generic error and changes nothing.
        status, _, err = run(home, ["id"], [(PROMPT, b"wrong-one\r")])
        assert status != 0 and "wrong passphrase, or the identity file is damaged" in err, err
        assert open(identity).read() == text

        # Ctrl+C at the prompt cancels and restores the terminal.
        status, _, err = run(home, ["id"], [(PROMPT, b"\x03")])
        assert status != 0 and "cancelled" in err, err

        # Without a controlling terminal the command fails and says why; nothing is read from the
        # environment or from standard input.
        status, _, err = run(home, ["id"], [], mode="none")
        assert status != 0 and "protected by a passphrase" in err, err
        assert "needs a controlling terminal" in err, err

        # A passphrase longer than the buffer is refused, not truncated.
        status, _, err = run(home, ["id"], [(PROMPT, b"x" * 2000 + b"\r")])
        assert status != 0 and "too long" in err, err

        # Rotating keeps the protection and asks the passphrase once.
        status, _, err = run(home, ["id", "rotate", "--address"], [(PROMPT, b"hunter2\r")])
        assert status == 0, err
        assert "version = 2" in open(identity).read()

        # Unprotect writes the unprotected format back.
        status, _, err = run(home, ["id", "unprotect"], [(PROMPT, b"hunter2\r")])
        assert status == 0, err
        assert "version = 1" in open(identity).read()
        status, out, _ = run(home, ["id", "--json"], [], mode="none")
        assert status == 0 and json.loads(out)["machine"] == machine_id, out


def check_signals():
    """SIGINT, SIGTERM, SIGHUP and SIGQUIT at the prompt leave the terminal as it was."""
    with tempfile.TemporaryDirectory() as home:
        status, _, err = run(home, ["id"], [])
        assert status == 0, err
        protect(home)
        for name in ("SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT"):
            master, slave = pty.openpty()
            process = spawn(home, ["id"], slave, "tty")
            try:
                wait_for(master, PROMPT, process)
                time.sleep(0.3)
                assert not echo_on(slave) and not canonical(slave)
                process.send_signal(getattr(signal, name))
                process.wait(timeout=10)
                assert process.returncode == -getattr(signal, name), (name, process.returncode)
                assert echo_on(slave) and canonical(slave), f"terminal not restored after {name}"
            finally:
                if process.poll() is None:
                    process.kill()
                    process.wait()
                os.close(master)
                os.close(slave)


def check_tunnel_client():
    """A tunnel client with a protected key asks the passphrase before it is ready, once, however
    many local connections arrive; the terminal is restored when it stops."""
    with tempfile.TemporaryDirectory() as nas, tempfile.TemporaryDirectory() as laptop:
        for home in (nas, laptop):
            status, _, err = run(home, ["id"], [])
            assert status == 0, err
        (nas_key, nas_salt), (laptop_key, laptop_salt) = machine(nas), machine(laptop)
        pair(nas, "laptop", laptop_key, laptop_salt)
        pair(laptop, "nas", nas_key, nas_salt)
        protect(laptop)

        service = socket.socket()
        service.bind(("127.0.0.1", 0))
        service.listen(16)
        service.settimeout(30)
        port, local = free_port(), free_port()
        server = subprocess.Popen(
            [binary, "--direct-only", "tunnel", "tcp", "serve", "--to",
             f"127.0.0.1:{service.getsockname()[1]}", "--allow", "all", "--port", str(port)],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env=environment(nas), start_new_session=True,
        )
        master, slave = pty.openpty()
        client, clients = None, []
        try:
            read_stderr(server, b"Direct addresses:")
            client = spawn(
                laptop,
                ["--direct-only", "tunnel", "tcp", "connect", "peer:nas",
                 "--addr", f"127.0.0.1:{port}", "--bind", f"127.0.0.1:{local}"],
                slave, "tty",
            )
            shown = wait_for(master, PROMPT, client)
            time.sleep(0.5)
            ready = b""
            if select.select([client.stderr], [], [], 0.3)[0]:
                ready = os.read(client.stderr.fileno(), 4096)
            assert b"local listener ready" not in ready, "ready before the passphrase was typed"
            assert not echo_on(slave)
            os.write(master, b"hunter2\r")
            drain(master, 0.5)
            read_stderr(client, b"local listener ready", ready)
            # Several connections at once: no second prompt, all of them are served.
            connections = []
            for _ in range(3):
                connection = socket.socket()
                connection.connect(("127.0.0.1", local))
                connection.sendall(b"x")
                connections.append(connection)
            for _ in range(3):
                accepted, _ = service.accept()
                assert accepted.recv(1) == b"x"
                accepted.close()
            shown += drain(master, 1.0)
            assert shown.count(PROMPT) == 1, f"{shown.count(PROMPT)} prompts: {shown!r}"
            assert echo_on(slave) and canonical(slave), "terminal left quiet while the client runs"
            for connection in connections:
                connection.close()
            client.send_signal(signal.SIGTERM)
            client.wait(timeout=15)
            assert echo_on(slave) and canonical(slave), "terminal not restored after the client stopped"

            # `tar c . | iroh-toolbox pipe serve --allow nas`: stdin is a pipe, the passphrase is
            # still asked on the terminal, and the server starts.
            producer = spawn(
                laptop, ["--direct-only", "pipe", "serve", "--allow", "nas"], slave, "pipe"
            )
            clients.append(producer)
            wait_for(master, PROMPT, producer)
            time.sleep(0.3)
            os.write(master, b"hunter2\r")
            read_stderr(producer, b"Direct addresses:")
            producer.send_signal(signal.SIGTERM)
            producer.wait(timeout=15)
            assert echo_on(slave) and canonical(slave)
        finally:
            for process in (client, server, *clients):
                if process is not None and process.poll() is None:
                    process.kill()
                    process.wait()
            os.close(master)
            os.close(slave)
            service.close()


check_basics()
check_signals()
check_tunnel_client()
