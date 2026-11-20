#!/bin/sh
# Install Taskmaster from an unpacked release tarball. Run it from inside that folder:
#   ./install.sh                     binaries in ~/.local/bin
#   ./install.sh ~/bin               binaries in ~/bin
#   ./install.sh --prefix /opt/tm    binaries in /opt/tm/bin
#   ./install.sh --no-path           do not touch shell startup files
#
# The app goes to $XDG_DATA_HOME/taskmaster/web (usually ~/.local/share/taskmaster/web),
# where `tm-daemon serve` looks when --web is not given. When the bin folder is not on PATH,
# one marked block is added to ~/.profile (and ~/.bashrc and ~/.zshrc when they exist).
# Every change is printed, and listed in <data>/taskmaster/installed.txt for uninstall.sh.
set -eu

here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
bindir=
touch_path=yes
while [ $# -gt 0 ]; do
  case $1 in
    --prefix) bindir=${2:?--prefix needs a folder}/bin; shift 2 ;;
    --no-path) touch_path=no; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    -*) echo "install.sh: unknown option $1" >&2; exit 2 ;;
    *) bindir=$1; shift ;;
  esac
done
: "${HOME:?set HOME, or pass a bin folder}"
bindir=${bindir:-$HOME/.local/bin}

if [ ! -f "$here/web/index.html" ]; then
  echo "install.sh: web/index.html is missing. Unpack the release and run this script from inside it." >&2
  exit 1
fi
for bin in tm-daemon tm-review tm-commons; do
  [ -f "$here/$bin" ] || { echo "install.sh: $bin is missing from $here" >&2; exit 1; }
done

data="${XDG_DATA_HOME:-$HOME/.local/share}/taskmaster"
web="$data/web"
manifest="$data/installed.txt"
mkdir -p "$bindir" "$data"
: > "$manifest.new"

for bin in tm-daemon tm-review tm-commons; do
  install -m 755 "$here/$bin" "$bindir/$bin"
  echo "bin $bindir/$bin" >> "$manifest.new"
  echo "  added    $bindir/$bin"
done
# `taskmaster` is tm-daemon by its product name: `taskmaster report`, `taskmaster serve .`.
ln -sf tm-daemon "$bindir/taskmaster"
echo "bin $bindir/taskmaster" >> "$manifest.new"
echo "  added    $bindir/taskmaster (the same program as tm-daemon)"

rm -rf "$web.new"
cp -R "$here/web" "$web.new"
rm -rf "$web"
mv "$web.new" "$web"
echo "web $web" >> "$manifest.new"
echo "  added    $web/ (the app; tm-daemon serve finds it here)"

if [ -f "$here/uninstall.sh" ]; then
  install -m 755 "$here/uninstall.sh" "$data/uninstall.sh"
  echo "  added    $data/uninstall.sh"
fi

# PATH: one marked block, so uninstall.sh can take it out again.
begin="# >>> taskmaster >>>"
end="# <<< taskmaster <<<"
on_path=no
case ":${PATH:-}:" in *":$bindir:"*) on_path=yes ;; esac
if [ "$touch_path" = yes ] && [ "$on_path" = no ]; then
  for rc in "$HOME/.profile" "$HOME/.bashrc" "$HOME/.zshrc"; do
    # ~/.profile is always written; the others only when the shell already uses them.
    [ "$rc" = "$HOME/.profile" ] || [ -f "$rc" ] || continue
    if [ -f "$rc" ] && grep -qF "$begin" "$rc"; then
      echo "rc $rc" >> "$manifest.new"
      echo "  kept     $rc (it already puts Taskmaster on PATH)"
      continue
    fi
    printf '\n%s\ncase ":$PATH:" in *":%s:"*) ;; *) export PATH="%s:$PATH" ;; esac\n%s\n' "$begin" "$bindir" "$bindir" "$end" >> "$rc"
    echo "rc $rc" >> "$manifest.new"
    echo "  changed  $rc (adds $bindir to PATH)"
  done
elif [ "$on_path" = no ]; then
  echo "  skipped  PATH: $bindir is not on PATH; add it yourself"
else
  echo "  kept     PATH ($bindir is already on it)"
fi

mv "$manifest.new" "$manifest"
echo "  added    $manifest (what uninstall.sh removes)"
echo
echo "Taskmaster is installed."
if [ "$on_path" = no ] && [ "$touch_path" = yes ]; then
  echo "Open a new terminal, or run: export PATH=\"$bindir:\$PATH\""
fi
echo "Start it:   taskmaster serve <project>     (the app is at http://127.0.0.1:8737)"
echo "Remove it:  $data/uninstall.sh"
