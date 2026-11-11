#!/bin/sh
# Install iroh-toolbox from a GitHub release on Linux.
#
#   curl -fsSL https://github.com/pytgaen/iroh-toolbox/releases/latest/download/install.sh | sh
#   curl -fsSL .../install.sh | sh -s -- --version v0.1.0 --bin-dir /usr/local/bin
#
# The archive is checked against the release's SHA-256 manifest before
# installation. Nothing outside the installation directory is modified.
set -eu

releases="${IROH_TOOLBOX_INSTALL_BASE_URL:-https://github.com/pytgaen/iroh-toolbox/releases}"

usage() {
    cat <<'EOF'
Install iroh-toolbox from a GitHub release.

Usage: install.sh [--version vX.Y.Z] [--bin-dir DIR]

Options:
  --version VERSION  Release to install (default: the latest release)
  --bin-dir DIR      Installation directory (default: $IROH_TOOLBOX_INSTALL_DIR,
                     or ~/.local/bin)
  -h, --help         Show this help

Supported platform: Linux x86_64. Windows uses install.ps1.
EOF
}

fail() {
    printf 'error: %s\n' "$1" >&2
    exit 1
}

source_build="build from source instead: https://github.com/pytgaen/iroh-toolbox#build-from-source"

detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Linux)
            case "$arch" in
                x86_64 | amd64) echo x86_64-unknown-linux-musl ;;
                *) fail "no release archive for Linux $arch; $source_build" ;;
            esac
            ;;
        Darwin)
            fail "no release archive for macOS ($os $arch); $source_build"
            ;;
        MINGW* | MSYS* | CYGWIN*)
            fail "on Windows, use install.ps1 from PowerShell"
            ;;
        *) fail "no release archive for $os $arch; $source_build" ;;
    esac
}

# Prints the protocols curl may use: HTTPS, plus HTTP for a loopback test server.
base_url_protocols() {
    case "$releases" in
        https://*) echo '=https'; return ;;
        *@*) ;;
        http://127.0.0.1[:/]* | http://localhost[:/]*) echo '=https,http'; return ;;
    esac
    fail "IROH_TOOLBOX_INSTALL_BASE_URL must use https (http is accepted only for 127.0.0.1 or localhost)"
}

download() {
    if command -v curl >/dev/null 2>&1; then
        curl --proto "$proto" -fsSL -o "$2" "$1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1"
    else
        fail "curl or wget is required"
    fi
}

# Prints the tag that the latest release redirects to.
latest_tag() {
    if command -v curl >/dev/null 2>&1; then
        url=$(curl --proto "$proto" -fsSLI -o /dev/null -w '%{url_effective}' "$releases/latest") ||
            fail "cannot reach $releases/latest"
    elif command -v wget >/dev/null 2>&1; then
        url=$(wget -S --spider "$releases/latest" 2>&1 | sed -n 's/^ *[Ll]ocation: *\([^ ]*\).*/\1/p' | tail -n 1)
    else
        fail "curl or wget is required"
    fi
    case "$url" in
        */tag/v*) echo "${url##*/tag/}" ;;
        *) fail "no published release found at $releases; $source_build" ;;
    esac
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 -r "$1" | cut -d ' ' -f 1
    else
        fail "sha256sum, shasum or openssl is required to verify the download"
    fi
}

main() {
    version=""
    bin_dir="${IROH_TOOLBOX_INSTALL_DIR:-${HOME:?HOME is not set}/.local/bin}"
    while [ $# -gt 0 ]; do
        case "$1" in
            --version) [ $# -ge 2 ] || fail "--version needs a value"; version=$2; shift 2 ;;
            --bin-dir) [ $# -ge 2 ] || fail "--bin-dir needs a value"; bin_dir=$2; shift 2 ;;
            -h | --help) usage; exit 0 ;;
            *) fail "unknown option: $1 (see --help)" ;;
        esac
    done

    proto=$(base_url_protocols)
    target=$(detect_target)
    latest=""
    if [ -z "$version" ]; then
        version=$(latest_tag)
        latest=" (latest)"
    fi
    case "$version" in
        v*) ;;
        *) version="v$version" ;;
    esac
    case "$version" in
        v[0-9]*) ;;
        *) fail "invalid version: $version (expected vX.Y.Z)" ;;
    esac
    case "$version" in
        *[!A-Za-z0-9.-]*) fail "invalid version: $version (expected vX.Y.Z)" ;;
    esac

    archive="iroh-toolbox-$version-$target.tar.gz"
    manifest="iroh-toolbox-$version-sha256sums.txt"
    temp=$(mktemp -d)
    trap 'rm -rf "$temp"' EXIT
    trap 'exit 130' INT TERM

    echo "iroh-toolbox installer"
    echo "  version    $version$latest"
    echo "  platform   $target"
    download "$releases/download/$version/$manifest" "$temp/$manifest" ||
        fail "release $version not found at $releases"
    expected=$(awk -v name="$archive" '$2 == name || $2 == "*" name { print $1; exit }' "$temp/$manifest")
    [ -n "$expected" ] || fail "release $version has no archive for $target"
    download "$releases/download/$version/$archive" "$temp/$archive" ||
        fail "cannot download $archive"
    [ "$(sha256 "$temp/$archive")" = "$expected" ] ||
        fail "checksum mismatch for $archive; nothing was installed"
    echo "  checksum   verified ($manifest)"

    mkdir -p "$temp/extract"
    tar -xzf "$temp/$archive" -C "$temp/extract" iroh-toolbox ||
        fail "cannot extract iroh-toolbox from $archive"
    mkdir -p "$bin_dir" || fail "cannot create $bin_dir"
    bin_dir=$(cd "$bin_dir" && pwd)
    destination="$bin_dir/iroh-toolbox"
    previous=""
    if [ -x "$destination" ]; then
        previous=$("$destination" -V 2>/dev/null || true)
    fi
    # Rename within the directory so that a running copy is never half-written.
    cp "$temp/extract/iroh-toolbox" "$bin_dir/.iroh-toolbox.$$" ||
        fail "cannot write to $bin_dir; choose another --bin-dir"
    chmod 755 "$bin_dir/.iroh-toolbox.$$"
    mv -f "$bin_dir/.iroh-toolbox.$$" "$destination"
    if [ -n "$previous" ]; then
        echo "  replaced   $previous"
    fi
    echo "  installed  $destination"
    "$destination" -V || fail "the installed executable does not run on this machine"

    case ":${PATH:-}:" in
        *":$bin_dir:"*)
            found=$(command -v iroh-toolbox || true)
            if [ "$found" != "$destination" ]; then
                echo "note: $found comes first on PATH; remove it or reorder PATH"
            fi
            ;;
        *)
            echo "note: $bin_dir is not on PATH; add this line to your shell profile:"
            # shellcheck disable=SC2016 # printed for the user's profile
            if [ "$bin_dir" = "$HOME/.local/bin" ]; then
                echo '  export PATH="$HOME/.local/bin:$PATH"'
            else
                echo "  export PATH=\"$bin_dir:\$PATH\""
            fi
            ;;
    esac
}

main "$@"
