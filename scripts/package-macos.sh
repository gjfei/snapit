#!/usr/bin/env bash
# Wrap SnapIt.app in a SwitchHostsLite-style zip + ULMO DMG.
#
# Usage: scripts/package-macos.sh [--app PATH] [--out DIR] [--name NAME]
#                                 [--bundle] [--release] [--target TRIPLE] [--version]
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
                                [--bundle] [--release] [--target TRIPLE] [--version]

  --app PATH      SnapIt.app to wrap (default: target/macos/SnapIt.app).
  --out DIR       Directory for the .dmg and .zip (default: dist).
  --name NAME     Basename without extension
                  (default: SnapIt-<version>-macos-<arch>).
  --bundle        Run scripts/bundle-macos.sh first.
  --release       With --bundle, pass --release to the bundler.
  --target TRIPLE With --bundle, pass --target to the bundler.
  --version       Print the workspace package version from cargo metadata.

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
PRINT_VERSION=0

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

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "scripts/package-macos.sh requires macOS (hdiutil / codesign / ditto)" >&2
  exit 1
fi

if [[ -z "$APP" ]]; then
  APP="$ROOT/target/macos/SnapIt.app"
fi

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

if [[ -z "$NAME" ]]; then
  NAME="SnapIt-${VERSION}-$(macos_key "$APP")"
fi
if [[ "$NAME" == */* || "$NAME" == "." || "$NAME" == ".." ]]; then
  echo "--name must be a basename, not a path: $NAME" >&2
  exit 1
fi

WORK=""
PUBLISH=""
KEYCHAIN=""
ORIG_KEYCHAIN=""
SIGN_STATUS="ad-hoc (MACOS_CERTIFICATE / MACOS_SIGNING_IDENTITY not set)"
ZIP_PREVIOUS=""
DMG_PREVIOUS=""
ZIP_PUBLISHED=0
DMG_PUBLISHED=0
PUBLISH_COMPLETE=0
RESTORE_FAILED=0

restore_artifact() {
  local target="$1" previous="$2" published="$3"
  if [[ -n "$previous" ]]; then
    if [[ "$published" -eq 1 ]]; then
      if [[ -e "$target" || -L "$target" ]] && ! rm -f "$target"; then
        RESTORE_FAILED=1
        return
      fi
    fi
    if [[ -e "$target" || -L "$target" ]]; then
      RESTORE_FAILED=1
    elif ! mv "$previous" "$target"; then
      RESTORE_FAILED=1
    fi
  elif [[ "$published" -eq 1 ]] && ! rm -f "$target"; then
    RESTORE_FAILED=1
  fi
}

cleanup() {
  if [[ -n "$ORIG_KEYCHAIN" ]]; then
    security default-keychain -s "$ORIG_KEYCHAIN" >/dev/null 2>&1 || true
  fi
  if [[ -n "$KEYCHAIN" ]]; then
    security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
  fi
  if [[ -n "$WORK" ]]; then
    rm -rf "$WORK" || true
  fi
  if [[ -n "$PUBLISH" ]]; then
    if [[ "$PUBLISH_COMPLETE" -eq 0 ]]; then
      restore_artifact "$OUT_DIR/$NAME.zip" "$ZIP_PREVIOUS" "$ZIP_PUBLISHED"
      restore_artifact "$OUT_DIR/$NAME.dmg" "$DMG_PREVIOUS" "$DMG_PUBLISHED"
    fi
    if [[ "$RESTORE_FAILED" -eq 0 ]]; then
      rm -rf "$PUBLISH" || true
    else
      echo "could not restore every previous artifact; backups preserved in $PUBLISH" >&2
    fi
  fi
}
trap cleanup EXIT

WORK="$(mktemp -d "${TMPDIR:-/tmp}/snapit-pkg.XXXXXX")"
PUBLISH="$(mktemp -d "$OUT_DIR/.snapit-publish.XXXXXX")"
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
ZIP_TMP="$PUBLISH/$NAME.zip"
DMG_TMP="$PUBLISH/$NAME.dmg"

ditto -c -k --zlibCompressionLevel 9 --keepParent "$STAGED" "$ZIP_TMP"

DMG_STAGE="$WORK/dmg"
mkdir -p "$DMG_STAGE"
ditto "$STAGED" "$DMG_STAGE/SnapIt.app"
ln -s /Applications "$DMG_STAGE/Applications"

# hdiutil occasionally fails with "Resource busy" on CI runners.
# ULMO (lzma) needs macOS 10.15+ to open and is a third smaller than UDZO.
attempt=1
while true; do
  if hdiutil create -volname "SnapIt" -srcfolder "$DMG_STAGE" -ov -format ULMO "$DMG_TMP"; then
    break
  fi
  if [[ "$attempt" -eq 5 ]]; then
    echo "hdiutil create failed after 5 attempts" >&2
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 5
done

if [[ ! -s "$ZIP_TMP" || ! -s "$DMG_TMP" ]]; then
  echo "packaging did not produce non-empty zip and dmg files" >&2
  exit 1
fi
if [[ -e "$ZIP_PATH" || -L "$ZIP_PATH" ]]; then
  ZIP_PREVIOUS="$PUBLISH/previous.zip"
  mv "$ZIP_PATH" "$ZIP_PREVIOUS"
fi
if [[ -e "$DMG_PATH" || -L "$DMG_PATH" ]]; then
  DMG_PREVIOUS="$PUBLISH/previous.dmg"
  mv "$DMG_PATH" "$DMG_PREVIOUS"
fi
mv "$ZIP_TMP" "$ZIP_PATH"
ZIP_PUBLISHED=1
mv "$DMG_TMP" "$DMG_PATH"
DMG_PUBLISHED=1
PUBLISH_COMPLETE=1

echo "packaged $DMG_PATH"
echo "zip      $ZIP_PATH"
echo "app      $APP"
echo "sign     $SIGN_STATUS"
