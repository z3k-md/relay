#!/usr/bin/env sh
# Build Relay from source and install the `relay` binary (macOS or Linux).
#
#   ./scripts/install.sh                 # needs Rust already installed
#   ./scripts/install.sh --install-rust  # installs Rust via rustup first
#
# The binary goes to ~/.cargo/bin/relay (rustup puts that on your PATH).
set -eu

cd "$(dirname "$0")/.."

if [ "${1:-}" = "--install-rust" ] && ! command -v cargo >/dev/null 2>&1; then
    echo "Installing Rust with rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1091
    . "$HOME/.cargo/env"
fi

if ! command -v cargo >/dev/null 2>&1; then
    echo "Rust is not installed. Re-run with --install-rust, or install it from https://rustup.rs" >&2
    exit 1
fi

if [ "$(uname -s)" = "Darwin" ] && ! xcode-select -p >/dev/null 2>&1; then
    echo "The Xcode command line tools are required (they provide the C compiler)." >&2
    echo "Run: xcode-select --install" >&2
    exit 1
fi

# rust-toolchain.toml pins the compiler; rustup fetches it on first use.
cargo install --path apps/relay-cli --locked --force

echo
echo "Installed: $(command -v relay || echo "$HOME/.cargo/bin/relay")"
relay --version 2>/dev/null || true
