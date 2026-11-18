#!/bin/sh
# Assemble mcpurge.app from the two macOS release binaries, and zip it.
#
# A bare Mach-O executable is not a Mac application, which is what the release
# published before this script existed. Downloaded from GitHub it arrives with
# no execute permission - an HTTP download carries no POSIX mode - so Finder
# shows a file that does nothing at all when double-clicked. Even after a
# chmod it is still not an app: double-clicking one opens Terminal and runs it
# there, with no Dock icon, no application menu, and privacy prompts attributed
# to Terminal rather than to mcpurge. mcpurge is a GUI program that also takes
# --list and --json, so the release needs both shapes: this bundle for opening,
# and the bare binaries it is built from for a PATH and for the self-updater.
#
#   usage: scripts/make-macos-app.sh <arm64-binary> <x86_64-binary> <out-dir>
#
# The version comes from Cargo.toml, which release.yml has already proved
# matches the tag, so there is one source of truth rather than two.
set -eu

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <arm64-binary> <x86_64-binary> <out-dir>" >&2
    exit 1
fi

ARM64="$1"
X86_64="$2"
OUT="$3"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

for f in "$ARM64" "$X86_64"; do
    [ -f "$f" ] || { echo "no such binary: $f" >&2; exit 1; }
done

VERSION="$(grep -m1 '^version = ' "$ROOT/Cargo.toml" | cut -d'"' -f2)"
[ -n "$VERSION" ] || { echo "could not read version from Cargo.toml" >&2; exit 1; }

APP="$OUT/mcpurge.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# One download that runs on both architectures. Two separate .app bundles would
# mean asking people which Mac they have, which is the kind of question an
# installer should answer for itself.
lipo -create -output "$APP/Contents/MacOS/mcpurge" "$ARM64" "$X86_64"
chmod 755 "$APP/Contents/MacOS/mcpurge"

# LSMinimumSystemVersion is 11.0 because that is the floor Rust's
# aarch64-apple-darwin target builds for; the Intel slice would run on older
# systems, but the bundle can only declare one minimum.
#
# The three folder usage strings are what macOS shows when a scan first reaches
# into Desktop, Documents or Downloads looking for project-scope MCP configs.
# Without them the prompt is a bare "mcpurge wants access", which is a worse
# question to be asked by a tool whose whole job is deleting things.
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>mcpurge</string>
    <key>CFBundleDisplayName</key>
    <string>mcpurge</string>
    <key>CFBundleIdentifier</key>
    <string>io.github.teebobaggens.mcpurge</string>
    <key>CFBundleExecutable</key>
    <string>mcpurge</string>
    <key>CFBundleIconFile</key>
    <string>mcpurge</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.developer-tools</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSSupportsAutomaticGraphicsSwitching</key>
    <true/>
    <key>NSDesktopFolderUsageDescription</key>
    <string>mcpurge looks for MCP server configs in project folders you scan.</string>
    <key>NSDocumentsFolderUsageDescription</key>
    <string>mcpurge looks for MCP server configs in project folders you scan.</string>
    <key>NSDownloadsFolderUsageDescription</key>
    <string>mcpurge looks for MCP server configs in project folders you scan.</string>
</dict>
</plist>
PLIST

# Built from assets/icon.svg rather than committed, so the icon in the Dock is
# always the icon in the repo. CFBundleIconFile names it without the extension,
# which is the spelling Launch Services expects.
"$ROOT/scripts/make-icon.sh" "$APP/Contents/Resources"

# Classic-era file, still read by Launch Services, and cheap to be right about.
printf 'APPL????' > "$APP/Contents/PkgInfo"

# lipo writes a fresh Mach-O, so whatever ad-hoc signature the linker put on
# each input is gone. On Apple Silicon unsigned arm64 code does not run at all,
# so this is not optional: an unsigned bundle would be killed on launch rather
# than merely warned about. "-" is ad-hoc - it proves nothing about who built
# the app, which is what the minisign signature beside it is for, but it is
# what makes the code loadable.
codesign --force --sign - --timestamp=none "$APP"
codesign --verify --deep --strict "$APP"

# ditto is the one zip on macOS that preserves the bundle: a plain `zip` loses
# the execute bit on Contents/MacOS/mcpurge, which would hand people the same
# unopenable download this script exists to fix.
( cd "$OUT" && ditto -c -k --keepParent --sequesterRsrc mcpurge.app mcpurge-macos-universal.app.zip )

echo "built $APP ($VERSION) and $OUT/mcpurge-macos-universal.app.zip"
lipo -archs "$APP/Contents/MacOS/mcpurge"
