"""Default real-terminal replacement question: accept, decline, timeout and Ctrl+C."""
import os
from pathlib import Path
import pty
import signal
import subprocess
import sys
import tempfile
import threading
import time

binary, relay = sys.argv[1:]
for scenario in ("yes", "no", "timeout", "interrupt"):
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        source = root / "file"
        source.write_bytes(b"verified replacement")
        output = root / "out"
        output.mkdir()
        (output / "file").write_bytes(b"original content")
        (output / "keep").write_bytes(b"unrelated")
        env = {k: v for k, v in os.environ.items() if not k.startswith("IROH_")}
        env["TMPDIR"] = str(root)
        sender = subprocess.Popen([binary, "--relay", relay, "send", str(source), "--no-progress", "--no-verify", "--transfer-timeout", "4"],
                                  stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
        receiver = None
        master, slave = pty.openpty()
        log = bytearray()
        reader = None
        try:
            code = sender.stdout.readline().decode().strip()
            assert len(code.split("-")) == 6, code
            receiver = subprocess.Popen(
                [binary, "--relay", relay, "receive", "code:" + code, "--output", str(output),
                 "--no-progress", "--no-verify", "--transfer-timeout", "2"],
                stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
            os.close(slave)
            slave = None

            def capture():
                while chunk := receiver.stderr.read(1):
                    log.extend(chunk)

            reader = threading.Thread(target=capture)
            reader.start()
            until = time.monotonic() + 15
            while b"Type yes to confirm" not in log:
                assert time.monotonic() < until, log.decode()
                assert receiver.poll() is None, log.decode()
                time.sleep(0.01)
            assert (output / "file").read_bytes() == b"original content"
            assert b"BLAKE3 verified" not in log
            if scenario in ("yes", "no"):
                os.write(master, (scenario + "\n").encode())
            elif scenario == "interrupt":
                receiver.send_signal(signal.SIGINT)
            status = receiver.wait(timeout=15)
            assert status == (0 if scenario == "yes" else 130 if scenario == "interrupt" else 1), log.decode()
            reader.join(timeout=2)
            sender.wait(timeout=15)
            assert receiver.stdout.read() == b""
            assert (output / "file").read_bytes() == (b"verified replacement" if scenario == "yes" else b"original content")
            assert (output / "keep").read_bytes() == b"unrelated"
            assert set(output.iterdir()) == {output / "file", output / "keep"}, list(output.iterdir())
            assert set(root.iterdir()) == {source, output}, list(root.iterdir())
        finally:
            for process in (receiver, sender):
                if process is not None:
                    if process.poll() is None:
                        process.kill()
                    process.wait()
            if reader is not None:
                reader.join(timeout=2)
            if slave is not None:
                os.close(slave)
            os.close(master)
print("Replacement accepted, declined, timed out and interrupted on real terminals; originals and unrelated files preserved")
