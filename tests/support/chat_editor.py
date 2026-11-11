"""Chat line editor on real Unix pseudo-terminals: bursts of keys, messages
above a partial line, Tab completion, Ctrl+C and terminal restoration."""
import os
from pathlib import Path
import pty
import re
import select
import subprocess
import sys
import tempfile
import termios
import time


class Client:
    def __init__(self, args, cwd):
        self.master, self.slave = pty.openpty()
        env = {k: v for k, v in os.environ.items() if not k.startswith("IROH_")}
        env["TERM"] = "xterm"
        self.process = subprocess.Popen(args, stdin=self.slave, stdout=self.slave,
                                        stderr=self.slave, env=env, cwd=cwd)
        self.log = bytearray()

    def pump(self, seconds):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            if select.select([self.master], [], [], 0.02)[0]:
                try:
                    data = os.read(self.master, 4096)
                except OSError:
                    return
                self.log.extend(data)
                # Answer cursor position queries as a terminal emulator does.
                for _ in range(data.count(b"\x1b[6n")):
                    os.write(self.master, b"\x1b[1;1R")

    def text(self):
        return re.sub(rb"\x1b\[[0-9;?]*[A-Za-z]|\x1b[78]|\r", b"", self.log).decode(errors="replace")

    def await_text(self, text):
        until = time.monotonic() + 15
        while text not in self.text():
            assert time.monotonic() < until, self.text()[-2000:]
            self.pump(0.05)

    def type(self, keys):
        # One write: the keys reach the editor together, as over SSH or from an IME.
        os.write(self.master, keys.encode())
        self.pump(0.3)

    def finish(self, status):
        assert self.process.wait(timeout=15) == status, self.text()[-2000:]
        lflag = termios.tcgetattr(self.slave)[3]
        assert lflag & termios.ICANON and lflag & termios.ECHO, "terminal left in raw mode"

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait()
        os.close(self.master)
        os.close(self.slave)


binary, relay = sys.argv[1:]
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    (root / "holiday photos").mkdir()
    (root / "holiday photos" / "picture").write_bytes(b"picture bytes")
    (root / "inbox").mkdir()
    clients = []
    try:
        # --no-hints prints the code alone on the terminal.
        a = Client([binary, "--relay", relay, "--no-hints", "chat", "serve", "--no-progress", "--no-verify"], root)
        clients.append(a)
        until = time.monotonic() + 15
        while not (found := re.search(r"^([a-z]+(?:-[a-z]+){5})$", a.text(), re.M)):
            assert time.monotonic() < until, a.text()[-2000:]
            a.pump(0.05)
        b = Client([binary, "--relay", relay, "chat", "connect", "code:" + found[1], "--no-progress", "--no-verify"], root)
        clients.append(b)
        for client in clients:
            client.await_text("Chat ready;")
        a.type("hel")
        b.type("salut A\r")
        a.await_text("peer> salut A")
        a.type("lo B\r")
        b.await_text("peer> hello B")
        # Tab completes the command, then a directory name containing a space.
        a.type("/se\t")
        a.type("hol\t")
        a.type("\r")
        b.await_text("Peer offers holiday photos — 13 B, 1 file.")
        b.type("/receive --f\t")
        b.type("in\t")
        b.type("\r")
        b.await_text("BLAKE3 verified")
        a.await_text("Peer confirms:")
        assert (root / "inbox" / "holiday photos" / "picture").read_bytes() == b"picture bytes"
        # Ctrl+C leaves; the peer, waiting for a key, ends and restores its terminal.
        a.type("\x03")
        a.finish(130)
        b.await_text("Peer left the chat.")
        b.finish(0)
    finally:
        for client in clients:
            client.close()
print("Chat line editor: key bursts, Tab completion, Ctrl+C and terminal restoration passed on PTYs")
