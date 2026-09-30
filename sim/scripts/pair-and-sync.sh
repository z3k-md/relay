#!/usr/bin/env bash
# Two daemons, no mailbox. An edit on each side shows up on the other.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

sim() {
  if [[ -n "${RELAY_SIM_BIN:-}" ]]; then
    "$RELAY_SIM_BIN" "$@"
  else
    cargo run -q -p relay-sim -- "$@"
  fi
}

lab=$(mktemp -d "${TMPDIR:-/tmp}/relay-sim.XXXXXX")
export RELAY_SIM_LAB="$lab"

cleanup() {
  status=$?
  if [[ $status -ne 0 ]]; then
    sim report >&2 || true
  fi
  sim down || true
  exit "$status"
}
trap cleanup EXIT

sim up mac pc
sim write mac mods/hello.txt from-a
sim wait-converged --timeout 20s
sim write pc mods/hello.txt from-b
sim wait-converged --timeout 20s
sim report
