#!/bin/sh
# Install the binaries and the web app from an unpacked Taskmaster release tarball.
# Run this script from inside the unpacked directory:
#   ./install.sh
#   ./install.sh ~/.local/bin
#   ./install.sh --prefix /opt/taskmaster
#
# Binaries go to ~/.local/bin, or to the directory you pass, or to <prefix>/bin.
# The app goes to $XDG_DATA_HOME/taskmaster/web, or ~/.local/share/taskmaster/web,
# which is where `tm-daemon serve` looks when --web is not given.
set -eu

here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)

if [ "${1:-}" = "--prefix" ]; then
  prefix=${2:?--prefix needs a directory}
  bindir="$prefix/bin"
elif [ -n "${1:-}" ]; then
  bindir=$1
else
  bindir=${HOME:?set HOME, or pass a bin directory}/.local/bin
fi

if [ ! -f "$here/web/index.html" ]; then
  echo "install.sh: web/index.html is missing. Unpack the release and run this script from inside it." >&2
  exit 1
fi
for bin in tm-daemon tm-review tm-commons; do
  if [ ! -x "$here/$bin" ] && [ ! -f "$here/$bin" ]; then
    echo "install.sh: $bin is missing" >&2
    exit 1
  fi
done

mkdir -p "$bindir"
install -m 755 "$here/tm-daemon" "$here/tm-review" "$here/tm-commons" "$bindir/"

web="${XDG_DATA_HOME:-$HOME/.local/share}/taskmaster/web"
mkdir -p "$(dirname "$web")"
rm -rf "$web.new"
cp -R "$here/web" "$web.new"
rm -rf "$web"
mv "$web.new" "$web"

echo "installed tm-daemon, tm-review, and tm-commons in $bindir"
echo "installed the app in $web"
case ":${PATH}:" in
  *":$bindir:"*) ;;
  *) echo "add $bindir to PATH" ;;
esac
echo "next: tm-daemon serve <project>"
