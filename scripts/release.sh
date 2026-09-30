#!/usr/bin/env bash
# Start a Relay release build. The workflow on main bumps the shared version,
# commits it, and builds the desktop app and CLI. Ordinary pushes do not.
#
#   ./scripts/release.sh                 # patch (default)
#   ./scripts/release.sh minor
#   ./scripts/release.sh 1.2.3
#   ./scripts/release.sh none            # rebuild the version already on main
#   ./scripts/release.sh --dry-run
#
# The workflow calls `./scripts/release.sh --apply patch` to edit the version
# files in its checkout. That mode does not commit or start a build.
#
# macOS ships bash 3.2: no associative arrays, no ${var,,}, no mapfile.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$ROOT"

DRY_RUN=0
APPLY=0
BUMP="patch"

usage() {
    cat <<'EOF'
Start the release workflow on main (needs gh). The workflow bumps the
version, commits `Release vX.Y.Z`, and builds that commit.

Usage:
  ./scripts/release.sh [--dry-run] [patch|minor|major|none|X.Y.Z]

  patch | minor | major   Semver bump (default: patch, the build number).
  none                    Rebuild the version already on origin/main.
  X.Y.Z                   Publish that version.
  --dry-run               Print the plan; start nothing.
  --apply KIND            Write the version files and print the new version.
                          Does not commit, push, or start a build.

Requires a clean tree on main, up to date with origin/main, except --apply.
The workflow updates Cargo.toml, Cargo.lock,
apps/relay-desktop/src-tauri/tauri.conf.json, and
apps/relay-desktop/package.json.
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
        --apply)
            APPLY=1
            shift
            ;;
        patch|minor|major|none)
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
    bun -e '
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
if [ "$BUMP" = "none" ]; then
    NEXT="$CURRENT"
else
    NEXT=$(next_version "$CURRENT" "$BUMP")
fi

if [ "$APPLY" = 1 ]; then
    [ "$DRY_RUN" = 0 ] || die "--apply and --dry-run together do nothing useful"
    if [ "$NEXT" != "$CURRENT" ]; then
        set_cargo_workspace_version "$NEXT"
        set_json_version apps/relay-desktop/src-tauri/tauri.conf.json "$NEXT"
        set_json_version apps/relay-desktop/package.json "$NEXT"
        if ! cargo update --workspace --offline; then
            echo "cargo update --offline failed; retrying without --offline" >&2
            cargo update --workspace
        fi
    fi
    printf '%s\n' "$NEXT"
    exit 0
fi

REPO=$(github_owner_repo)
ACTIONS_URL="https://github.com/${REPO}/actions"

echo "Current version: $CURRENT"
echo "Next version:    $NEXT"
echo "Commit:          Release v${NEXT} (made by the workflow on origin/main)"
echo "Tag:             v${NEXT}"
echo "Actions:         $ACTIONS_URL"
if [ "$BUMP" = "patch" ]; then
    echo "If v${CURRENT} is not published yet, the workflow rebuilds it instead."
fi

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
    echo "Dry run: workflow not started."
    exit 0
fi

if ! command -v gh >/dev/null 2>&1 || ! gh auth status >/dev/null 2>&1; then
    case "$BUMP" in
        patch|minor|major|none)
            hint="gh workflow run release.yml --repo ${REPO} --ref main -f bump=${BUMP}"
            ;;
        *)
            hint="gh workflow run release.yml --repo ${REPO} --ref main -f bump=patch -f version=${NEXT}"
            ;;
    esac
    die "gh is missing or not logged in. Cut the release with: ${hint}"
fi

case "$BUMP" in
    patch|minor|major|none)
        gh workflow run release.yml --repo "$REPO" --ref main -f "bump=${BUMP}"
        ;;
    *)
        gh workflow run release.yml --repo "$REPO" --ref main -f bump=patch -f "version=${NEXT}"
        ;;
esac

echo
echo "Started the v${NEXT} release. Watch the build at:"
echo "  $ACTIONS_URL"
