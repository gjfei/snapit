#!/usr/bin/env bash
# Stage dist/windows/SnapIt/SnapIt.exe and build the all-users NSIS installer
# dist/SnapIt-<version>-windows-<arch>-setup.exe.
# Does not change application behavior or install cross compilers.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/bundle-windows.sh [--bin PATH] [--out DIR] [--setup-out DIR] [--release] [--target TRIPLE]

  --bin PATH      Use this Windows snapit.exe instead of building.
  --out DIR       Staging directory for SnapIt/ (default: dist/windows).
  --setup-out DIR Directory for the setup executable (default: dist).
  --release       Build release (default).
  --debug         Use/build the debug profile instead.
  --target TRIPLE Host triple on Windows; x86_64-pc-windows-msvc elsewhere.

Setup: dist/SnapIt-<version>-windows-x86_64-setup.exe or ...-windows-aarch64-setup.exe.
Installs to %ProgramFiles%\SnapIt for all users. Needs makensis (NSIS);
set MAKENSIS or put it on PATH. Without NSIS the script stages the exe and
prints a skip message.
Cross-compiling from a non-Windows host requires that Rust target and a
Windows linker (native Windows, cargo-xwin, lld-link + xwin, or
CARGO_TARGET_<TRIPLE>_LINKER). If either is missing the script prints a
skip message and exits 0.
EOF
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

BIN=""
OUT_DIR="$ROOT/dist/windows"
SETUP_OUT="$ROOT/dist"
BUILD_RELEASE=1
TARGET=""
TARGET_EXPLICIT=0

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
    --setup-out)
      SETUP_OUT="${2:?--setup-out requires a directory}"
      shift 2
      ;;
    --release)
      BUILD_RELEASE=1
      shift
      ;;
    --debug)
      BUILD_RELEASE=0
      shift
      ;;
    --target)
      TARGET="${2:?--target requires a value}"
      TARGET_EXPLICIT=1
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

workspace_version() {
  awk '
    /^\[workspace\.package\]$/ { in_pkg = 1; next }
    in_pkg && /^\[/ { exit }
    in_pkg && /^version[[:space:]]*=/ {
      sub(/^[^=]*=[[:space:]]*/, "")
      gsub(/["[:space:]]/, "")
      print
      exit
    }
  ' Cargo.toml
}

version_quad() {
  local major rest minor patch
  IFS=. read -r major minor patch rest <<< "$1"
  major="${major//[^0-9]/}"
  minor="${minor//[^0-9]/}"
  patch="${patch//[^0-9]/}"
  printf '%s.%s.%s.0\n' "${major:-0}" "${minor:-0}" "${patch:-0}"
}

windows_key() {
  local target="$TARGET"
  # A --bin path carries the target triple when --target was omitted, so an
  # ARM64 binary does not get an x86_64 installer name.
  if [[ "$TARGET_EXPLICIT" -eq 0 ]]; then
    case "$BIN" in
      *aarch64-pc-windows-msvc*) target="aarch64-pc-windows-msvc" ;;
    esac
  fi
  case "$target" in
    aarch64-pc-windows-msvc) echo windows-aarch64 ;;
    *) echo windows-x86_64 ;;
  esac
}

target_linker_env() {
  printf 'CARGO_TARGET_%s_LINKER' "$(printf '%s' "$TARGET" | tr 'abcdefghijklmnopqrstuvwxyz-' 'ABCDEFGHIJKLMNOPQRSTUVWXYZ_')"
}

is_windows_host() {
  case "$(uname -s 2>/dev/null || true)" in
    MINGW*|MSYS*|CYGWIN*) return 0 ;;
  esac
  return 1
}

host_triple() {
  local rustc_bin="${RUSTC:-rustc}"
  "$rustc_bin" -vV 2>/dev/null | awk '/^host:/ { print $2; exit }' || true
}

is_host_target() {
  local host
  host="$(host_triple)"
  [[ -n "$host" && "$TARGET" == "$host" ]]
}

rust_target_installed() {
  command -v rustup >/dev/null 2>&1 || return 1
  rustup target list --installed 2>/dev/null | grep -Fxq "$TARGET"
}

cargo_config_has_windows_linker() {
  local f
  for f in \
    "$ROOT/.cargo/config.toml" \
    "$ROOT/.cargo/config" \
    "${CARGO_HOME:-$HOME/.cargo}/config.toml" \
    "${CARGO_HOME:-$HOME/.cargo}/config"
  do
    [[ -f "$f" ]] || continue
    if grep -q "$TARGET" "$f" && grep -q "linker" "$f"; then
      return 0
    fi
  done
  return 1
}

windows_linker_available() {
  local env_name
  env_name="$(target_linker_env)"
  if [[ -n "${!env_name:-}" ]]; then
    return 0
  fi
  command -v cargo-xwin >/dev/null 2>&1 && return 0
  command -v xwin >/dev/null 2>&1 && return 0
  if command -v lld-link >/dev/null 2>&1 && [[ -d "${XWIN_DIR:-$HOME/.xwin}" ]]; then
    return 0
  fi
  command -v link.exe >/dev/null 2>&1 && return 0
  cargo_config_has_windows_linker
}

can_build_target() {
  is_host_target && return 0
  rust_target_installed || return 1
  is_windows_host || windows_linker_available
}

skip_windows() {
  local reasons=()
  if ! is_host_target && ! rust_target_installed; then
    reasons+=("Rust target $TARGET is not installed")
  fi
  if ! is_host_target && ! is_windows_host && ! windows_linker_available; then
    reasons+=("no Windows linker (native Windows, cargo-xwin, lld-link+xwin, or $(target_linker_env))")
  fi
  if [[ ${#reasons[@]} -eq 0 ]]; then
    reasons+=("Windows toolchain for $TARGET is not usable on this machine")
  fi
  echo "skip: Windows bundle ($TARGET): ${reasons[*]}. Not installing a cross compiler."
  exit 0
}

profile_dir() {
  if [[ "$BUILD_RELEASE" -eq 1 ]]; then
    echo release
  else
    echo debug
  fi
}

default_bin_path() {
  if is_host_target; then
    echo "$ROOT/target/$(profile_dir)/snapit.exe"
  else
    echo "$ROOT/target/$TARGET/$(profile_dir)/snapit.exe"
  fi
}

looks_like_windows_exe() {
  local path="$1"
  if command -v file >/dev/null 2>&1; then
    file "$path" | grep -Eqi 'PE32(\+|)|MS Windows|Windows PE'
  else
    return 0
  fi
}

find_makensis() {
  if [[ -n "${MAKENSIS:-}" && -x "$MAKENSIS" ]]; then
    printf '%s\n' "$MAKENSIS"
    return 0
  fi
  if command -v makensis >/dev/null 2>&1; then
    command -v makensis
    return 0
  fi
  local candidate
  for candidate in \
    "/c/Program Files (x86)/NSIS/makensis.exe" \
    "/c/Program Files/NSIS/makensis.exe"
  do
    if [[ -x "$candidate" ]]; then
      printf '%s\n' "$candidate"
      return 0
    fi
  done
  return 1
}

native_path() {
  # Backslashes: NSIS's File command treats a forward-slash drive path as options.
  # -a is required because cygpath -w keeps a relative path relative, and NSIS
  # resolves File/OutFile paths against the .nsi's directory (scripts/windows),
  # not the caller's cwd. With --out dist/windows that staged exe was looked up
  # at scripts/windows/dist/windows/SnapIt/SnapIt.exe and the build failed.
  if command -v cygpath >/dev/null 2>&1; then
    cygpath -aw "$1"
  else
    printf '%s' "$1"
  fi
}

makensis_flag() {
  case "$(uname -s 2>/dev/null || true)" in
    MINGW*|MSYS*|CYGWIN*) printf '/%s' "$1" ;;
    *) printf -- '-%s' "$1" ;;
  esac
}

if [[ -z "$TARGET" ]]; then
  if is_windows_host; then
    host="$(host_triple)"
    case "$host" in
      *-pc-windows-*) TARGET="$host" ;;
    esac
  fi
  TARGET="${TARGET:-x86_64-pc-windows-msvc}"
fi

if [[ -z "$BIN" ]]; then
  candidate="$(default_bin_path)"
  if [[ -f "$candidate" ]]; then
    BIN="$candidate"
  elif can_build_target; then
    if is_host_target; then
      extra=()
    else
      extra=(--target "$TARGET")
    fi
    if [[ "$BUILD_RELEASE" -eq 1 ]]; then
      extra+=(--release)
    fi
    if command -v cargo-xwin >/dev/null 2>&1 && ! is_windows_host; then
      cargo xwin build -p snapit "${extra[@]}"
    else
      cargo build -p snapit "${extra[@]}"
    fi
    BIN="$(default_bin_path)"
  else
    skip_windows
  fi
fi

if [[ ! -f "$BIN" ]]; then
  echo "not a Windows executable: $BIN" >&2
  exit 1
fi
if ! looks_like_windows_exe "$BIN"; then
  echo "not a Windows PE executable: $BIN" >&2
  exit 1
fi

BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
VERSION="$(workspace_version)"
KEY="$(windows_key)"
STAGE="$OUT_DIR/SnapIt"
rm -rf "$STAGE"
mkdir -p "$STAGE"
# NSIS resolves File paths against the .nsi directory, not our cwd, so the
# staged exe handed to -DBIN must be absolute (see also native_path below).
STAGE="$(cd "$STAGE" && pwd)"
cp "$BIN" "$STAGE/SnapIt.exe"

echo "bundled $STAGE/SnapIt.exe"

MAKENSIS_BIN="$(find_makensis || true)"
if [[ -z "$MAKENSIS_BIN" ]]; then
  echo "skip: Windows setup (makensis not found; install NSIS or set MAKENSIS)"
  echo "binary   $BIN"
  exit 0
fi

mkdir -p "$SETUP_OUT"
SETUP_OUT="$(cd "$SETUP_OUT" && pwd)"
SETUP="$SETUP_OUT/SnapIt-${VERSION}-${KEY}-setup.exe"
rm -f "$SETUP"
if is_windows_host; then
  export MSYS_NO_PATHCONV=1
fi
"$MAKENSIS_BIN" "$(makensis_flag V2)" \
  "$(makensis_flag "DVERSION=$VERSION")" \
  "$(makensis_flag "DVERSIONQUAD=$(version_quad "$VERSION")")" \
  "$(makensis_flag "DBIN=$(native_path "$STAGE/SnapIt.exe")")" \
  "$(makensis_flag "DICON=$(native_path "$ROOT/assets/snapit.ico")")" \
  "$(makensis_flag "DOUT=$(native_path "$SETUP")")" \
  "$(native_path "$ROOT/scripts/windows/snapit.nsi")"

echo "setup    $SETUP"
echo "binary   $BIN"
