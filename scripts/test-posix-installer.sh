#!/bin/sh
set -eu

root=$(mktemp -d "${TMPDIR:-/tmp}/codexify-installer-test.XXXXXX")
server_pid=
cleanup() {
    if [ -n "$server_pid" ]; then
        kill "$server_pid" >/dev/null 2>&1 || true
    fi
    rm -rf "$root"
}
trap cleanup 0
trap 'exit 1' HUP INT TERM

case $(uname -s) in
    Darwin) platform_os=darwin ;;
    Linux) platform_os=linux ;;
    *) exit 0 ;;
esac
case $(uname -m) in
    x86_64|amd64) platform_arch=x64 ;;
    arm64|aarch64) platform_arch=arm64 ;;
    *) exit 0 ;;
esac

tag=v9.9.9
platform=$platform_os-$platform_arch
asset=codexify-$tag-$platform.tar.gz
release_root=$root/releases
stage=$root/stage/codexify-$tag-$platform
mkdir -p "$stage" "$release_root/$tag"
cat > "$stage/codexify" <<'SCRIPT'
#!/bin/sh
case "${1:-}" in
    --help) exit 0 ;;
    migrate-legacy-install)
        if [ "${2:-}" = --help ]; then
            exit 0
        fi
        [ -z "${2:-}" ] || exit 2
        count=0
        [ ! -f "$HOME/legacy-migrations" ] || count=$(cat "$HOME/legacy-migrations")
        printf '%s\n' "$((count + 1))" > "$HOME/legacy-migrations"
        exit 0
        ;;
    service)
        if [ "${2:-}" = --help ]; then
            exit 0
        fi
        [ "${2:-}" = install ] || exit 2
        count=0
        [ ! -f "$HOME/service-installs" ] || count=$(cat "$HOME/service-installs")
        printf '%s\n' "$((count + 1))" > "$HOME/service-installs"
        exit 0
        ;;
esac
printf 'fake-codexify\n'
SCRIPT
chmod 755 "$stage/codexify"
tar -czf "$release_root/$tag/$asset" -C "$root/stage" "codexify-$tag-$platform"
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$release_root/$tag" && sha256sum "$asset" > checksums.txt)
else
    (cd "$release_root/$tag" && shasum -a 256 "$asset" > checksums.txt)
fi

home=$root/home
mkdir -p "$home/.config/fish" "$home/.config/nushell" "$home/.config/powershell" "$home/.codexify/bin"
printf 'old-binary\n' > "$home/.codexify/bin/codexify"
for profile in .profile .bash_profile .bashrc .zprofile .cshrc .tcshrc; do
    : > "$home/$profile"
done
: > "$home/.config/fish/config.fish"
: > "$home/.config/nushell/config.nu"
: > "$home/.config/powershell/Microsoft.PowerShell_profile.ps1"

port=$(python3 - <<'PY'
import socket
socket_ = socket.socket()
socket_.bind(("127.0.0.1", 0))
print(socket_.getsockname()[1])
socket_.close()
PY
)
python3 -m http.server "$port" --bind 127.0.0.1 --directory "$release_root" > "$root/http.log" 2>&1 &
server_pid=$!
ready=0
attempt=0
while [ "$attempt" -lt 50 ]; do
    if curl -q -fsS "http://127.0.0.1:$port/$tag/checksums.txt" >/dev/null 2>&1; then
        ready=1
        break
    fi
    if ! kill -0 "$server_pid" >/dev/null 2>&1; then
        cat "$root/http.log" >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 0.1
done
[ "$ready" -eq 1 ] || {
    cat "$root/http.log" >&2
    exit 1
}

for run in 1 2; do
    HOME=$home \
    SHELL=/bin/zsh \
    CODEXIFY_VERSION=$tag \
    CODEXIFY_RELEASE_ROOT=http://127.0.0.1:$port \
        sh ./install.sh > "$root/run-$run.log"
done

HOME=$home \
SHELL=/bin/zsh \
CODEXIFY_VERSION=$tag \
CODEXIFY_RELEASE_ROOT=http://127.0.0.1:$port \
CODEXIFY_SKIP_SERVICE=1 \
    sh ./install.sh > "$root/run-skip-service.log"

[ "$("$home/.codexify/bin/codexify")" = fake-codexify ]
[ "$(cat "$home/service-installs")" = 2 ]
[ "$(cat "$home/legacy-migrations")" = 3 ]
[ -f "$home/.zshrc" ]
for profile in \
    "$home/.profile" \
    "$home/.bash_profile" \
    "$home/.bashrc" \
    "$home/.zprofile" \
    "$home/.zshrc" \
    "$home/.cshrc" \
    "$home/.tcshrc" \
    "$home/.config/fish/config.fish" \
    "$home/.config/nushell/config.nu" \
    "$home/.config/powershell/Microsoft.PowerShell_profile.ps1"
do
    [ "$(grep -c '^# >>> codexify path >>>$' "$profile")" -eq 1 ]
done
