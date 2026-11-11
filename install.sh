#!/usr/bin/env bash
# Install the latest shw release binary into ~/.local/bin (or $SHW_INSTALL_DIR).
#   curl -fsSL https://raw.githubusercontent.com/TillBeemelmanns/shw/main/install.sh | bash
set -euo pipefail

trap 'echo "shw installation FAILED" >&2' ERR

REPO="TillBeemelmanns/shw"
DEST="${SHW_INSTALL_DIR:-$HOME/.local/bin}"

OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
    Linux)  OS_TARGET="unknown-linux-musl" ;;
    Darwin) OS_TARGET="apple-darwin" ;;
    *)      echo "Unsupported OS: $OS" >&2; exit 1 ;;
esac

case "$ARCH" in
    x86_64 | amd64)  ARCH_TARGET="x86_64" ;;
    aarch64 | arm64) ARCH_TARGET="aarch64" ;;
    *)               echo "Unsupported architecture: $ARCH" >&2; exit 1 ;;
esac

# Tags and asset names keep the 'v' prefix (e.g. v0.1.0), like bat.
TAG="${SHW_VERSION:-$(curl -fsSI "https://github.com/${REPO}/releases/latest" \
    | grep -i '^location:' | sed 's|.*/tag/||' | tr -d '\r\n')}"

TARGET="shw-${TAG}-${ARCH_TARGET}-${OS_TARGET}"
URL="https://github.com/${REPO}/releases/download/${TAG}/${TARGET}.tar.gz"

TMPDIR=$(mktemp -d)
trap 'rm -rf "$TMPDIR"' EXIT

echo "Downloading shw ${TAG} from $URL ..."
curl -fsSL "$URL" -o "$TMPDIR/shw.tar.gz"

# Verify the archive against the checksum published with the release.
curl -fsSL "${URL}.sha256" -o "$TMPDIR/shw.tar.gz.sha256"
EXPECTED=$(awk '{print $1}' "$TMPDIR/shw.tar.gz.sha256")
if command -v sha256sum >/dev/null 2>&1; then
    ACTUAL=$(sha256sum "$TMPDIR/shw.tar.gz" | awk '{print $1}')
else
    ACTUAL=$(shasum -a 256 "$TMPDIR/shw.tar.gz" | awk '{print $1}')
fi
if [ -z "$EXPECTED" ] || [ "$EXPECTED" != "$ACTUAL" ]; then
    echo "Checksum mismatch for ${TARGET}.tar.gz (expected ${EXPECTED:-none}, got ${ACTUAL})" >&2
    exit 1
fi
echo "Checksum OK (sha256 ${ACTUAL})"

# Optional: verify the signed build provenance when the GitHub CLI is available.
if command -v gh >/dev/null 2>&1 && gh auth status >/dev/null 2>&1; then
    gh attestation verify "$TMPDIR/shw.tar.gz" --repo "$REPO" >/dev/null \
        && echo "Build provenance verified (built by ${REPO} GitHub Actions)"
fi

tar -xzf "$TMPDIR/shw.tar.gz" -C "$TMPDIR"

mkdir -p "$DEST"
install -m 755 "$TMPDIR/$TARGET/shw" "$DEST/shw"

# Shell completions (best effort)
if [ -d "$TMPDIR/$TARGET/completions" ]; then
    mkdir -p "$HOME/.local/share/zsh/site-functions" "$HOME/.local/share/bash-completion/completions" "$HOME/.config/fish/completions" 2>/dev/null || true
    cp "$TMPDIR/$TARGET/completions/_shw" "$HOME/.local/share/zsh/site-functions/_shw" 2>/dev/null || true
    cp "$TMPDIR/$TARGET/completions/shw.bash" "$HOME/.local/share/bash-completion/completions/shw" 2>/dev/null || true
    cp "$TMPDIR/$TARGET/completions/shw.fish" "$HOME/.config/fish/completions/shw.fish" 2>/dev/null || true
fi

echo "shw successfully installed to $DEST/shw"
