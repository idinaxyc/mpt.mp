#!/usr/bin/env bash
# Install the latest mpt.mp Linux x86_64 release into ~/.local/bin.
set -euo pipefail

REPOSITORY="${MPT_GITHUB_REPO:-${1:-}}"
if [[ -z "$REPOSITORY" ]]; then
  read -r -p "GitHub repository (owner/name): " REPOSITORY
fi
if [[ ! "$REPOSITORY" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]]; then
  echo "Invalid repository. Expected owner/name." >&2
  exit 1
fi

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "This installer is for Linux." >&2
  exit 1
fi
ARCH="$(uname -m)"
if [[ "$ARCH" != "x86_64" ]]; then
  echo "Unsupported architecture: $ARCH (the published build currently targets x86_64)." >&2
  exit 1
fi

ASSET="mpt-mp-linux-x86_64"
URL="https://github.com/${REPOSITORY}/releases/latest/download/${ASSET}"
DEST_DIR="${HOME}/.local/bin"
DEST="${DEST_DIR}/mpt-mp"
mkdir -p "$DEST_DIR"
TMP="$(mktemp)"
trap 'rm -f "$TMP"' EXIT

if command -v curl >/dev/null 2>&1; then
  curl -fL --retry 3 "$URL" -o "$TMP"
elif command -v wget >/dev/null 2>&1; then
  wget -O "$TMP" "$URL"
else
  echo "Please install curl or wget and try again." >&2
  exit 1
fi

install -m 0755 "$TMP" "$DEST"
echo "mpt.mp installed to: $DEST"
case ":${PATH}:" in
  *":${DEST_DIR}:"*) echo "Run it with: mpt-mp" ;;
  *) echo "Add this to your shell profile: export PATH=\"\$HOME/.local/bin:\$PATH\"" ;;
esac
echo "Linux audio runtime: install your distribution's ALSA runtime package if it is missing (for example, libasound2)."
