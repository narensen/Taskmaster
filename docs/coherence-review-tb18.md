# TB-18 Implementation Coherence Review

Post-implementation coherence audit across all TB-18 commits (`e37c4f9..HEAD` on branch `feat/next`), reviewing every touched crate (`tm-daemon`, `tm-flow`, `tm-harness`, `tm-review`), CLI/API/settings wiring, scripts, and documentation.

---

## 1. Scope Reviewed

| Item | Commit | Summary | Primary Files |
| --- | --- | --- | --- |
| **#1** | `090469e` | Honor `test(paths)` arguments and filter `testAffected` execution while preserving full-suite bare `test()`. | `crates/tm-daemon/src/{runtime,code}.rs` |
| **#2** | `847c766` | Race-safe atomic live sibling worktree sync (`WorktreeSync` + `sync_worktree_excluding`) and tightened `dependency_is_contract_only` for integrators without covering contracts. | `crates/tm-daemon/src/{git,code,gates,engine,tests}.rs` |
| **#3** | `96bc275` | Reuse existing Python venv (`!python_bin.is_file()`), hash only dependency-affecting `pyproject.toml` fields, and keep RO shared venv with `PYTHONPATH=.` for stdlib packages without console scripts. | `crates/tm-daemon/src/{runtime,code,engine}.rs` |
| **#4** | `ddc2980` | Relax Code Mode Call 1 one-shot write+test prompt rule to 1–2 batched write calls ending in targeted or full `test()`; no mid-turn silence nudge. | `crates/tm-daemon/src/{engine,code,tests}.rs`, `crates/tm-harness/src/lib.rs`, `README.md` |
| **#5** | `d7b7e1f` | Enrich deterministic brownfield `.flow` packing (10 KB `repo_summary` with exported signatures & test list, 16 KB `BROWNFIELD_UNIT_MAP_BUDGET`, full owned-file + 1-hop source packing) and wire `Atlas::render_for_planner` into the planner brief. | `crates/tm-flow/src/{lib,render}.rs`, `crates/tm-daemon/src/{atlas,engine,tests}.rs` |
| **#6** | `617a327` | Enable native web access by default (`provider: "native"`, worker search on, verifier strictly offline, empty-allowlist SSRF-guarded worker fetch cap = 5), add `--offline` / `TASKMASTER_OFFLINE=1`, cap & log native searches across ACP/Claude/Codex, keep native fetch denied, fix fresh-planner dictionary hole, prune stdlib `docs`/`advisories`. | `crates/tm-harness/src/{lib,gate,claude,codex,acp}.rs`, `crates/tm-daemon/src/{web_access,settings,lib,api,api/ops,api/desk,main,eval,engine,suggest,tests}.rs`, `scripts/bench.sh`, `config.example.json`, `README.md` |
| **#7** | `9065f3b` | Threshold-gated (`>= 25` source files), read-only, offline `-low` effort brownfield LLM scope-scout extractor writing ephemeral `.taskmaster/runs/<id>/scope.json` and hydrating verified `.flow` nodes deterministically into planner/worker context with silent fallback. | `crates/tm-daemon/src/{scout,reply,lib,settings,api,api/desk,main,engine,tests}.rs`, `config.example.json`, `README.md` |
| **#8** | `2f71758` | Add concise 1-line Grok Build (`grok` / `grok-build`) `search_tool` hint in `code_mode_guide` pointing to single `dictionary__run_code` discovery and `await flow(...)` inside it; never block `search_tool`. | `crates/tm-daemon/src/{engine,tests}.rs` |

---

## 2. Shared Helpers Reused & Extracted

No parallel duplicate paths were introduced across TB-18 items:

- **Test step execution (`tm-daemon/src/code.rs`, `runtime.rs`):** Both `test(...)` and `testAffected(...)` share `CodeTools::execute_test_steps` and `runtime::filter_test_steps`.
- **Worktree sync (`tm-daemon/src/git.rs`, `code.rs`, `engine.rs`):** `git::sync_worktree` delegates directly to `git::sync_worktree_excluding`; all 6 worktree diff/review/judge filters share `Driver::is_synced_or_unchanged` backed by `WorktreeSync` SHA-256 snapshots.
- **External dependency detection (`tm-daemon/src/runtime.rs`):** Moved `project_has_external_deps` from `engine.rs` into `runtime.rs` and shared it across `runtime.rs`, `code.rs::select_test_steps`, and `engine.rs::worker_tool_policy` (avoiding drift between venv reuse and stdlib tool pruning).
- **Native search detection (`tm-harness/src/gate.rs`):** Exported `is_native_search_tool_name` and reused it in both `WorktreeGate` (permission + atomic cap enforcement) and `tm-daemon/src/engine.rs` (live `"web_search"` event and `"web-access"` ledger recording).
- **Offline detection (`tm-daemon/src/settings.rs`):** Single `offline_env_enabled()` helper shared by `Settings::load`, `main.rs`, `api.rs`, and `engine.rs`.
- **Lenient JSON extraction (`tm-daemon/src/reply.rs`):** Reused `reply::parse_json_object_lenient` inside `scout::parse_scope` rather than adding a second fenced-JSON extractor.
- **Effort routing (`tm-harness/src/acp.rs`, `claude.rs`, `codex.rs`):** `Driver::run_brownfield_scout` passes `effort: self.config.effort_routing.then(|| "low".into())`, letting existing harness adapters map Antigravity `*-low`, Claude `--effort low`, Grok `--reasoning-effort low`, and Codex `model_reasoning_effort="low"` uniformly.
- **Bounded line clipping (`tm-daemon/src/atlas.rs`, `scout.rs`):** Unified on `atlas::clip_lines` (fixed in this pass; see §3).

---

## 3. Coherence Findings Fixed in This Pass (`tb18 #9`)

1. **Duplicate `clip_lines` & oversized first-line edge case (`crates/tm-daemon/src/atlas.rs`, `scout.rs`):**
   - `scout.rs` had a private `clip_lines` helper that broke out and returned `""` if the very first line exceeded `max_chars`, duplicating the superior UTF-8 char-boundary loop already written in `atlas.rs::render_for_planner`.
   - **Fix:** Extracted `pub(crate) fn clip_lines(text: &str, max_chars: usize) -> String` in `atlas.rs`, reused it from both `Atlas::render_for_planner` and `scout.rs`, and added `atlas::tests::tb18_clip_lines_handles_multibyte_utf8_and_oversized_first_line`.
2. **Asymmetric `scout` settings API surface (`crates/tm-daemon/src/api/desk.rs`, `settings.rs`):**
   - `GET /api/settings` returned `"scout": { "enabled", "min_files" }` and `SETTING_KEYS` exported `"scout"`, but `PATCH /api/settings` (`SettingsBody`) did not yet accept `scout` updates.
   - **Fix:** Added `ScoutBody { enabled: Option<bool>, min_files: Option<usize> }` to `SettingsBody` with bounds validation (`1..=10_000`), added `api::desk::tests::tb18_settings_body_accepts_scout_patch_fields`, and expanded `settings::tests::tb18_example_config_sets_native_web_access_defaults` to assert `example.scout` defaults match `ScoutSettings::default()`.
3. **Incomplete `WorktreeSync` read-op flush set (`crates/tm-daemon/src/code.rs`):**
   - `CodeTools::op` flushed deferred sibling syncs (`sync.sync_if_dirty()`) on `read`, `readMany`, `ls`, `grep`, `symbols`, `probeAst`, and `check`, but omitted LSP read ops (`outline`, `definition`, `references`, `diagnostics`) that also inspect files in `scope.cwd`.
   - **Fix:** Added `"outline" | "definition" | "references" | "diagnostics"` to the O(1) atomic `sync.is_dirty()` flush guard so every worktree-reading Code Mode op observes landed siblings consistently.

---

## 4. Invariants & Safety Checks Verified

- **No forbidden files touched:** `.github/workflows` has zero diff; no tags, secrets, external checkouts, or other worktrees were modified.
- **Brownfield `.flow` integrity:** `scout.rs` writes solely to `.taskmaster/runs/<id>/scope.json`; `.flow/files/*.json` remains exclusively parser-owned and editable files are never locked as `Plan.contracts`. Hallucinated node IDs are dropped during `FlowQuery` hydration.
- **Web security boundaries intact:** Raw native `web_fetch` / `read_url_content` remain strictly denied in `WorktreeGate` and Claude `--disallowedTools WebFetch`; empty worker allowlist still enforces HTTPS-only, egress policy, pinned-DNS SSRF blocking, response limits, caching, and `<untrusted_web_content>` framing with quota charged only on non-cached successful fetches; verifier and brownfield scout stay strictly offline; `WarmNode` stays `--unshare-net`.
- **Benchmarks offline by default:** `scripts/bench.sh` exports `TASKMASTER_OFFLINE=1` and passes `--offline`; `tm-daemon/src/eval.rs` sets `config.offline = true`.
- **Zero-cost idle fast paths (multiplicative-path hygiene):** `WorktreeSync::is_dirty()` is a single `AtomicBool::load(Acquire)` on every read op; `scout::should_run` runs once per run before planning and skips `< 25`-file repos deterministically with zero LLM calls; `WorktreeGate` native search cap uses lock-free `AtomicUsize::compare_exchange_weak`.

---

## 5. Deferred Larger Refactors (Out of TB-18 Scope)

These pre-existing structural items (also tracked in [`docs/coherence-review.md`](coherence-review.md)) were intentionally kept out of TB-18 to avoid churning unrelated subsystems:

- **Splitting `crates/tm-daemon/src/engine.rs` and `crates/tm-daemon/src/tests.rs`** into focused submodules (high churn, no behavior change).
- **Extracting a standalone `FlowTools` MCP extension out of `crates/tm-daemon/src/web_access.rs`** so `.flow` graph queries do not live inside `WebTools`.
- **Centralizing `TASKMASTER_*` environment variable parsing** (`TASKMASTER_OFFLINE`, `TASKMASTER_WEB_ACCESS`, `TASKMASTER_EFFORT_ROUTING`, `TASKMASTER_STALL_SECS`, etc.) into a single `tm-daemon/src/env.rs` registry.
- **Consolidating remaining text clipping/tailing helpers across all 5 crates** into `tm-core::text`.

---

## 6. Verification

- `CARGO_BUILD_JOBS=4 cargo clippy --workspace --all-targets -- -D warnings`: **pass (0 warnings)**
- `CARGO_BUILD_JOBS=4 cargo build --workspace`: **pass**
- `TMPDIR=/tmp CARGO_BUILD_JOBS=4 cargo test --workspace`: **pass (699 passed, 0 failed; baseline 678, +21 tests)**
