#!/usr/bin/env bash
# Build this checkout and install it as the Relay background service on this
# Mac and, if configured, on a Windows PC over SSH.
#
#   ./scripts/deploy.sh --pc you@pc-host --pair     # first time
#   git pull && ./scripts/deploy.sh                 # everyday
#
# macOS ships bash 3.2: no associative arrays, no ${var,,}, no mapfile.
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd -P)
cd "$ROOT"

CONFIG_FILE="$ROOT/.relay-deploy"
DEFAULT_PORT=47321
WIN_TARGET=x86_64-pc-windows-gnu
WIN_EXE="target/$WIN_TARGET/release/relay.exe"

PC_ARG=""
PC=""
PAIR=0
MAC_ONLY=0
PC_ONLY=0
MAC_NAME=mac
PC_NAME=pc
LISTEN_PORT=""
RELAY=""
PC_HAS_NEW_EXE=0
SSH_DIR=""
SSH_CLEANED=0
SSH_OPTS=()

usage() {
    cat <<'EOF'
Deploy this checkout to the Mac (cargo install + LaunchAgent) and optionally
cross-compile and SSH-install the same revision on a Windows PC.

Usage:
  ./scripts/deploy.sh [--pc user@host] [--pair] [--mac-only] [--pc-only]
                      [--mac-name NAME] [--pc-name NAME] [--listen-port N]

  --pc user@host     Windows OpenSSH target (remembered in .relay-deploy).
                     RELAY_PC=user@host also works.
  --pair             Exchange `peer add` both ways. Addresses come from the
                     SSH session (%SSH_CONNECTION%).
  --mac-only         Only install on this machine.
  --pc-only          Only cross-compile and install on the PC.
  --mac-name NAME    Device name for `relay init` on this Mac (default: mac).
  --pc-name NAME     Device name for `relay init` on the PC (default: pc).
  --listen-port N    Pass --listen 0.0.0.0:N to `service install`.
  -h, --help         Show this help.

Everyday loop after the first --pc / --pair:
  git pull && ./scripts/deploy.sh

If no PC is configured, the Mac is still deployed and the script prints how
to add one.
EOF
}

die() {
    echo "error: $*" >&2
    exit 1
}

need_arg() {
    # $1 = flag, $2 = remaining argc before consuming the value
    if [ "$2" -lt 2 ]; then
        die "$1 needs a value"
    fi
}

simple_name() {
    # Device names are used unquoted inside `cmd /c "..."`.
    case "$1" in
        ''|*[!A-Za-z0-9._-]*)
            die "$2 must be a single token matching [A-Za-z0-9._-]+"
            ;;
    esac
}

while [ $# -gt 0 ]; do
    case "$1" in
        --pc)
            need_arg "$1" $#
            PC_ARG=$2
            shift 2
            ;;
        --pair)
            PAIR=1
            shift
            ;;
        --mac-only)
            MAC_ONLY=1
            shift
            ;;
        --pc-only)
            PC_ONLY=1
            shift
            ;;
        --mac-name)
            need_arg "$1" $#
            MAC_NAME=$2
            shift 2
            ;;
        --pc-name)
            need_arg "$1" $#
            PC_NAME=$2
            shift 2
            ;;
        --listen-port)
            need_arg "$1" $#
            LISTEN_PORT=$2
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1 (try --help)"
            ;;
    esac
done

if [ "$MAC_ONLY" = 1 ] && [ "$PC_ONLY" = 1 ]; then
    die "--mac-only and --pc-only cannot be used together"
fi

simple_name "$MAC_NAME" "--mac-name"
simple_name "$PC_NAME" "--pc-name"

if [ -n "$LISTEN_PORT" ]; then
    case "$LISTEN_PORT" in
        *[!0-9]*) die "--listen-port must be a number" ;;
    esac
fi

pair_port() {
    if [ -n "$LISTEN_PORT" ]; then
        printf '%s' "$LISTEN_PORT"
    else
        printf '%s' "$DEFAULT_PORT"
    fi
}

# Run `relay service install`, optionally with --listen 0.0.0.0:N.
service_install_local() {
    if [ -n "$LISTEN_PORT" ]; then
        "$RELAY" service install --listen "0.0.0.0:$LISTEN_PORT"
    else
        "$RELAY" service install
    fi
}

service_install_pc() {
    if [ -n "$LISTEN_PORT" ]; then
        pc_relay service install --listen "0.0.0.0:$LISTEN_PORT"
    else
        pc_relay service install
    fi
}

load_pc() {
    if [ -n "$PC_ARG" ]; then
        PC=$PC_ARG
        printf 'PC=%s\n' "$PC" >"$CONFIG_FILE"
        return
    fi
    if [ -n "${RELAY_PC:-}" ]; then
        PC=$RELAY_PC
        return
    fi
    if [ -f "$CONFIG_FILE" ]; then
        # One `PC=user@host` line. Read it without sourcing.
        while IFS= read -r line || [ -n "$line" ]; do
            line=${line%$'\r'}
            case "$line" in
                PC=*)
                    PC=${line#PC=}
                    ;;
            esac
        done <"$CONFIG_FILE"
    fi
}

load_pc

if [ "$PC_ONLY" = 1 ] && [ -z "$PC" ]; then
    die "--pc-only needs a PC: pass --pc user@host, set RELAY_PC, or write PC=user@host to .relay-deploy"
fi
if [ "$PAIR" = 1 ] && [ -z "$PC" ]; then
    die "--pair needs a PC: pass --pc user@host (then re-run --pair)"
fi

ensure_cargo() {
    if command -v cargo >/dev/null 2>&1; then
        return
    fi
    if [ -f "$HOME/.cargo/env" ]; then
        # shellcheck disable=SC1091
        . "$HOME/.cargo/env"
    fi
    command -v cargo >/dev/null 2>&1 || die "cargo not found. Install Rust (./scripts/install.sh --install-rust) and retry."
}

resolve_relay() {
    if [ -x "$HOME/.cargo/bin/relay" ]; then
        RELAY="$HOME/.cargo/bin/relay"
    elif command -v relay >/dev/null 2>&1; then
        RELAY=$(command -v relay)
    else
        RELAY=""
    fi
}

cleanup() {
    if [ "$SSH_CLEANED" = 1 ]; then
        return
    fi
    SSH_CLEANED=1
    if [ -n "$SSH_DIR" ] && [ -n "$PC" ]; then
        ssh -o "ControlPath=$SSH_DIR/cm" -O exit "$PC" >/dev/null 2>&1 || true
        rm -rf "$SSH_DIR"
    fi
}
trap cleanup EXIT INT TERM

# Run a cmd.exe command on the PC. Windows OpenSSH may use cmd or PowerShell
# as the default shell; wrapping in `cmd /c` makes both work. Remote cwd is
# the user home (OpenSSH default).
pc_cmd() {
    # $* is expanded here on purpose: we send one `cmd /c "..."` string.
    # shellcheck disable=SC2029
    ssh "${SSH_OPTS[@]}" "$PC" "cmd /c \"$*\""
}

# Talk to relay on the PC. While this run's upload is still at .\relay-new.exe
# we use that path (no spaces, no %LOCALAPPDATA% quoting). After it is deleted
# — or when the PC was already installed — cd into the install dir and run
# relay.exe from there, which avoids quoting a path that may contain spaces.
pc_relay() {
    if [ "$PC_HAS_NEW_EXE" = 1 ]; then
        pc_cmd ".\\relay-new.exe $*"
    else
        pc_cmd "cd /d %LOCALAPPDATA%\\Programs\\Relay && relay.exe $*"
    fi
}

pc_relay_quiet() {
    if [ "$PC_HAS_NEW_EXE" = 1 ]; then
        pc_cmd ".\\relay-new.exe $* >nul 2>&1"
    else
        pc_cmd "cd /d %LOCALAPPDATA%\\Programs\\Relay && relay.exe $* >nul 2>&1"
    fi
}

setup_ssh() {
    SSH_DIR=$(mktemp -d "${TMPDIR:-/tmp}/relay-cm.XXXXXX")
    SSH_OPTS=(
        -o ControlMaster=auto
        -o "ControlPath=$SSH_DIR/cm"
        -o ControlPersist=120
    )
    echo "==> Connecting to $PC"
    if ! pc_cmd "echo ok" >/dev/null; then
        echo "error: could not SSH to $PC." >&2
        echo "Set up key auth so you are not prompted every time:" >&2
        echo "  ssh-copy-id $PC" >&2
        echo "Then retry. See the README if $PC is a Windows administrator account." >&2
        exit 1
    fi
}

strip_cr() {
    printf '%s' "$1" | tr -d '\r'
}

nth_field() {
    # nth_field N STRING — bash 3.2: no mapfile. word-split a single line.
    _n=$1
    _s=${2-}
    [ -n "$_s" ] || { printf ''; return 0; }
    # shellcheck disable=SC2086
    set -- $_s
    case $_n in
        1) printf '%s' "${1:-}" ;;
        2) printf '%s' "${2:-}" ;;
        3) printf '%s' "${3:-}" ;;
        4) printf '%s' "${4:-}" ;;
        *) printf '' ;;
    esac
}

first_field() {
    nth_field 1 "${1-}"
}

print_revision() {
    rev=$(git rev-parse --short HEAD)
    echo "==> Deploying $rev"
    if [ -n "$(git status --porcelain)" ]; then
        echo "warning: working tree is dirty; the binary may report +dirty" >&2
    fi
}

deploy_mac() {
    ensure_cargo
    echo "==> Installing on this Mac"
    cargo install --path apps/relay-cli --locked --force --quiet
    resolve_relay
    if [ -z "$RELAY" ]; then
        die "relay was installed but is not on PATH. Expected $HOME/.cargo/bin/relay"
    fi
    if ! "$RELAY" id >/dev/null 2>&1; then
        echo "==> Initializing this Mac as $MAC_NAME"
        "$RELAY" init --name "$MAC_NAME"
    fi
    echo "==> Installing the Mac service"
    service_install_local
}

pc_prereqs() {
    if ! command -v x86_64-w64-mingw32-gcc >/dev/null 2>&1; then
        die "x86_64-w64-mingw32-gcc not found. On macOS: brew install mingw-w64"
    fi
    command -v rustup >/dev/null 2>&1 || die "rustup not found. Install Rust (./scripts/install.sh --install-rust)."
    echo "==> Adding Rust target $WIN_TARGET"
    rustup target add "$WIN_TARGET"
}

pc_upload_and_init() {
    ensure_cargo
    pc_prereqs
    echo "==> Building for Windows"
    cargo build --release --locked --target "$WIN_TARGET" -p relay-cli --quiet
    if [ ! -f "$WIN_EXE" ]; then
        die "build succeeded but $WIN_EXE is missing"
    fi
    echo "==> Copying relay.exe to $PC"
    scp "${SSH_OPTS[@]}" "$WIN_EXE" "$PC:relay-new.exe"
    PC_HAS_NEW_EXE=1
    if ! pc_relay_quiet id; then
        echo "==> Initializing the PC as $PC_NAME"
        pc_relay init --name "$PC_NAME"
    fi
}

pc_install_service() {
    echo "==> Installing the PC service"
    service_install_pc
    pc_cmd "del relay-new.exe"
    PC_HAS_NEW_EXE=0
}

do_pair() {
    resolve_relay
    if [ -z "$RELAY" ]; then
        die "relay not found on this Mac; deploy the Mac first (omit --pc-only)"
    fi
    if ! mac_id_line=$("$RELAY" id); then
        die "this Mac is not initialized (relay id failed). Deploy the Mac first."
    fi
    mac_id=$(first_field "$(strip_cr "$mac_id_line")")
    [ -n "$mac_id" ] || die "could not read this Mac's device id from: $mac_id_line"

    echo "==> Reading the PC device id"
    if ! pc_id_line=$(pc_relay id); then
        die "the PC is not initialized (relay id failed). Deploy the PC first (omit --mac-only)."
    fi
    pc_id_line=$(strip_cr "$pc_id_line")
    pc_id=$(first_field "$pc_id_line")
    [ -n "$pc_id" ] || die "could not read the PC device id from: $pc_id_line"

    echo "==> Reading addresses from SSH_CONNECTION"
    ssh_conn=$(pc_cmd "echo %SSH_CONNECTION%")
    ssh_conn=$(strip_cr "$ssh_conn")
    # %SSH_CONNECTION% is `<client-ip> <client-port> <server-ip> <server-port>`.
    # Field 1 is the Mac (SSH client); field 3 is the PC (SSH server).
    mac_ip=$(nth_field 1 "$ssh_conn")
    pc_ip=$(nth_field 3 "$ssh_conn")
    if [ -z "$mac_ip" ] || [ -z "$pc_ip" ]; then
        die "could not parse %SSH_CONNECTION% (got: $ssh_conn)"
    fi
    port=$(pair_port)

    echo "==> Pairing $MAC_NAME <-> $PC_NAME"
    "$RELAY" peer remove "$PC_NAME" >/dev/null 2>&1 || true
    "$RELAY" peer add "$PC_NAME" "$pc_id" --addr "${pc_ip}:${port}"
    pc_relay_quiet peer remove "$MAC_NAME" || true
    pc_relay peer add "$MAC_NAME" "$mac_id" --addr "${mac_ip}:${port}"

    echo "    Mac  $MAC_NAME  $mac_id  ${mac_ip}:${port}"
    echo "    PC   $PC_NAME  $pc_id  ${pc_ip}:${port}"
}

print_status() {
    echo
    echo "==> Status"
    resolve_relay
    if [ -n "$RELAY" ]; then
        echo "-- Mac"
        "$RELAY" --version || true
        "$RELAY" service status || true
    fi
    if [ -n "$PC" ] && [ -n "$SSH_DIR" ]; then
        echo "-- PC ($PC)"
        pc_relay --version || true
        pc_relay service status || true
    fi
    echo
    echo "Logs:"
    echo "  relay service logs -f"
    if [ -n "$PC" ]; then
        echo "  ssh $PC relay service logs -f"
    fi
}

print_revision

if [ "$PC_ONLY" != 1 ]; then
    deploy_mac
else
    resolve_relay
fi

HINT_ADD_PC=0
if [ -z "$PC" ]; then
    if [ "$MAC_ONLY" != 1 ]; then
        HINT_ADD_PC=1
    fi
else
    setup_ssh
    if [ "$MAC_ONLY" != 1 ]; then
        pc_upload_and_init
    fi
fi

if [ "$PAIR" = 1 ]; then
    do_pair
fi

if [ "$PC_HAS_NEW_EXE" = 1 ]; then
    pc_install_service
fi

print_status

if [ "$HINT_ADD_PC" = 1 ]; then
    echo
    echo "No Windows PC configured. This Mac is up to date."
    echo "To add a PC (remembered in .relay-deploy):"
    echo "  ./scripts/deploy.sh --pc you@pc-host --pair"
    echo "or:  export RELAY_PC=you@pc-host"
fi
