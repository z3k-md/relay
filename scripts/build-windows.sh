#!/usr/bin/env sh
# Cross-compile relay.exe for 64-bit Windows from macOS or Linux and package it
# as dist/relay-windows-x86_64.zip together with install.ps1.
#
# Prerequisites:
#   macOS:  brew install mingw-w64
#   Ubuntu: sudo apt-get install gcc-mingw-w64-x86-64
#   both:   rustup target add x86_64-pc-windows-gnu
#
# The result is a single self-contained exe (no MinGW DLLs needed).
set -eu

cd "$(dirname "$0")/.."

TARGET=x86_64-pc-windows-gnu
if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
    echo "x86_64-w64-mingw32-gcc not found; install mingw-w64 (see the header of this script)." >&2
    exit 1
fi
rustup target add "$TARGET" >/dev/null

cargo build --release --locked --target "$TARGET" -p relay-cli

OUT=dist/relay-windows-x86_64
rm -rf "$OUT" "$OUT.zip"
mkdir -p "$OUT"
cp "target/$TARGET/release/relay.exe" "$OUT/"
cp scripts/install.ps1 "$OUT/"
cp README.md "$OUT/README.md"
(cd dist && zip -qr relay-windows-x86_64.zip relay-windows-x86_64)
echo "Built dist/relay-windows-x86_64.zip"
