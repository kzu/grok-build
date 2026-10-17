#!/usr/bin/env bash
#
# Remove the grok binaries installed by scripts/install.sh.
# Leaves ~/.grok/config.toml, auth, and sessions in place.
#
#   curl -fsSL https://github.com/kzu/grok-build/releases/latest/download/uninstall.sh | bash

set -euo pipefail

BIN_DIR="${GROK_BIN_DIR:-$HOME/.grok/bin}"
DOWNLOAD_DIR="${GROK_DOWNLOAD_DIR:-$HOME/.grok/downloads}"

remove_link_if_ours() {
    local path="$1"
    [ -L "$path" ] || return 0
    local target
    target="$(readlink "$path" || true)"
    case "$target" in
        *".grok/bin/grok"* | *".grok/bin/agent"* | *".grok/downloads/grok-"*)
            rm -f "$path"
            echo "Removed $path" >&2
            ;;
    esac
}

if [ -d "$BIN_DIR" ]; then
    rm -f "$BIN_DIR/grok" "$BIN_DIR/grok.exe" "$BIN_DIR/agent" "$BIN_DIR/agent.exe"
    rm -f "$BIN_DIR/grok.exe.old" "$BIN_DIR/agent.exe.old"
    echo "Removed binaries from $BIN_DIR" >&2
fi

if [ -d "$DOWNLOAD_DIR" ]; then
    find "$DOWNLOAD_DIR" -maxdepth 1 -type f -name 'grok-*' -delete
    echo "Removed downloaded binaries from $DOWNLOAD_DIR" >&2
fi

remove_link_if_ours "$HOME/.local/bin/grok"
remove_link_if_ours "$HOME/.local/bin/agent"
remove_link_if_ours "/usr/local/bin/grok"
remove_link_if_ours "/usr/local/bin/agent"

strip_rc() {
    local rc="$1"
    [ -f "$rc" ] || return 0
    grep -qs "grok installer" "$rc" || return 0
    local tmp="$rc.tmp.$$"
    awk '
        /# >>> grok installer >>>/ { skip=1; next }
        /# <<< grok installer <<</ { skip=0; next }
        !skip { print }
    ' "$rc" > "$tmp" && mv "$tmp" "$rc"
    echo "Removed PATH block from $rc" >&2
}

strip_rc "$HOME/.bashrc"
strip_rc "$HOME/.zshrc"
strip_rc "$HOME/.config/fish/config.fish"

rm -f "$HOME/.grok/completions/bash/grok.bash" \
    "$HOME/.grok/completions/zsh/_grok" \
    "$HOME/.config/fish/completions/grok.fish"

echo "grok was uninstalled. Config and sessions under ~/.grok were kept." >&2
echo "Open a new terminal so PATH picks up the change." >&2
