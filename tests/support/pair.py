"""Unix PTY check: two machines with separate configuration directories pair
through a local relay, answer the confirmation question, and list each other
with `peers`. Pairing asks no access question; pairing again keeps the local
name, and a `peers` edit waits while a pairing holds the book lock."""
import os
from pathlib import Path
import pty
import re
import subprocess
import sys
import tempfile
import threading
import time


class Client:
    def __init__(self, args, home, stdin_tty=True):
        env = {k: v for k, v in os.environ.items() if not k.startswith("IROH_")}
        env["IROH_TOOLBOX_HOME"] = str(home)
        self.master = None
        stdin = subprocess.DEVNULL
        if stdin_tty:
            self.master, slave = pty.openpty()
            stdin = slave
        self.process = subprocess.Popen(
            args, stdin=stdin, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env
        )
        if stdin_tty:
            os.close(slave)
        self.log = bytearray()
        self.reader = threading.Thread(target=self.capture)
        self.reader.start()

    def capture(self):
        while chunk := self.process.stderr.read(1):
            self.log.extend(chunk)

    def text(self):
        return self.log.decode()

    def await_text(self, text, after=0):
        until = time.monotonic() + 20
        while text not in self.text()[after:]:
            assert time.monotonic() < until, self.text()
            assert self.process.poll() is None, self.text()
            time.sleep(0.01)

    def answer(self, value):
        os.write(self.master, value)

    def finish(self, status):
        assert self.process.wait(timeout=20) == status, self.text()
        self.reader.join(timeout=2)

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait()
        self.reader.join(timeout=2)
        if self.master is not None:
            os.close(self.master)


binary, relay = sys.argv[1:]
common = [binary, "--relay", relay, "--no-hints"]


def peers(home):
    env = {k: v for k, v in os.environ.items() if not k.startswith("IROH_")}
    env["IROH_TOOLBOX_HOME"] = str(home)
    result = subprocess.run([binary, "peers"], env=env, capture_output=True, text=True)
    assert result.returncode == 0, result.stderr
    return result.stdout


def pair(nas_home, laptop_home, first=False):
    clients = []
    try:
        nas = Client([*common, "pair", "--as", "nas", "--pairing-timeout", "30"], nas_home)
        clients.append(nas)
        nas.await_text("Code: ")
        code = re.search(r"Code: ([a-z-]+)", nas.text())[1]
        assert len(code.split("-")) == 6, nas.text()
        assert "This machine: nas" in nas.text()
        assert nas.text().index("This machine") < nas.text().index("Code: ")
        laptop = Client([*common, "pair", "code:", "--as", "laptop", "--pairing-timeout", "30"], laptop_home)
        clients.append(laptop)
        laptop.await_text("Code: ")
        laptop.answer(code.encode() + b"\n")
        for client in clients:
            client.await_text("Their confirmation number")
        numbers = [re.search(r"Your confirmation number: ([0-9]{3} [0-9]{3})", c.text())[1] for c in clients]
        # Nothing is stored before the confirmation.
        assert not first or "laptop" not in peers(nas_home)
        nas.answer(numbers[1].encode() + b"\n")
        laptop.answer(numbers[0].encode() + b"\n")
        nas.finish(0)
        laptop.finish(0)
        for client in clients:
            assert "Which slots" not in client.text() and "may reach" not in client.text(), client.text()
        return nas.text(), laptop.text()
    finally:
        for client in clients:
            client.close()


def listed(home, name):
    return re.search(name + r"\s+\w+…\w+\s+\d{4}-\d{2}-\d{2}$", peers(home), re.MULTILINE)


with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    nas_home, laptop_home = root / "nas", root / "laptop"
    nas_log, laptop_log = pair(nas_home, laptop_home, first=True)
    assert re.search(r"Paired with laptop \(machine \w+…\w+\)$", nas_log, re.MULTILINE), nas_log
    assert re.search(r"Paired with nas \(machine \w+…\w+\)$", laptop_log, re.MULTILINE), laptop_log
    assert peers(nas_home).splitlines()[0].split() == ["NAME", "MACHINE", "PAIRED"]
    assert listed(nas_home, "laptop"), peers(nas_home)
    assert listed(laptop_home, "nas"), peers(laptop_home)

    # The same machines again: the local names stay, nothing is duplicated.
    nas_log, _ = pair(nas_home, laptop_home)
    assert "Paired with laptop" in nas_log, nas_log
    assert len(peers(nas_home).strip().splitlines()) == 2
    assert len(peers(laptop_home).strip().splitlines()) == 2

    # A name taken on the joiner by another machine: nothing is stored anywhere.
    # While the joiner asks for another name it holds the book lock: a
    # `peers` edit waits, then applies once the pairing gave up.
    other_home, fresh_home, laptop_home = root / "other", root / "fresh", root / "laptop2"
    pair(other_home, laptop_home)
    clients = []
    try:
        fresh = Client([*common, "pair", "--as", "nas", "--pairing-timeout", "30"], fresh_home)
        clients.append(fresh)
        fresh.await_text("Code: ")
        code = re.search(r"Code: ([a-z-]+)", fresh.text())[1]
        joiner = Client([*common, "pair", "code:" + code, "--as", "laptop", "--pairing-timeout", "30"], laptop_home)
        clients.append(joiner)
        for client in clients:
            client.await_text("Their confirmation number")
        numbers = [re.search(r"Your confirmation number: ([0-9]{3} [0-9]{3})", c.text())[1] for c in clients]
        fresh.answer(numbers[1].encode() + b"\n")
        joiner.answer(numbers[0].encode() + b"\n")
        joiner.await_text("Another name for the other machine")
        assert "used by machine" in joiner.text() and "peers rm nas" in joiner.text(), joiner.text()
        rename = Client([binary, "peers", "rename", "nas", "home"], laptop_home, stdin_tty=False)
        clients.append(rename)
        rename.await_text("waiting for another pairing or peers command to finish")
        time.sleep(0.3)
        assert rename.process.poll() is None, rename.text()
        assert listed(laptop_home, "nas"), peers(laptop_home)
        joiner.answer(b"\n")
        joiner.finish(1)
        fresh.finish(1)
        rename.finish(0)
        assert "name taken" in joiner.text(), joiner.text()
        assert "no free name" in fresh.text(), fresh.text()
        # Only the rename changed the book: the cancelled pairing stored nothing.
        assert listed(laptop_home, "home") and len(peers(laptop_home).strip().splitlines()) == 2
        assert not (fresh_home / "peers.toml").exists() or "laptop" not in peers(fresh_home)
    finally:
        for client in clients:
            client.close()

    # Without a terminal, pair refuses before any network activity.
    refused = Client([*common, "pair"], root / "other", stdin_tty=False)
    refused.finish(1)
    assert "pair needs an interactive terminal on each machine" in refused.text(), refused.text()
    refused.close()
print("pair, peers listing, re-pairing, lock waiting and name conflict passed with real PTYs")
