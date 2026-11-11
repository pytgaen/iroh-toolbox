# Installation

## Install script

The install scripts download the archive for your platform from the latest
[release](https://github.com/pytgaen/iroh-toolbox/releases), check it against the
release's SHA-256 manifest, and install the executable. No Rust or Python
installation is needed.

### Linux

```sh
curl -fsSL https://github.com/pytgaen/iroh-toolbox/releases/latest/download/install.sh | sh
```

```text
iroh-toolbox installer
  version    v0.1.0 (latest)
  platform   x86_64-unknown-linux-musl
  checksum   verified (iroh-toolbox-v0.1.0-sha256sums.txt)
  installed  /home/me/.local/bin/iroh-toolbox
iroh-toolbox v0.1.0
```

The command goes to `~/.local/bin`. The script edits no shell profile: when that
directory is not on PATH, it prints the line to add. Choose a release or another
directory with options after `sh -s --`:

```sh
curl -fsSL https://github.com/pytgaen/iroh-toolbox/releases/latest/download/install.sh \
  | sh -s -- --version v0.1.0 --bin-dir /usr/local/bin
```

`IROH_TOOLBOX_INSTALL_DIR` also selects the directory. The script needs `curl`
or `wget`, `tar`, and `sha256sum`, `shasum` or `openssl`. It supports Linux
x86_64; other platforms, macOS included, need a
[source build](#build-from-source).

### Windows

In PowerShell:

```powershell
irm https://github.com/pytgaen/iroh-toolbox/releases/latest/download/install.ps1 | iex
```

```text
iroh-toolbox installer
  version    v0.1.0 (latest)
  platform   x86_64-pc-windows-msvc
  checksum   verified (iroh-toolbox-v0.1.0-sha256sums.txt)
  installed  C:\Users\me\AppData\Local\Programs\iroh-toolbox\iroh-toolbox.exe
  PATH       added to your user PATH; open a new terminal
iroh-toolbox v0.1.0
```

The script adds its directory to your user PATH and to the current session.
To choose a release, keep PATH unchanged or select another directory, pass
parameters:

```powershell
& ([scriptblock]::Create((irm https://github.com/pytgaen/iroh-toolbox/releases/latest/download/install.ps1))) -Version v0.1.0 -NoModifyPath
```

`-BinDir` or `IROH_TOOLBOX_INSTALL_DIR` selects the directory. Windows on ARM64
receives the x64 build, which Windows 11 runs through emulation.

### Read the script first

A piped script runs as soon as it downloads. To inspect it before running it:

```sh
curl -fsSLO https://github.com/pytgaen/iroh-toolbox/releases/latest/download/install.sh
less install.sh
sh install.sh
```

```powershell
irm https://github.com/pytgaen/iroh-toolbox/releases/latest/download/install.ps1 -OutFile install.ps1
notepad install.ps1
powershell -ExecutionPolicy Bypass -File .\install.ps1
```

Each release lists both scripts in its checksum manifest. The checksum detects a
changed download from the same release; it is not a signature.

### Update and uninstall

Run the same command again to update: the script reports the version it
replaces. To uninstall, delete the executable, and on Windows remove its
directory from your user PATH (**Edit environment variables for your account**).

## Release archives

To install by hand, each tagged release provides these archives in the repository's
[Releases](https://github.com/pytgaen/iroh-toolbox/releases) section. CI builds
each one and exercises the extracted archive on its native platform:

| Platform | Archive |
| --- | --- |
| Linux x86_64 | `iroh-toolbox-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz` |
| Windows x86_64 | `iroh-toolbox-vX.Y.Z-x86_64-pc-windows-msvc.zip` |

Each archive contains the executable, `LICENSE.txt`, `NOTICE.txt` (word list
attribution), an offline `QUICKSTART.txt` and `BUILD.json` recording its version,
target, commit, dirty-checkout status and executable hash. No Rust or Python
installation is needed to run the prebuilt command. macOS and other
architectures currently require a source build.

Download the release's `iroh-toolbox-vX.Y.Z-sha256sums.txt` alongside the selected
archive. On Linux, verify that archive before extracting it:

```sh
sha256sum --check --ignore-missing iroh-toolbox-vX.Y.Z-sha256sums.txt
tar -xzf iroh-toolbox-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz
mkdir -p "$HOME/.local/bin"
install -m 755 ./iroh-toolbox "$HOME/.local/bin/iroh-toolbox"
export PATH="$HOME/.local/bin:$PATH"
iroh-toolbox --version
```

On Windows, compare the following hash with the exact archive's entry in the
manifest:

```powershell
Get-FileHash .\iroh-toolbox-vX.Y.Z-x86_64-pc-windows-msvc.zip -Algorithm SHA256
```

Compare all 64 hexadecimal characters with the matching entry, then extract:

```powershell
Expand-Archive .\iroh-toolbox-vX.Y.Z-x86_64-pc-windows-msvc.zip -DestinationPath .\iroh-toolbox
Set-Location .\iroh-toolbox
.\iroh-toolbox.exe --version
```

Put `iroh-toolbox.exe` in a tools directory and add that directory to your user
PATH via **Edit environment variables for your account**, then reopen the
terminal. On Linux, add `$HOME/.local/bin` to PATH in your shell profile for
future terminals. Try `iroh-toolbox --version` before the
[first folder or service walkthrough](quick-start.md).

Obtain the archive and checksum from the same trusted release: a checksum
detects changed downloads but is not a signature.

Releases published after 0.1.0 also carry a build provenance attestation, which
proves that GitHub Actions built the file from this repository. With the
[GitHub CLI](https://cli.github.com/), verify an archive or install script with:

```sh
gh attestation verify iroh-toolbox-vX.Y.Z-x86_64-unknown-linux-musl.tar.gz \
  --repo pytgaen/iroh-toolbox
```

## Build from source

Every build includes all features: files, chat, streams, tunnels and code
pairing. A source build needs:

- Rust at least the version declared by `rust-version` in `Cargo.toml`
  (currently 1.99), for example through [rustup](https://rustup.rs/);
- a C compiler and linker: the system build tools on Linux, the Xcode Command
  Line Tools on macOS (`xcode-select --install`), or the Visual Studio C++ build
  tools and Windows SDK on Windows.

macOS has no prebuilt archive: build from source. The code supports it, but CI
does not test macOS yet.

```sh
git clone https://github.com/pytgaen/iroh-toolbox
cd iroh-toolbox
cargo install --path . --locked
```

This installs `iroh-toolbox` in Cargo's bin directory (`~/.cargo/bin`, or
`%USERPROFILE%\.cargo\bin` on Windows), which rustup adds to PATH. `--locked`
uses the dependency versions tested by the project.

Contributors set up the development tools with mise instead: see the
[development guide](../maintainers/development.md) and `CONTRIBUTING.md`.
