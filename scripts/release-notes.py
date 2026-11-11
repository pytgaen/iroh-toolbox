"""Extract a development draft or validate the published notes for a stable tag."""

import argparse
import re
import tomllib
from datetime import date
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("tag", help="Stable release tag, e.g. v0.1.0")
parser.add_argument("--draft", action="store_true",
                    help="Preview notes/changelog-X.Y.Z.md; allow a pending date and version bump")
parser.add_argument("--output", type=Path, help="Write notes to this file")
args = parser.parse_args()

if not re.fullmatch(r"v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", args.tag):
    parser.error("expected a stable vX.Y.Z tag")

version = args.tag[1:]
package = tomllib.loads(Path("Cargo.toml").read_text())["package"]
if not args.draft and package["version"] != version:
    parser.error(f"tag version {version} differs from Cargo.toml {package['version']}")

source = Path(f"notes/changelog-{version}.md") if args.draft else Path("CHANGELOG.md")
if not source.is_file():
    parser.error(f"missing {source}; prepare the editorial draft during development")
changelog = source.read_text()
section = re.search(
    rf"^## \[{re.escape(version)}\] — (\d{{4}}-\d{{2}}-\d{{2}}|YYYY-MM-DD)\n(.*?)(?=^## \[|\Z)",
    changelog,
    re.MULTILINE | re.DOTALL,
)
if section is None or not section[2].strip():
    parser.error(f"{source} needs a dated [{version}] section with non-empty notes")
if not (args.draft and section[1] == "YYYY-MM-DD"):
    try:
        date.fromisoformat(section[1])
    except ValueError:
        parser.error("release date must be a valid YYYY-MM-DD date; use --draft to preview a pending release")

notes = section[2].strip() + "\n"
if args.output:
    args.output.write_text(notes)
else:
    print(notes, end="")
