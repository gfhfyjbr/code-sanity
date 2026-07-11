#!/usr/bin/env bash

set -Eeuo pipefail
IFS=$'\n\t'

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)"
readonly SCRIPT_DIR
readonly APP_NAME="code-sanity"
readonly MIN_RUST_VERSION="1.85.0"

BIN_DIR="${CODE_SANITY_BIN_DIR:-${CARGO_HOME:-$HOME/.cargo}/bin}"
ADD_TO_PATH=0
BUILD=1
UNINSTALL=0
COLOR=1
BUILD_PID=""
TEMP_BINARY=""
BUILD_LOG=""

if [[ ! -t 2 || -n "${NO_COLOR:-}" ]]; then
    COLOR=0
fi

usage() {
    cat <<'EOF'
Install code-sanity from this source checkout.

Usage:
  ./install.sh [options]

Options:
  --bin-dir DIR    Install into DIR (default: $CARGO_HOME/bin or ~/.cargo/bin)
  --add-to-path    Add the selected directory to the current shell's rc file
  --no-build       Install an existing target/release/code-sanity binary
  --uninstall      Remove code-sanity from the selected directory
  --no-color       Disable ANSI colors
  -h, --help       Show this help

Environment:
  CODE_SANITY_BIN_DIR  Default installation directory
  CARGO_HOME           Cargo home used to derive the default directory
  NO_COLOR             Disable ANSI colors when set
EOF
}

paint() {
    local code="$1"
    shift
    if (( COLOR )); then
        printf '\033[%sm%s\033[0m' "$code" "$*"
    else
        printf '%s' "$*"
    fi
}

info() {
    printf '%s %s\n' "$(paint '1;36' '::')" "$*" >&2
}

success() {
    printf '%s %s\n' "$(paint '1;32' 'ok')" "$*" >&2
}

warn() {
    printf '%s %s\n' "$(paint '1;33' '!!')" "$*" >&2
}

fail() {
    printf '%s %s\n' "$(paint '1;31' 'xx')" "$*" >&2
    exit 1
}

cleanup() {
    local status=$?
    if [[ -n "$BUILD_PID" ]] && kill -0 "$BUILD_PID" 2>/dev/null; then
        kill "$BUILD_PID" 2>/dev/null || true
        wait "$BUILD_PID" 2>/dev/null || true
    fi
    [[ -n "$TEMP_BINARY" ]] && rm -f -- "$TEMP_BINARY"
    [[ -n "$BUILD_LOG" ]] && rm -f -- "$BUILD_LOG"
    exit "$status"
}
trap cleanup EXIT INT TERM

version_at_least() {
    local actual="$1"
    local required="$2"
    local actual_major actual_minor actual_patch required_major required_minor required_patch
    IFS=. read -r actual_major actual_minor actual_patch <<<"${actual%%-*}"
    IFS=. read -r required_major required_minor required_patch <<<"${required%%-*}"
    (( actual_major > required_major )) ||
        (( actual_major == required_major && actual_minor > required_minor )) ||
        (( actual_major == required_major && actual_minor == required_minor && actual_patch >= required_patch ))
}

path_contains() {
    case ":${PATH:-}:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

shell_rc_file() {
    case "${SHELL##*/}" in
        zsh) printf '%s/.zshrc\n' "$HOME" ;;
        bash)
            if [[ "$(uname -s)" == "Darwin" ]]; then
                printf '%s/.bash_profile\n' "$HOME"
            else
                printf '%s/.bashrc\n' "$HOME"
            fi
            ;;
        fish) printf '%s/.config/fish/config.fish\n' "$HOME" ;;
        *) printf '%s/.profile\n' "$HOME" ;;
    esac
}

add_bin_dir_to_path() {
    local rc_file line marker
    rc_file="$(shell_rc_file)"
    marker="# code-sanity installer"
    mkdir -p -- "$(dirname -- "$rc_file")"
    touch -- "$rc_file"
    if grep -Fq "$marker" "$rc_file"; then
        success "PATH is already managed in $rc_file"
        return
    fi
    if [[ "${SHELL##*/}" == "fish" ]]; then
        line="fish_add_path -- '$BIN_DIR'"
    else
        line="export PATH=\"$BIN_DIR:\$PATH\""
    fi
    printf '\n%s\n%s\n' "$marker" "$line" >>"$rc_file"
    success "Added $BIN_DIR to PATH in $rc_file"
    warn "Open a new shell or source $rc_file"
}

run_build() {
    local started frame_index frames="|/-\\"
    BUILD_LOG="$(mktemp "${TMPDIR:-/tmp}/code-sanity-build.XXXXXX")"
    started=$SECONDS
    (
        cd -- "$SCRIPT_DIR"
        cargo build --release --locked
    ) >"$BUILD_LOG" 2>&1 &
    BUILD_PID=$!

    if [[ -t 2 ]]; then
        frame_index=0
        while kill -0 "$BUILD_PID" 2>/dev/null; do
            printf '\r\033[2K%s Building optimized binary... %ss' \
                "$(paint '1;36' "${frames:frame_index++%4:1}")" "$((SECONDS - started))" >&2
            sleep 0.12
        done
        printf '\r\033[2K' >&2
    fi

    if ! wait "$BUILD_PID"; then
        BUILD_PID=""
        printf '%s\n' "$(paint '1;31' 'Build failed. Cargo output:')" >&2
        sed 's/^/  /' "$BUILD_LOG" >&2
        return 1
    fi
    BUILD_PID=""
    success "Release build finished in $((SECONDS - started))s"
}

while (($#)); do
    case "$1" in
        --bin-dir)
            (($# >= 2)) || fail "--bin-dir requires a directory"
            BIN_DIR="$2"
            shift 2
            ;;
        --add-to-path)
            ADD_TO_PATH=1
            shift
            ;;
        --no-build)
            BUILD=0
            shift
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        --no-color)
            COLOR=0
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            fail "unknown option: $1 (try --help)"
            ;;
    esac
done

BIN_DIR="${BIN_DIR/#\~/$HOME}"
readonly DESTINATION="$BIN_DIR/$APP_NAME"

printf '\n%s\n' "$(paint '1;36' '  code-sanity installer')" >&2
printf '%s\n\n' "$(paint '2' '  sanitized source workflows, one command away')" >&2

case "$(uname -s)" in
    Darwin|Linux) ;;
    *) fail "only macOS and Linux are supported" ;;
esac

if (( UNINSTALL )); then
    if [[ -e "$DESTINATION" ]]; then
        rm -f -- "$DESTINATION"
        success "Removed $DESTINATION"
    else
        warn "$DESTINATION is not installed"
    fi
    exit 0
fi

[[ -f "$SCRIPT_DIR/Cargo.toml" ]] || fail "run install.sh from a code-sanity source checkout"
command -v cargo >/dev/null 2>&1 || fail "cargo was not found; install Rust from https://rustup.rs"
command -v rustc >/dev/null 2>&1 || fail "rustc was not found; install Rust from https://rustup.rs"

rust_version="$(rustc --version | awk '{print $2}')"
version_at_least "$rust_version" "$MIN_RUST_VERSION" || \
    fail "Rust $MIN_RUST_VERSION or newer is required (found $rust_version)"
success "Rust $rust_version"

if (( BUILD )); then
    info "Building from $SCRIPT_DIR"
    run_build || exit 1
else
    info "Using the existing release binary"
fi

source_binary="$SCRIPT_DIR/target/release/$APP_NAME"
[[ -x "$source_binary" ]] || fail "$source_binary does not exist; rerun without --no-build"

mkdir -p -- "$BIN_DIR"
[[ -w "$BIN_DIR" ]] || fail "$BIN_DIR is not writable; choose another directory with --bin-dir"
TEMP_BINARY="$BIN_DIR/.${APP_NAME}.install.$$"
install -m 0755 "$source_binary" "$TEMP_BINARY"
mv -f -- "$TEMP_BINARY" "$DESTINATION"
TEMP_BINARY=""

installed_version="$($DESTINATION --version)"
success "Installed $installed_version"
success "Binary: $DESTINATION"

if (( ADD_TO_PATH )); then
    add_bin_dir_to_path
elif ! path_contains "$BIN_DIR"; then
    warn "$BIN_DIR is not currently in PATH"
    printf '   Run %s again or add this line to your shell config:\n   %s\n' \
        "$(paint '1' './install.sh --add-to-path')" \
        "$(paint '1' "export PATH=\"$BIN_DIR:\$PATH\"")" >&2
fi

printf '\n%s\n' "$(paint '1;32' 'Ready.')" >&2
printf '  %s\n' "$(paint '2' 'code-sanity init && code-sanity index && code-sanity verify')" >&2
