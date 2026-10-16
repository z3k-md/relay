# Working in this repo

Relay keeps chosen folders identical across a user's own devices, peer to
peer, from ordinary local files. Rust workspace, Tauri 2 + Vue 3 desktop app.

Read in this order, and stop when you have what you need:

1. [`docs/DESIGN.md`](docs/DESIGN.md): how it works, the safety invariants
   (§9), and a code map (§10) from "a file changed" to "it is on the other
   device".
2. [`docs/DECISIONS.md`](docs/DECISIONS.md): one numbered entry per choice.
   Code comments cite them as `D16`; the index at the top says which are
   current.
3. [`docs/ROADMAP.md`](docs/ROADMAP.md): what is being built, what is next.
4. [`docs/USAGE.md`](docs/USAGE.md): the CLI and user-facing rules.

## Commands

```bash
cargo build --workspace --exclude relay-desktop
cargo nextest run --workspace --exclude relay-desktop   # what CI runs (cargo test works too)
cargo clippy --workspace --exclude relay-desktop --all-targets -- -D warnings
cargo fmt --all
python3 scripts/check-docs.py                           # links, D-numbers, DESIGN §refs
cargo run -p relay-vopr -- run --scenario chaos --seed 7 --trace
```

The desktop crate builds with its frontend and system packages; see
[`apps/relay-desktop/README.md`](apps/relay-desktop/README.md). More in
[`docs/DEVELOPMENT.md`](docs/DEVELOPMENT.md).

## Rules for every change

- **The invariants in DESIGN §9 hold.** Never delete user files because a
  rule, policy, share, or mount changed; never write unverified bytes into the
  working tree; keep both versions when unsure.
- **No `unsafe`.** The workspace forbids it. Platform APIs go through
  maintained safe wrappers (D43).
- **The engine stays sans-I/O.** `relay-engine` does not depend on Tokio or
  `relay-net` (D19). Keep `Syncer::set_now` and the `relay_core::faults`
  hooks when touching sync, apply, or materialization; `relay-vopr` needs
  them.
- **Tests bind loopback only** (`127.0.0.1`). A test that binds `0.0.0.0`
  pops a firewall prompt on Windows dev machines.
- **Config changes are data.** New settings a running host must apply go
  through `ConfigChange` on the live loop (D36), not a direct database write,
  which reloads the host.
- **Wire changes are additive.** Old peers must keep working: new protobuf
  fields are optional, new frames go only to peers that advertise a feature
  bit (D37).

## Keeping docs true

Docs change in the same PR as the behavior:

| You changed | Update |
| --- | --- |
| A design choice, protocol, format, or limit | Add or amend a `DECISIONS.md` entry (next free number; check open PRs) and its index line |
| How the system works at the level of DESIGN | The DESIGN section, citing the D-number |
| A command, flag, or user-visible rule | `docs/USAGE.md` |
| What shipped or what is next | `docs/ROADMAP.md` |
| A plan not yet decided | A file in `docs/proposals/` starting with a `**Status:**` line |

Remove superseded text rather than annotating it; git keeps history. A
superseded decision becomes a one-line pointer, because code cites its number.
`scripts/check-docs.py` runs in CI.

PRs are squash-merged into `main`.
