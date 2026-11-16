#!/usr/bin/env bash
set -euo pipefail

REPO="idinaxyc/mpt.mp"
ASSET="mpt-mp-linux-x86_64.tar.gz"
BASE="https://github.com/${REPO}/releases/latest/download/${ASSET}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

command -v curl >/dev/null 2>&1 || { echo "curl is required." >&2; exit 1; }
[[ "$(uname -s)" == "Linux" ]] || { echo "This installer supports Linux only." >&2; exit 1; }
[[ "$(uname -m)" == "x86_64" ]] || { echo "Only Linux x86_64 builds are currently published." >&2; exit 1; }

curl -fL --retry 3 "$BASE" -o "$TMP/release.tar.gz"
mkdir -p "$TMP/unpacked"
tar -xzf "$TMP/release.tar.gz" -C "$TMP/unpacked"

BIN="$TMP/unpacked/mpt-mp"
[[ -f "$BIN" ]] || { echo "The release archive does not contain mpt-mp." >&2; exit 1; }

APP_DIR="$HOME/.local/opt/mpt.mp"
BIN_DIR="$HOME/.local/bin"
mkdir -p "$APP_DIR" "$BIN_DIR"
rm -rf "$APP_DIR/plugins"
install -m 0755 "$BIN" "$APP_DIR/mpt-mp"
if [[ -d "$TMP/unpacked/plugins" ]]; then
  cp -R "$TMP/unpacked/plugins" "$APP_DIR/plugins"
fi
ln -sfn "$APP_DIR/mpt-mp" "$BIN_DIR/mpt-mp"

echo "mpt.mp installed."
echo "Run: $BIN_DIR/mpt-mp"
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) echo "Add ~/.local/bin to PATH if the command is not found." ;;
esac
