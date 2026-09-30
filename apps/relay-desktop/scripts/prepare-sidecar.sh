#!/usr/bin/env bash
# Build the `relay` CLI and copy it to the Tauri externalBin location:
#   src-tauri/binaries/relay-<target-triple>[.exe]
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
APP_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE="$(cd "$APP_DIR/../.." && pwd)"
DEST="$APP_DIR/src-tauri/binaries"
mkdir -p "$DEST"

host_triple() {
  rustc -vV | sed -n 's/^host: //p'
}

is_windows_triple() {
  [[ "$1" == *windows* ]]
}

copy_bin() {
  local triple="$1"
  local src="$2"
  local dest_name="relay-${triple}"
  if is_windows_triple "$triple"; then
    dest_name="${dest_name}.exe"
  fi
  cp "$src" "$DEST/$dest_name"
  echo "wrote $DEST/$dest_name"
}

TARGET="${1:-$(host_triple)}"

if [[ "$TARGET" == "universal-apple-darwin" ]]; then
  cargo build --release -p relay-cli --target aarch64-apple-darwin --manifest-path "$WORKSPACE/Cargo.toml"
  cargo build --release -p relay-cli --target x86_64-apple-darwin --manifest-path "$WORKSPACE/Cargo.toml"
  lipo -create \
    "$WORKSPACE/target/aarch64-apple-darwin/release/relay" \
    "$WORKSPACE/target/x86_64-apple-darwin/release/relay" \
    -output "$DEST/relay-universal-apple-darwin"
  echo "wrote $DEST/relay-universal-apple-darwin"
  exit 0
fi

cargo build --release -p relay-cli --target "$TARGET" --manifest-path "$WORKSPACE/Cargo.toml"
SRC="$WORKSPACE/target/$TARGET/release/relay"
if is_windows_triple "$TARGET"; then
  SRC="${SRC}.exe"
fi
if [[ ! -f "$SRC" ]]; then
  echo "error: expected CLI binary at $SRC" >&2
  exit 1
fi
copy_bin "$TARGET" "$SRC"
