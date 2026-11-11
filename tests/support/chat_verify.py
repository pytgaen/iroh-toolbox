"""Chat confirmation and /send, /receive on actual Unix pseudo-terminals."""
import os
from pathlib import Path
import pty
import re
import select
import signal
import subprocess
import sys
import tempfile
import threading
import time


class Client:
    def __init__(self, args, root):
        self.master, slave = pty.openpty()
        env = {k: v for k, v in os.environ.items() if not k.startswith("IROH_")}
        env["TMPDIR"] = str(root)
        self.process = subprocess.Popen(args, stdin=slave, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, env=env)
        os.close(slave)
        self.log = bytearray()
        self.reader = threading.Thread(target=self.capture)
        self.reader.start()

    def capture(self):
        while chunk := self.process.stderr.read(1):
            self.log.extend(chunk)

    def await_text(self, text):
        until = time.monotonic() + 15
        while text not in self.log.decode():
            assert time.monotonic() < until, self.log.decode()
            assert self.process.poll() is None, self.log.decode()
            time.sleep(0.01)

    def answer(self, text):
        os.write(self.master, (text + "\n").encode())

    def finish(self, status):
        assert self.process.wait(timeout=15) == status, self.log.decode()
        self.reader.join(timeout=2)

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait()
        self.reader.join(timeout=2)
        os.close(self.master)


binary, relay = sys.argv[1:]
for scenario in ("confirm", "decline", "timeout", "interrupt"):
    clients = []
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        source = root / "file"
        source.write_bytes(b"chat through real terminals")
        output = root / "received"
        try:
            common = ["--pairing-timeout", "5"]
            a = Client([binary, "--relay", relay, "chat", "serve", *common], root)
            clients.append(a)
            assert select.select([a.process.stdout], [], [], 15)[0], a.log.decode()
            code = a.process.stdout.readline().decode().strip()
            assert len(code.split("-")) == 6
            b = Client([binary, "--relay", relay, "chat", "connect", "code:" + code, *common], root)
            clients.append(b)
            for client in clients:
                client.await_text("Their confirmation number")
            numbers = [re.search(r"Your confirmation number: ([0-9]{3} [0-9]{3})", c.log.decode())[1]
                       for c in clients]
            if scenario == "confirm":
                a.answer(numbers[1])
                time.sleep(0.15)
                assert all(b"Chat ready;" not in c.log for c in clients)
                b.answer(numbers[0])
                for client in clients:
                    client.await_text("Chat ready;")
                a.answer(f"/send {source}")
                b.await_text("Peer offers")
                assert not output.exists()
                b.answer(f"/receive {output}")
                b.await_text("BLAKE3 verified")
                a.await_text("Peer confirms:")
                a.answer("/quit")
                a.finish(0)
                b.finish(0)
                assert (output / "file").read_bytes() == b"chat through real terminals"
            else:
                if scenario == "decline":
                    a.answer("")
                elif scenario == "interrupt":
                    a.process.send_signal(signal.SIGINT)
                a.finish(130 if scenario == "interrupt" else 1)
                b.finish(1)
                assert all(b"Chat ready;" not in c.log for c in clients)
                assert not output.exists()
            assert set(root.iterdir()) == ({source, output} if scenario == "confirm" else {source}), list(root.iterdir())
        finally:
            for client in clients:
                client.close()

# --no-verify on one machine only is refused on both, with no chat started.
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    clients = []
    try:
        a = Client([binary, "--relay", relay, "chat", "serve", "--no-verify", "--pairing-timeout", "5"], root)
        clients.append(a)
        assert select.select([a.process.stdout], [], [], 15)[0], a.log.decode()
        a.await_text("confirmation number disabled")
        code = a.process.stdout.readline().decode().strip()
        b = Client([binary, "--relay", relay, "chat", "connect", "code:" + code, "--pairing-timeout", "5"], root)
        clients.append(b)
        a.finish(1)
        b.finish(1)
        for client in clients:
            assert b"--no-verify" in client.log, client.log.decode()
            assert b"Chat ready;" not in client.log
    finally:
        for client in clients:
            client.close()
print("Chat 6-digit confirmation, refusal, timeout, Ctrl+C and /send /receive passed on PTYs")
