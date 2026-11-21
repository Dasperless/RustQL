#!/usr/bin/env bash
# RustQL Linux & macOS Installer
# https://github.com/Dasperless/RustQL
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/Dasperless/RustQL/main/install.sh | bash
#
# Optional environment variables:
#   VERSION="v0.1.0"      # Specify exact release version tag (defaults to latest)
#   BIN_DIR="/usr/local/bin" # Specify installation directory (defaults to /usr/local/bin or ~/.local/bin)

set -euo pipefail

REPO="Dasperless/RustQL"
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

# Pre-flight requirement checks
if ! command -v curl >/dev/null 2>&1; then
    error "curl is required to download RustQL releases."
fi

if ! command -v tar >/dev/null 2>&1; then
    error "tar is required to extract RustQL releases."
fi

# 1. Detect OS and Architecture
OS="$(uname -s)"
ARCH="$(uname -m)"

case "$OS" in
    Linux)
        case "$ARCH" in
            x86_64)
                TARGET="x86_64-unknown-linux-musl"
                FALLBACK_TARGET="x86_64-unknown-linux-gnu"
                ;;
            *)
                error "Unsupported Linux architecture: $ARCH (currently x86_64 is prebuilt; build from source using 'cargo build --release' or 'cargo install --path .')"
                ;;
        esac
        ;;
    Darwin)
        case "$ARCH" in
            arm64|aarch64)
                TARGET="aarch64-apple-darwin"
                FALLBACK_TARGET=""
                ;;
            x86_64)
                TARGET="x86_64-apple-darwin"
                FALLBACK_TARGET=""
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

info "Installing RustQL ${VERSION} (${TARGET})..."

# 3. Determine Installation Directory
if [ -z "${BIN_DIR:-}" ]; then
    if [ "$(id -u)" -eq 0 ] || ([ -d "/usr/local/bin" ] && [ -w "/usr/local/bin" ]); then
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

# Candidate archive names in priority order:
# 1. rustql target
# 2. rustql fallback target
# 3. rustrix target (backward compatibility with v0.1.0 release naming)
# 4. rustrix fallback target
CANDIDATES=("rustql-${VERSION}-${TARGET}.tar.gz")
if [ -n "${FALLBACK_TARGET:-}" ]; then
    CANDIDATES+=("rustql-${VERSION}-${FALLBACK_TARGET}.tar.gz")
fi
CANDIDATES+=("rustrix-${VERSION}-${TARGET}.tar.gz")
if [ -n "${FALLBACK_TARGET:-}" ]; then
    CANDIDATES+=("rustrix-${VERSION}-${FALLBACK_TARGET}.tar.gz")
fi

DOWNLOADED=false
DOWNLOADED_ARCHIVE=""

for ARCHIVE in "${CANDIDATES[@]}"; do
    DOWNLOAD_URL="${GITHUB_URL}/releases/download/${VERSION}/${ARCHIVE}"
    info "Attempting download: ${ARCHIVE}..."
    if curl -fsSL "$DOWNLOAD_URL" -o "${TEMP_DIR}/${ARCHIVE}" 2>/dev/null; then
        DOWNLOADED=true
        DOWNLOADED_ARCHIVE="$ARCHIVE"
        break
    fi
done

if [ "$DOWNLOADED" = false ]; then
    error "Failed to download release archive from ${GITHUB_URL}/releases/tag/${VERSION}"
fi

# 5. Checksum Verification (if sha256 tool available)
SHA_URL="${GITHUB_URL}/releases/download/${VERSION}/${DOWNLOADED_ARCHIVE}.sha256"
if curl -fsSL "$SHA_URL" -o "${TEMP_DIR}/${DOWNLOADED_ARCHIVE}.sha256" 2>/dev/null; then
    info "Verifying SHA256 checksum..."
    EXPECTED_HASH="$(awk '{print $1}' "${TEMP_DIR}/${DOWNLOADED_ARCHIVE}.sha256" | tr -d '\r\n')"
    ACTUAL_HASH=""
    if command -v sha256sum >/dev/null 2>&1; then
        ACTUAL_HASH="$(sha256sum "${TEMP_DIR}/${DOWNLOADED_ARCHIVE}" | awk '{print $1}')"
    elif command -v shasum >/dev/null 2>&1; then
        ACTUAL_HASH="$(shasum -a 256 "${TEMP_DIR}/${DOWNLOADED_ARCHIVE}" | awk '{print $1}')"
    fi

    if [ -n "$ACTUAL_HASH" ] && [ -n "$EXPECTED_HASH" ]; then
        if [ "$EXPECTED_HASH" != "$ACTUAL_HASH" ]; then
            error "SHA256 checksum verification failed!"
        fi
        success "Checksum verified (${EXPECTED_HASH})"
    fi
fi

# 6. Extract and Install
info "Extracting binary..."
tar -xzf "${TEMP_DIR}/${DOWNLOADED_ARCHIVE}" -C "$TEMP_DIR"

SRC_BIN=""
for candidate in \
    "${TEMP_DIR}"/*/rustql \
    "${TEMP_DIR}/rustql" \
    "${TEMP_DIR}"/*/rustrix \
    "${TEMP_DIR}/rustrix"; do
    if [ -f "$candidate" ] && [ -x "$candidate" ]; then
        SRC_BIN="$candidate"
        break
    fi
done

if [ -z "$SRC_BIN" ] || [ ! -f "$SRC_BIN" ]; then
    error "Compiled binary ('rustql' or 'rustrix') not found in release archive."
fi

DEST_BIN="${BIN_DIR}/rustql"
if ! cp "$SRC_BIN" "$DEST_BIN" 2>/dev/null; then
    if command -v sudo >/dev/null 2>&1 && [ -t 0 ]; then
        warn "Permission denied copying to ${DEST_BIN}. Attempting with sudo..."
        sudo cp "$SRC_BIN" "$DEST_BIN"
        sudo chmod +x "$DEST_BIN"
    else
        error "Permission denied copying to ${DEST_BIN}. Try setting BIN_DIR=\"\${HOME}/.local/bin\" or running with sudo."
    fi
else
    chmod +x "$DEST_BIN"
fi

# 7. Post-install macOS quarantine clearance
if [ "$OS" = "Darwin" ]; then
    if command -v xattr >/dev/null 2>&1; then
        xattr -d com.apple.quarantine "$DEST_BIN" 2>/dev/null || true
    fi
fi

success "RustQL successfully installed to: ${DEST_BIN}"

# 8. Check PATH
case ":${PATH}:" in
    *:"${BIN_DIR}":*)
        ;;
    *)
        warn "'${BIN_DIR}' is not in your PATH."
        printf "To use rustql from any terminal, add this to your ~/.bashrc or ~/.zshrc:\n"
        printf "    ${BOLD}export PATH=\"%s:\$PATH\"${NC}\n\n" "$BIN_DIR"
        ;;
esac

# 9. Verify Installation
if [ -x "$DEST_BIN" ]; then
    "$DEST_BIN" --version
fi

printf "\n${BOLD}Quick Start:${NC}\n"
printf "  Interactive TUI:   ${GREEN}rustql examples/users.json examples/orders.ndjson${NC}\n"
printf "  Non-interactive:   ${GREEN}rustql -q \"SELECT * FROM users\" examples/users.json${NC}\n"
printf "  Documentation:     ${BLUE}https://github.com/Dasperless/RustQL#readme${NC}\n"
