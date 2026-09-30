#!/usr/bin/env bash
# Kill a node after it has appended to the mailbox and before it advances
# the local watermark, then restart it and require both trees to match.
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

sim up mac pc --mailbox shared
sim write mac mods/weapon.esp v1
sim wait-converged --timeout 20s
sim stall mac
sim write mac mods/weapon.esp v2
sim wait-stalled mac --timeout 20s
sim kill mac
sim unstall mac
sim start mac
sim wait-converged --timeout 30s
sim report
