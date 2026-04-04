#!/usr/bin/env bash
# =============================================================================
# Quill-Core Build Script (2026)
# =============================================================================
# Compiles the Rust shared library and organizes release artifacts.
#
# Usage:
#   ./scripts/build.sh [--release]
# =============================================================================

set -e

# Configuration
PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="${PROJECT_ROOT}/bin"
MODE="debug"
CARGO_FLAGS=""

if [[ "$1" == "--release" ]]; then
    MODE="release"
    CARGO_FLAGS="--release"
fi

# -- Building -----------------------------------------------------------------
echo "🚀 Building quill-core [${MODE}]..."
cd "${PROJECT_ROOT}"
cargo build ${CARGO_FLAGS}

# -- Organizing ---------------------------------------------------------------
mkdir -p "${BIN_DIR}"

# Detect OS extension
EXT=".so"
if [[ "$OSTYPE" == "darwin"* ]]; then
    EXT=".dylib"
elif [[ "$OSTYPE" == "msys" || "$OSTYPE" == "win32" ]]; then
    EXT=".dll"
fi

SOURCE_LIB="${PROJECT_ROOT}/target/${MODE}/libquill_core${EXT}"
TARGET_LIB="${BIN_DIR}/libquill${EXT}"

if [ -f "${SOURCE_LIB}" ]; then
    cp "${SOURCE_LIB}" "${TARGET_LIB}"
    cp "${PROJECT_ROOT}/quill.h" "${BIN_DIR}/quill.h"
    echo "✅ Success: Binary and Header copied to ${BIN_DIR}/"
else
    echo "❌ Error: Built library not found at ${SOURCE_LIB}"
    exit 1
fi
