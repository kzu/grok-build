#!/usr/bin/env bash
#
# Install grok from this repository's GitHub Releases.
#
#   curl -fsSL https://github.com/kzu/grok-build/releases/latest/download/install.sh | bash
#   curl -fsSL https://github.com/kzu/grok-build/releases/latest/download/install.sh | bash -s 1.0.45
#   GROK_VERSION=1.0.45 bash install.sh
#
# Linux and WSL install the Linux binary. Git Bash on Windows installs the
# Windows binary. PowerShell users should run install.ps1 instead.
#
# GROK_GITHUB_REPO   default kzu/grok-build
# GROK_BIN_DIR       default ~/.grok/bin
# GROK_DOWNLOAD_DIR  default ~/.grok/downloads
# GROK_PRINT_PLAN=1  print the asset URL and exit

set -euo pipefail

REPO="${GROK_GITHUB_REPO:-kzu/grok-build}"
VERSION="${1:-${GROK_VERSION:-}}"
BIN_DIR="${GROK_BIN_DIR:-$HOME/.grok/bin}"
DOWNLOAD_DIR="${GROK_DOWNLOAD_DIR:-$HOME/.grok/downloads}"

if [ -n "$VERSION" ]; then
    VERSION="${VERSION#v}"
    if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9._]+)?$ ]]; then
        echo "Invalid version: $VERSION (expected X.Y.Z or X.Y.Z-suffix)" >&2
        exit 1
    fi
fi

DOWNLOADER=""
if command -v curl >/dev/null 2>&1; then
    DOWNLOADER="curl"
elif command -v wget >/dev/null 2>&1; then
    DOWNLOADER="wget"
else
    echo "Either curl or wget is required" >&2
    exit 1
fi

download_file() {
    local url="$1" output="$2"
    if [ "$DOWNLOADER" = "curl" ]; then
        curl -fsSL -o "$output" "$url"
    else
        wget -q -O "$output" "$url"
    fi
}

case "$(uname -s)" in
    Linux) os="linux" ;;
    MINGW* | MSYS* | CYGWIN*) os="windows" ;;
    Darwin)
        echo "This repository publishes Linux and Windows binaries." >&2
        echo "On macOS, install the upstream build: curl -fsSL https://x.ai/cli/install.sh | bash" >&2
        exit 1
        ;;
    *)
        echo "Unsupported OS: $(uname -s)" >&2
        exit 1
        ;;
esac

case "$(uname -m)" in
    x86_64 | amd64 | AMD64) arch="x86_64" ;;
    arm64 | aarch64 | ARM64) arch="aarch64" ;;
    *)
        echo "Unsupported architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

platform="${os}-${arch}"
if [ -n "$VERSION" ]; then
    asset="grok-${VERSION}-${platform}"
else
    asset="grok-${platform}"
fi
if [ -n "${GROK_RELEASE_BASE:-}" ]; then
    base_url="${GROK_RELEASE_BASE%/}"
elif [ -n "$VERSION" ]; then
    base_url="https://github.com/${REPO}/releases/download/v${VERSION}"
else
    base_url="https://github.com/${REPO}/releases/latest/download"
fi
asset_url="${base_url}/${asset}"
sums_url="${base_url}/SHA256SUMS"

if [ "${GROK_PRINT_PLAN:-}" = "1" ]; then
    printf 'repo=%s\nplatform=%s\nasset=%s\nurl=%s\nsums=%s\nbin_dir=%s\n' \
        "$REPO" "$platform" "$asset" "$asset_url" "$sums_url" "$BIN_DIR"
    exit 0
fi

mkdir -p "$DOWNLOAD_DIR" "$BIN_DIR"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

echo "Downloading ${asset}..." >&2
if ! download_file "$asset_url" "$work/grok"; then
    echo "Download failed: ${asset_url}" >&2
    echo "Published platforms are linux-x86_64 and windows-x86_64." >&2
    exit 1
fi

if ! download_file "$sums_url" "$work/SHA256SUMS"; then
    echo "Download failed: ${sums_url}" >&2
    exit 1
fi

expected="$(awk -v name="$asset" '$2 == name { print $1; exit }' "$work/SHA256SUMS")"
if [ -z "$expected" ]; then
    echo "SHA256SUMS has no entry for ${asset}" >&2
    exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$work/grok" | awk '{print $1}')"
elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$work/grok" | awk '{print $1}')"
else
    echo "sha256sum or shasum is required to verify the download" >&2
    exit 1
fi
if [ "$actual" != "$expected" ]; then
    echo "Checksum mismatch for ${asset}" >&2
    echo "  expected ${expected}" >&2
    echo "  actual   ${actual}" >&2
    exit 1
fi

if [ "$os" = "windows" ]; then
    dest="$DOWNLOAD_DIR/${asset}"
    mv -f "$work/grok" "$dest"
    install_exe() {
        local name="$1"
        local target="$BIN_DIR/$name"
        rm -f "${target}.old" 2>/dev/null || true
        if ! cp -f "$dest" "$target" 2>/dev/null; then
            mv -f "$target" "${target}.old" 2>/dev/null || true
            if ! cp -f "$dest" "$target" 2>/dev/null; then
                mv -f "${target}.old" "$target" 2>/dev/null || true
                echo "Failed to install $name" >&2
                exit 1
            fi
        fi
    }
    install_exe grok.exe
    install_exe agent.exe
    echo "Installed $BIN_DIR/grok.exe and $BIN_DIR/agent.exe" >&2
else
    chmod +x "$work/grok"
    if ! "$work/grok" --version </dev/null >/dev/null 2>&1; then
        echo "Downloaded grok failed to run; the existing install was left in place." >&2
        exit 1
    fi
    dest="$DOWNLOAD_DIR/${asset}"
    mv -f "$work/grok" "$dest"
    chmod +x "$dest"
    if [ "$(dirname "$BIN_DIR")" = "$(dirname "$DOWNLOAD_DIR")" ]; then
        link_target="../$(basename "$DOWNLOAD_DIR")/$(basename "$dest")"
    else
        link_target="$dest"
    fi
    ln -sfn "$link_target" "$BIN_DIR/grok"
    ln -sfn "$link_target" "$BIN_DIR/agent"
    echo "Installed $BIN_DIR/grok and $BIN_DIR/agent" >&2
fi

mkdir -p "$HOME/.grok/completions/bash" "$HOME/.grok/completions/zsh"
if [ "$os" = "windows" ]; then
    grok_bin="$BIN_DIR/grok.exe"
else
    grok_bin="$BIN_DIR/grok"
fi
"$grok_bin" completions bash > "$HOME/.grok/completions/bash/grok.bash" 2>/dev/null || true
"$grok_bin" completions zsh > "$HOME/.grok/completions/zsh/_grok" 2>/dev/null || true
if mkdir -p "$HOME/.config/fish/completions" 2>/dev/null; then
    "$grok_bin" completions fish > "$HOME/.config/fish/completions/grok.fish" 2>/dev/null || true
fi

config_file="$HOME/.grok/config.toml"
cli_line='installer = "github"'
if [ ! -f "$config_file" ]; then
    printf '[cli]\n%s\n' "$cli_line" > "$config_file"
elif grep -q '^\[cli\]' "$config_file"; then
    tmp="$config_file.tmp.$$"
    awk -v line="$cli_line" '
        /^\[cli\][[:space:]]*(#.*)?$/ { print; print line; in_cli=1; next }
        /^\[.*\][[:space:]]*(#.*)?$/  { in_cli=0 }
        in_cli && /^[[:space:]]*installer[[:space:]]*=/ { next }
        { print }
    ' "$config_file" > "$tmp" && mv "$tmp" "$config_file"
else
    printf '\n[cli]\n%s\n' "$cli_line" >> "$config_file"
fi

path_has_dir() {
    case ":$PATH:" in
        *":$1:"*) return 0 ;;
        *) return 1 ;;
    esac
}

symlink_created=""
if [ "$os" != "windows" ] && ! path_has_dir "$BIN_DIR"; then
    for candidate in "$HOME/.local/bin" "/usr/local/bin"; do
        if path_has_dir "$candidate" && [ -d "$candidate" ] && [ -w "$candidate" ]; then
            ln -sfn "$BIN_DIR/grok" "$candidate/grok"
            ln -sfn "$BIN_DIR/agent" "$candidate/agent"
            symlink_created="$candidate"
            echo "Linked $candidate/grok and $candidate/agent" >&2
            break
        fi
    done
fi

user_shell="$(basename "${SHELL:-}")"
rc_file=""
case "$user_shell" in
    bash) rc_file="$HOME/.bashrc" ;;
    zsh) rc_file="$HOME/.zshrc" ;;
    fish) rc_file="$HOME/.config/fish/config.fish" ;;
esac

if [ -n "$rc_file" ]; then
    mkdir -p "$(dirname "$rc_file")"
    if [ "$user_shell" = "fish" ]; then
        new_block='# >>> grok installer >>>
fish_add_path $HOME/.grok/bin
# <<< grok installer <<<'
    elif [ "$user_shell" = "zsh" ]; then
        new_block='# >>> grok installer >>>
export PATH="$HOME/.grok/bin:$PATH"
fpath=(~/.grok/completions/zsh $fpath)
autoload -Uz compinit && compinit -C
# <<< grok installer <<<'
    else
        new_block='# >>> grok installer >>>
export PATH="$HOME/.grok/bin:$PATH"
[[ -r "$HOME/.grok/completions/bash/grok.bash" ]] && source "$HOME/.grok/completions/bash/grok.bash"
# <<< grok installer <<<'
    fi
    if [ -f "$rc_file" ] && grep -qs "grok installer" "$rc_file"; then
        tmp="$rc_file.tmp.$$"
        awk '
            /# >>> grok installer >>>/ { skip=1; next }
            /# <<< grok installer <<</ { skip=0; next }
            !skip { print }
        ' "$rc_file" > "$tmp" && mv "$tmp" "$rc_file"
    fi
    printf '\n%s\n' "$new_block" >> "$rc_file"
    echo "Updated PATH in $rc_file" >&2
fi

echo "" >&2
if path_has_dir "$BIN_DIR" || [ -n "$symlink_created" ]; then
    echo "Run 'grok' to get started." >&2
elif [ -n "$rc_file" ]; then
    echo "Restart your terminal, then run 'grok'." >&2
else
    echo "Add $BIN_DIR to your PATH, then run 'grok':" >&2
    echo '  export PATH="$HOME/.grok/bin:$PATH"' >&2
fi
