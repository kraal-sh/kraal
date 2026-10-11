#!/bin/sh
# Install kraal from its GitHub releases, for Linux and macOS:
#
#   curl -fsSL https://kraal.sh/install | sh
#
# The archive's checksum is checked before anything is installed. Settings:
#   KRAAL_VERSION       a release tag such as v0.1.0 (the latest by default)
#   KRAAL_INSTALL_DIR   where the binary goes (~/.local/bin by default)
#   KRAAL_DOWNLOAD_URL  where the archives are, instead of GitHub (a mirror)
set -eu

repo="kraal-sh/kraal"
version="${KRAAL_VERSION:-latest}"
dir="${KRAAL_INSTALL_DIR:-$HOME/.local/bin}"

say() { printf '%s\n' "$*" >&2; }
fail() {
    say "kraal: $*"
    exit 1
}

from_source="build it from source: cargo install --git https://github.com/$repo"
case "$(uname -s)" in
    Linux) os=unknown-linux-musl ;;
    Darwin) os=apple-darwin ;;
    *) fail "no release for $(uname -s); $from_source" ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=aarch64 ;;
    *) fail "no release for $(uname -m); $from_source" ;;
esac
# A shell under Rosetta says x86_64 on an Apple Silicon Mac: take the native build.
if [ "$os" = apple-darwin ] && [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
    arch=aarch64
fi
target="$arch-$os"
asset="kraal-$target.tar.gz"

if [ -n "${KRAAL_DOWNLOAD_URL:-}" ]; then
    base="$KRAAL_DOWNLOAD_URL"
elif [ "$version" = latest ]; then
    base="https://github.com/$repo/releases/latest/download"
else
    base="https://github.com/$repo/releases/download/$version"
fi

fetch() {
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto '=https,file' -o "$2" "$1" || fail "could not download $1"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$2" "$1" || fail "could not download $1"
    else
        fail "curl or wget is needed"
    fi
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | cut -d ' ' -f 1
    else
        fail "sha256sum or shasum is needed to check the download"
    fi
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading $asset ($version)"
fetch "$base/$asset" "$tmp/$asset"
fetch "$base/$asset.sha256" "$tmp/$asset.sha256"
want=$(cut -d ' ' -f 1 <"$tmp/$asset.sha256")
got=$(sha256 "$tmp/$asset")
[ "$want" = "$got" ] || fail "$asset does not match its checksum: nothing was installed"
tar -xzf "$tmp/$asset" -C "$tmp"
new="$tmp/kraal-$target/kraal"
"$new" --version >/dev/null 2>&1 || fail "the downloaded kraal does not run here: nothing was installed"

mkdir -p "$dir"
had=0
[ -e "$dir/kraal" ] && had=1
# Copied next to its place, then renamed over it: a running kraal keeps its own file.
cp "$new" "$dir/.kraal.new.$$"
chmod 755 "$dir/.kraal.new.$$"
mv -f "$dir/.kraal.new.$$" "$dir/kraal"

say "Installed $("$dir/kraal" --version) in $dir"
case ":$PATH:" in
    *":$dir:"*) ;;
    *) say "$dir is not in your PATH: add it, or run $dir/kraal" ;;
esac
if [ "$had" = 1 ]; then
    say "A kraal server already running keeps the old version until: kraal kill"
fi
