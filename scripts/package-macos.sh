#!/usr/bin/env bash
# Wrap SnapIt.app in a SwitchHostsLite-style zip + ULMO DMG.
#
# Usage: scripts/package-macos.sh [--app PATH] [--out DIR] [--name NAME]
#                                 [--bundle] [--release] [--target TRIPLE]
#
# Mirrors SwitchHostsLite (.github/workflows/release.yml + scripts/bundle-macos.sh):
#   1. Keep the existing .app step (scripts/bundle-macos.sh → SnapIt.app).
#   2. Optional Developer ID sign, notarytool, stapler (same secret names).
#   3. ditto zip (--zlibCompressionLevel 9 --keepParent).
#   4. hdiutil create -format ULMO with an /Applications symlink
#      (retries on "Resource busy"). Not create-dmg; no brew package needed.
#
# Secrets (all optional). If any sign secret is missing, keep the ad-hoc
# signature and still write the DMG (exit 0). Do not invent certificates.
#   MACOS_CERTIFICATE            base64-encoded Developer ID .p12
#   MACOS_CERTIFICATE_PASSWORD
#   MACOS_SIGNING_IDENTITY       "Developer ID Application: …"
#   APPLE_ID  APPLE_TEAM_ID  APPLE_APP_PASSWORD   notarize + staple
#
# CI, after the .app exists (NAME matches SwitchHostsLite: App-version-macos-arch):
#   scripts/package-macos.sh --app dist/SnapIt.app --out out --name "$NAME"
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/package-macos.sh [--app PATH] [--out DIR] [--name NAME]
                                [--bundle] [--release] [--target TRIPLE]

  --app PATH      SnapIt.app to wrap (default: target/macos/SnapIt.app).
  --out DIR       Directory for the .dmg and .zip (default: dist).
  --name NAME     Basename without extension
                  (default: SnapIt-<version>-macos-<arch>).
  --bundle        Run scripts/bundle-macos.sh first.
  --release       With --bundle, pass --release to the bundler.
  --target TRIPLE With --bundle, pass --target to the bundler.

Sign / notarize / staple only when the SwitchHostsLite secret names are set.
Otherwise the ad-hoc .app is packed as-is. Missing secrets are not an error.
EOF
}

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

APP=""
OUT_DIR="$ROOT/dist"
NAME=""
DO_BUNDLE=0
BUILD_RELEASE=0
TARGET=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --app)
      APP="${2:?--app requires a path}"
      shift 2
      ;;
    --out)
      OUT_DIR="${2:?--out requires a directory}"
      shift 2
      ;;
    --name)
      NAME="${2:?--name requires a value}"
      shift 2
      ;;
    --bundle)
      DO_BUNDLE=1
      shift
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

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "scripts/package-macos.sh requires macOS (hdiutil / codesign / ditto)" >&2
  exit 1
fi

if [[ -z "$APP" ]]; then
  APP="$ROOT/target/macos/SnapIt.app"
fi

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
print("0.0.2")
PY
}

macos_key() {
  local bin="$1/Contents/MacOS/SnapIt"
  local archs=""
  if [[ -f "$bin" ]] && command -v lipo >/dev/null 2>&1; then
    archs="$(lipo -archs "$bin" 2>/dev/null || true)"
  fi
  archs="$(printf '%s' "$archs" | xargs)"
  case "$archs" in
    arm64) echo macos-aarch64 ;;
    x86_64) echo macos-x86_64 ;;
    "arm64 x86_64"|"x86_64 arm64") echo macos-universal ;;
    *)
      case "$(uname -m)" in
        arm64) echo macos-aarch64 ;;
        *) echo macos-x86_64 ;;
      esac
      ;;
  esac
}

secret_value() {
  case "$1" in
    MACOS_CERTIFICATE) printf '%s' "${MACOS_CERTIFICATE:-}" ;;
    MACOS_CERTIFICATE_PASSWORD) printf '%s' "${MACOS_CERTIFICATE_PASSWORD:-}" ;;
    MACOS_SIGNING_IDENTITY) printf '%s' "${MACOS_SIGNING_IDENTITY:-}" ;;
    APPLE_ID) printf '%s' "${APPLE_ID:-}" ;;
    APPLE_TEAM_ID) printf '%s' "${APPLE_TEAM_ID:-}" ;;
    APPLE_APP_PASSWORD) printf '%s' "${APPLE_APP_PASSWORD:-}" ;;
    *) return 1 ;;
  esac
}

secret_set() {
  local v
  v="$(secret_value "$1" | tr -d '[:space:]')"
  [[ -n "$v" ]]
}

all_secrets_set() {
  local n
  for n in "$@"; do
    secret_set "$n" || return 1
  done
  return 0
}

if [[ "$DO_BUNDLE" -eq 1 ]]; then
  bundle_out="$(dirname "$APP")"
  mkdir -p "$bundle_out"
  bundle_out="$(cd "$bundle_out" && pwd)"
  bundle_args=(--out "$bundle_out")
  if [[ -n "$TARGET" ]]; then
    bundle_args+=(--target "$TARGET")
  elif [[ "$BUILD_RELEASE" -eq 1 ]]; then
    bundle_args+=(--release)
  fi
  "$ROOT/scripts/bundle-macos.sh" "${bundle_args[@]}"
  APP="$bundle_out/SnapIt.app"
fi

if [[ ! -d "$APP" ]]; then
  echo "missing $APP — run scripts/bundle-macos.sh first, or pass --bundle" >&2
  exit 1
fi

APP="$(cd "$APP" && pwd)"
mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

VERSION="$(workspace_version)"
if [[ -z "$NAME" ]]; then
  NAME="SnapIt-${VERSION}-$(macos_key "$APP")"
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/snapit-pkg.XXXXXX")"
KEYCHAIN=""
ORIG_KEYCHAIN=""
SIGN_STATUS="ad-hoc (MACOS_CERTIFICATE / MACOS_SIGNING_IDENTITY not set)"

cleanup() {
  if [[ -n "$ORIG_KEYCHAIN" ]]; then
    security default-keychain -s "$ORIG_KEYCHAIN" >/dev/null 2>&1 || true
  fi
  if [[ -n "$KEYCHAIN" ]]; then
    security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT

STAGED="$WORK/SnapIt.app"
ditto "$APP" "$STAGED"

sign_and_notarize() {
  local cert="$WORK/cert.p12"
  local zip="$WORK/notarize.zip"
  local keychain_pw="ci"

  ORIG_KEYCHAIN="$(security default-keychain 2>/dev/null | tr -d ' "')"
  KEYCHAIN="$WORK/build.keychain"

  security create-keychain -p "$keychain_pw" "$KEYCHAIN"
  security default-keychain -s "$KEYCHAIN"
  security unlock-keychain -p "$keychain_pw" "$KEYCHAIN"
  printf '%s' "$MACOS_CERTIFICATE" | base64 --decode > "$cert"
  security import "$cert" -k "$KEYCHAIN" -P "$MACOS_CERTIFICATE_PASSWORD" -T /usr/bin/codesign
  security set-key-partition-list -S apple-tool:,apple: -s -k "$keychain_pw" "$KEYCHAIN"
  rm -f "$cert"

  codesign --force --deep --options runtime --timestamp \
    -s "$MACOS_SIGNING_IDENTITY" "$STAGED"

  SIGN_STATUS="Developer ID ($MACOS_SIGNING_IDENTITY)"

  if ! all_secrets_set APPLE_ID APPLE_TEAM_ID APPLE_APP_PASSWORD; then
    echo "skip notarize: set APPLE_ID, APPLE_TEAM_ID, APPLE_APP_PASSWORD" >&2
    SIGN_STATUS="Developer ID, not notarized (Apple notary secrets not set)"
    return 0
  fi

  ditto -c -k --keepParent "$STAGED" "$zip"
  xcrun notarytool submit "$zip" \
    --apple-id "$APPLE_ID" \
    --team-id "$APPLE_TEAM_ID" \
    --password "$APPLE_APP_PASSWORD" \
    --wait
  xcrun stapler staple "$STAGED"
  SIGN_STATUS="Developer ID, notarized, stapled"
}

if all_secrets_set MACOS_CERTIFICATE MACOS_CERTIFICATE_PASSWORD MACOS_SIGNING_IDENTITY; then
  sign_and_notarize
else
  echo "skip sign/notarize: set MACOS_CERTIFICATE, MACOS_CERTIFICATE_PASSWORD, MACOS_SIGNING_IDENTITY (and APPLE_ID, APPLE_TEAM_ID, APPLE_APP_PASSWORD to notarize)" >&2
fi

ZIP_PATH="$OUT_DIR/$NAME.zip"
DMG_PATH="$OUT_DIR/$NAME.dmg"
rm -f "$ZIP_PATH" "$DMG_PATH"

ditto -c -k --zlibCompressionLevel 9 --keepParent "$STAGED" "$ZIP_PATH"

DMG_STAGE="$WORK/dmg"
mkdir -p "$DMG_STAGE"
ditto "$STAGED" "$DMG_STAGE/SnapIt.app"
ln -s /Applications "$DMG_STAGE/Applications"

# hdiutil occasionally fails with "Resource busy" on CI runners.
# ULMO (lzma) needs macOS 10.15+ to open and is a third smaller than UDZO.
attempt=1
while true; do
  if hdiutil create -volname "SnapIt" -srcfolder "$DMG_STAGE" -ov -format ULMO "$DMG_PATH"; then
    break
  fi
  if [[ "$attempt" -eq 5 ]]; then
    echo "hdiutil create failed after 5 attempts" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 5
done

echo "packaged $DMG_PATH"
echo "zip      $ZIP_PATH"
echo "app      $APP"
echo "sign     $SIGN_STATUS"
