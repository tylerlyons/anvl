#!/bin/sh
set -e

REPO="inhesrom/anvl"
INSTALL_DIR="${ANVL_INSTALL_DIR:-$HOME/.local/bin}"

# Detect platform
OS=$(uname -s | tr '[:upper:]' '[:lower:]')
ARCH=$(uname -m)

case "$OS" in
  darwin) ;;
  linux) ;;
  *) echo "Error: unsupported OS: $OS" >&2; exit 1 ;;
esac

case "$ARCH" in
  arm64|aarch64) ARCH="aarch64" ;;
  x86_64) ;;
  *) echo "Error: unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

case "${OS}-${ARCH}" in
  darwin-aarch64) TARGET="aarch64-apple-darwin" ;;
  linux-x86_64)  TARGET="x86_64-unknown-linux-gnu" ;;
  *) echo "Error: unsupported platform: ${OS} ${ARCH}" >&2; exit 1 ;;
esac

if ! command -v curl >/dev/null 2>&1; then
  echo "Error: curl is required but not found" >&2
  exit 1
fi

# Fetch latest release version
VERSION=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" | grep '"tag_name"' | sed 's/.*"tag_name": *"//;s/".*//')
if [ -z "$VERSION" ]; then
  echo "Error: could not determine latest release version" >&2
  exit 1
fi

URL="https://github.com/${REPO}/releases/download/${VERSION}/anvl-${TARGET}.tar.gz"

echo "Installing anvl ${VERSION} for ${TARGET}..."

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

curl -fsSL "$URL" -o "$TMPDIR/anvl.tar.gz"
tar xzf "$TMPDIR/anvl.tar.gz" -C "$TMPDIR"

mkdir -p "$INSTALL_DIR"
mv "$TMPDIR/anvl" "$INSTALL_DIR/anvl"
chmod +x "$INSTALL_DIR/anvl"

echo "Installed anvl ${VERSION} to ${INSTALL_DIR}/anvl"

case ":$PATH:" in
  *":${INSTALL_DIR}:"*) ;;
  *) echo "Note: ${INSTALL_DIR} is not in your PATH. Add it with:"
     echo "  export PATH=\"${INSTALL_DIR}:\$PATH\"" ;;
esac
