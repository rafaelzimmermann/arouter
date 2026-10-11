#!/usr/bin/env bash
# arouter installer — build + install the binary, then hand off to
# `arouter init` for config/systemd/pi-integration (see docs/).
#
#   ./install.sh                 # release build → ~/.local/bin/arouter
#   PREFIX=... ./install.sh      # custom install dir (e.g. /usr/local/bin)
#   ./install.sh --skip-init     # binary only, no `arouter init` prompt
#
# Requires: rust toolchain (cargo), a C linker. Idempotent: re-running
# upgrades in place; `arouter init` is also safe to re-run (it backs up
# pi's models.json and skips already-installed pieces).
set -euo pipefail

cd "$(dirname "$0")"

PREFIX_DIR="${PREFIX:-$HOME/.local/bin}"
SKIP_INIT=0
[[ "${1:-}" == "--skip-init" ]] && SKIP_INIT=1

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

command -v cargo >/dev/null 2>&1 || die "cargo not found — install rust: https://rustup.rs"

say "building release binary (first build takes a few minutes)"
cargo build --release --locked

mkdir -p "$PREFIX_DIR"
BIN="$PREFIX_DIR/arouter"
install -m 0755 target/release/arouter "$BIN"
say "installed $BIN ($("$BIN" --version))"

# ~/.local/bin is often not on PATH by default
case ":$PATH:" in
  *":$PREFIX_DIR:"*) ;;
  *)
    say "note: $PREFIX_DIR is not on your PATH"
    echo "    add:  export PATH=\"\$PATH:$PREFIX_DIR\"   (e.g. in ~/.bashrc)"
    ;;
esac

if [[ $SKIP_INIT -eq 1 ]]; then
  say "done. next: $BIN init   (config + systemd unit + pi models.json)"
  exit 0
fi

say "running 'arouter init' (config, systemd user unit, pi integration)"
if "$BIN" init; then
  say "done. start with:  systemctl --user start arouter   then:  arouter monitor"
else
  say "init declined or failed — run '$BIN init' manually when ready"
fi
