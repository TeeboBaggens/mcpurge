#!/bin/sh
# Build mcpurge.icns, and the window icon the GUI carries, from assets/*.svg.
#
# Nothing here needs installing: sips and iconutil both ship with macOS, which
# is why the icon sources are SVG and the rendering happens at build time
# rather than ten PNGs being committed and slowly drifting out of step.
#
#   usage: scripts/make-icon.sh <out-dir>     writes <out-dir>/mcpurge.icns
#          scripts/make-icon.sh --window      rewrites assets/icon-window.png
#
# An .icns holds ten images, and macOS picks between them by size. The two
# smallest slots get icon-small.svg: at 16 and 32 points the full mark closes
# up into a smudge, so those sizes carry a simplified version of the same
# gesture. Everything from 128 up gets icon.svg.
set -eu

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIG="$ROOT/assets/icon.svg"
SMALL="$ROOT/assets/icon-small.svg"

for f in "$BIG" "$SMALL"; do
    [ -f "$f" ] || { echo "missing icon source: $f" >&2; exit 1; }
done

# The GUI's own window icon, for the Linux and Windows title bars and task
# switchers; macOS takes its icon from the bundle instead. Committed as a PNG
# because the app reads it with include_bytes! and cannot run sips at startup.
if [ "${1:-}" = "--window" ]; then
    sips -s format png -Z 256 "$BIG" --out "$ROOT/assets/icon-window.png" >/dev/null
    echo "wrote assets/icon-window.png"
    exit 0
fi

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <out-dir> | --window" >&2
    exit 1
fi

OUT="$1"
mkdir -p "$OUT"
SET="$OUT/mcpurge.iconset"
rm -rf "$SET"
mkdir -p "$SET"

render() {
    sips -s format png -Z "$2" "$1" --out "$SET/$3" >/dev/null
}

render "$SMALL"  16 icon_16x16.png
render "$SMALL"  32 icon_16x16@2x.png
render "$SMALL"  32 icon_32x32.png
render "$SMALL"  64 icon_32x32@2x.png
render "$BIG"   128 icon_128x128.png
render "$BIG"   256 icon_128x128@2x.png
render "$BIG"   256 icon_256x256.png
render "$BIG"   512 icon_256x256@2x.png
render "$BIG"   512 icon_512x512.png
render "$BIG"  1024 icon_512x512@2x.png

iconutil -c icns "$SET" -o "$OUT/mcpurge.icns"
rm -rf "$SET"
echo "built $OUT/mcpurge.icns"
