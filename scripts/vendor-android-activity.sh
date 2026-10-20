#!/usr/bin/env bash
# Vendor android-activity with the CraftApps stylus patch.
#
# WHY: the S Pen's side button and the tool type (eraser end, palm) live in
# the `MotionEvent` — the one piece of input data winit drops. On the Galaxy
# Tab S6 Lite the button is reported by *replacing* the motion action with a
# proprietary one (211/213/212/214), which the NDK maps to
# `MotionAction::__Unknown`; winit then drops the whole event, so the stroke
# even stops while the button is held.
#
# The fix belongs in the crate that owns the input queue (android-activity),
# so we vendor it and apply our patch. The vendored copy is a build artefact
# (git-ignored): this script copies it out of the cargo registry and applies
# `patches/android-activity-<version>-stylus.patch`. Only our patch — which
# is ours — is kept in the repository.
#
# Usage: scripts/vendor-android-activity.sh [workspace-dir]
set -euo pipefail

VERSION="0.6.1"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PATCH="$REPO/patches/android-activity-$VERSION-stylus.patch"
TARGET="$REPO/vendor/android-activity"

if [ -f "$TARGET/.craftapps-patched" ]; then
    echo "== android-activity $VERSION already vendored and patched =="
    exit 0
fi

# Find the crate in the cargo registry (cargo has fetched it for a build).
SRC=""
for dir in "${CARGO_HOME:-$HOME/.cargo}"/registry/src/*/android-activity-$VERSION; do
    if [ -d "$dir" ]; then
        SRC="$dir"
        break
    fi
done
if [ -z "$SRC" ]; then
    echo "android-activity $VERSION is not in the cargo registry yet." >&2
    echo "Run 'cargo fetch' in the app's upstream workspace first, then re-run this script." >&2
    exit 1
fi

echo "== vendoring android-activity $VERSION from $SRC =="
rm -rf "$TARGET"
mkdir -p "$REPO/vendor"
cp -r "$SRC" "$TARGET"
chmod -R u+w "$TARGET"

# The registry copy is read-only; cargo needs to write the .cargo-ok marker.
touch "$TARGET/.cargo-ok"

if [ ! -f "$PATCH" ]; then
    echo "missing patch: $PATCH" >&2
    exit 1
fi

echo "== applying $PATCH =="
( cd "$TARGET" && patch -p1 --forward < "$PATCH" )

touch "$TARGET/.craftapps-patched"
echo "== android-activity $VERSION vendored and patched =="
