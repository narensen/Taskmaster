#!/bin/sh
# Build Taskmaster and put `tm-daemon` and `tm-review` in a directory on your PATH.
#   scripts/install.sh [dir]        default: ~/.local/bin
set -eu

dir="${1:-$HOME/.local/bin}"
cd "$(dirname "$0")/.."

command -v cargo >/dev/null || { echo "cargo is not installed: https://rustup.rs" >&2; exit 1; }
command -v git >/dev/null || { echo "git is not installed" >&2; exit 1; }
# Antigravity, Grok Build and Opencode are confined with bubblewrap.
command -v bwrap >/dev/null || echo "warning: bubblewrap (bwrap) is not installed; only the claude harness will run" >&2

cargo build --release --locked -p tm-daemon -p tm-review
mkdir -p "$dir"
install -m 755 target/release/tm-daemon target/release/tm-review "$dir/"
echo "installed tm-daemon and tm-review in $dir"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "add $dir to your PATH to run them by name" ;;
esac
"$dir/tm-daemon" providers | grep -E '"(id|installed)"' | paste - - | sed 's/[",]//g; s/  */ /g'
