"""Package a native binary, offline instructions and provenance with SHA-256."""

import argparse
import hashlib
import io
import json
import re
import subprocess
import tarfile
import tomllib
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path)
    parser.add_argument("--target")
    parser.add_argument("--label")
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    args = parser.parse_args()
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    rust = subprocess.check_output(["rustc", "-vV"], text=True)
    target = args.target or next(line.removeprefix("host: ") for line in rust.splitlines() if line.startswith("host: "))
    label = args.label or f"v{version}-dev"
    if not re.fullmatch(r"[A-Za-z0-9_.-]+", target) or not re.fullmatch(r"v[0-9][A-Za-z0-9_.-]*", label):
        parser.error("target and label must be plain filename components")
    executable = "iroh-toolbox.exe" if "windows" in target else "iroh-toolbox"
    binary = (args.binary or ROOT / "target" / "release" / executable).resolve(strict=True)
    actual = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if subprocess.check_output([str(binary), "-V"], text=True).strip() != f"iroh-toolbox v{version}":
        parser.error(f"binary version differs from Cargo.toml: {actual}")
    if f"  Platform  {target}" not in actual.splitlines():
        parser.error("binary platform differs from the archive target")
    commit = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    short_commit = subprocess.check_output(["git", "rev-parse", "--short=7", "HEAD"], cwd=ROOT, text=True).strip()
    if f"  Commit    {short_commit}" not in actual.splitlines():
        parser.error("binary commit differs from the source checkout; rebuild before packaging")
    dirty = bool(subprocess.check_output(["git", "status", "--porcelain"], cwd=ROOT))
    if dirty and "-dev" not in label:
        parser.error("stable archives require a clean checkout; use a -dev label for local builds")
    files = {
        executable: binary.read_bytes(),
        "LICENSE.txt": (ROOT / "LICENSE.txt").read_bytes(),
        "NOTICE.txt": (ROOT / "NOTICE").read_bytes(),
        "QUICKSTART.txt": (ROOT / "docs" / "distribution" / "QUICKSTART.txt").read_bytes(),
        "BUILD.json": (json.dumps({
            "version": version, "label": label, "target": target,
            "commit": commit,
            "dirty": dirty,
            "rustc": rust.splitlines()[0],
            "features": [],
            "version_output": actual,
            "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        }, indent=2) + "\n").encode(),
    }
    args.output.mkdir(parents=True, exist_ok=True)
    suffix = "zip" if "windows" in target else "tar.gz"
    archive = args.output / f"iroh-toolbox-{label}-{target}.{suffix}"
    if suffix == "zip":
        with zipfile.ZipFile(archive, "w", compression=zipfile.ZIP_DEFLATED) as output:
            for name, content in files.items():
                output.writestr(name, content)
    else:
        with tarfile.open(archive, "w:gz") as output:
            for name, content in files.items():
                entry = tarfile.TarInfo(name)
                entry.mode = 0o755 if name == executable else 0o644
                entry.size = len(content)
                output.addfile(entry, io.BytesIO(content))
    checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
    archive.with_name(archive.name + ".sha256").write_text(
        f"{checksum}  {archive.name}\n", encoding="utf-8", newline="\n"
    )
    print(archive)


if __name__ == "__main__":
    main()
