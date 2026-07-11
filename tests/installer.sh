#!/usr/bin/env bash

set -Eeuo pipefail
IFS=$'\n\t'

ROOT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)"
TEMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/code-sanity-installer-test.XXXXXX")"
trap 'rm -rf -- "$TEMP_ROOT"' EXIT

SOURCE_DIR="$TEMP_ROOT/source"
HOME_DIR="$TEMP_ROOT/home"
BIN_DIR="$TEMP_ROOT/bin"
COMPLETIONS_DIR="$TEMP_ROOT/completions with spaces"
ZSH_RC_FILE="$HOME_DIR/.zshrc"
printf -v QUOTED_COMPLETIONS_DIR '%q' "$COMPLETIONS_DIR"
mkdir -p -- "$SOURCE_DIR/target/release" "$HOME_DIR"
cp "$ROOT_DIR/install.sh" "$SOURCE_DIR/install.sh"
: >"$SOURCE_DIR/Cargo.toml"
printf '%s\n' 'export KEEP_ME=1' >"$ZSH_RC_FILE"

cat >"$SOURCE_DIR/target/release/code-sanity" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
case "${1-}" in
    --version)
        printf '%s\n' 'code-sanity 0.4.6'
        ;;
    completions)
        [[ -z "${CODE_SANITY_TEST_NO_COMPLETIONS:-}" ]] || exit 64
        [[ "${2-}" == "zsh" ]]
        printf '%s\n' '#compdef code-sanity' '' '_code-sanity() { _arguments "1: :((init index verify))" }' 'compdef _code-sanity code-sanity'
        ;;
    *)
        exit 64
        ;;
esac
EOF
chmod 0755 "$SOURCE_DIR/target/release/code-sanity"

run_installer() {
    HOME="$HOME_DIR" \
    SHELL=/bin/zsh \
    CODE_SANITY_ZSH_RC_FILE="$ZSH_RC_FILE" \
        "$SOURCE_DIR/install.sh" \
        --no-build \
        --bin-dir "$BIN_DIR" \
        --zsh-completions-dir "$COMPLETIONS_DIR" \
        --no-color
}

run_installer
test -x "$BIN_DIR/code-sanity"
test -f "$COMPLETIONS_DIR/_code-sanity"
grep -Fqx '#compdef code-sanity' "$COMPLETIONS_DIR/_code-sanity"
grep -Fq '# >>> code-sanity completions >>>' "$ZSH_RC_FILE"
grep -Fq "$QUOTED_COMPLETIONS_DIR" "$ZSH_RC_FILE"

# Reinstalling refreshes the generated file and replaces, rather than
# duplicating, the managed zshrc block.
run_installer
test "$(grep -Fc '# >>> code-sanity completions >>>' "$ZSH_RC_FILE")" = 1
test "$(grep -Fc '# <<< code-sanity completions <<<' "$ZSH_RC_FILE")" = 1
if command -v zsh >/dev/null 2>&1; then
    zsh -n "$ZSH_RC_FILE"
    ZSH_RC_FILE="$ZSH_RC_FILE" zsh -fc '
        autoload -Uz compinit
        compinit -D
        source "$ZSH_RC_FILE"
        [[ "${_comps[code-sanity]-}" == "_code-sanity" ]]
    '
fi

HOME="$HOME_DIR" \
SHELL=/bin/zsh \
CODE_SANITY_ZSH_RC_FILE="$ZSH_RC_FILE" \
    "$SOURCE_DIR/install.sh" \
    --uninstall \
    --bin-dir "$BIN_DIR" \
    --zsh-completions-dir "$COMPLETIONS_DIR" \
    --no-color

test ! -e "$BIN_DIR/code-sanity"
test ! -e "$COMPLETIONS_DIR/_code-sanity"
if grep -Fq '# >>> code-sanity completions >>>' "$ZSH_RC_FILE"; then
    exit 1
fi
if grep -Fq '# <<< code-sanity completions <<<' "$ZSH_RC_FILE"; then
    exit 1
fi
grep -Fqx 'export KEEP_ME=1' "$ZSH_RC_FILE"

# A newer installer may be used before a completion-aware release exists.
# The binary still installs successfully and completion setup is skipped.
CODE_SANITY_TEST_NO_COMPLETIONS=1 run_installer
test -x "$BIN_DIR/code-sanity"
test ! -e "$COMPLETIONS_DIR/_code-sanity"
if grep -Fq '# >>> code-sanity completions >>>' "$ZSH_RC_FILE"; then
    exit 1
fi
