#!/usr/bin/env bash
# Build one locked plan in fresh projects under different settings, and record wall time
# and the four-way token totals of each build.
#
#   scripts/bench.sh plan  <name> <prompt-file> [tm-daemon role flags]   plan once: bench/<name>/plan.json
#   scripts/bench.sh build <name> <label> [--checker] [--code-mode] [tm-daemon flags]   one build of that plan
#   scripts/bench.sh single <name> <harness> [model]   the baseline: one agent builds the whole prompt
#
# Each build appends a line to bench/results.jsonl:
#   {"name","label","seconds","ok","calls","input","output","cache_read","cache_write","cost_usd"}
# Planning is left out of the timing, so variants are compared on the same plan.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
daemon=${TM_DAEMON:-$root/target/release/tm-daemon}
[ -x "$daemon" ] || { echo "build it first: cargo build --release -p tm-daemon" >&2; exit 1; }
cmd=$1 name=$2; shift 2
dir=$root/bench/$name
fresh() {
  local p; p=$(mktemp -d "${TMPDIR:-/tmp}/tm-bench-XXXXXX")
  git -C "$p" init -q -b main
  git -C "$p" -c user.email=bench@example.invalid -c user.name=bench commit -q --allow-empty -m start
  echo "$p"
}
case $cmd in
  plan)
    prompt=$1; shift
    mkdir -p "$dir"
    p=$(fresh)
    "$daemon" "$p" --prompt-file "$prompt" --commons "$p/.commons" "$@" >/dev/null 2>"$dir/plan.log" || true
    cp "$p/.taskmaster/runs/current/plan.json" "$dir/plan.json"
    cp "$prompt" "$dir/prompt.txt"
    printf '%s\n' "$@" > "$dir/roles"
    echo "planned: $dir/plan.json"
    ;;
  build)
    label=$1; shift
    checker=false code=false
    while :; do
      case ${1:-} in
        --checker) checker=true; shift ;;
        --code-mode) code=true; shift ;;
        *) break ;;
      esac
    done
    mapfile -t roles < "$dir/roles"
    p=$(fresh)
    mkdir -p "$p/.taskmaster"
    printf '{"checker":{"enabled":%s},"code_mode":{"enabled":%s}}\n' "$checker" "$code" > "$p/.taskmaster/config.json"
    start=$(date +%s.%N)
    ok=true
    "$daemon" "$p" --plan "$dir/plan.json" --yes --commons "$p/.commons" "${roles[@]}" "$@" >"$dir/$label.out" 2>"$dir/$label.log" || ok=false
    end=$(date +%s.%N)
    python3 - "$p/.taskmaster/runs/current/ledger.jsonl" "$name" "$label" "$start" "$end" "$ok" >> "$root/bench/results.jsonl" <<'PY'
import json, sys
path, name, label, start, end, ok = sys.argv[1:]
rows = [json.loads(l) for l in open(path) if l.strip()] if __import__("os").path.exists(path) else []
sum_ = lambda k: sum(r.get(k, 0) for r in rows)
print(json.dumps({"name": name, "label": label, "seconds": round(float(end) - float(start), 1), "ok": ok == "true",
  "calls": len(rows), "input": sum_("input"), "output": sum_("output"), "cache_read": sum_("cache_read"),
  "cache_write": sum_("cache_write"), "cost_usd": round(sum(r.get("cost_usd") or 0 for r in rows), 4)}))
PY
    tail -n 1 "$root/bench/results.jsonl"
    ;;
  single)
    harness=$1; shift
    single=$root/target/release/examples/single_agent
    [ -x "$single" ] || { echo "build it first: cargo build --release -p tm-harness --example single_agent" >&2; exit 1; }
    p=$(fresh)
    line=$("$single" "$harness" "$p" "$dir/prompt.txt" "$@" 2>"$dir/single-$harness.log")
    files=$(git -C "$p" ls-files --others --exclude-standard | wc -l)
    python3 -c 'import json,sys; r=json.loads(sys.argv[1]); r.update(name=sys.argv[2], label="single agent ("+sys.argv[3]+")", files=int(sys.argv[4])); r.pop("error", None) if r.get("error") is None else None; print(json.dumps(r))' "$line" "$name" "$harness" "$files" >> "$root/bench/results.jsonl"
    tail -n 1 "$root/bench/results.jsonl"
    ;;
  *) echo "usage: bench.sh plan|build|single ..." >&2; exit 2 ;;
esac
