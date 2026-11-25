"""`chat --auto-cmd` on real Unix pseudo-terminals: an offer opens the form,
messages show above it, a wrong destination keeps it open, the --force box
and the buttons answer it, Esc keeps the offer, a withdrawn offer closes it,
the line being typed survives, and the terminal is restored."""
import os
from pathlib import Path
import re
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).parent))
from chat_editor import Client  # noqa: E402



def await_new_form(client, before):
    """Waits for one more form than `before` in the client's terminal."""
    until = time.monotonic() + 15
    while client.text().count("[ accept ]") <= before:
        assert time.monotonic() < until, client.text()[-2000:]
        client.pump(0.05)
    client.pump(0.3)


binary, relay = sys.argv[1:]
with tempfile.TemporaryDirectory() as directory:
    root = Path(directory)
    (root / "a" / "photos").mkdir(parents=True)
    (root / "a" / "photos" / "picture").write_bytes(b"picture bytes")
    (root / "b" / "inbox" / "photos").mkdir(parents=True)
    (root / "b" / "inbox" / "photos" / "picture").write_bytes(b"old bytes")
    clients = []
    try:
        a = Client([binary, "--relay", relay, "--no-hints", "chat", "serve", "--no-progress", "--no-verify"], root / "a")
        clients.append(a)
        until = time.monotonic() + 15
        while not (found := re.search(r"^([a-z]+(?:-[a-z]+){5})$", a.text(), re.M)):
            assert time.monotonic() < until, a.text()[-2000:]
            a.pump(0.05)
        b = Client([binary, "--relay", relay, "chat", "connect", "code:" + found[1], "--no-progress", "--no-verify",
                    "--auto-cmd"], root / "b")
        clients.append(b)
        for client in clients:
            client.await_text("Chat ready;")

        # B is typing when the offer arrives: the form replaces the prompt.
        b.type("abc")
        a.type("/send photos\r")
        b.await_text("Peer offers photos — 13 B, 1 file.")
        b.await_text("[ accept ]")
        assert "Use /receive" not in b.text()
        assert "[ ] --force" in b.text()
        # Messages show above the open form.
        a.type("pendant le formulaire\r")
        b.await_text("peer> pendant le formulaire")
        # A missing folder keeps the form open, with the error.
        b.type("\x15missing/inbox\t\t\r")
        b.await_text("/missing: no such folder")
        # Fix the destination, tick --force, accept.
        b.type("\x15inbox\t \t\r")
        b.await_text("you> /receive --force inbox")
        b.await_text("BLAKE3 verified")
        a.await_text("Peer confirms:")
        assert (root / "b" / "inbox" / "photos" / "picture").read_bytes() == b"picture bytes"
        # The line typed before the form is still there.
        b.type("def\r")
        a.await_text("peer> abcdef")

        # Esc keeps the offer pending; the usual commands answer it.
        forms = b.text().count("[ accept ]")
        a.type("/send photos\r")
        await_new_form(b, forms)
        b.type("\x1b")
        b.await_text("File offer kept; use /receive DESTINATION or /reject.")
        b.type("/reject\r")
        a.await_text("rejected")

        # A withdrawn offer closes its form; the editor takes keys again.
        forms = b.text().count("[ accept ]")
        a.type("/send photos\r")
        await_new_form(b, forms)
        a.type("/cancel\r")
        b.await_text("withdr")
        b.pump(0.5)
        b.type("encore là\r")
        a.await_text("peer> encore là")

        # Ctrl+C in the editor leaves; both terminals are restored.
        b.type("\x03")
        b.finish(130)
        a.await_text("Peer left the chat.")
        a.type("\x04")
        a.finish(0)
    finally:
        for client in clients:
            client.close()
print("Chat --auto-cmd: form, messages above it, errors, --force, Esc, withdrawal and restoration passed on PTYs")
