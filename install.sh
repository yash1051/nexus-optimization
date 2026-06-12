#!/usr/bin/env sh
# Nexus installer — builds from source via cargo.
# Usage:
#   ./install.sh            # build + install into ~/.cargo/bin
#   PREFIX=/usr/local sh install.sh   # custom install prefix (requires sudo for /usr/local)

set -e

BINARY_NAME="nexus"

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { printf "${GREEN}[INFO]${NC}  %s\n" "$1"; }
warn()  { printf "${YELLOW}[WARN]${NC}  %s\n" "$1"; }
error() { printf "${RED}[ERROR]${NC} %s\n" "$1"; exit 1; }

# --- Prerequisite: cargo ---------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
    warn "cargo not found on PATH."
    warn "Install Rust from https://rustup.rs and re-run this script:"
    warn "    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    error "Aborting: cargo required."
fi

info "Found cargo: $(cargo --version)"

# --- Build & install -------------------------------------------------------
info "Building ${BINARY_NAME} (release profile)..."
cargo build --release

if [ -n "${PREFIX:-}" ]; then
    DEST="${PREFIX}/bin/${BINARY_NAME}"
    info "Installing to ${DEST}"
    install -m 755 "target/release/${BINARY_NAME}" "${DEST}"
else
    info "Installing via 'cargo install --path .' (→ ~/.cargo/bin/${BINARY_NAME})"
    cargo install --path . --force
fi

# --- Verify ----------------------------------------------------------------
if command -v "${BINARY_NAME}" >/dev/null 2>&1; then
    info "Installed: $(${BINARY_NAME} --version)"
    info "Try: ${BINARY_NAME} --help"
else
    warn "${BINARY_NAME} is installed but not on PATH."
    warn "Add this to your shell rc (~/.zshrc or ~/.bashrc):"
    warn "    export PATH=\"\$HOME/.cargo/bin:\$PATH\""
fi

info "Done."
