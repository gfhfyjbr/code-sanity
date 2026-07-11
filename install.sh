#!/usr/bin/env bash

set -Eeuo pipefail
IFS=$'\n\t'

SCRIPT_SOURCE="${BASH_SOURCE[0]-}"
if [[ -n "$SCRIPT_SOURCE" && -f "$SCRIPT_SOURCE" ]]; then
    SCRIPT_DIR="$(cd -- "$(dirname -- "$SCRIPT_SOURCE")" && pwd -P)"
else
    SCRIPT_DIR="$PWD"
fi
readonly SCRIPT_DIR
readonly APP_NAME="code-sanity"
readonly DEFAULT_REPOSITORY="gfhfyjbr/code-sanity"
readonly MIN_RUST_VERSION="1.85.0"

BIN_DIR="${CODE_SANITY_BIN_DIR:-${CARGO_HOME:-$HOME/.cargo}/bin}"
REPOSITORY="${CODE_SANITY_REPOSITORY:-$DEFAULT_REPOSITORY}"
VERSION="${CODE_SANITY_VERSION:-latest}"
TARGET="${CODE_SANITY_TARGET:-}"
SOURCE_MODE=0
BUILD_SOURCE=1
ADD_TO_PATH=0
UNINSTALL=0
PRINT_TARGET=0
COLOR=1
TASK_PID=""
TASK_LOG=""
TEMP_DIR=""
TEMP_BINARY=""

if [[ ! -t 2 || -n "${NO_COLOR:-}" ]]; then
    COLOR=0
fi

usage() {
    cat <<'EOF'
Install the matching code-sanity binary from GitHub Releases.

Usage:
  ./install.sh [options]
  curl -fsSL https://raw.githubusercontent.com/gfhfyjbr/code-sanity/main/install.sh | bash

Options:
  --version VERSION  Install a release tag such as v0.4.4 (default: latest)
  --repo OWNER/REPO  Download from another GitHub repository
  --bin-dir DIR      Install into DIR (default: $CARGO_HOME/bin or ~/.cargo/bin)
  --add-to-path      Add the selected directory to the current shell's rc file
  --from-source      Build this checkout with cargo instead of downloading
  --no-build         Install an existing target/release/code-sanity binary
  --uninstall        Remove code-sanity from the selected directory
  --print-target     Print the detected release target and exit
  --no-color         Disable ANSI colors
  -h, --help         Show this help

Environment:
  CODE_SANITY_BIN_DIR     Default installation directory
  CODE_SANITY_REPOSITORY  Default GitHub OWNER/REPO
  CODE_SANITY_VERSION     Default release version or "latest"
  CODE_SANITY_TARGET      Override platform detection (primarily for CI)
  GITHUB_TOKEN            Optional token for GitHub downloads
  CARGO_HOME              Cargo home used to derive the default directory
  NO_COLOR                Disable ANSI colors when set
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
    if [[ -n "$TASK_PID" ]] && kill -0 "$TASK_PID" 2>/dev/null; then
        kill "$TASK_PID" 2>/dev/null || true
        wait "$TASK_PID" 2>/dev/null || true
    fi
    [[ -n "$TEMP_BINARY" ]] && rm -f -- "$TEMP_BINARY"
    [[ -n "$TASK_LOG" ]] && rm -f -- "$TASK_LOG"
    [[ -n "$TEMP_DIR" ]] && rm -rf -- "$TEMP_DIR"
    exit "$status"
}
trap cleanup EXIT INT TERM

detect_target() {
    local os arch
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$arch" in
        x86_64|amd64) arch="x86_64" ;;
        arm64|aarch64) arch="aarch64" ;;
        *) fail "unsupported CPU architecture: $arch" ;;
    esac
    case "$os" in
        Darwin) printf '%s-apple-darwin\n' "$arch" ;;
        Linux) printf '%s-unknown-linux-gnu\n' "$arch" ;;
        *) fail "only macOS and Linux are supported" ;;
    esac
}

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

github_curl() {
    local args=(
        --fail
        --silent
        --show-error
        --location
        --retry 3
        --retry-delay 1
        --connect-timeout 15
        --max-time 180
        --proto '=https'
        --tlsv1.2
    )
    if [[ -n "${GITHUB_TOKEN:-}" ]]; then
        args+=(--header "Authorization: Bearer $GITHUB_TOKEN")
    fi
    curl "${args[@]}" "$@"
}

run_task() {
    local label="$1"
    shift
    local started frame_index frames="|/-\\"
    TASK_LOG="$(mktemp "${TMPDIR:-/tmp}/code-sanity-task.XXXXXX")"
    started=$SECONDS
    "$@" >"$TASK_LOG" 2>&1 &
    TASK_PID=$!

    if [[ -t 2 ]]; then
        frame_index=0
        while kill -0 "$TASK_PID" 2>/dev/null; do
            printf '\r\033[2K%s %s... %ss' \
                "$(paint '1;36' "${frames:frame_index++%4:1}")" "$label" "$((SECONDS - started))" >&2
            sleep 0.12
        done
        printf '\r\033[2K' >&2
    else
        info "$label..."
    fi

    if ! wait "$TASK_PID"; then
        TASK_PID=""
        printf '%s\n' "$(paint '1;31' "$label failed:")" >&2
        sed 's/^/  /' "$TASK_LOG" >&2
        return 1
    fi
    TASK_PID=""
    rm -f -- "$TASK_LOG"
    TASK_LOG=""
    success "$label ($((SECONDS - started))s)"
}

resolve_release_tag() {
    local effective
    if [[ "$VERSION" != "latest" ]]; then
        case "$VERSION" in
            v*) printf '%s\n' "$VERSION" ;;
            *) printf 'v%s\n' "$VERSION" ;;
        esac
        return
    fi
    info "Resolving latest release from $REPOSITORY"
    if ! effective="$(github_curl --output /dev/null --write-out '%{url_effective}' \
        "https://github.com/$REPOSITORY/releases/latest")"; then
        fail "could not resolve the latest release; use --version or --from-source"
    fi
    effective="${effective%/}"
    printf '%s\n' "${effective##*/}"
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    else
        fail "sha256sum or shasum is required to verify the release archive"
    fi
}

verify_archive_layout() {
    local archive="$1"
    local root="$2"
    local entry
    while IFS= read -r entry; do
        case "/$entry/" in
            *"/../"*|*"/./"*) fail "release archive contains an unsafe path: $entry" ;;
        esac
        case "$entry" in
            "$root"|"$root/"|"$root/"*) ;;
            *) fail "release archive contains an unexpected path: $entry" ;;
        esac
    done < <(tar -tzf "$archive")
}

download_release() {
    local tag name archive checksum base expected actual
    command -v curl >/dev/null 2>&1 || fail "curl is required to download a release"
    command -v tar >/dev/null 2>&1 || fail "tar is required to unpack a release"
    tag="$(resolve_release_tag)"
    [[ "$tag" =~ ^v[0-9][A-Za-z0-9._-]*$ ]] || fail "invalid release tag: $tag"
    name="$APP_NAME-$tag-$TARGET"
    archive="$name.tar.gz"
    checksum="$archive.sha256"
    base="https://github.com/$REPOSITORY/releases/download/$tag"
    TEMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/code-sanity-install.XXXXXX")"

    run_task "Downloading $archive" github_curl --output "$TEMP_DIR/$archive" "$base/$archive" || \
        fail "release asset not found; verify --version/--repo or use --from-source"
    run_task "Downloading checksum" github_curl --output "$TEMP_DIR/$checksum" "$base/$checksum" || \
        fail "release checksum not found; refusing an unverified install"

    expected="$(awk 'NR == 1 {print $1}' "$TEMP_DIR/$checksum" | tr 'A-F' 'a-f')"
    [[ "$expected" =~ ^[0-9a-fA-F]{64}$ ]] || fail "release checksum file is malformed"
    actual="$(sha256_file "$TEMP_DIR/$archive")"
    [[ "$actual" == "$expected" ]] || fail "release checksum mismatch"
    success "SHA-256 verified"

    verify_archive_layout "$TEMP_DIR/$archive" "$name"
    tar -xzf "$TEMP_DIR/$archive" -C "$TEMP_DIR"
    SOURCE_BINARY="$TEMP_DIR/$name/$APP_NAME"
    [[ -f "$SOURCE_BINARY" && ! -L "$SOURCE_BINARY" && -x "$SOURCE_BINARY" ]] || \
        fail "release archive does not contain a regular executable $APP_NAME"
    RELEASE_TAG="$tag"
}

build_source() {
    cd -- "$SCRIPT_DIR"
    cargo build --release --locked
}

prepare_source_binary() {
    [[ -f "$SCRIPT_DIR/Cargo.toml" ]] || fail "--from-source requires a code-sanity source checkout"
    if (( BUILD_SOURCE )); then
        command -v cargo >/dev/null 2>&1 || fail "cargo was not found; install Rust from https://rustup.rs"
        command -v rustc >/dev/null 2>&1 || fail "rustc was not found; install Rust from https://rustup.rs"
        local rust_version
        rust_version="$(rustc --version | awk '{print $2}')"
        version_at_least "$rust_version" "$MIN_RUST_VERSION" || \
            fail "Rust $MIN_RUST_VERSION or newer is required (found $rust_version)"
        success "Rust $rust_version"
        run_task "Building optimized binary" build_source || exit 1
    else
        info "Using the existing release binary"
    fi
    SOURCE_BINARY="$SCRIPT_DIR/target/release/$APP_NAME"
    [[ -x "$SOURCE_BINARY" ]] || fail "$SOURCE_BINARY does not exist; use --from-source without --no-build"
    RELEASE_TAG=""
}

while (($#)); do
    case "$1" in
        --version)
            (($# >= 2)) || fail "--version requires a release tag"
            VERSION="$2"
            shift 2
            ;;
        --repo)
            (($# >= 2)) || fail "--repo requires OWNER/REPO"
            REPOSITORY="$2"
            shift 2
            ;;
        --bin-dir)
            (($# >= 2)) || fail "--bin-dir requires a directory"
            BIN_DIR="$2"
            shift 2
            ;;
        --add-to-path)
            ADD_TO_PATH=1
            shift
            ;;
        --from-source)
            SOURCE_MODE=1
            shift
            ;;
        --no-build)
            SOURCE_MODE=1
            BUILD_SOURCE=0
            shift
            ;;
        --uninstall)
            UNINSTALL=1
            shift
            ;;
        --print-target)
            PRINT_TARGET=1
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
        *) fail "unknown option: $1 (try --help)" ;;
    esac
done

[[ "$REPOSITORY" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || fail "invalid GitHub repository: $REPOSITORY"
BIN_DIR="${BIN_DIR/#\~/$HOME}"
TARGET="${TARGET:-$(detect_target)}"
case "$TARGET" in
    x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-apple-darwin|aarch64-apple-darwin) ;;
    *) fail "unsupported release target: $TARGET" ;;
esac
readonly DESTINATION="$BIN_DIR/$APP_NAME"

if (( PRINT_TARGET )); then
    printf '%s\n' "$TARGET"
    exit 0
fi

printf '\n%s\n' "$(paint '1;36' '  code-sanity installer')" >&2
if (( SOURCE_MODE )); then
    install_source="source"
else
    install_source="$VERSION"
fi
printf '%s\n\n' "$(paint '2' "  $TARGET / $install_source")" >&2

if (( UNINSTALL )); then
    if [[ -e "$DESTINATION" ]]; then
        rm -f -- "$DESTINATION"
        success "Removed $DESTINATION"
    else
        warn "$DESTINATION is not installed"
    fi
    exit 0
fi

SOURCE_BINARY=""
RELEASE_TAG=""
if (( SOURCE_MODE )); then
    prepare_source_binary
else
    download_release
fi

candidate_version="$($SOURCE_BINARY --version)"
if [[ -n "$RELEASE_TAG" && "$candidate_version" != "$APP_NAME ${RELEASE_TAG#v}" ]]; then
    fail "release tag $RELEASE_TAG contains unexpected binary version: $candidate_version"
fi
success "Verified $candidate_version"

mkdir -p -- "$BIN_DIR"
[[ -w "$BIN_DIR" ]] || fail "$BIN_DIR is not writable; choose another directory with --bin-dir"
TEMP_BINARY="$BIN_DIR/.${APP_NAME}.install.$$"
install -m 0755 "$SOURCE_BINARY" "$TEMP_BINARY"
mv -f -- "$TEMP_BINARY" "$DESTINATION"
TEMP_BINARY=""

installed_version="$($DESTINATION --version)"
[[ "$installed_version" == "$candidate_version" ]] || fail "installed binary failed version verification"
success "Installed $installed_version"
success "Binary: $DESTINATION"

if (( ADD_TO_PATH )); then
    add_bin_dir_to_path
elif ! path_contains "$BIN_DIR"; then
    warn "$BIN_DIR is not currently in PATH"
    printf '   Re-run with %s or add this line to your shell config:\n   %s\n' \
        "$(paint '1' '--add-to-path')" \
        "$(paint '1' "export PATH=\"$BIN_DIR:\$PATH\"")" >&2
fi

printf '\n%s\n' "$(paint '1;32' 'Ready.')" >&2
printf '  %s\n' "$(paint '2' 'code-sanity init && code-sanity index && code-sanity verify')" >&2
