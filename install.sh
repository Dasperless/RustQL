#!/usr/bin/env bash
# Rustrix Linux & macOS Installer
# https://github.com/Dasperless/Rustrix
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Dasperless/Rustrix/main/install.sh | bash
#
# Optional environment variables:
#   VERSION="v0.1.0"      # Specify exact release version tag (defaults to latest)
#   BIN_DIR="/usr/local/bin" # Specify installation directory (defaults to /usr/local/bin or ~/.local/bin)

set -euo pipefail

REPO="Dasperless/Rustrix"
GITHUB_URL="https://github.com/${REPO}"

BOLD='\033[1m'
GREEN='\033[0;32m'
BLUE='\033[0;34m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m' # No Color

info() {
    printf "${BLUE}==>${NC} ${BOLD}%s${NC}\n" "$1"
}

success() {
    printf "${GREEN}==>${NC} ${BOLD}%s${NC}\n" "$1"
}

warn() {
    printf "${YELLOW}warning:${NC} %s\n" "$1"
}

error() {
    printf "${RED}error:${NC} %s\n" "$1" >&2
    exit 1
}

# 1. Detect OS and Architecture
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux)
        case "$ARCH" in
            x86_64)
                TARGET="x86_64-unknown-linux-gnu"
                ;;
            *)
                error "Unsupported Linux architecture: $ARCH (currently x86_64 is prebuilt; build from source using 'cargo build --release')"
                ;;
        esac
        ;;
    Darwin)
        case "$ARCH" in
            arm64|aarch64)
                TARGET="aarch64-apple-darwin"
                ;;
            x86_64)
                TARGET="x86_64-apple-darwin"
                ;;
            *)
                error "Unsupported macOS architecture: $ARCH"
                ;;
        esac
        ;;
    *)
        error "Unsupported operating system: $OS. For Windows, use install.ps1 or download the release zip."
        ;;
esac

# 2. Determine Version
if [ -z "${VERSION:-}" ]; then
    info "Resolving latest release for ${REPO}..."
    LATEST_JSON=$(curl -fsSL "https://api.github.com/repos/${REPO}/releases/latest" 2>/dev/null || true)
    if [ -n "$LATEST_JSON" ]; then
        VERSION=$(echo "$LATEST_JSON" | grep '"tag_name":' | head -n1 | cut -d'"' -f4)
    fi
    if [ -z "${VERSION:-}" ]; then
        VERSION="v0.1.0"
    fi
fi

# Ensure version has 'v' prefix
case "$VERSION" in
    v*) ;;
    *) VERSION="v${VERSION}" ;;
esac

info "Installing Rustrix ${VERSION} (${TARGET})..."

# 3. Determine Installation Directory
if [ -z "${BIN_DIR:-}" ]; then
    if [ "$(id -u)" -eq 0 ] || [ -w "/usr/local/bin" ]; then
        BIN_DIR="/usr/local/bin"
    else
        BIN_DIR="${HOME}/.local/bin"
    fi
fi

mkdir -p "$BIN_DIR"

# 4. Download Release Archive
TEMP_DIR="$(mktemp -d)"
cleanup() {
    rm -rf "$TEMP_DIR"
}
trap cleanup EXIT

ARCHIVE_NAME="rustrix-${VERSION}-${TARGET}.tar.gz"
DOWNLOAD_URL="${GITHUB_URL}/releases/download/${VERSION}/${ARCHIVE_NAME}"
SHA_URL="${DOWNLOAD_URL}.sha256"

info "Downloading ${ARCHIVE_NAME}..."
if ! curl -fsSL "$DOWNLOAD_URL" -o "${TEMP_DIR}/${ARCHIVE_NAME}"; then
    error "Failed to download release archive from: ${DOWNLOAD_URL}"
fi

# 5. Checksum Verification (if sha256 tool available)
if curl -fsSL "$SHA_URL" -o "${TEMP_DIR}/${ARCHIVE_NAME}.sha256" 2>/dev/null; then
    info "Verifying SHA256 checksum..."
    cd "$TEMP_DIR"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum -c "${ARCHIVE_NAME}.sha256" --status || error "SHA256 checksum verification failed!"
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 -c "${ARCHIVE_NAME}.sha256" --status || error "SHA256 checksum verification failed!"
    fi
    cd - >/dev/null
fi

# 6. Extract and Install
info "Extracting binary..."
tar -xzf "${TEMP_DIR}/${ARCHIVE_NAME}" -C "$TEMP_DIR"

SRC_BIN="${TEMP_DIR}/rustrix-${VERSION}-${TARGET}/rustrix"
if [ ! -f "$SRC_BIN" ]; then
    # In case archive root is flat
    SRC_BIN="${TEMP_DIR}/rustrix"
fi

if [ ! -f "$SRC_BIN" ]; then
    error "Compiled binary 'rustrix' not found in release archive."
fi

DEST_BIN="${BIN_DIR}/rustrix"
cp "$SRC_BIN" "$DEST_BIN"
chmod +x "$DEST_BIN"

# 7. Post-install macOS quarantine clearance
if [ "$OS" = "Darwin" ]; then
    if command -v xattr >/dev/null 2>&1; then
        xattr -d com.apple.quarantine "$DEST_BIN" 2>/dev/null || true
    fi
fi

success "Rustrix successfully installed to: ${DEST_BIN}"

# 8. Check PATH
case ":${PATH}:" in
    *:"${BIN_DIR}":*)
        ;;
    *)
        warn "'${BIN_DIR}' is not in your PATH."
        printf "To use rustrix from any terminal, add this to your ~/.bashrc or ~/.zshrc:\n"
        printf "    ${BOLD}export PATH=\"%s:\$PATH\"${NC}\n\n" "$BIN_DIR"
        ;;
esac

# 9. Verify Installation
if command -v "$DEST_BIN" >/dev/null 2>&1; then
    "$DEST_BIN" --version
fi

printf "\n${BOLD}Quick Start:${NC}\n"
printf "  Interactive TUI:   ${GREEN}rustrix examples/users.json examples/orders.ndjson${NC}\n"
printf "  Non-interactive:   ${GREEN}rustrix -q \"SELECT * FROM users\" examples/users.json${NC}\n"
printf "  Documentation:     ${BLUE}https://github.com/Dasperless/Rustrix#readme${NC}\n"
