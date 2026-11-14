#!/usr/bin/env bash
# Dispatch platform bundlers: macos | windows | all.
# Does not change application behavior.
set -euo pipefail

usage() {
  cat <<'EOF'
Usage: scripts/bundle.sh macos|windows|all [bundler-args...]

  macos     scripts/bundle-macos.sh → target/macos/SnapIt.app
  windows   scripts/bundle-windows.sh → dist/SnapIt-<version>-windows-<arch>-setup.exe
  all       run macos, then windows (a missing macOS bundler is skipped)

Extra arguments are forwarded to the chosen bundler. `all` does not forward args.
Windows prints a skip message and exits 0 when the toolchain or NSIS is missing.
EOF
}

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

run_script() {
  local script="$1"
  shift
  bash "$script" "$@"
}

run_macos() {
  local script="$ROOT/scripts/bundle-macos.sh"
  if [[ -f "$script" ]]; then
    run_script "$script" "$@"
  else
    echo "macOS bundler not in tree yet"
  fi
}

cmd="${1:-}"
if [[ $# -gt 0 ]]; then
  shift
fi

case "$cmd" in
  macos)
    run_macos "$@"
    ;;
  windows)
    run_script "$ROOT/scripts/bundle-windows.sh" "$@"
    ;;
  all)
    echo "== macOS =="
    run_macos
    echo "== Windows =="
    run_script "$ROOT/scripts/bundle-windows.sh"
    ;;
  -h|--help|"")
    usage
    if [[ -z "$cmd" ]]; then
      exit 1
    fi
    ;;
  *)
    echo "unknown platform: $cmd" >&2
    usage >&2
    exit 1
    ;;
esac
