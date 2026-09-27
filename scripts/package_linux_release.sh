#!/usr/bin/env bash
# Package TackleCast for Linux distribution
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$ROOT_DIR/dist"
PKG_NAME="TackleCast-Linux-x86_64"
TARGET_DIR="$DIST_DIR/$PKG_NAME"

cd "$ROOT_DIR"

echo "Building release binary for Linux..."
cargo build --release

echo "Preparing package directory at $TARGET_DIR..."
rm -rf "$TARGET_DIR"
mkdir -p "$TARGET_DIR" "$TARGET_DIR/assets" "$TARGET_DIR/logs"

cp "$ROOT_DIR/target/release/tacklecast" "$TARGET_DIR/tacklecast"
chmod +x "$TARGET_DIR/tacklecast"

cp "$ROOT_DIR/scripts/run.sh" "$TARGET_DIR/run.sh"
chmod +x "$TARGET_DIR/run.sh"

if [[ -d "$ROOT_DIR/assets" ]]; then
    cp -r "$ROOT_DIR/assets/"* "$TARGET_DIR/assets/"
fi

if [[ -f "$ROOT_DIR/README.md" ]]; then
    cp "$ROOT_DIR/README.md" "$TARGET_DIR/"
fi

if [[ -f "$ROOT_DIR/LICENSE" ]]; then
    cp "$ROOT_DIR/LICENSE" "$TARGET_DIR/"
fi

echo "Creating tarball..."
cd "$DIST_DIR"
tar -czvf "${PKG_NAME}.tar.gz" "$PKG_NAME"

echo "Package created successfully at $DIST_DIR/${PKG_NAME}.tar.gz"
