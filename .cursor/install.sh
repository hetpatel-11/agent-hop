#!/usr/bin/env bash
# Idempotent Cursor Cloud install. Prepares Rust, Zig 0.15.2, clang, Node,
# and wrangler so a phone/cloud agent can build `ah` and query D1.
set -euo pipefail

export DEBIAN_FRONTEND=noninteractive
export PATH="${HOME}/.local/bin:${HOME}/.cargo/bin:${PATH}"

sudo_if() {
  if [ "$(id -u)" -eq 0 ]; then
    "$@"
  else
    sudo "$@"
  fi
}

sudo_if apt-get update -y
sudo_if apt-get install -y --no-install-recommends \
  build-essential \
  pkg-config \
  libssl-dev \
  clang \
  libclang-dev \
  llvm \
  curl \
  ca-certificates \
  xz-utils \
  git \
  tmux \
  nodejs \
  npm

if ! command -v rustup >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable
fi
# shellcheck disable=SC1091
. "${HOME}/.cargo/env"
rustup toolchain install stable
rustup default stable

# edition = "2024" needs rustc >= 1.85
rustc --version

ARCH="$(uname -m)"
case "${ARCH}" in
  x86_64) ZIG_SLUG="x86_64-linux" ;;
  aarch64|arm64) ZIG_SLUG="aarch64-linux" ;;
  *) echo "unsupported arch: ${ARCH}" >&2; exit 1 ;;
esac

ZIG_VER="0.15.2"
ZIG_DIR="${HOME}/.local/zig-${ZIG_VER}"
mkdir -p "${HOME}/.local/bin"
if [ ! -x "${ZIG_DIR}/zig" ]; then
  FILE="zig-${ZIG_SLUG}-${ZIG_VER}.tar.xz"
  TMP="$(mktemp -d)"
  curl -fL --retry 6 --retry-all-errors --retry-delay 5 \
    -o "${TMP}/${FILE}" "https://ziglang.org/download/${ZIG_VER}/${FILE}"
  mkdir -p "${HOME}/.local"
  tar -xf "${TMP}/${FILE}" -C "${HOME}/.local"
  rm -rf "${ZIG_DIR}"
  mv "${HOME}/.local/zig-${ZIG_SLUG}-${ZIG_VER}" "${ZIG_DIR}"
  rm -rf "${TMP}"
fi
ln -sfn "${ZIG_DIR}/zig" "${HOME}/.local/bin/zig"
export ZIG="${HOME}/.local/bin/zig"
"${ZIG}" version

# Persist PATH for later agent shells (exports do not survive the Build snapshot).
PROFILE="${HOME}/.bashrc"
touch "${PROFILE}"
if ! grep -q 'agent-hop-cloud-path' "${PROFILE}"; then
  cat >> "${PROFILE}" <<'EOF'
# agent-hop-cloud-path
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
export ZIG="${ZIG:-$HOME/.local/bin/zig}"
export CLOUDFLARE_ACCOUNT_ID="${CLOUDFLARE_ACCOUNT_ID:-f45021db74a97feaafae2e3e131d4d82}"
if [ -f "$HOME/.cargo/env" ]; then . "$HOME/.cargo/env"; fi
EOF
fi

if ! command -v wrangler >/dev/null 2>&1; then
  sudo_if npm install -g wrangler
fi
wrangler --version || true

if [ -d /usr/lib/llvm-18/lib ]; then
  export LIBCLANG_PATH="${LIBCLANG_PATH:-/usr/lib/llvm-18/lib}"
elif [ -d /usr/lib/llvm-17/lib ]; then
  export LIBCLANG_PATH="${LIBCLANG_PATH:-/usr/lib/llvm-17/lib}"
fi

# Warm the crate + libghostty-vt compile so the first agent is not cold.
if ! cargo test --offline; then
  cargo test
fi
echo "install ok: rustc=$(rustc --version) zig=$(zig version) wrangler=$(wrangler --version 2>/dev/null || echo missing)"
