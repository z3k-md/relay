# Releasing Relay

Desktop builds and signed auto-updates are published to
[github.com/z3k-md/relay](https://github.com/z3k-md/relay) only when a release
is cut. A normal commit or push does not publish a new version.

Commands below write `OWNER/REPO` as a placeholder.
`scripts/release.sh` and `scripts/setup-updater-key.sh` fill it from
`git remote get-url origin` when that remote points at github.com.
The updater endpoint in `tauri.conf.json` already points at this repository.

## One-time setup

1. **Confirm the release repo.** Releases and the updater download from the
   public GitHub repo. If `origin` is not set:

   ```bash
   git remote add origin https://github.com/z3k-md/relay.git
   git push -u origin main
   ```

   If the source repo is private, Releases have to live in a public repo
   (the updater downloads `latest.json` and the installers). Note its
   `owner/name` and pass it to the signing-key script below.

2. **Generate the updater signing key** on a trusted machine (Bun):

   ```bash
   ./scripts/setup-updater-key.sh
   # or, if Releases live in a different public repo:
   ./scripts/setup-updater-key.sh --release-repo public-owner/public-repo
   ```

   `--no-password` is allowed (the Tauri CLI has no such flag; the script
   calls `signer generate --ci -p ""`). The script refuses to overwrite
   `$HOME/.tauri/relay-updater.key`.

   **Losing the private key or its password means existing desktop installs
   can never auto-update again.** Back the key up offline. Do not commit it.

3. **Commit `tauri.conf.json`**. The script writes `plugins.updater.pubkey`
   and, when it can, replaces `OWNER/REPO` in the updater endpoint
   `https://github.com/OWNER/REPO/releases/latest/download/latest.json`.
   Point that endpoint at the *public* release repo.

4. **Actions secrets and variables** (GitHub → Settings → Secrets and
   variables → Actions), on the repo that runs the workflows:

   | Kind | Name | Required | Purpose |
   | --- | --- | --- | --- |
   | Secret | `TAURI_SIGNING_PRIVATE_KEY` | yes | Contents of `~/.tauri/relay-updater.key` |
   | Secret | `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | yes (empty OK) | Key password |
   | Secret | `RELEASE_TOKEN` | if source is private | PAT with `contents:write` on the public release repo |
   | Variable | `RELEASE_REPO` | if source is private | `owner/name` of the public release repo |

   `GITHUB_TOKEN` is provided by Actions. `setup-updater-key.sh` sets the
   two signing secrets via `gh` when you are logged in; otherwise follow
   the table.

5. **First install on each machine** — download from the Release, not from
   Actions artifacts:

   - macOS: the `.dmg`. Drag Relay.app to `/Applications`.
   - Windows: the NSIS `*-setup.exe` (per-user, no admin).

   **macOS first launch** of an ad-hoc-signed app that arrived through a
   browser is blocked by Gatekeeper. Either:

   ```bash
   xattr -dr com.apple.quarantine /Applications/Relay.app
   ```

   or right-click the app → Open → Open. There is no Apple notarization yet.

   **Windows SmartScreen** may warn on an unsigned installer: More info →
   Run anyway.

## Everyday loop

```bash
git checkout main
git pull
./scripts/release.sh          # default: patch. also: minor | major | none | X.Y.Z
# ./scripts/release.sh --dry-run
```

The script requires a clean `main` that matches `origin/main`, and `gh`
logged in. It does not edit the tree. It dispatches the release workflow
on `main`. That workflow increments the patch number (the build number:
`0.1.2` becomes `0.1.3`) in `[workspace.package] version`, in
`apps/relay-desktop/src-tauri/tauri.conf.json`, in
`apps/relay-desktop/package.json`, and in `Cargo.lock`, commits
`Release vX.Y.Z`, and builds that commit. `minor`, `major`, and an exact
`X.Y.Z` are the other choices. `none` rebuilds the version already on
`main`.

If the current version is not published yet (no release, or still a draft),
a patch cut finishes that version instead of incrementing again. Pushing a
`v*` tag does not start a build. Running on `main` matters for speed:
Actions caches saved by a tag run are visible only to that tag, while
`main`'s caches are shared by every later release. The tag is created when
the release is published.

An agent with Actions write access can cut the same release without the
script:

```bash
gh workflow run release.yml --repo OWNER/REPO --ref main -f bump=patch
# or:
gh api -X POST repos/OWNER/REPO/actions/workflows/release.yml/dispatches \
  -f ref=main -f inputs[bump]=patch
```

The call returns as soon as the run is queued. `ref` is the branch to
release (use `main`).

GitHub Actions (`.github/workflows/release.yml`) then builds: about
15 minutes cold, a few minutes with a warm cache. It creates a draft
release, builds into it:

- macOS universal (`aarch64` + `x86_64`) via `--target universal-apple-darwin`
- Windows `x86_64-pc-windows-msvc` NSIS

and publishes it only after checking that `latest.json` lists
`darwin-aarch64`, `darwin-x86_64` and `windows-x86_64`. Installed apps never
see a half-built release.

Watch `https://github.com/OWNER/REPO/actions`. Running desktop apps pick
the new version up within about 30 minutes, or immediately from the tray
**Check for updates**. Headless installs can download `relay-macos-universal`
or `relay-windows-x86_64.exe` from the same Release.

`bump=none`, or a re-run of the failed build jobs, rebuilds the current
version. The job fails if the committed version and the tag it publishes
disagree.

## SSH `deploy.sh` fast loop

`scripts/deploy.sh` is the untagged dev loop: `cargo install` on this Mac
and, optionally, a MinGW cross-build copied over SSH to a Windows PC. It
does **not** produce a GitHub Release or updater signatures.

Do not run `relay service` (the headless LaunchAgent / Windows task) and
the desktop app on the same machine unless you know they should not both
sync. They share the same home and would fight over the engine.

## Troubleshooting

**Updater signature mismatch.** The private key that signed `latest.json`
does not match `plugins.updater.pubkey` baked into the installed app.
You cannot rotate the key for existing installs; they must reinstall from
the Release. Confirm `TAURI_SIGNING_PRIVATE_KEY` is the same key
`setup-updater-key.sh` wrote, and that `tauri.conf.json` was committed
before the tag.

**Version not bumped.** The published tag (`vX.Y.Z`) is the `"version"` in
`tauri.conf.json`, which the workflow keeps equal to the workspace Cargo
version and `package.json`. A hand-pushed tag does not start a release.

**`latest.json` missing.** The updater endpoint 404s. Check that
both build jobs and the `publish release` job succeeded (a failed
`publish release` leaves the release as a draft; it names any platform
missing from `latest.json`), and that the endpoint's OWNER/REPO is
the *public* repo that actually has the assets. Private-repo Releases are
not downloadable by installed apps.
