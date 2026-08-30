#!/bin/sh
set -eu

REPOSITORY=${CODEXIFY_GITHUB_REPOSITORY:-devnoname120/codexify}
INSTALL_DIR="$HOME/.codexify/bin"
RELEASE_ROOT=${CODEXIFY_RELEASE_ROOT:-"https://github.com/$REPOSITORY/releases/download"}
VERSION=${CODEXIFY_VERSION:-}

fail() {
    printf 'codexify installer: %s\n' "$*" >&2
    exit 1
}

need() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

append_path_block() (
    profile=$1
    line=$2
    marker='# >>> codexify path >>>'

    if grep -F "$marker" "$profile" >/dev/null 2>&1 || grep -F '.codexify/bin' "$profile" >/dev/null 2>&1; then
        exit 0
    fi

    printf '\n%s\n%s\n%s\n' "$marker" "$line" '# <<< codexify path <<<' >> "$profile"
    printf 'Updated PATH in %s\n' "$profile"
)

ensure_primary_profile() {
    shell_name=${SHELL##*/}
    case "$shell_name" in
        zsh)
            profile=$HOME/.zshrc
            line='export PATH="$HOME/.codexify/bin:$PATH"'
            ;;
        bash)
            profile=$HOME/.bashrc
            line='export PATH="$HOME/.codexify/bin:$PATH"'
            ;;
        fish)
            profile=$HOME/.config/fish/config.fish
            line='fish_add_path --global "$HOME/.codexify/bin"'
            ;;
        csh|tcsh)
            profile=$HOME/.cshrc
            line='setenv PATH "$HOME/.codexify/bin:$PATH"'
            ;;
        nu)
            profile=$HOME/.config/nushell/config.nu
            line='$env.PATH = ($env.PATH | prepend ($env.HOME | path join ".codexify" "bin"))'
            ;;
        *)
            profile=$HOME/.profile
            line='export PATH="$HOME/.codexify/bin:$PATH"'
            ;;
    esac

    if [ ! -e "$profile" ]; then
        mkdir -p "$(dirname "$profile")"
        : > "$profile"
    fi
    append_path_block "$profile" "$line"
}

configure_path() {
    found=0

    for profile in \
        "$HOME/.profile" \
        "$HOME/.bash_profile" \
        "$HOME/.bash_login" \
        "$HOME/.bashrc" \
        "$HOME/.zprofile" \
        "$HOME/.zshrc" \
        "$HOME/.kshrc"
    do
        if [ -f "$profile" ]; then
            found=1
            append_path_block "$profile" 'export PATH="$HOME/.codexify/bin:$PATH"'
        fi
    done

    fish_profile=$HOME/.config/fish/config.fish
    if [ -f "$fish_profile" ]; then
        found=1
        append_path_block "$fish_profile" 'fish_add_path --global "$HOME/.codexify/bin"'
    fi

    for profile in "$HOME/.cshrc" "$HOME/.tcshrc"; do
        if [ -f "$profile" ]; then
            found=1
            append_path_block "$profile" 'setenv PATH "$HOME/.codexify/bin:$PATH"'
        fi
    done

    nu_profile=$HOME/.config/nushell/config.nu
    if [ -f "$nu_profile" ]; then
        found=1
        append_path_block "$nu_profile" '$env.PATH = ($env.PATH | prepend ($env.HOME | path join ".codexify" "bin"))'
    fi

    for profile in \
        "$HOME/.config/powershell/profile.ps1" \
        "$HOME/.config/powershell/Microsoft.PowerShell_profile.ps1"
    do
        if [ -f "$profile" ]; then
            found=1
            append_path_block "$profile" '$env:PATH = "$HOME/.codexify/bin" + [IO.Path]::PathSeparator + $env:PATH'
        fi
    done

    if [ "$found" -eq 0 ]; then
        ensure_primary_profile
    else
        shell_name=${SHELL##*/}
        case "$shell_name" in
            zsh) primary=$HOME/.zshrc ;;
            bash) primary=$HOME/.bashrc ;;
            fish) primary=$HOME/.config/fish/config.fish ;;
            csh|tcsh) primary=$HOME/.cshrc ;;
            nu) primary=$HOME/.config/nushell/config.nu ;;
            *) primary=$HOME/.profile ;;
        esac
        if [ ! -e "$primary" ]; then
            ensure_primary_profile
        fi
    fi
}

sha256_file() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$1" | awk '{print $NF}'
    else
        fail 'sha256sum, shasum, or openssl is required to verify the release'
    fi
}

: "${HOME:?HOME must be set}"
need curl
need tar
need awk
need find
need head
need mktemp
need tr

case $(uname -s) in
    Darwin) platform_os=darwin ;;
    Linux) platform_os=linux ;;
    *) fail "unsupported operating system: $(uname -s)" ;;
esac

case $(uname -m) in
    x86_64|amd64) platform_arch=x64 ;;
    arm64|aarch64) platform_arch=arm64 ;;
    *) fail "unsupported architecture: $(uname -m)" ;;
esac

if [ -z "$VERSION" ]; then
    latest_url=$(curl -qfsSL -o /dev/null -w '%{url_effective}' "https://github.com/$REPOSITORY/releases/latest") \
        || fail 'could not resolve the latest GitHub release'
    VERSION=${latest_url%%\?*}
    VERSION=${VERSION%/}
    VERSION=${VERSION##*/}
fi

case "$VERSION" in
    ''|*[!A-Za-z0-9._-]*) fail "invalid release tag: $VERSION" ;;
esac

asset="codexify-$VERSION-$platform_os-$platform_arch.tar.gz"
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/codexify-install.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0
trap 'exit 1' HUP INT TERM
archive=$tmp_dir/$asset
checksums=$tmp_dir/checksums.txt
release_url=$RELEASE_ROOT/$VERSION

printf 'Downloading Codexify %s for %s-%s...\n' "$VERSION" "$platform_os" "$platform_arch"
curl -qfsSL --retry 3 --connect-timeout 15 -o "$archive" "$release_url/$asset" \
    || fail "could not download $asset"
curl -qfsSL --retry 3 --connect-timeout 15 -o "$checksums" "$release_url/checksums.txt" \
    || fail 'could not download checksums.txt'

expected=$(awk -v file="$asset" '$2 == file || $2 == "*" file { print $1; exit }' "$checksums")
[ -n "$expected" ] || fail "checksums.txt does not contain $asset"
actual=$(sha256_file "$archive" | tr '[:upper:]' '[:lower:]')
expected=$(printf '%s' "$expected" | tr '[:upper:]' '[:lower:]')
[ "$actual" = "$expected" ] || fail "checksum mismatch for $asset"

extract_dir=$tmp_dir/extract
mkdir -p "$extract_dir"
tar -xzf "$archive" -C "$extract_dir"
binary=$(find "$extract_dir" -type f -name codexify -print | head -n 1)
[ -n "$binary" ] || fail 'release archive does not contain the codexify executable'

mkdir -p "$INSTALL_DIR"
target=$INSTALL_DIR/codexify
staged=$INSTALL_DIR/.codexify.new.$$
cp "$binary" "$staged"
chmod 755 "$staged"
mv -f "$staged" "$target"

if [ "$platform_os" = darwin ]; then
    xattr -d com.apple.quarantine "$target" >/dev/null 2>&1 || true
fi

"$target" --help >/dev/null 2>&1 || fail 'the installed executable did not start successfully'
if "$target" migrate-legacy-install --help >/dev/null 2>&1; then
    "$target" migrate-legacy-install \
        || fail 'the executable was installed, but legacy Codex Free state migration failed'
else
    printf 'The installed release does not provide legacy Codex Free state migration; continuing without it.\n' >&2
fi
configure_path

if [ "${CODEXIFY_SKIP_SERVICE:-0}" != 1 ]; then
    if "$target" service --help >/dev/null 2>&1; then
        "$target" service install \
            || fail 'the executable was installed, but the background service could not be installed; rerun with CODEXIFY_SKIP_SERVICE=1 to install without it'
    else
        printf 'The installed release does not provide service management; executable installation will continue.\n' >&2
    fi
fi

printf '\nInstalled Codexify %s to %s\n' "$VERSION" "$target"
printf 'Restart your terminal, then run:\n  codexify quickstart\n'
