#!/usr/bin/env bash
# One-time: generate the Tauri updater signing key and install the public
# half into tauri.conf.json.
#
#   ./scripts/setup-updater-key.sh
#   ./scripts/setup-updater-key.sh --no-password
#   ./scripts/setup-updater-key.sh --release-repo owner/name
#
# The CLI has no --no-password flag (`bunx @tauri-apps/cli signer generate
# --help`): we pass --ci -p "" ourselves. Env overrides for tests:
#   RELAY_TAURI_CONF  path to tauri.conf.json
#   RELAY_UPDATER_KEY path to the private key file
#
# macOS ships bash 3.2: no associative arrays, no ${var,,}, no mapfile.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$ROOT"

KEY="${RELAY_UPDATER_KEY:-$HOME/.tauri/relay-updater.key}"
CONF="${RELAY_TAURI_CONF:-$ROOT/apps/relay-desktop/src-tauri/tauri.conf.json}"
NO_PASSWORD=0
RELEASE_REPO=""
PASSWORD=""

usage() {
    cat <<'EOF'
Generate the Tauri updater minisign key and write the public key into
apps/relay-desktop/src-tauri/tauri.conf.json.

Usage:
  ./scripts/setup-updater-key.sh [--no-password] [--release-repo owner/name]

  --no-password          Generate an unencrypted private key (the CLI has
                         no such flag; we call `signer generate --ci -p ""`).
  --release-repo O/N     Use this GitHub repo in the updater endpoint
                         instead of `origin` (needed when Releases live in
                         a public repo while the source repo is private).

Refuses to overwrite an existing key. If `gh` is authenticated, also sets
the TAURI_SIGNING_PRIVATE_KEY and TAURI_SIGNING_PRIVATE_KEY_PASSWORD
repository secrets.

Env:
  RELAY_TAURI_CONF   override path to tauri.conf.json
  RELAY_UPDATER_KEY  override private-key path
EOF
}

die() {
    echo "error: $*" >&2
    exit 1
}

need_arg() {
    if [ "$2" -lt 2 ]; then
        die "$1 needs a value"
    fi
}

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

while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help)
            usage
            exit 0
            ;;
        --no-password)
            NO_PASSWORD=1
            shift
            ;;
        --release-repo)
            need_arg "$1" $#
            RELEASE_REPO=$2
            shift 2
            ;;
        *)
            die "unknown option: $1 (try --help)"
            ;;
    esac
done

if [ -n "$RELEASE_REPO" ]; then
    case "$RELEASE_REPO" in
        */*) ;;
        *) die "--release-repo must be owner/name" ;;
    esac
fi

[ -f "$CONF" ] || die "missing $CONF"

if [ -e "$KEY" ] || [ -e "${KEY}.pub" ]; then
    die "refusing to overwrite existing key at $KEY (delete it yourself if you really want a new one)"
fi

command -v bun >/dev/null 2>&1 || die "bun not found; install Bun"

if [ "$NO_PASSWORD" = 1 ]; then
    PASSWORD=""
else
    printf 'Password for the updater private key (empty for none): '
    read -r -s PASSWORD
    echo
    printf 'Confirm password: '
    read -r -s PASSWORD2
    echo
    if [ "$PASSWORD" != "$PASSWORD2" ]; then
        die "passwords do not match"
    fi
fi

mkdir -p "$(dirname "$KEY")"

echo "==> Generating updater key at $KEY"
# Current flags (bunx @tauri-apps/cli signer generate --help):
#   -w path, -p password, --ci (skip prompts), -f force (we never pass -f).
bunx @tauri-apps/cli signer generate -w "$KEY" --ci -p "$PASSWORD"

if [ ! -f "${KEY}.pub" ]; then
    die "expected public key at ${KEY}.pub"
fi

ENDPOINT_REPO=$RELEASE_REPO
if [ -z "$ENDPOINT_REPO" ]; then
    ENDPOINT_REPO=$(github_owner_repo)
fi

echo "==> Writing public key into $CONF"
bun -e '
    const fs = require("fs");
    const confPath = process.argv[1];
    const pubPath = process.argv[2];
    const ownerRepo = process.argv[3];
    const pub = fs.readFileSync(pubPath, "utf8").trim();
    if (!pub) {
        console.error("empty public key at " + pubPath);
        process.exit(1);
    }
    let text = fs.readFileSync(confPath, "utf8");
    if (text.indexOf("REPLACE_WITH_TAURI_UPDATER_PUBKEY") !== -1) {
        text = text.split("REPLACE_WITH_TAURI_UPDATER_PUBKEY").join(pub);
    } else {
        const j = JSON.parse(text);
        if (!j.plugins) j.plugins = {};
        if (!j.plugins.updater) j.plugins.updater = {};
        j.plugins.updater.pubkey = pub;
        text = JSON.stringify(j, null, 2) + "\n";
    }
    if (ownerRepo && ownerRepo !== "OWNER/REPO" && text.indexOf("OWNER/REPO") !== -1) {
        text = text.split("OWNER/REPO").join(ownerRepo);
    }
    fs.writeFileSync(confPath, text);
' "$CONF" "${KEY}.pub" "$ENDPOINT_REPO"

if [ "$ENDPOINT_REPO" = "OWNER/REPO" ]; then
    echo "note: origin is not a github.com remote; left OWNER/REPO in the updater endpoint."
    echo "      Edit plugins.updater.endpoints in $CONF once the GitHub repo exists."
elif [ -n "$RELEASE_REPO" ]; then
    echo "note: updater endpoint now points at https://github.com/${RELEASE_REPO}/releases/latest/download/latest.json"
else
    echo "note: replaced OWNER/REPO in the updater endpoint with ${ENDPOINT_REPO}"
fi

echo
echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
echo "WARNING: If you lose this private key or its password,"
echo "existing Relay desktop installs can NEVER auto-update again."
echo "Back up:"
echo "  $KEY"
echo "and the password, offline. Do not commit the private key."
echo "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
echo

SECRETS_SET=0
if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    GH_REPO=""
    if GH_REPO=$(gh repo view --json nameWithOwner -q .nameWithOwner 2>/dev/null); then
        :
    else
        origin_repo=$(github_owner_repo)
        if [ "$origin_repo" != "OWNER/REPO" ]; then
            GH_REPO=$origin_repo
        fi
    fi
    if [ -n "$GH_REPO" ]; then
        echo "==> Setting Actions secrets on $GH_REPO"
        gh secret set TAURI_SIGNING_PRIVATE_KEY --repo "$GH_REPO" <"$KEY"
        printf '%s' "$PASSWORD" | gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD --repo "$GH_REPO"
        SECRETS_SET=1
        echo "Set TAURI_SIGNING_PRIVATE_KEY and TAURI_SIGNING_PRIVATE_KEY_PASSWORD."
    fi
fi

if [ "$SECRETS_SET" != 1 ]; then
    echo "gh is missing or not authenticated. Set the secrets by hand:"
    echo
    echo "  GitHub > Settings > Secrets and variables > Actions"
    echo
    echo "  Secret TAURI_SIGNING_PRIVATE_KEY"
    echo "    paste the contents of $KEY"
    echo "  Secret TAURI_SIGNING_PRIVATE_KEY_PASSWORD"
    echo "    the password you just chose (empty if --no-password)"
    echo
    echo "  Optional (private source repo, public releases):"
    echo "    Variable RELEASE_REPO  = owner/name of the public repo"
    echo "    Secret   RELEASE_TOKEN = PAT with contents:write on that repo"
    echo
    echo "Then: gh auth login && $0  (will refuse: key already exists;"
    echo "delete only the GitHub secrets step, or set them with:)"
    echo "  gh secret set TAURI_SIGNING_PRIVATE_KEY < \"$KEY\""
    echo "  printf '%s' '<password>' | gh secret set TAURI_SIGNING_PRIVATE_KEY_PASSWORD"
fi

echo
echo "Commit the updated tauri.conf.json (public key + endpoint) and push:"
echo "  git add apps/relay-desktop/src-tauri/tauri.conf.json"
echo "  git commit -m \"Set Tauri updater public key\""
echo
echo "Private key stays on this machine: $KEY"
