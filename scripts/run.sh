#!/usr/bin/env bash
# TackleCast Linux Launcher Script
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Find binary
if [[ -f "$SCRIPT_DIR/tacklecast" ]]; then
    BIN="$SCRIPT_DIR/tacklecast"
elif [[ -f "$ROOT_DIR/target/release/tacklecast" ]]; then
    BIN="$ROOT_DIR/target/release/tacklecast"
elif [[ -f "$ROOT_DIR/target/debug/tacklecast" ]]; then
    BIN="$ROOT_DIR/target/debug/tacklecast"
else
    echo "TackleCast binary not found. Building release binary..."
    cd "$ROOT_DIR"
    cargo build --release
    BIN="$ROOT_DIR/target/release/tacklecast"
fi

# Check permissions for video/audio devices
if [[ -d "/dev" ]]; then
    VIDEO_DEVS=$(ls /dev/video* 2>/dev/null || true)
    if [[ -n "$VIDEO_DEVS" ]]; then
        for dev in $VIDEO_DEVS; do
            if [[ ! -r "$dev" || ! -w "$dev" ]]; then
                echo "Warning: No read/write access to $dev."
                echo "To fix device permissions, run:"
                echo "  sudo usermod -a -G video,audio $USER"
                echo "and log out then log back in."
                break
            fi
        done
    fi
fi

# Launch
echo "Starting TackleCast from $BIN..."
exec "$BIN" "$@"
