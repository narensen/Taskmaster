# Taskmaster Workspace Design & Coherence Review

Whole-system architectural review of the 10 Rust workspace crates under `crates/*` (~86.2k LOC) on branch `feat/flow`.

## 1. Workspace Overview & Dependency Map

| Crate | Approx. LOC | Primary Responsibility | Workspace Dependencies |
| --- | ---: | --- | --- |
| `tm-core` | 4,160 | Pure domain model, run state machine, plan validation, ledger, errorography, playbooks (zero I/O) | *(leaf)* |
| `tm-token` | 250 | Ed25519 / JWT verification and account allow-lists | *(leaf)* |
| `tm-launcher` | 300 | Desktop/CLI `tm-daemon` process launcher | *(leaf)* |
| `tm-harness` | 6,460 | Coding CLI adapters (`claude`, `codex`, ACP: `agy`/`grok`/`opencode`), `WorktreeGate`, `Sandbox`, `hold` | *(leaf)* |
| `tm-verify` | 1,410 | Deterministic ownership checks and static import graph | `tm-core` |
| `tm-dictionary` | 990 | Content-addressed Dictionary/Commons store and read-only MCP HTTP server | `tm-core` |
| `tm-flow` | 6,820 | Tree-sitter `.flow` code knowledge graph, SQLite index, `flow_query`, prompt renderers, `.flow/env.json` | `tm-core` |
| `tm-review` | 3,150 | Standalone git/PR review engine, linters, context files, review history | `tm-harness`, `tm-verify` |
| `tm-commons` | 2,520 | Hosted Commons server (Postgres/memory DB, OAuth, token sealing, credential scanner) | `tm-core`, `tm-token` |
| `tm-daemon` | 60,180 | HTTP API, run orchestration (`engine.rs`), Code Mode, git, runtime checks, web access, settings, tests | `tm-core`, `tm-dictionary`, `tm-flow`, `tm-harness`, `tm-verify`, `tm-review`, `tm-token` |

### Architectural Strengths

- **Clean dependency DAG:** Zero circular dependencies; `tm-core`, `tm-token`, `tm-harness`, and `tm-launcher` remain clean leaf crates.
- **Strict zero-I/O core:** `tm-core` keeps pure deterministic state transitions, plan validation, and ledger math decoupled from disk and network I/O.
- **Minimal dead code:** Only two intentional `#[allow(dead_code)]` sites exist across the entire workspace (RAII drop guards in `crates/tm-daemon/src/browser.rs:205-208` and non-Unix `cfg_attr` guards in `crates/tm-harness/src/hold.rs:127-194`).

---

## 2. Low-Risk Improvements Implemented in This Session

Each improvement below was kept small, behavior-preserving, tested, and verified with `cargo clippy --workspace --all-targets -- -D warnings`, `cargo build --workspace`, and `cargo test --workspace`.

| Commit | Summary | Files Touched |
| --- | --- | --- |
| `8235533` | **Unified Dictionary MCP tool prefixes.** Exported `DICT_TOOL_PREFIXES` and `strip_dict_prefix` from `tm-dictionary` and removed the duplicate implementation in `tm-daemon/src/code.rs` plus the diverging prefix list in `tm-daemon/src/engine.rs::dict_prefixes()` (which had omitted `taskmaster__`). Added `mcp::tests::strip_dict_prefix_handles_all_known_mcp_prefixes`. | `crates/tm-dictionary/src/{lib,mcp}.rs`, `crates/tm-daemon/src/{code,browser,web_access,engine}.rs` |
| `0316d2d` | **Centralized `safe_rel` and `is_flow_path` in `tm-core`.** Replaced verbatim duplicates of `safe_rel` (`tm-core/src/plan.rs`, `tm-daemon/src/git.rs`) and `is_flow_path` (`tm-flow/src/model.rs`, `tm-verify/src/ownership.rs`) with single canonical helpers in `tm-core/src/types.rs` while preserving existing re-exports. Added `types::unit_tests::safe_rel_and_is_flow_path_handle_traversal_and_separators`. | `crates/tm-core/src/{types,plan}.rs`, `crates/tm-daemon/src/git.rs`, `crates/tm-flow/src/model.rs`, `crates/tm-verify/src/ownership.rs` |
| `77b6b74` | **Removed redundant uncached `bwrap` probe and deduplicated pytest detection.** Deleted `code.rs::bwrap_works()` (which spawned an extra uncached `bwrap` subprocess alongside the `OnceLock`-cached `tm_harness::sandbox::bwrap_usable()`) and replaced three copies of pytest step detection in `runtime.rs` with `is_pytest_step`. Added `runtime::tests::is_pytest_step_matches_runners_and_ignores_setup_steps`. | `crates/tm-daemon/src/{code,runtime}.rs` |
| `98cb9c2` | **Extracted `AgentRequest::to_worker_task` and `apply_run_end_fields`.** Collapsed three identical 13-line `WorkerTask` conversions (`resume_or_ask`, `reattach_or_resume`, `ask_once`) and four identical transcript `"end"` event population blocks (`follow_up`, `resume_or_ask`, `reattach_or_resume`, `ask_once`) into single helpers. Added `engine_coherence_tests::{agent_request_to_worker_task_strips_shell_when_read_only, apply_run_end_fields_records_ok_usage_error_and_refused_consistently}`. | `crates/tm-daemon/src/engine.rs` |

---

## 3. Detailed Findings

### A. Duplicated Logic

1. **String tail / head truncation helpers (13 copies across 5 crates):**
   - Tail-keeping helpers: `crates/tm-daemon/src/runtime.rs:565` (`tail`), `crates/tm-daemon/src/code.rs:1328` (`tail`), `crates/tm-daemon/src/engine.rs:197` (`tail_chars`), `crates/tm-daemon/src/preview.rs:463` (`tail`), `crates/tm-daemon/src/api/desk.rs:652` (`tail`), `crates/tm-daemon/src/api/connect.rs:455` (`tail`), `crates/tm-review/src/lint.rs:598` (`tail`).
   - Head-clipping helpers: `crates/tm-core/src/errorography.rs:303` (`clip`), `crates/tm-daemon/src/engine.rs:309` (`clip`), `crates/tm-daemon/src/hosts.rs:634` (`clip`), `crates/tm-flow/src/query.rs:732` (`clip_chars`), `crates/tm-flow/src/render.rs:1186` (`clip_inline`), `crates/tm-harness/src/lib.rs:494` (`clip_result`).
   - Subtle inconsistencies exist: some count UTF-8 bytes, others count Unicode scalar values; some emit `"…\n"`, others `"…"` or nothing.

2. **RFC 3339 / civil calendar math duplicated across two crates:**
   - `crates/tm-daemon/src/time.rs` (`rfc3339`, `parse_rfc3339`, `civil_from_days`, `days_from_civil`) and `crates/tm-commons/src/time.rs` (`rfc3339_at`, `civil_from_days`) both implement Howard Hinnant's algorithm independently. Both crates depend on `tm-core`, which could host pure timestamp formatting/parsing.

3. **Four overlapping source-scanning / symbol / import extraction layers:**
   - `crates/tm-flow/src/parse.rs` + `resolve.rs` (tree-sitter AST symbols + call/import graph for Python, TS/JS, Rust).
   - `crates/tm-verify/src/languages.rs` + `graph.rs` (regex/lexer import extraction across TS, JS, Python, Rust, Go, etc.).
   - `crates/tm-daemon/src/repomap.rs` (~440 LOC regex/heuristic repo symbol outline).
   - `crates/tm-daemon/src/lsp.rs` (~826 LOC LSP client + fallback regex outline/definition/references/context pack).
   - Maintaining four independent language scanners risks drift whenever a new language or import syntax is added.

4. **Duplicate git subprocess and worktree wrappers:**
   - `crates/tm-daemon/src/git.rs` (~700 LOC) and `crates/tm-review/src/git.rs` (~317 LOC) independently wrap `git` commands, detached worktree creation, and diff reading.

5. **Duplicate glob expansion helpers:**
   - `crates/tm-core/src/types.rs:291` (`glob_alternatives`), `crates/tm-daemon/src/gates.rs:816` (`concrete_paths_in_glob`), and `crates/tm-daemon/src/engine.rs` (`expand_glob_in_worktree`) implement overlapping brace-glob expansion logic.

6. **Duplicate `.flow` path check in `tm-harness`:**
   - `crates/tm-harness/src/gate.rs:188` (`WorktreeGate::is_flow_path`) keeps a `Component::Normal(".flow")` check because `tm-harness` intentionally does not depend on `tm-core`. Acceptable as a leaf-crate tradeoff, but worth noting if `.flow` path rules ever change.

7. **Two HTTP client stacks in the workspace:**
   - `tm-daemon` and `tm-launcher` use synchronous `ureq = "2"` (with `ring`/`rustls`), while `tm-commons` uses async `reqwest = "0.12"`. Keeping `ureq` inside async Tokio handlers in `tm-daemon` (`commons.rs`, `hosts.rs`, `web_access.rs`) requires careful `spawn_blocking` isolation to avoid blocking worker threads.

### B. Unclear Module Boundaries & Monolith Growth

1. **`crates/tm-daemon/src/engine.rs` (~9,150 LOC):**
   - Houses the main state-machine drive loop, planner/repair orchestration, contract partitioning, worker brief rendering, warm session pooling, git worktree lifecycle, permission gate wiring, transcript/ledger recording, and merge/verify coordination in a single file.
   - Makes navigating brief-construction vs. execution-loop vs. session-management code slow and encourages private helper duplication.

2. **`crates/tm-daemon/src/tests.rs` (~12,400 LOC):**
   - Single monolithic test file covering orchestration, planning, warm sessions, effort routing, Code Mode, web access, browser previews, and API endpoints. Slows incremental test compilation and makes locating test fixtures harder.

3. **`crates/tm-daemon/src/web_access.rs` (~3,100 LOC):**
   - Bundles unrelated concerns under one module: SSRF DNS-pinning HTTP client, HTML-to-markdown conversion, local `node_modules` `.d.ts` / package docs reader, OSV security advisory queries, prompt-injection stripping, AND the `.flow` `flow_query` MCP tool dispatch.
   - Having `flow_query` dispatched inside `WebTools` (`web_access.rs:2409`) is surprising from a module-boundary standpoint, since `.flow` queries are purely local graph reads with no web dependency.

4. **`crates/tm-daemon/src/code.rs` (~2,700 LOC):**
   - Combines the warm Node IPC harness, bubblewrap sandbox runner, unified-diff parser/applier, `test`/`testAffected` step runner, and the unrelated `set_status` MCP extension (`StatusTool`).

5. **`crates/tm-daemon/src/api.rs` (~2,730 LOC) + `crates/tm-daemon/src/api/desk.rs` (~2,170 LOC):**
   - Although `api/` has already begun splitting into focused submodules (`alerts.rs`, `connect.rs`, `hosts.rs`, `ops.rs`, `race.rs`, `settle.rs`, `setup.rs`, `skills.rs`, `studio.rs`, `switch.rs`, `terms.rs`), `api.rs` and `api/desk.rs` still hold ~4,900 LOC of mixed router wiring, DTOs, settings updates, and terminal/SSE handlers.

### C. Config, Policy & Environment Sprawl

1. **Colliding `Policy` struct names in `tm-daemon`:**
   - `crates/tm-daemon/src/policy.rs:17` defines `pub struct Policy` (repository file/command deny rules from `.taskmaster/policy.json`).
   - `crates/tm-daemon/src/approvals.rs:70` defines `pub struct Policy` (runtime interactive approval policy: `Always` / `Risky` / `Never`).
   - Having two identically named public `Policy` structs in the same crate forces qualified paths and invites confusion.

2. **Five overlapping configuration surfaces:**
   - `crates/tm-daemon/src/lib.rs:355` (`Config` — per-run runtime parameters)
   - `crates/tm-daemon/src/settings.rs:23` (`Settings` — persistent `~/.taskmaster/config.json` / `.taskmaster/config.json`)
   - `crates/tm-daemon/src/project_config.rs:21` (`ProjectConfig` — committed `taskmaster.yaml` worktree commands)
   - `crates/tm-daemon/src/web_access.rs:78` (`WebAccessSettings` — nested inside `Settings` and mirrored in API bodies)
   - `crates/tm-daemon/src/api/desk.rs:1723` (`SettingsBody` — HTTP PATCH mirror of `Settings`)
   - Adding a new toggle (such as `agents.effort_routing`) currently requires plumbing through `Settings`, `Config`, CLI flags (`main.rs`), `SettingsBody`, and `engine.rs`.

3. **Scattered `TASKMASTER_*` environment variables (20+ variables across 11 files):**
   - `TASKMASTER_ACP_TRACE` (`tm-harness/src/acp.rs`), `TASKMASTER_HOLD` (`tm-harness/src/hold.rs`), `TASKMASTER_DICTIONARY_TOKEN` (`tm-harness/src/codex.rs`), `TASKMASTER_EFFORT_ROUTING` / `TASKMASTER_SEARCH_URL` (`tm-daemon/src/settings.rs`), `TASKMASTER_LOG_FORMAT` (`tm-daemon/src/log.rs`), `TASKMASTER_TELEMETRY_URL` (`tm-daemon/src/telemetry.rs`), `TASKMASTER_HOSTS` / `TASKMASTER_GH` / `TASKMASTER_GLAB` (`tm-daemon/src/hosts.rs`), `TASKMASTER_SKILLS` (`tm-daemon/src/skills.rs`), `TASKMASTER_WEB` (`tm-daemon/src/main.rs`), `TASKMASTER_CHROME` (`tm-daemon/src/runtime.rs`), `TASKMASTER_STALL_SECS` / `TASKMASTER_ALLOW_UNVERIFIED_DEPS` / `TASKMASTER_SCRATCH` / `TASKMASTER_SHARED_VENV` (`tm-daemon/src/engine.rs`).
   - There is no single inventory or validation pass, and boolean env parsing (`0`/`false`/`off` vs presence checks) varies slightly between call sites.

### D. Inconsistent Abstractions

1. **Verbose 20-field `AgentRequest` struct literals:**
   - `crates/tm-daemon/src/engine.rs` still constructs `AgentRequest` via raw struct literals in 6 locations (`3480`, `5000`, `5170`, `5444`, `6444`), so every new field (like `no_file_reads`) must be manually threaded across all 6 sites. A constructor/builder (`AgentRequest::new(harness, cwd, brief, timeout)`) would localize defaults cleanly.
2. **Inconsistent MCP extension placement:**
   - `McpState` (`tm-dictionary`) supports pluggable `Extension` trait objects, but `tm-daemon` attaches `flow_query` inside `WebTools` (`web_access.rs`) and `set_status` inside `code.rs` (`StatusTool`). Extracting a tiny `FlowTools` extension keeps local graph queries decoupled from `web_access.rs`.
3. **Harness adapter boilerplate:**
   - `crates/tm-harness/src/{claude.rs, codex.rs, acp.rs}` each manage child process spawning, stall/timeout racing, rate-limit extraction, and session teardown with subtly different helpers and event buffering.

---

## 4. Recommended Larger Refactors

| Priority | Refactor | Target Files | Risk | Effort |
| --- | --- | --- | --- | --- |
| P1 | **Split `engine.rs` into submodules (`engine/{mod,brief,planner,sessions,dispatch,verify}.rs`)** keeping `Driver` methods cohesive by concern without changing public APIs. | `crates/tm-daemon/src/engine.rs` | Medium | M (1–2 days) |
| P1 | **Extract `FlowTools` MCP extension out of `web_access.rs`** so `.flow` graph queries do not live inside the web/SSRF module. | `crates/tm-daemon/src/web_access.rs`, `crates/tm-daemon/src/engine.rs` | Low | S (2–4 hours) |
| P1 | **Introduce `AgentRequest::new(...)` builder and `#[derive(Default)]` on `RunSummary`** to remove 20-field struct literal boilerplate and prevent missed default fields. | `crates/tm-daemon/src/engine.rs`, `crates/tm-harness/src/lib.rs` | Low | S (1–2 hours) |
| P2 | **Unify text clipping/tailing and RFC 3339 helpers in `tm-core::text` and `tm-core::time`** (replacing 13 `tail`/`clip` copies and duplicate Hinnant calendar math in `tm-daemon` + `tm-commons`). | `crates/tm-core/src/`, `crates/tm-daemon/src/{time,runtime,code,engine,preview,hosts,api/desk,api/connect}.rs`, `crates/tm-commons/src/time.rs`, `crates/tm-flow/src/{query,render}.rs` | Low | S–M (3–5 hours) |
| P2 | **Rename colliding `Policy` types and centralize `TASKMASTER_*` env parsing** (e.g. `FilePolicy` vs `ApprovalPolicy`, plus a `tm-daemon/src/env.rs` registry with consistent bool/duration parsing). | `crates/tm-daemon/src/{policy,approvals,settings,hosts,skills,runtime,engine,log,telemetry,main}.rs` | Medium | M (0.5–1 day) |
| P2 | **Split `tm-daemon/src/tests.rs` (~12.4k LOC) into domain test modules (`tests/{planning,workers,flow,code_mode,api,web_access}.rs`)** sharing a common `tests/harness.rs` fixture module. | `crates/tm-daemon/src/tests.rs` | Low | M (0.5–1 day) |
| P3 | **Consolidate `tm-daemon/src/repomap.rs` and `lsp.rs` fallback symbol scanning onto `tm-flow`** for supported languages (Python, TS/JS, Rust), keeping regex fallbacks only for unsupported languages. | `crates/tm-flow/src/`, `crates/tm-daemon/src/{repomap,lsp}.rs` | Medium | M–L (1–2 days) |
| P3 | **Extract shared `tm-git` helper (or share worktree primitives)** between `tm-daemon/src/git.rs` and `tm-review/src/git.rs`, and evaluate migrating blocking `ureq` calls in async daemon paths onto a single async HTTP client. | `crates/tm-daemon/src/git.rs`, `crates/tm-review/src/git.rs`, `crates/*/Cargo.toml` | High | L (2–4 days) |
