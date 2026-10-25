#!/bin/sh
# Builds a release binary, ad-hoc signs it (stable TCC identity), installs it and
# registers a launchd agent that runs it with the flags given to this script.
#
#   ./install.sh --server https://photos.example.com --api-key-file ~/.config/immich-photo-sync/api-key
#
# Every argument is passed through to immich-photo-sync unchanged.
set -eu

cd "$(dirname "$0")"

LABEL="com.immich-photo-sync.agent"
BIN_DIR="${BIN_DIR:-$HOME/.local/bin}"
BIN="$BIN_DIR/immich-photo-sync"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
LOG_DIR="$HOME/Library/Logs/immich-photo-sync"

if [ "$#" -eq 0 ]; then
    echo "usage: $0 --server URL (--api-key KEY | --api-key-file PATH) [other immich-photo-sync flags]" >&2
    exit 2
fi

cargo build --release
mkdir -p "$BIN_DIR" "$LOG_DIR" "$HOME/Library/LaunchAgents"

if launchctl print "gui/$(id -u)/$LABEL" >/dev/null 2>&1; then
    launchctl bootout "gui/$(id -u)/$LABEL"
fi

install -m 755 target/release/immich-photo-sync "$BIN"
codesign --force --sign - --identifier com.immich-photo-sync.cli "$BIN"

"$BIN" "$@" launchd-plist --label "$LABEL" --log-dir "$LOG_DIR" > "$PLIST" 2>/dev/null
launchctl bootstrap "gui/$(id -u)" "$PLIST"

echo
echo "Installed $BIN and started agent $LABEL."
echo "macOS will ask for Photos access the first time the agent runs; grant it, or enable it later in"
echo "System Settings > Privacy & Security > Photos."
echo
echo "  logs:    tail -f $LOG_DIR/immich-photo-sync.err.log"
echo "  status:  $BIN status"
echo "  stop:    launchctl bootout gui/\$(id -u)/$LABEL"
