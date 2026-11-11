"""Unix PTY check: the typed code is not echoed and the terminal is restored after Ctrl+C or SIGTERM."""
import os
import pty
import select
import signal
import subprocess
import sys
import tempfile
import termios
import time

binary, relay = sys.argv[1:]


def echo_on(fd):
    return bool(termios.tcgetattr(fd)[3] & termios.ECHO)


def drain(fd, seconds):
    seen = b""
    until = time.monotonic() + seconds
    while time.monotonic() < until:
        if select.select([fd], [], [], 0.05)[0]:
            try:
                seen += os.read(fd, 4096)
            except OSError:
                break
    return seen


for sent in (signal.SIGINT, signal.SIGTERM):
    with tempfile.TemporaryDirectory() as home:
        master, slave = pty.openpty()
        # Before the process starts: it may turn echo off at once.
        assert echo_on(slave)
        env = {k: v for k, v in os.environ.items() if not k.startswith("IROH_")}
        env["IROH_TOOLBOX_HOME"] = home
        process = subprocess.Popen(
            [binary, "--relay", relay, "receive", "code:"],
            stdin=slave, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
        )
        try:
            prompt = b""
            until = time.monotonic() + 15
            while b"Code: " not in prompt:
                assert time.monotonic() < until and process.poll() is None, prompt
                prompt += process.stderr.read1(100)
            time.sleep(0.2)
            assert not echo_on(slave), "echo must be off while the code is typed"
            os.write(master, b"secretwords")
            assert b"secretwords" not in drain(master, 0.5), "the typed code was echoed"
            process.send_signal(sent)
            process.wait(timeout=15)
            assert echo_on(slave), f"terminal not restored after {sent!r}"
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            os.close(master)
            os.close(slave)
