#!/usr/bin/env bash
# Demonstrate the `.flow` code knowledge graph end-to-end on a temporary copy of a sample repo.
# Never modifies the source repository.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SAMPLE_SRC="${1:-/workspace/t3setup/h7/bench/runs/n4-tinydb-tmh}"

if [[ ! -d "$SAMPLE_SRC" ]]; then
  echo "error: sample repo directory not found: $SAMPLE_SRC" >&2
  exit 1
fi

TMP_BASE="$(mktemp -d /tmp/tm-flow-demo.XXXXXX)"
trap 'rm -rf "$TMP_BASE"' EXIT
TMP_REPO="$TMP_BASE/repo"
PLAN_JSON="$TMP_BASE/plan.json"

mkdir -p "$TMP_REPO"
# Copy sample repo without VCS/cache artifacts so the source repo is untouched.
tar -C "$SAMPLE_SRC" \
  --exclude='.git' \
  --exclude='.flow' \
  --exclude='.taskmaster' \
  --exclude='__pycache__' \
  --exclude='.pytest_cache' \
  --exclude='*.pyc' \
  -cf - . | tar -C "$TMP_REPO" -xf -

git -C "$TMP_REPO" init -q
git -C "$TMP_REPO" add -A
git -C "$TMP_REPO" -c user.name="tm-flow-demo" -c user.email="demo@taskmaster.local" commit -q -m "initial fixture"

CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-4}" \
  cargo build --manifest-path "$REPO_ROOT/Cargo.toml" -p tm-flow --bin tm-flow --quiet
BIN="$REPO_ROOT/target/debug/tm-flow"

# Seed a realistic Taskmaster plan matching the tinydb module split (or generic if another repo).
if [[ -f "$TMP_REPO/tinydb/executor.py" ]]; then
  cat > "$PLAN_JSON" <<'EOF'
{
  "kind": "cli",
  "summary": "tinydb demo plan",
  "contracts": [
    { "path": "tinydb/contract.py", "content": "" },
    { "path": "tinydb/ast_contract.py", "content": "" },
    { "path": "tests/test_acceptance_contract.py", "content": "" }
  ],
  "pages": [
    { "id": "tokenizer", "owned_glob": "{tinydb/tokenizer.py,tests/test_tokenizer.py}", "brief": "SQL tokenizer" },
    { "id": "parser", "owned_glob": "{tinydb/parser.py,tests/test_parser.py}", "brief": "SQL parser", "depends_on": ["tokenizer"] },
    { "id": "btree", "owned_glob": "{tinydb/btree.py,tests/test_btree.py}", "brief": "B-tree index" },
    { "id": "storage", "owned_glob": "{tinydb/storage.py,tests/test_storage.py}", "brief": "Page storage", "depends_on": ["btree"] },
    { "id": "planner", "owned_glob": "tinydb/planner.py", "brief": "Query planner" },
    { "id": "executor", "owned_glob": "{tinydb/executor.py,tests/test_executor.py}", "brief": "SQL executor", "depends_on": ["planner", "storage"] },
    { "id": "cli", "owned_glob": "{tinydb/__main__.py,tinydb/__init__.py,tests/test_cli.py}", "brief": "CLI entrypoint", "depends_on": ["tokenizer", "parser", "storage", "executor"] }
  ],
  "estimate": { "tokens": 0, "usd": 0.0 },
  "dictionary": []
}
EOF
fi

echo "=== 0. Build, seed, and verify .flow on temp copy ($TMP_REPO) ==="
"$BIN" build "$TMP_REPO"
if [[ -f "$PLAN_JSON" ]]; then
  "$BIN" seed "$TMP_REPO" "$PLAN_JSON"
fi
echo -n "verify_all: "
"$BIN" verify "$TMP_REPO"
echo

SHARD_PATH="$TMP_REPO/.flow/files/tinydb/planner.py.json"
if [[ ! -f "$SHARD_PATH" ]]; then
  SHARD_PATH="$(find "$TMP_REPO/.flow/files" -name '*.json' | sort | head -n 1)"
fi

echo "=== 1. Sample shard (${SHARD_PATH#"$TMP_REPO/"}) ==="
cat "$SHARD_PATH"
echo

echo "=== 2. flow_query: find ==="
"$BIN" query "$TMP_REPO" find '{"query":"Executor","limit":12}'
echo

NODE_ID="py:tinydb/executor.py#Executor"
SOURCE_ID="py:tinydb/planner.py#Planner.plan_select"
if [[ ! -f "$TMP_REPO/tinydb/executor.py" ]]; then
  NODE_ID="$(python3 -c 'import json,sys; d=json.load(sys.stdin); print(d["nodes"][0]["id"])' < "$SHARD_PATH")"
  SOURCE_ID="$NODE_ID"
fi

echo "=== 3. flow_query: node ($NODE_ID) ==="
"$BIN" query "$TMP_REPO" node "{\"id\":\"$NODE_ID\"}"
echo

echo "=== 4. flow_query: neighbors ($NODE_ID, k=1, dir=both) ==="
"$BIN" query "$TMP_REPO" neighbors "{\"id\":\"$NODE_ID\",\"k\":1,\"dir\":\"both\",\"limit\":25}"
echo

echo "=== 5. flow_query: source ($SOURCE_ID) ==="
"$BIN" query "$TMP_REPO" source "{\"id\":\"$SOURCE_ID\"}"
echo

UNIT="executor"
echo "=== 6. Unit map ($UNIT) ==="
MAP_OUT="$("$BIN" unit-map "$TMP_REPO" "$UNIT" --budget 6000)"
printf '%s\n' "$MAP_OUT"
CHARS="${#MAP_OUT}"
TOKENS=$(( (CHARS + 3) / 4 ))
echo "---"
echo "unit_map($UNIT) size: ${CHARS} chars (~${TOKENS} tokens at chars/4)"
