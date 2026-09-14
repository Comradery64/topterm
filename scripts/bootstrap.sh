#!/usr/bin/env bash
# Checks (and optionally installs) everything needed to build topterm.
# Usage: scripts/bootstrap.sh [--install]
set -euo pipefail

INSTALL=0
[[ "${1:-}" == "--install" ]] && INSTALL=1

fail=0
note() { printf '  %s\n' "$1"; }
ok()   { printf '\033[32m✓\033[0m %s\n' "$1"; }
bad()  { printf '\033[31m✗\033[0m %s\n' "$1"; fail=1; }

echo "topterm dependency check"
echo "-------------------------"

# --- platform ---------------------------------------------------------
if [[ "$(uname -s)" == "Darwin" ]]; then
  ok "macOS ($(sw_vers -productVersion 2>/dev/null || echo unknown))"
else
  bad "macOS required — topterm's collector uses Darwin-only ps/lsof/sysctl calls"
fi

# --- Xcode Command Line Tools (needed to link) -------------------------
if xcode-select -p >/dev/null 2>&1; then
  ok "Xcode Command Line Tools"
else
  bad "Xcode Command Line Tools not found"
  note "install with: xcode-select --install"
fi

# --- Rust toolchain ------------------------------------------------------
if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
  ok "Rust toolchain ($(rustc --version))"
else
  bad "cargo/rustc not found"
  if [[ "$INSTALL" == "1" ]]; then
    note "installing via rustup..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1090
    source "$HOME/.cargo/env"
    ok "Rust toolchain ($(rustc --version))"
    fail=0
  else
    note "install with: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    note "or re-run this script with --install"
  fi
fi

echo "-------------------------"
if [[ "$fail" == "0" ]]; then
  echo "All set — run: cargo build --release"
else
  echo "Fix the items above, then re-run this script."
  exit 1
fi
