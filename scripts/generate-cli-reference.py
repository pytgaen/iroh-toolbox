"""Generate or verify the CLI reference from the compiled native binary."""

import argparse
import os
import subprocess
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "--binary",
    type=Path,
    default=Path("target/debug/iroh-toolbox" + (".exe" if os.name == "nt" else "")),
)
parser.add_argument("--check", action="store_true")
args = parser.parse_args()

commands = [
    [], ["send"], ["receive"], ["chat"], ["chat", "serve"], ["chat", "connect"],
    ["pipe", "serve"], ["pipe", "connect"],
    ["tunnel", "tcp", "serve"], ["tunnel", "tcp", "connect"],
    ["tunnel", "http", "serve"], ["tunnel", "http", "connect"], ["relay", "check"],
    ["ping", "serve"], ["ping", "connect"], ["bench", "serve"], ["bench", "connect"],
    ["ticket", "inspect"], ["pair"], ["id"], ["id", "rotate"],
    ["peers"], ["peers", "rename"], ["peers", "rm"],
]
text = (
    "# CLI reference\n\n"
    "Generated from the native CLI's `--help` output. Global options are shown\n"
    "first; place them before the command in portable examples. See\n"
    "[configuration](configuration.md) for environment variables and precedence.\n\n"
    "Use `iroh-toolbox --version` for the build commit, target platform, project\n"
    "license, and locked versions of directly used iroh components and of the\n"
    "SPAKE2 implementation used by code pairing.\n"
    "`iroh-toolbox -V` prints only the application name and version.\n\n"
)
for command in commands:
    help_text = subprocess.check_output(
        [str(args.binary.resolve()), *command, "--help"], text=True, encoding="utf-8"
    ).rstrip()
    help_text = "\n".join(line.rstrip() for line in help_text.splitlines())
    title = " ".join(command) if command else "iroh-toolbox"
    text += f"## {title}\n\n```text\n{help_text}\n```\n\n"

text = text.rstrip() + "\n"
output = Path("docs/reference/cli.md")
if args.check:
    if not output.exists() or output.read_text(encoding="utf-8") != text:
        parser.error("CLI reference is stale; run mise run docs:cli")
else:
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(text, encoding="utf-8")
