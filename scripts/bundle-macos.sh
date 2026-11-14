#!/usr/bin/env bash
# Assemble an ad-hoc signed SnapIt.app so Screen Recording and notifications
# attach to the bundle (dev.SnapIt.SnapIt) instead of Terminal / Cursor / `snapit`.
# Does not change `cargo run -p snapit`.
#
# Usage: scripts/bundle-macos.sh [--bin PATH] [--out DIR] [--release] [--target TRIPLE]
#   --target  Rust target triple, e.g. aarch64-apple-darwin or x86_64-apple-darwin
#             (default: the host). Builds a release binary with --locked, like
#             SwitchHostsLite.
#   --out     Directory to put the .app in (default: target/macos)
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/bundle-macos.sh [--bin PATH] [--out DIR] [--release] [--target TRIPLE]

  --bin PATH      Use this snapit binary (default: target/debug/snapit if present,
                  else target/release/snapit, else cargo build -p snapit).
  --out DIR       Directory that will contain SnapIt.app (default: target/macos).
  --release       Prefer/build target/release/snapit instead of debug.
  --target TRIPLE Rust target, e.g. aarch64-apple-darwin or x86_64-apple-darwin.
                  Builds `cargo build --release --locked --target` (CI).

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

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

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
      cargo build -p snapit --release
    fi
    BIN="$target_dir/release/snapit"
    return
  fi
  if [[ -x "$target_dir/debug/snapit" ]]; then
    BIN="$target_dir/debug/snapit"
  elif [[ -x "$target_dir/release/snapit" ]]; then
    BIN="$target_dir/release/snapit"
  else
    cargo build -p snapit
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
  python3 - <<'PY'
from pathlib import Path
text = Path("Cargo.toml").read_text()
in_pkg = False
for line in text.splitlines():
    if line.strip() == "[workspace.package]":
        in_pkg = True
        continue
    if in_pkg and line.startswith("["):
        break
    if in_pkg and line.startswith("version"):
        print(line.split("=", 1)[1].strip().strip('"'))
        raise SystemExit(0)
print("0.0.1")
PY
}

resolve_bin
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
ICON="$(ensure_icon)"
VERSION="$(workspace_version)"
APP="$OUT_DIR/SnapIt.app"
CONTENTS="$APP/Contents"

rm -rf "$APP"
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

codesign --sign - --force --deep "$APP"

echo "bundled $APP"
echo "binary  $BIN"
echo "id      dev.SnapIt.SnapIt"
echo "sign    ad-hoc (codesign --sign - --force --deep)"
