#!/bin/sh
# Installs the osh binary, then runs `osh setup` to install what it needs.
#
#   curl -fsSL https://raw.githubusercontent.com/Guilospanck/oh-my-openshell/main/install.sh | sh
#
# Environment:
#   OSH_VERSION       release tag to install (default: the latest release)
#   OSH_BIN_DIR       where to put osh (default: ~/.local/bin)
#   OSH_RELEASE_BASE  directory or URL holding the archives, e.g. file:///tmp/dist (default: GitHub)
#   OSH_NO_SETUP=1    install the binary only
set -eu

repo="Guilospanck/oh-my-openshell"
bin_dir="${OSH_BIN_DIR:-$HOME/.local/bin}"

fail() { echo "install.sh: $*" >&2; exit 1; }
command -v curl >/dev/null 2>&1 || fail "curl is required"

case "$(uname -s)" in Darwin) os=darwin ;; Linux) os=linux ;; *) fail "unsupported OS $(uname -s)" ;; esac
case "$(uname -m)" in arm64|aarch64) arch=aarch64 ;; x86_64|amd64) arch=x86_64 ;; *) fail "unsupported CPU $(uname -m)" ;; esac

version="${OSH_VERSION:-}"
if [ -z "$version" ]; then
  [ -z "${OSH_RELEASE_BASE:-}" ] || fail "set OSH_VERSION when OSH_RELEASE_BASE is set"
  version="$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest")"
  version="${version##*/}"
  [ -n "$version" ] || fail "cannot find the latest release"
fi
base="${OSH_RELEASE_BASE:-https://github.com/$repo/releases/download/$version}"
name="osh-$version-$os-$arch.tar.gz"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
echo "Downloading $name"
curl -fsSL "$base/$name" -o "$tmp/$name" || fail "cannot download $base/$name"
curl -fsSL "$base/$name.sha256" -o "$tmp/$name.sha256" || fail "cannot download the checksum"

expected="$(cut -d ' ' -f 1 "$tmp/$name.sha256")"
if command -v sha256sum >/dev/null 2>&1; then actual="$(sha256sum "$tmp/$name" | cut -d ' ' -f 1)"
else actual="$(shasum -a 256 "$tmp/$name" | cut -d ' ' -f 1)"; fi
[ -n "$expected" ] && [ "$expected" = "$actual" ] || fail "checksum mismatch for $name; nothing was installed"

tar -xzf "$tmp/$name" -C "$tmp" osh
mkdir -p "$bin_dir"
install -m 755 "$tmp/osh" "$bin_dir/osh"
echo "Installed $bin_dir/osh"
case ":$PATH:" in *":$bin_dir:"*) ;; *) echo "warning: $bin_dir is not on your PATH" >&2 ;; esac

[ "${OSH_NO_SETUP:-}" = 1 ] || exec "$bin_dir/osh" setup
