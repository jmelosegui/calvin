#!/bin/sh
# calvin installer for macOS and Linux
# Usage: curl -fsSL https://raw.githubusercontent.com/jmelosegui/calvin/main/docs/install.sh | sh
#
# Optional environment variables:
#   CALVIN_VERSION          install this tag (e.g. v0.2.0) instead of the latest release
#   CALVIN_INSTALL_DIR      where to put calvin (default: ~/.local/bin)
#   CALVIN_INSTALL_ARCHIVE  install from a local .tar.gz instead of downloading (used by CI)

set -eu

REPO="jmelosegui/calvin"
INSTALL_DIR="${CALVIN_INSTALL_DIR:-$HOME/.local/bin}"
BIN="$INSTALL_DIR/calvin"

info() { printf '\033[33m==>\033[0m %s\n' "$*"; }
fail() { printf '\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
have() { command -v "$1" >/dev/null 2>&1; }

fetch() { # url output
  if have curl; then curl -fsSL -H "User-Agent: calvin-installer" -o "$2" "$1"
  elif have wget; then wget -q --header="User-Agent: calvin-installer" -O "$2" "$1"
  else fail "need curl or wget"; fi
}

sha256() {
  if have sha256sum; then sha256sum "$1" | cut -d ' ' -f1
  elif have shasum; then shasum -a 256 "$1" | cut -d ' ' -f1
  else fail "need sha256sum or shasum to verify the download"; fi
}

info "Installing calvin..."

case "$(uname -s)" in
  Linux) os=linux ;;
  Darwin) os=macos ;;
  *) fail "unsupported OS $(uname -s). On Windows, use install.ps1." ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=amd64 ;;
  arm64 | aarch64) arch=arm64 ;;
  *) fail "no prebuilt calvin for $(uname -m). Install from source: cargo install --git https://github.com/$REPO" ;;
esac
file="calvin-$os-$arch.tar.gz"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if [ -n "${CALVIN_INSTALL_ARCHIVE:-}" ]; then
  archive="$CALVIN_INSTALL_ARCHIVE"
  info "Using local archive $archive"
else
  version="${CALVIN_VERSION:-}"
  if [ -z "$version" ]; then
    fetch "https://api.github.com/repos/$REPO/releases/latest" "$tmp/latest.json" \
      || fail "could not find the latest release. Check https://github.com/$REPO/releases"
    version=$(sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' "$tmp/latest.json" | head -n1)
    [ -n "$version" ] || fail "could not read the latest version"
  fi
  info "Version $version"

  base="https://github.com/$REPO/releases/download/$version"
  archive="$tmp/$file"
  info "Downloading $base/$file"
  fetch "$base/$file" "$archive"
  fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS"

  expected=$(grep " \*\{0,1\}$file\$" "$tmp/SHA256SUMS" | cut -d ' ' -f1)
  [ -n "$expected" ] || fail "$file is missing from SHA256SUMS"
  actual=$(sha256 "$archive")
  [ "$expected" = "$actual" ] || fail "checksum mismatch for $file (expected $expected, got $actual)"
  info "Checksum verified"
fi

mkdir -p "$tmp/x"
tar -xzf "$archive" -C "$tmp/x"

# Replace a running calvin cleanly, and start it again afterwards.
was_running=false
if [ -x "$BIN" ] && "$BIN" status 2>/dev/null | grep -q "is running"; then
  info "Stopping the running calvin..."
  "$BIN" stop >/dev/null
  was_running=true
fi

info "Installing to $INSTALL_DIR"
mkdir -p "$INSTALL_DIR"
cp "$tmp/x/calvin" "$BIN.tmp"
chmod 755 "$BIN.tmp"
mv -f "$BIN.tmp" "$BIN"

info "Installed $("$BIN" --version)"

case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *)
    echo ""
    echo "$INSTALL_DIR is not on your PATH. Add this line to your shell profile:"
    echo "  export PATH=\"$INSTALL_DIR:\$PATH\""
    ;;
esac

if [ "$was_running" = true ]; then
  "$BIN" start --no-open
else
  echo ""
  echo "Get started:  calvin start"
fi
