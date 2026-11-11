#!/usr/bin/env bash
# Records the animated README demo (docs/assets/pair-tunnel-demo.gif) and its
# transcript. Two containers, nas and laptop, on separate Docker bridge
# networks that cannot reach each other, pair with a code and then reach a
# folder by name, through iroh's public relays. Requires only Docker.
#
# Usage: scripts/demo/record.sh [MUSL_BINARY]
# (default: dist/local/iroh-toolbox-x86_64-unknown-linux-musl, built by
# `mise run release-local`; `mise run demo:record` does both).
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
demo=$root/scripts/demo
assets=$root/docs/assets
binary=$(realpath "${1:-$root/dist/local/iroh-toolbox-x86_64-unknown-linux-musl}")
socket=/var/run/docker.sock
if [[ ${DOCKER_HOST:-} == unix://* ]]; then socket=${DOCKER_HOST#unix://}; fi

work=$(mktemp -d)
remove_containers() {
  docker rm -f demo-recorder demo-nas demo-laptop >/dev/null 2>&1 || true
  docker network rm demo-nas demo-laptop >/dev/null 2>&1 || true
}
trap 'remove_containers; rm -rf "$work"' EXIT
remove_containers

echo "Building the demo images…"
docker build --quiet --target host --tag iroh-toolbox-demo-host "$demo" >/dev/null
docker build --quiet --target recorder --tag iroh-toolbox-demo-recorder "$demo" >/dev/null

for machine in nas laptop; do
  docker network create "demo-$machine" >/dev/null
  docker run --detach --name "demo-$machine" --hostname "$machine" \
    --network "demo-$machine" \
    --volume "$binary:/usr/local/bin/iroh-toolbox:ro" \
    iroh-toolbox-demo-host sleep infinity >/dev/null
done

echo "Recording (about a minute, through iroh's public relays)…"
docker run --rm --name demo-recorder \
  --volume "$socket:/var/run/docker.sock" \
  --volume "$demo:/demo:ro" \
  --volume "$work:/out" \
  --entrypoint /demo/session.sh \
  iroh-toolbox-demo-recorder

echo "Measuring the route between the two machines…"
docker exec --detach demo-nas iroh-toolbox ping serve --peers-only
sleep 3
route=$(docker exec demo-laptop iroh-toolbox --no-hints ping connect peer:nas --count 3 2>&1 \
  | grep '^seq=' | tail -n1)

version=$("$binary" --version | head -n1)
commit=$("$binary" --version | sed -n 's/^ *Commit *//p')
sha=$(sha256sum "$binary" | cut -d' ' -f1)

{
  cat <<EOF
iroh-toolbox — pair once with a code, then reach a folder by name
Recorded: $(date -u +%Y-%m-%d), Linux x86_64, with \`mise run demo:record\`

Two Docker containers, nas and laptop, each on its own bridge network behind
the host's NAT. The two networks cannot reach each other: the machines meet
through iroh's public relays and n0's DNS, as two machines on the Internet
would. Both leave through the same public IP address, so this shows the
workflow over the Internet, not NAT traversal between two providers.

Binary: $version (commit $commit), x86_64-unknown-linux-musl release build
SHA-256: $sha
Route measured after the recording (\`ping connect peer:nas\`):
  $route

Each machine starts with a fresh identity. The code typed at laptop's
\`Code:\` prompt is the one nas printed; the prompt does not echo it. The code
is single-use and was consumed by this pairing.

EOF
  echo "Terminal nas:"
  cat "$work/nas.txt"
  echo
  echo "Terminal laptop:"
  cat "$work/laptop.txt"
  echo
  echo "Second terminal on laptop, while \`tunnel http connect\` runs:"
  cat "$work/browser.txt"
} >"$assets/pair-tunnel-demo.txt"

cp "$work/pair-tunnel.gif" "$assets/pair-tunnel-demo.gif"
echo "Wrote docs/assets/pair-tunnel-demo.gif ($(du -h "$assets/pair-tunnel-demo.gif" | cut -f1)) and docs/assets/pair-tunnel-demo.txt"
echo "Route: $route"
