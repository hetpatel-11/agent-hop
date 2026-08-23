#!/usr/bin/env bash
# Install the native `ah` binary without Node or npm.
# Same artifact the npm platform packages ship.
#
#   curl -fsSL https://raw.githubusercontent.com/hetpatel-11/agent-hop/main/install.sh | bash
#
# Override install location with AH_BIN_DIR (default: ~/.local/bin).
# Pin a version with AH_VERSION (default: latest on npm).
set -euo pipefail

REPO="hetpatel-11/agent-hop"
BIN_DIR="${AH_BIN_DIR:-$HOME/.local/bin}"

need() {
  if ! command -v "$1" >/dev/null 2>&1; then
    echo "agent-hop: missing required command: $1" >&2
    exit 1
  fi
}

need curl
need tar

os="$(uname -s)"
arch="$(uname -m)"

case "${os}-${arch}" in
  Darwin-arm64)   pkg="agent-hop-darwin-arm64" ;;
  Darwin-x86_64)  pkg="agent-hop-darwin-x64" ;;
  Linux-x86_64)   pkg="agent-hop-linux-x64" ;;
  Linux-aarch64)  pkg="agent-hop-linux-arm64" ;;
  *)
    echo "agent-hop: no prebuilt binary for ${os}-${arch}." >&2
    echo "Supported: macOS arm64/x64, Linux x64/arm64. On Windows use npm: npm install -g agent-hop" >&2
    exit 1
    ;;
esac

version="${AH_VERSION:-}"
if [ -z "$version" ]; then
  if command -v python3 >/dev/null 2>&1; then
    version="$(curl -fsSL https://registry.npmjs.org/agent-hop/latest | python3 -c "import sys,json; print(json.load(sys.stdin)['version'])")"
  else
    version="$(curl -fsSL https://registry.npmjs.org/agent-hop/latest | sed -n 's/.*"version":"\([^"]*\)".*/\1/p' | head -1)"
  fi
fi

if [ -z "$version" ]; then
  echo "agent-hop: could not resolve the latest version from npm." >&2
  exit 1
fi

url="https://registry.npmjs.org/${pkg}/-/${pkg}-${version}.tgz"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Installing agent-hop ${version} (${pkg}) -> ${BIN_DIR}/ah"
curl -fsSL "$url" -o "$tmp/pkg.tgz"
tar -xzf "$tmp/pkg.tgz" -C "$tmp"

src="$tmp/package/bin/ah"
if [ ! -f "$src" ]; then
  echo "agent-hop: tarball did not contain bin/ah (fetched ${url})" >&2
  exit 1
fi

mkdir -p "$BIN_DIR"
install -m 755 "$src" "$BIN_DIR/ah"
ln -sfn "$BIN_DIR/ah" "$BIN_DIR/agent-hop"

if ! command -v ah >/dev/null 2>&1; then
  echo
  echo "Installed, but ${BIN_DIR} is not on your PATH. Add this to your shell rc and reload:"
  echo "  export PATH=\"${BIN_DIR}:\$PATH\""
fi

echo "Done. Run: ah"
