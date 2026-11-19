#!/bin/sh
# Remove what install.sh put on this machine, and nothing else:
#   uninstall.sh            the binaries, the app, and the PATH lines install.sh added
#   uninstall.sh --purge    also ~/.taskmaster (settings, paired devices, host tokens)
#                           and the daemon log
# Projects, and the .taskmaster folder inside each project, are never touched.
# Every change is printed.
set -eu

purge=no
for arg in "$@"; do
  case $arg in
    --purge) purge=yes ;;
    -h|--help) sed -n '2,7p' "$0"; exit 0 ;;
    *) echo "uninstall.sh: unknown option $arg" >&2; exit 2 ;;
  esac
done
: "${HOME:?set HOME}"
data="${XDG_DATA_HOME:-$HOME/.local/share}/taskmaster"
manifest="$data/installed.txt"
begin="# >>> taskmaster >>>"
end="# <<< taskmaster <<<"

# Take the marked PATH block out of one shell startup file.
unpath() {
  [ -f "$1" ] && grep -qF "$begin" "$1" || return 0
  tmp="$1.taskmaster-uninstall"
  # Drop the block, and the blank line install.sh put before it.
  awk -v b="$begin" -v e="$end" '
    $0 == b { skip = 1; if (have && prev == "") have = 0; next }
    skip { if ($0 == e) skip = 0; next }
    { if (have) print prev; prev = $0; have = 1 }
    END { if (have) print prev }
  ' "$1" > "$tmp"
  cat "$tmp" > "$1" && rm -f "$tmp"
  echo "  changed  $1 (removed the Taskmaster PATH lines)"
}

rcs="$HOME/.profile $HOME/.bashrc $HOME/.zshrc"
if [ -f "$manifest" ]; then
  while read -r kind path; do
    case $kind in
      bin) if [ -e "$path" ]; then rm -f "$path"; echo "  removed  $path"; fi ;;
      rc) case " $rcs " in *" $path "*) ;; *) rcs="$rcs $path" ;; esac ;;
    esac
  done < "$manifest"
else
  # No list: an install from before uninstall.sh existed. Look in the usual place.
  for bin in tm-daemon tm-review tm-commons; do
    path="$HOME/.local/bin/$bin"
    if [ -e "$path" ]; then rm -f "$path"; echo "  removed  $path"; fi
  done
fi
for rc in $rcs; do unpath "$rc"; done

if [ -d "$data" ]; then
  rm -rf "$data"
  echo "  removed  $data/ (the app, and runtimes Taskmaster installed)"
fi

state="${XDG_STATE_HOME:-$HOME/.local/state}/taskmaster"
if [ "$purge" = yes ]; then
  for dir in "$HOME/.taskmaster" "$state"; do
    if [ -d "$dir" ]; then rm -rf "$dir"; echo "  removed  $dir/"; fi
  done
else
  for dir in "$HOME/.taskmaster" "$state"; do
    if [ -d "$dir" ]; then echo "  kept     $dir/ (run with --purge to remove it)"; fi
  done
fi
echo
echo "Taskmaster is uninstalled. Your projects were not touched."
