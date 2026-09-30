#!/usr/bin/env bash
# Bump Relay's shared version, commit, push, and start the release workflow
# on main so GitHub Actions builds the desktop app and CLI.
#
#   ./scripts/release.sh                 # patch (default)
#   ./scripts/release.sh minor
#   ./scripts/release.sh 1.2.3
#   ./scripts/release.sh --dry-run
#
# macOS ships bash 3.2: no associative arrays, no ${var,,}, no mapfile.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$ROOT"

DRY_RUN=0
BUMP="patch"

usage() {
    cat <<'EOF'
Bump the workspace version, commit, push to origin, and start the release
workflow for vX.Y.Z on main (needs gh; falls back to pushing the tag).

Usage:
  ./scripts/release.sh [--dry-run] [patch|minor|major|X.Y.Z]

  patch | minor | major   Semver bump of [workspace.package] version
                          in the root Cargo.toml (default: patch).
  X.Y.Z                   Set that version explicitly.
  --dry-run               Print the plan; change nothing.

Requires a clean tree on main, up to date with origin/main. Updates
Cargo.toml, apps/relay-desktop/src-tauri/tauri.conf.json,
apps/relay-desktop/package.json, and Cargo.lock.
EOF
}

die() {
    echo "error: $*" >&2
    exit 1
}

while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help)
            usage
            exit 0
            ;;
        --dry-run)
            DRY_RUN=1
            shift
            ;;
        patch|minor|major)
            BUMP="$1"
            shift
            ;;
        -*)
            die "unknown option: $1 (try --help)"
            ;;
        *)
            BUMP="$1"
            shift
            ;;
    esac
done

# OWNER/REPO is the fallback when `origin` is not a github.com remote.
github_owner_repo() {
    url=$(git remote get-url origin 2>/dev/null || true)
    url=${url%.git}
    url=${url%/}
    case "$url" in
        *github.com[:/]*)
            rest=${url#*github.com}
            rest=${rest#:}
            rest=${rest#/}
            # Drop a trailing slash or query if any.
            rest=${rest%%[?#]*}
            rest=${rest%/}
            if [ -n "$rest" ]; then
                printf '%s' "$rest"
                return 0
            fi
            ;;
    esac
    printf '%s' "OWNER/REPO"
}

cargo_workspace_version() {
    awk '
        $0 == "[workspace.package]" { hit=1; next }
        hit && /^\[/ { exit }
        hit && $1 == "version" {
            gsub(/"/, "", $3)
            print $3
            exit
        }
    ' Cargo.toml
}

# next_version CURRENT KIND  — KIND is patch|minor|major|X.Y.Z
next_version() {
    cur=$1
    kind=$2
    case "$kind" in
        patch|minor|major)
            major=${cur%%.*}
            rest=${cur#*.}
            minor=${rest%%.*}
            patch=${rest#*.}
            case "$major$minor$patch" in
                *[!0-9]*) die "cannot parse [workspace.package] version: $cur" ;;
            esac
            case "$cur" in
                *.*.*.*) die "cannot parse [workspace.package] version: $cur" ;;
            esac
            case "$kind" in
                patch) patch=$((patch + 1)) ;;
                minor) minor=$((minor + 1)); patch=0 ;;
                major) major=$((major + 1)); minor=0; patch=0 ;;
            esac
            printf '%s.%s.%s' "$major" "$minor" "$patch"
            ;;
        *)
            case "$kind" in
                *.*.*.*) die "version must be X.Y.Z (got $kind)" ;;
                [0-9]*.[0-9]*.[0-9]*)
                    a=${kind%%.*}
                    r=${kind#*.}
                    b=${r%%.*}
                    c=${r#*.}
                    case "$a$b$c" in
                        *[!0-9]*) die "version must be X.Y.Z (got $kind)" ;;
                    esac
                    printf '%s' "$kind"
                    ;;
                *)
                    die "usage: $0 [--dry-run] [patch|minor|major|X.Y.Z]"
                    ;;
            esac
            ;;
    esac
}

set_json_version() {
    file=$1
    ver=$2
    node -e '
        const fs = require("fs");
        const file = process.argv[1];
        const ver = process.argv[2];
        const text = fs.readFileSync(file, "utf8");
        const out = text.replace(/("version"\s*:\s*")[^"]*(")/, function (_, a, b) { return a + ver + b; });
        if (out === text) {
            console.error("no top-level \"version\" string in " + file);
            process.exit(1);
        }
        fs.writeFileSync(file, out);
    ' "$file" "$ver"
}

set_cargo_workspace_version() {
    ver=$1
    tmp=$(mktemp "${TMPDIR:-/tmp}/relay-cargo-toml.XXXXXX")
    awk -v newver="$ver" '
        $0 == "[workspace.package]" { hit=1 }
        hit && /^version = / && !done {
            print "version = \"" newver "\""
            done=1
            hit=0
            next
        }
        { print }
    ' Cargo.toml >"$tmp"
    if ! cmp -s Cargo.toml "$tmp"; then
        mv "$tmp" Cargo.toml
    else
        rm -f "$tmp"
        die "did not find [workspace.package] version in Cargo.toml"
    fi
}

need_file() {
    [ -f "$1" ] || die "missing $1"
}

[ -f Cargo.toml ] || die "not a Relay checkout (no Cargo.toml)"
need_file apps/relay-desktop/src-tauri/tauri.conf.json
need_file apps/relay-desktop/package.json

CURRENT=$(cargo_workspace_version)
[ -n "$CURRENT" ] || die "could not read [workspace.package] version from Cargo.toml"
NEXT=$(next_version "$CURRENT" "$BUMP")

REPO=$(github_owner_repo)
ACTIONS_URL="https://github.com/${REPO}/actions"

echo "Current version: $CURRENT"
echo "Next version:    $NEXT"
echo "Commit:          Release v${NEXT}"
echo "Tag:             v${NEXT}"
echo "Files:           Cargo.toml, Cargo.lock,"
echo "                 apps/relay-desktop/src-tauri/tauri.conf.json,"
echo "                 apps/relay-desktop/package.json"
echo "Actions:         $ACTIONS_URL"

if [ -n "$(git status --porcelain)" ]; then
    die "working tree is dirty; commit or stash first"
fi

branch=$(git symbolic-ref --short HEAD 2>/dev/null || true)
[ "$branch" = "main" ] || die "must be on main (currently ${branch:-detached})"

git fetch origin
if ! git rev-parse --verify -q origin/main >/dev/null; then
    die "origin/main does not exist; push main first"
fi
if [ "$(git rev-parse HEAD)" != "$(git rev-parse origin/main)" ]; then
    die "main is not up to date with origin/main (git pull and retry)"
fi

if [ "$DRY_RUN" = 1 ]; then
    echo
    echo "Dry run: no files changed, nothing committed or pushed."
    exit 0
fi

set_cargo_workspace_version "$NEXT"
set_json_version apps/relay-desktop/src-tauri/tauri.conf.json "$NEXT"
set_json_version apps/relay-desktop/package.json "$NEXT"

if ! cargo update --workspace --offline; then
    echo "cargo update --offline failed; retrying without --offline" >&2
    cargo update --workspace
fi

git add Cargo.toml Cargo.lock \
    apps/relay-desktop/src-tauri/tauri.conf.json \
    apps/relay-desktop/package.json
git commit -m "Release v${NEXT}"
git push origin main

# Dispatching on main (instead of pushing a tag) lets the build reuse and
# refresh main's Rust cache. The release job creates the tag when it publishes.
if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    gh workflow run release.yml --repo "$REPO" --ref main -f tag="v${NEXT}"
else
    echo "gh is missing or not logged in; pushing tag v${NEXT} instead (slower, uncached build)" >&2
    git tag "v${NEXT}"
    git push origin "v${NEXT}"
fi

echo
echo "Started the v${NEXT} release. Watch the build at:"
echo "  $ACTIONS_URL"
