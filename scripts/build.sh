#!/usr/bin/env bash
#
# Build ExcelScry and collect the two deliverables:
#
#   dist/ExcelScry-portable/ExcelScry.exe   single-file portable executable
#   dist/ExcelScry-<version>-setup.exe      NSIS installer
#
# Run from anywhere; the script resolves the repository root itself.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# The version is read from the manifest, never hardcoded here: a stale literal
# would silently mislabel the installer. scripts/bump_version.py keeps
# tauri.conf.json, Cargo.toml, Cargo.lock and the README in step.
VERSION="$(sed -n 's/.*"version": *"\([^"]*\)".*/\1/p' src-tauri/tauri.conf.json 2>/dev/null | head -1)" || true
if [[ -z "$VERSION" ]]; then
    echo "error: no version found in src-tauri/tauri.conf.json" >&2
    echo "       (scripts/bump_version.py writes it, and reports when it is missing)" >&2
    exit 1
fi

echo "==> Building ExcelScry ${VERSION}"
cargo tauri build

BIN="src-tauri/target/release/ExcelScry.exe"
if [[ ! -f "$BIN" ]]; then
    echo "error: expected $BIN to exist after the build" >&2
    exit 1
fi

SETUP="$(find src-tauri/target/release/bundle/nsis -maxdepth 1 -name '*-setup.exe' -print -quit)"
if [[ -z "$SETUP" ]]; then
    echo "error: no NSIS installer was produced" >&2
    exit 1
fi

echo "==> Collecting artifacts into dist/"
rm -rf dist
mkdir -p dist/ExcelScry-portable
cp "$BIN" dist/ExcelScry-portable/ExcelScry.exe
cp README.md dist/ExcelScry-portable/README.md
mkdir -p dist/ExcelScry-portable/samples
cp samples/*.xlsx samples/*.xlsm dist/ExcelScry-portable/samples/ 2>/dev/null || true
cp "$SETUP" "dist/ExcelScry-${VERSION}-setup.exe"

echo
echo "Artifacts:"
ls -la dist/ExcelScry-portable/ExcelScry.exe "dist/ExcelScry-${VERSION}-setup.exe" |
    awk '{ printf "  %10.2f MB  %s\n", $5 / 1048576, $9 }'
echo
echo "The installer adds a 'Scry Excel File' context-menu entry and registers"
echo "ExcelScry with Windows' Default Apps page. On Windows 11 the menu entry"
echo "appears under 'Show more options'."
