#!/usr/bin/env bash
# Assemble an ad-hoc signed SnapIt.app so Screen Recording and notifications
# attach to the bundle (dev.SnapIt.SnapIt) instead of Terminal / Cursor / `snapit`.
# Does not change `cargo run -p snapit`.
#
# Usage: scripts/bundle-macos.sh [--bin PATH] [--out DIR] [--release] [--target TRIPLE] [--version]
#   --target  Rust target triple, e.g. aarch64-apple-darwin or x86_64-apple-darwin
#             (default: the host). Builds a release binary with --locked, like
#             SwitchHostsLite.
#   --out     Directory to put the .app in (default: target/macos)
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/bundle-macos.sh [--bin PATH] [--out DIR] [--release] [--target TRIPLE] [--version]

  --bin PATH      Use this snapit binary (default: target/debug/snapit if present,
                  else target/release/snapit, else cargo build -p snapit).
  --out DIR       Directory that will contain SnapIt.app (default: target/macos).
  --release       Prefer/build target/release/snapit instead of debug.
  --target TRIPLE Rust target, e.g. aarch64-apple-darwin or x86_64-apple-darwin.
                  Builds `cargo build --release --locked --target` (CI).
  --version       Print the workspace package version from cargo metadata.

The app is ad-hoc signed (codesign --sign - --force --deep). Rebuilding
changes the signature; macOS may require Screen Recording to be re-granted.
EOF
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN=""
OUT_DIR="$ROOT/target/macos"
BUILD_RELEASE=0
TARGET=""
PRINT_VERSION=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --bin)
      BIN="${2:?--bin requires a path}"
      shift 2
      ;;
    --out)
      OUT_DIR="${2:?--out requires a directory}"
      shift 2
      ;;
    --release)
      BUILD_RELEASE=1
      shift
      ;;
    --target)
      TARGET="${2:?--target requires a value}"
      shift 2
      ;;
    --version)
      PRINT_VERSION=1
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 1
      ;;
  esac
done

cargo_target_dir() {
  cargo metadata --format-version 1 --no-deps \
    | python3 -c "import json,sys; print(json.load(sys.stdin)['target_directory'])"
}

resolve_bin() {
  if [[ -n "$BIN" ]]; then
    if [[ ! -x "$BIN" ]]; then
      echo "not an executable binary: $BIN" >&2
      exit 1
    fi
    return
  fi
  local target_dir
  target_dir="$(cargo_target_dir)"
  if [[ -n "$TARGET" ]]; then
    cargo build --release --locked --target "$TARGET" -p snapit
    BIN="$target_dir/$TARGET/release/snapit"
    return
  fi
  if [[ "$BUILD_RELEASE" -eq 1 ]]; then
    if [[ ! -x "$target_dir/release/snapit" ]]; then
      cargo build -p snapit --release --locked
    fi
    BIN="$target_dir/release/snapit"
    return
  fi
  if [[ -x "$target_dir/debug/snapit" ]]; then
    BIN="$target_dir/debug/snapit"
  elif [[ -x "$target_dir/release/snapit" ]]; then
    BIN="$target_dir/release/snapit"
  else
    cargo build -p snapit --locked
    BIN="$target_dir/debug/snapit"
  fi
}

ensure_icon() {
  local icns="$ROOT/assets/AppIcon.icns"
  if [[ ! -f "$icns" ]]; then
    icns="$ROOT/macos/AppIcon.icns"
  fi
  if [[ ! -f "$icns" ]]; then
    python3 "$ROOT/scripts/generate-macos-icon.py" "$icns" >/dev/null
  fi
  echo "$icns"
}

workspace_version() {
  cargo metadata --format-version 1 --no-deps | python3 -c '
import json, re, sys
metadata = json.load(sys.stdin)
packages = [p for p in metadata["packages"] if p["name"] == "snapit"]
if len(packages) != 1:
    raise SystemExit("cargo metadata did not return exactly one snapit package")
version = packages[0]["version"]
if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version):
    raise SystemExit(f"invalid snapit package version: {version!r}")
print(version)
'
}

VERSION="$(workspace_version)"
if [[ "$PRINT_VERSION" -eq 1 ]]; then
  printf '%s\n' "$VERSION"
  exit 0
fi

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

resolve_bin
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
ICON="$(ensure_icon)"
APP="$OUT_DIR/SnapIt.app"
WORK="$(mktemp -d "$OUT_DIR/.SnapIt-build.XXXXXX")"
TEMP_APP="$WORK/SnapIt.app"
PREVIOUS_APP=""
CONTENTS="$TEMP_APP/Contents"

cleanup() {
  local status=$?
  if [[ -n "$PREVIOUS_APP" && ! -e "$APP" && ! -L "$APP" ]]; then
    if ! mv "$PREVIOUS_APP" "$APP"; then
      echo "could not restore $APP; previous bundle preserved in $PREVIOUS_APP" >&2
    fi
  fi
  if [[ -z "$PREVIOUS_APP" || ! -e "$PREVIOUS_APP" ]]; then
    rm -rf "$WORK" || true
  else
    echo "preserving previous bundle and build directory: $WORK" >&2
  fi
  exit "$status"
}
trap cleanup EXIT

mkdir -p "$CONTENTS/MacOS" "$CONTENTS/Resources"
cp "$ROOT/macos/Info.plist" "$CONTENTS/Info.plist"
if [[ -x /usr/libexec/PlistBuddy ]]; then
  /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$CONTENTS/Info.plist"
  /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $VERSION" "$CONTENTS/Info.plist"
fi
printf 'APPL????' > "$CONTENTS/PkgInfo"
cp "$BIN" "$CONTENTS/MacOS/SnapIt"
chmod +x "$CONTENTS/MacOS/SnapIt"
cp "$ICON" "$CONTENTS/Resources/AppIcon.icns"

codesign --sign - --force --deep "$TEMP_APP"

# Keep the previous bundle until the newly signed one is ready. If the final
# rename fails, the EXIT trap puts the previous bundle back.
if [[ -e "$APP" || -L "$APP" ]]; then
  PREVIOUS_APP="$WORK/SnapIt.previous.app"
  mv "$APP" "$PREVIOUS_APP"
fi
mv "$TEMP_APP" "$APP"
PREVIOUS_APP=""

echo "bundled $APP"
echo "binary  $BIN"
echo "id      dev.SnapIt.SnapIt"
echo "sign    ad-hoc (codesign --sign - --force --deep)"
