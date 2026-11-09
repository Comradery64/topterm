#!/usr/bin/env bash
# Interactive onboarding wizard for topterm: checks your system, then walks
# you through installing whatever's missing and getting the binary built.
#
# Run it with a single command, from a clone:
#   ./scripts/onboard.sh
# or straight off GitHub:
#   curl -fsSL https://raw.githubusercontent.com/Comradery64/topterm/main/scripts/onboard.sh | bash
set -uo pipefail

# curl | bash pipes the script itself into stdin, so all interactive reads
# below explicitly go to /dev/tty rather than stdin.
TTY=/dev/tty
if : 2>/dev/null < "$TTY"; then
  INTERACTIVE=1
else
  INTERACTIVE=0
fi

REPO_URL="https://github.com/Comradery64/topterm"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-.}")" 2>/dev/null && pwd || true)"

# Non-interactive escape hatch: pass one of these to skip the menus entirely
# (for CI, scripts, or a piped `curl | bash` with no controlling terminal).
ACTION=""
case "${1:-}" in
  --build-link|--install) ACTION=build_and_link ;;
  --build)                ACTION=build_only ;;
  --test)                 ACTION=run_tests ;;
  --demo)                 ACTION=run_demo ;;
  --install-rust)         ACTION=install_rust ;;
  --uninstall-rust)       ACTION=uninstall_rust ;;
  --reinstall)            ACTION=reinstall_topterm ;;
  --uninstall)            ACTION=uninstall_topterm ;;
  "") ;;
  *) echo "unknown flag: $1" >&2; exit 1 ;;
esac

bold()  { printf '\033[1m%s\033[0m' "$1"; }
dim()   { printf '\033[2m%s\033[0m' "$1"; }
ok()    { printf '  \033[32m✓\033[0m %s\n' "$1"; }
bad()   { printf '  \033[31m✗\033[0m %s\n' "$1"; }
info()  { printf '  \033[36m→\033[0m %s\n' "$1"; }

banner() {
  echo
  bold "topterm onboarding"
  echo "  — find and kill the terminal tab leaking memory"
  echo
}

# --- system checks, updates HAVE_MACOS / HAVE_CLT / HAVE_RUST -------------
HAVE_MACOS=0 HAVE_CLT=0 HAVE_RUST=0 HAVE_LOCAL_BIN_ON_PATH=0

run_checks() {
  echo "$(bold "System checks")"
  if [[ "$(uname -s)" == "Darwin" ]]; then
    ok "macOS ($(sw_vers -productVersion 2>/dev/null || echo unknown))"
    HAVE_MACOS=1
  else
    bad "macOS required — topterm's collector uses Darwin-only ps/lsof/sysctl calls"
  fi

  if xcode-select -p >/dev/null 2>&1; then
    ok "Xcode Command Line Tools"
    HAVE_CLT=1
  else
    bad "Xcode Command Line Tools not found (install with: xcode-select --install)"
  fi

  if command -v cargo >/dev/null 2>&1 && command -v rustc >/dev/null 2>&1; then
    ok "Rust toolchain ($(rustc --version))"
    HAVE_RUST=1
  else
    bad "Rust toolchain not found"
  fi

  case ":$PATH:" in
    *":$HOME/.local/bin:"*) HAVE_LOCAL_BIN_ON_PATH=1 ;;
  esac
  echo
}

# --- a small numbered menu (arrow keys work too) --------------------------
# Prints options. A digit key confirms that option immediately (no Enter
# needed); Up/Down move the highlight and Enter confirms it. Echoes the
# chosen index into $MENU_CHOICE (0-based).
menu() {
  local prompt="$1"; shift
  local -a options=("$@")
  local n="${#options[@]}"
  local selected=0
  MENU_CHOICE=0

  if [[ "$INTERACTIVE" == "0" ]]; then
    # No tty (e.g. piped into a non-interactive shell) and no --flag matched
    # what's needed at this point — can't prompt, so stop instead of guessing.
    printf '%s\n' "$prompt"
    echo
    bad "no interactive terminal detected"
    info "re-run with an explicit flag instead, e.g.:"
    info "  ./scripts/onboard.sh --install-rust"
    info "  ./scripts/onboard.sh --build-link   (build + link to ~/.local/bin)"
    info "  ./scripts/onboard.sh --build        (build only)"
    info "  ./scripts/onboard.sh --test"
    info "  ./scripts/onboard.sh --demo"
    info "  ./scripts/onboard.sh --reinstall"
    info "  ./scripts/onboard.sh --uninstall"
    info "  ./scripts/onboard.sh --uninstall-rust"
    exit 1
  fi

  # A digit key confirms instantly (see below), so a trailing Enter typed
  # out of habit is left buffered — drain it before the next menu starts, or
  # it gets misread as an Enter-confirms-default on that menu. Only needed
  # right after a digit confirm (arrow+Enter never leaves anything behind),
  # so plain arrow-key navigation stays instant with no drain at all. macOS
  # ships bash 3.2, which only accepts integer -t values, hence -t 1 — it
  # returns as soon as the buffered byte is seen, so this rarely blocks.
  if [[ "${NEEDS_DRAIN:-0}" == "1" ]]; then
    local _junk
    while read -rsn1 -t 1 _junk 2>/dev/null < "$TTY"; do :; done
    NEEDS_DRAIN=0
  fi

  local draw redraw=0
  draw() {
    if [[ "$redraw" == "1" ]]; then
      printf '\033[%dA' "$((n + 1))" > "$TTY"
    fi
    printf '%s\n' "$prompt" > "$TTY"
    local i
    for i in "${!options[@]}"; do
      if [[ "$i" == "$selected" ]]; then
        printf '  \033[36m❯ %d) %s\033[0m\033[K\n' "$((i + 1))" "${options[$i]}" > "$TTY"
      else
        printf '    %d) %s\033[K\n' "$((i + 1))" "${options[$i]}" > "$TTY"
      fi
    done
    redraw=1
  }

  draw
  while true; do
    local key
    if ! IFS= read -rsn1 key < "$TTY"; then
      printf '\n' > "$TTY"
      bad "lost the terminal while waiting for a keypress (EOF on /dev/tty)" > "$TTY"
      info "re-run with an explicit flag instead, e.g. ./scripts/onboard.sh --build-link" > "$TTY"
      exit 1
    fi
    if [[ "$key" == $'\x1b' ]]; then
      local rest
      IFS= read -rsn2 -t 1 rest < "$TTY" || true
      case "$rest" in
        '[A') ((selected--)); ((selected < 0)) && selected=$((n - 1)); draw ;;
        '[B') ((selected++)); ((selected >= n)) && selected=0; draw ;;
      esac
    elif [[ -z "$key" ]]; then
      break
    elif [[ "$key" =~ ^[0-9]$ ]] && (( key >= 1 && key <= n )); then
      selected=$((key - 1))
      draw
      NEEDS_DRAIN=1
      break
    fi
  done
  MENU_CHOICE="$selected"
}

install_clt() {
  info "opening the Xcode Command Line Tools installer..."
  xcode-select --install
  info "finish that install, then re-run: ./scripts/onboard.sh"
}

install_rust() {
  info "installing Rust via rustup (official https://rustup.rs installer)..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  # shellcheck disable=SC1090
  source "$HOME/.cargo/env"
  HAVE_RUST=1
  ok "Rust toolchain ($(rustc --version))"
}

uninstall_rust() {
  if ! command -v rustup >/dev/null 2>&1; then
    info "rustup not found — nothing to uninstall"
    return
  fi
  info "uninstalling Rust via rustup self uninstall..."
  rustup self uninstall -y
  HAVE_RUST=0
  ok "Rust toolchain removed"
}

ensure_repo() {
  # Onboarding may run via curl|bash outside a clone — grab one if needed.
  if [[ -n "$SCRIPT_DIR" && -f "$SCRIPT_DIR/../Cargo.toml" ]]; then
    cd "$SCRIPT_DIR/.."
    return
  fi
  info "cloning $REPO_URL..."
  git clone --depth 1 "$REPO_URL" topterm
  cd topterm
}

build_and_link() {
  ensure_repo
  info "building release binary (this takes ~20-30s the first time)..."
  cargo build --release --quiet
  mkdir -p "$HOME/.local/bin"
  ln -sf "$PWD/target/release/topterm" "$HOME/.local/bin/topterm"
  ok "built and linked to ~/.local/bin/topterm"
  if [[ "$HAVE_LOCAL_BIN_ON_PATH" == "0" ]]; then
    echo
    bad "~/.local/bin isn't on your PATH yet"
    info 'add this to your shell profile: export PATH="$HOME/.local/bin:$PATH"'
  else
    echo
    info "run it: topterm        (one-shot table)"
    info "        topterm -t     (interactive TUI)"
  fi
}

build_only() {
  ensure_repo
  info "building release binary..."
  cargo build --release --quiet
  ok "built: $PWD/target/release/topterm"
}

reinstall_topterm() {
  ensure_repo
  info "removing previous build artifacts..."
  cargo clean --release --quiet
  info "rebuilding from scratch (slower than a normal build)..."
  cargo build --release --quiet
  mkdir -p "$HOME/.local/bin"
  ln -sf "$PWD/target/release/topterm" "$HOME/.local/bin/topterm"
  ok "reinstalled: ~/.local/bin/topterm"
}

uninstall_topterm() {
  local removed=0
  if [[ -e "$HOME/.local/bin/topterm" || -L "$HOME/.local/bin/topterm" ]]; then
    rm -f "$HOME/.local/bin/topterm"
    ok "removed ~/.local/bin/topterm"
    removed=1
  fi
  if [[ -e "$HOME/.cargo/bin/topterm" ]] && command -v cargo >/dev/null 2>&1; then
    if cargo uninstall topterm >/dev/null 2>&1; then
      ok "removed ~/.cargo/bin/topterm (cargo uninstall)"
      removed=1
    fi
  fi
  if [[ "$removed" == "0" ]]; then
    info "nothing to remove — topterm isn't linked at ~/.local/bin or installed via cargo install"
  fi
}

run_tests() {
  ensure_repo
  info "running cargo test..."
  cargo test --quiet
}

run_demo() {
  ensure_repo
  if [[ ! -x target/release/topterm ]]; then
    info "no build yet — building first..."
    cargo build --release --quiet
  fi
  ./target/release/topterm --fake-data
}

# Runs exactly one action, then exits — a wizard walks you through a job
# and stops, it doesn't loop back and ask you to redo it.
main_menu() {
  echo
  menu "$(bold "What next?")" \
    "Build topterm and link it onto my PATH (recommended)" \
    "Just build (leave the binary in target/release)" \
    "Run the test suite" \
    "Preview the one-shot table with fake data (no build required first)" \
    "Reinstall (clean rebuild + relink)" \
    "Uninstall (remove the linked binary)" \
    "Exit"
  case "$MENU_CHOICE" in
    0) build_and_link ;;
    1) build_only ;;
    2) run_tests ;;
    3) run_demo ;;
    4) reinstall_topterm ;;
    5) uninstall_topterm ;;
    6) echo; info "see you around." ;;
  esac
}

banner
run_checks

if [[ "$HAVE_MACOS" == "0" ]]; then
  bad "topterm only runs on macOS — nothing more this script can do here."
  exit 1
fi

# --- non-interactive / flag-driven path: run one action and exit ---------
if [[ -n "$ACTION" ]]; then
  case "$ACTION" in
    install_rust|uninstall_rust|uninstall_topterm) ;;
    *)
      if [[ "$HAVE_CLT" == "0" ]]; then
        bad "Xcode Command Line Tools not found — run: xcode-select --install"
        exit 1
      fi
      if [[ "$HAVE_RUST" == "0" ]]; then
        bad "Rust not found — run: ./scripts/onboard.sh --install-rust"
        exit 1
      fi
      ;;
  esac
  "$ACTION"
  exit 0
fi

if [[ "$HAVE_CLT" == "0" ]]; then
  menu "$(bold "Xcode Command Line Tools are missing.") What next?" \
    "Install them now" \
    "Skip (I'll install manually)" \
    "Exit"
  case "$MENU_CHOICE" in
    0) install_clt; exit 0 ;;
    1) : ;;
    2) exit 0 ;;
  esac
fi

if [[ "$HAVE_RUST" == "0" ]]; then
  menu "$(bold "Rust isn't installed.") What next?" \
    "Install it now via rustup (recommended)" \
    "Show the manual install command and exit" \
    "Exit"
  case "$MENU_CHOICE" in
    0) install_rust ;;
    1) echo; info 'curl --proto '"'"'=https'"'"' --tlsv1.2 -sSf https://sh.rustup.rs | sh'; exit 0 ;;
    2) exit 0 ;;
  esac
fi

main_menu
