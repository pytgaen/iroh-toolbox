"""Unix PTY check: 6-digit confirmation, refusal, timeout and Ctrl+C of native clients."""
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
    def __init__(self, args, temp_root):
        self.master, slave = pty.openpty()
        env = {k: v for k, v in os.environ.items() if not k.startswith("IROH_")}
        env["TMPDIR"] = str(temp_root)
        self.process = subprocess.Popen(
            args, stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env
        )
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

    def answer(self, value):
        os.write(self.master, value)

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
        source.write_bytes(b"human verified bytes")
        output = root / "received"
        try:
            common = ["--pairing-timeout", "5"]
            sender = Client([binary, "--relay", relay, "--no-hints", "send", str(source), *common], root)
            clients.append(sender)
            assert select.select([sender.process.stdout], [], [], 15)[0], sender.log.decode()
            code = sender.process.stdout.readline().decode().strip()
            assert len(code.split("-")) == 6, sender.log.decode()
            receiver = Client([binary, "--relay", relay, "receive", "code:", "--output", str(output), *common], root)
            clients.append(receiver)
            # The code is typed, never given in the arguments of the process.
            receiver.await_text("Code: ")
            assert code not in " ".join(receiver.process.args)
            receiver.answer(code.encode() + b"\n")
            for client in clients:
                client.await_text("Their confirmation number")
            numbers = [re.search(r"Your confirmation number: ([0-9]{3} [0-9]{3})", c.log.decode())[1] for c in clients]
            assert not output.exists()
            if scenario == "confirm":
                # A wrong number is asked again; each side types the other's number.
                sender.answer(b"000 000\n" if numbers[1] != "000 000" else b"111 111\n")
                sender.await_text("2 attempts left")
                sender.answer(numbers[1].encode() + b"\n")
                time.sleep(0.15)
                assert not output.exists()
                receiver.answer(numbers[0].replace(" ", "").encode() + b"\n")
                receiver.finish(0)
                sender.finish(0)
                assert (output / "file").read_bytes() == b"human verified bytes"
            else:
                if scenario == "decline":
                    sender.answer(b"\n")
                elif scenario == "interrupt":
                    sender.process.send_signal(signal.SIGINT)
                sender.finish(130 if scenario == "interrupt" else 1)
                receiver.finish(1)
                assert not output.exists()
                if scenario == "decline":
                    assert b"did not confirm the confirmation numbers" in receiver.log, receiver.log.decode()
                elif scenario == "timeout":
                    assert b"pairing timed out" in sender.log, sender.log.decode()
            assert set(root.iterdir()) == ({source, output} if scenario == "confirm" else {source}), list(root.iterdir())
        finally:
            for client in clients:
                client.close()

# --no-verify on both machines delivers without a question; on one machine only,
# both sides refuse with the mismatch message and nothing is delivered.
for scenario in ("skip", "mismatch"):
    clients = []
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        source = root / "file"
        source.write_bytes(b"unconfirmed bytes")
        output = root / "received"
        try:
            sender = Client([binary, "--relay", relay, "--no-hints", "send", str(source),
                             "--no-verify", "--pairing-timeout", "5"], root)
            clients.append(sender)
            sender.await_text("note: confirmation number disabled; whoever uses the code first is served")
            assert select.select([sender.process.stdout], [], [], 15)[0], sender.log.decode()
            code = sender.process.stdout.readline().decode().strip()
            flags = ["--no-verify"] if scenario == "skip" else []
            receiver = Client([binary, "--relay", relay, "receive", "code:" + code,
                               "--output", str(output), "--pairing-timeout", "5", *flags], root)
            clients.append(receiver)
            if scenario == "skip":
                receiver.finish(0)
                sender.finish(0)
                assert b"confirmation number" not in receiver.log
                assert (output / "file").read_bytes() == b"unconfirmed bytes"
            else:
                receiver.finish(1)
                sender.finish(1)
                for client in clients:
                    assert b"--no-verify" in client.log, client.log.decode()
                assert not output.exists()
        finally:
            for client in clients:
                client.close()
print("6-digit confirmation, refusal, timeout and Ctrl+C passed with real PTYs")
