# Taskmaster roadmap: web access, production readiness, and a better pipeline

Written 2026-11-22. Each item says what it is, where it goes, and the test that proves it.

## Where we stand

Already solid, and not repeated below: locked contracts and globs, parallel worktrees, the
shared cache-friendly prefix, zero-token checks, the tier ladder, Errorography with code-based
signatures and project notes, early per-page checks, autofix, contract-test routing, stuck-repair
escalation, flaky-test reruns, error digests, the scorecard, budget caps, graceful and force
cancel, resumable runs, undo of a merged run, approvals, live preview, a browser for UI workers,
Code Mode, skills, typed limits, and log rotation.

The gaps that matter most:

- **Agents cannot use the internet.** `/api/search` proxies SearXNG for the person only
  (`api.rs`, `search.rs`). Agents may fetch only the package indexes
  (`tm_harness::PACKAGE_INDEXES`, `gate.with_fetch_hosts`), the sandboxed shell runs with
  `--unshare-net` (`code.rs`), and a refused `web_search` or `web_fetch` marks the unit
  Blocked (`needs_capability`). The planner cannot read docs before it decides.
- **Feedback arrives late.** Type errors show up after a merge, not while the agent is still
  writing. Each early check starts its own `tsc`.
- **Nothing measures the pipeline.** `bench/` has three prompts and a results file, with no
  suite, no hidden tests, and no gate on prompt or engine changes.
- **Two agents can edit one checkout.** It happened today. Worktrees isolate runs, but not
  the threads that develop Taskmaster itself.

## What makes a model do better: the principles behind the plan

These come from what worked and what didn't while building the last two rounds.

1. **Feedback inside the turn beats feedback after the merge.** An agent that sees a type
   error seconds after writing fixes it in the same turn. One that sees it after the
   barrier pays for a whole repair round. That is why LSP is in the loop (2.1).
2. **Proof over claims.** A test that fails before the change and passes after it is worth
   more than any review. That is why there is a red/green gate (2.3).
3. **Look things up, don't search for them.** Grep and `ls` turns are most of an agent's
   wasted calls. Code intelligence tools and per-unit context packs (2.2) remove them.
4. **Research before deciding.** Wrong library versions and invented APIs come from planning
   blind. That is the planner's research phase (1.4).
5. **Disagree early.** A plan reviewed before approval is cheaper to fix than code reviewed
   after it. Hence plan review (2.4), and best-of-N on hard units (2.5).
6. **Measure every pipeline change.** Prompts and briefs are code. Without a suite, an
   "improvement" is a guess. Hence the eval suite (Phase 5, started early).
7. **One writer per checkout.** One thread or run per worktree, always (Phase 0).
8. **Project live state, do not drown in append-only logs.** Context entropy after turn 10
   destroys attention. Compiled Working Memory (2.9) keeps Tier-0 prefixes cache-identical
   while projecting clean diffs, active diagnostics, and a structured Attempts Ledger.
9. **Close the loop inside the turn.** Transactional `tryPatch`, reverse-dependency
   `testAffected`, and deterministic runtime/AST `probe()` (2.10) cut multi-turn repair
   loops into single-turn self-correction.
10. **Negotiate contracts dynamically instead of stalling.** Additive, non-breaking contract
    proposals auto-approve via AST reference checks (2.12); breaking changes pause only the
    affected blast radius.
11. **Show a parallel DAG like a parallel DAG.** Flattening 6 concurrent worktrees into one
    chat feed hides critical paths, dependencies, and bottlenecks. A 4-Lens Spatial Run
    Studio with purposeful visual coding (Phase 6) makes orchestration effortless to steer.

## Phase 0: land what is here (1–2 days)

- Review and commit the uncommitted work in slices: core (`tm-core`), engine, harnesses, web.
  Run CI locally before each slice.
- Rule for this repository's own development threads: each thread gets its own worktree
  (`t3_thread_launch` with a worktree strategy). Add the rule to `AGENTS.md`.
- CHANGELOG entries for the two improvement rounds.

## Phase 1: internet access for agents (the T3 Code parity item)

**Goal:** the planner researches, workers read docs, and reviewers look up advisories. Every
request is answered by the daemon, cached, budgeted, audited, and framed as untrusted.

1.1 **`web_search` and `web_fetch` MCP tools served by the daemon.** They go beside the
   Dictionary server (`tm-dictionary/src/mcp.rs`) or in a new `crates/tm-web`.
   - Providers sit behind one trait: SearXNG (the existing `settings.search.endpoint`), Brave
     Search API, Tavily or Exa, and a "native" passthrough that lets a harness use its own
     search when the person allows it.
   - The sandbox keeps `--unshare-net`. Egress is the daemon's, so it can be audited.
   - Test: a fixture provider answers. A worker calls `web_search` and gets hits. The call is
     in the transcript and the ledger.

1.2 **Fetch pipeline.**
   - https only, with SSRF protection: no loopback, RFC 1918, link-local or metadata
     addresses. This is checked after DNS resolution and after every redirect.
   - A size cap, content-type checks, HTML converted to markdown with the main content
     extracted, and text pulled from PDFs.
   - A content-addressed cache under `$XDG_CACHE_HOME/taskmaster/web`, with a TTL.
   - Tests: the SSRF table (127.0.0.1, `[::1]`, 169.254.169.254, a redirect to 10.0.0.1),
     the size cap, a cache hit with no network, and markdown output on a fixture page.

1.3 **Who gets what**, set in Settings → Web access, per role.
   - Planner: search and fetch, with a budget of N searches and M fetches per plan.
   - Workers: fetch only from an allowlist derived from the unit's dependencies (package
     docs, the project's own docs domains), and search only when the person allows it.
   - Reviewer: advisory lookups (OSV, GitHub Security Advisories).
   - Verifier: none.
   - An off-allowlist domain becomes an approval in `risky` mode (`approvals.rs`, `ToolKind::Fetch`).
   - Test: a worker's fetch outside the allowlist is denied, or asks for approval in risky
     mode. A planner over budget is told so plainly.

1.4 **Planner research phase.**
   - Before drafting, the planner may research. It writes a short "Research" section into
     `PROJECT.md`: decisions, each with its source URL and the version it applies to.
   - Workers inherit the decisions, not the raw pages, which keeps the shared prefix small
     and stable.
   - Test: a plan made with a fixture provider has a Research section with sources. The
     shared prefix is byte-identical across that run's workers.

1.5 **Local-first docs.**
   - A `docs(package, symbol?)` tool reads the installed package first: the README and type
     definitions in `node_modules/<pkg>`, `site-packages`, or `~/.cargo/registry/src`, at the
     version in the lockfile. The web is only the fallback.
   - Most "what's the API" questions are answered offline and at the right version.
   - Test: a fixture `node_modules` package's `.d.ts` is returned for its version.

1.6 **Untrusted-content framing (prompt-injection defence).**
   - Results come back inside a provenance frame with the source and `untrusted`. Hidden text,
     HTML comments and zero-width characters are stripped.
   - Imperative text aimed at the agent ("ignore previous…", "run…") is flagged. Fetched text
     can never grant a tool or widen a gate. Fetched content never enters the shared prefix.
   - Test: a fixture page with an injected instruction comes back flagged and framed. The
     gate's decisions are unchanged.

1.7 **UI.**
   - Settings → Web access: provider, API key (stored with secrets, see 3.1), per-role
     switches, allowlist, budget, cache size.
   - The Run view shows each agent's searches and sources. These are our own tool calls, so
     showing them keeps the rule "show only what was sent".
   - Playwright: the toggle is kept, and the sources show for a fixture run.

## Phase 2: pipeline intelligence (the biggest lever on quality, speed and cost)

2.1 **LSP in the loop.**
   - Keep `tsserver`/`typescript-language-server`, `pyright` or `basedpyright`, and
     `rust-analyzer` warm, one per run tree, shared by the worktrees through file overlays.
   - Add a `diagnostics(files)` tool. `run_code` writes return diagnostics for the files they
     touched automatically.
   - Early checks and `mechanical_checks` ask the warm server instead of starting `tsc` per
     page.
   - Tests:
     - A worker that writes a type error gets the diagnostic in the same turn.
     - Early-check wall time drops on a fixture project.
     - A server that crashes is restarted, and the checks fall back to `tsc`.

2.2 **Code intelligence and context packs.**
   - Tools: `definition`, `references`, `outline`, `symbols`, backed by LSP or tree-sitter.
   - Each unit gets a context pack built from the import graph (`tm-verify`) and the repo map:
     the exact contracts, symbols and neighbour signatures it needs, in a fixed order.
   - Measure tokens and tool calls per unit before and after on the suite.

2.3 **Red/green proof gate.**
   - For bug-fix and follow-up runs, the worker adds a test. Taskmaster runs it on the base
     (it must fail) and on the result (it must pass).
   - Mutation sanity on contract tests: stub the implementation, and the test must fail.
     This catches vacuous tests.
   - Test: a test that passes on the base is rejected with a clear sentence.

2.4 **Plan review before approval.**
   - A different model reviews the plan for ownership gaps, missing contracts, units that
     are too big, and wrong difficulty labels. The planner applies the findings once.
   - The person sees the review beside the plan.
   - Test: a fixture plan with overlapping globs comes back corrected.

2.5 **Best-of-N on hard units.**
   - For `difficulty: hard`, two models build in parallel worktrees. The checks, the tests,
     then the reviewer pick the winner, and the loser's diff is discarded.
   - The pair comes from the scorecard. It is off unless the budget allows it.
   - Test: the winning diff is merged and the loser's worktree is removed. The ledger has
     both calls.

2.6 **Release gate.** If a worker could run tests, did not, and then released, it is asked
   once more to run them (read from its tool calls). Test: the fake harness that skips tests
   is sent back once.

2.7 **Playbooks per error code.** Curated hints per code (TS2307, E0432, F821, …) are added
   only when that code appears, separate from the per-model Errorography.

2.8 **Brief versioning.** Hash every brief template and record the hash in the ledger, so
   evals can A/B a prompt change.

2.9 **Compiled Working Memory (`crates/tm-daemon/src/memory.rs`).**
   - Split every prompt into 3 cache tiers: Tier 0 (frozen shared prefix across parallel
     workers), Tier 1 (unit context pack of imported signatures and locked tests), and
     Tier 2 (live state projection: current `git diff --stat`, deduplicated active
     diagnostics, and a 5-bullet Attempts Ledger).
   - Auto-compact warm sessions once context entropy or consecutive repairs cross a
     threshold so repairs never drown in stale tool logs.
   - Test: repeated repair attempts project a deduplicated Attempts Ledger and keep Tier 0
     byte-identical across parallel workers.

2.10 **Transactional `run_code` v2: `tryPatch`, `testAffected`, and `probe()`.**
   - `tryPatch(diff)` validates ownership globs and runs fast checks on touched files in the
     same turn, rolling back automatically on ownership violation.
   - `testAffected(files?)` runs only the reverse-dependency test slice impacted by edited
     files instead of the entire repository test suite.
   - `probe(target)` provides deterministic runtime/AST introspection (`probe.ast(path)` for
     structured exports/imports, `probe.http(...)` scoped to the worker's preview origin).
   - Test: `tryPatch` rolls back an out-of-glob edit and returns structured diagnostics in
     one call; `probe.ast` extracts exports from a fixture file.

2.11 **Adversarial Dual-Spec Synthesis.**
   - Optionally generate locked behavioral and property-based tests with a second model
     family before builders start, mutation-checked red against base code.

2.12 **Dynamic Mid-Flight Contract RFCs & Critical-Path Hedging.**
   - Workers emit typed `ContractProposal` events; additive non-breaking additions
     auto-merge into the contract branch via AST reference verification with zero Master
     round-trip; breaking proposals pause only impacted dependents.
   - Critical-path bottleneck units that stall past `p90` can speculatively race a second
     model family within budget, taking the first green diff and cancelling the loser.

2.13 **Living Codebase Atlas (`crates/tm-daemon/src/atlas.rs`) & Errorography v2.**
   - Incrementally index module clusters, language breakdown, import centrality hubs, git
     churn hotspots, and test-to-source mappings in `.taskmaster/atlas.json` (served at
     `GET /api/atlas`).
   - Distill semantic/architectural lessons from repairs, diff comments, and run undos into
     glob-scoped project invariants with confidence decay.

## Phase 3: production hardening

3.1 **Secrets.**
   - Store host tokens and web API keys in the OS keychain (Secret Service, macOS Keychain),
     not plaintext, keeping `hosts.json` as the index only.
   - A redaction filter over transcripts, logs, terminal output and exports, for known secret
     values and common token patterns.
   - Test: a secret printed by a worker shows as `•••` everywhere.

3.2 **Egress audit and policy.** Every daemon-side network request is logged with run,
   agent, host and bytes, and shown in Settings. A deny list can be set by policy.

3.3 **Supply-chain gate.**
   - When a unit adds a dependency, check before the merge:
     - OSV advisories;
     - the package's age and downloads;
     - typosquat distance to popular names;
     - whether its license is on the allowlist.
   - Block the merge, or ask, with the reason.
   - Test: a fixture lockfile adding a known-vulnerable version is held.

3.4 **Merge queue and semantic conflicts.**
   - Serialize merges across concurrent runs.
   - When the base moved since the run started, merge into a temporary tree and run the
     checks there before delivering.
   - Test: two runs that each pass alone, but break together, do not both merge.

3.5 **Preflight / doctor.**
   - Before planning spends tokens, check:
     - each harness's version and sign-in;
     - quota hints;
     - disk space;
     - `node_modules` and the venv;
     - whether Chrome is there for UI checks.
   - The person gets one screen listing what is missing, each with a fix button.

3.6 **Service and lifecycle.**
   - A systemd user unit and a launchd plist, both installed by `install.sh`.
   - Auto-resume of interrupted runs on start, and a graceful drain on SIGTERM.
   - Retention settings for runs, worktrees and the web cache, with garbage collection.

3.7 **Records schema versioning.** A version field on `events.jsonl` and `status.json`,
   with migrations and a test per migration.

3.8 **Observability.**
   - OpenTelemetry traces (run → phase → agent call → tool call), a Prometheus `/metrics`
     endpoint, and an option for JSON logs.
   - Cost per project per month, with alerts at a threshold.

3.9 **API idempotency.** An `Idempotency-Key` on `POST /api/runs` and the other POSTs that
   start work.

3.10 **Policy-as-Code (`taskmaster.policy.toml`, `crates/tm-daemon/src/policy.rs`).**
   - Repo-versioned team guardrails: `protected_globs` (always require approval or review),
     `denied_globs`, allowed/denied egress domains, `max_run_cost_usd`, `daily_budget_usd`,
     `require_cross_family_review`, and custom secret `redact_patterns`.
   - Served and editable via `GET /api/policy` and `POST /api/policy`.
   - Test: a policy with `denied_globs` and `max_run_cost_usd` enforces both deterministically.

3.11 **Flight Recorder (`.tmrun.json`) & Universal Secret Redaction.**
   - `GET /api/runs/{id}/bundle` exports a portable, secret-redacted replay archive (plan,
     events, ledger, changes, cache economics, waterfall timings).
   - Shared redaction filter scrubs API keys (`sk-…`, `ghp_…`, `xai-…`, Bearer tokens,
     custom policy patterns) across bundles, logs, and exports.

3.12 **Checkpoint Time-Travel & A/B Run Forking.**
   - `GET /api/runs/{id}/checkpoints` lists every phase/unit commit and checkpoint ref.
   - `POST /api/runs/{id}/fork` forks any checkpoint into a new parallel run with optional
     prompt, model, or budget override so teams can A/B compare approaches side-by-side.

3.13 **Remote Runner Mesh.**
   - Offload parallel worktrees to remote Linux daemons over mTLS / Tailscale while keeping
     the local desktop/web Studio responsive and cool.

## Phase 4: T3 Code-level workflow

4.1 **PR loop.**
   - Opening a PR already works (`POST /api/runs/{id}/pr`). Then:
     - watch CI through `gh`;
     - pull failing job logs;
     - attribute them to pages with `runtime::attribute`;
     - repair, and push.
   - PR review comments become follow-up notes to the page that owns the file.

4.2 **Start from an issue.** A GitHub or GitLab issue URL becomes the prompt, and the result
   links back to the issue.

4.3 **Attachments.** Images (mockups, screenshots) go to the planner and UI workers on the
   harnesses that take images. Files are attached as context.

4.4 **The person's own MCP servers.** Database, Sentry, Linear and others are offered per
   role, under the approval policy, with tool calls in the transcript.

4.5 **Notifications.** Desktop notifications, plus an optional webhook or push, when an
   approval waits, a limit stops a run, or a run finishes.

4.6 **Scheduled runs.** Nightly dependency bumps, a flaky-test sweep, a docs refresh.

4.7 **Diff comments and checkpoints.**
   - A comment on a line in the diff panel becomes a note to that page's worker.
   - A checkpoint timeline per run, with restore and fork. Each page commit is a checkpoint,
     and undo already exists for the whole run.

4.8 **Environments.** Switch between this machine and a headless daemon on a server. The
   headless tarball and pairing already exist.

## Phase 5: measure and learn (start 5.1 right after Phase 0)

5.1 **Eval suite.**
   - Grow `bench/` into 20–30 tasks across web, library, CLI and Python, each with hidden
     tests the agents never see.
   - Runner: `tm-daemon eval --suite bench/`. It records pass rate, cost, wall time,
     repairs and cache hits to `bench/results.jsonl`.

5.2 **Gate.** A change to an engine prompt or the pipeline must not lower the pass rate or
   raise the cost by more than 10% on the suite. A nightly CI job runs it.

5.3 **Learned routing.** Record difficulty and language on ledger entries. The scorecard then
   scores per (model, difficulty, language). An opt-in "Auto" role picks the cheapest model
   at ≥85% first pass for each unit.

## Phase 6: Spatial Mission Control & Rich Visual UI/UX Overhaul

**Goal:** make Taskmaster's parallel, contract-first orchestration visually unmistakable,
directly steerable, and radically faster to review than a linear chat thread, introducing
rich semantic visual coding (contracts, workers, checks, critical path, cache economics)
while preserving ≥4.5:1 contrast in both dark and light themes.

6.1 **3-Pillar Spatial Workspace Shell (`web/src/App.tsx`).**
   - Collapse 9 flat sidebar links into a focused Workspace Rail: Project Switcher +
     `+ New run`, live Runs with visual progress rings/spend/approval dots, and a compact
     bottom system bar (`Knowledge`, `Reviews`, `Doctor`, `Settings`). All legacy routes
     remain directly navigable.

6.2 **4-Lens Run Studio (`1`–`4` keyboard switching in `RunView.tsx`).**
   - **Lens 1 — Mission & Interactive Plan Workbench (`MissionLens.tsx`)**: layered visual
     DAG (Contracts → Parallel Units → Verification) with curved SVG dependency edges,
     difficulty/tier badges, live diff stats, and a Node Focus Drawer with targeted
     Soft-Steer. During `awaiting_approval`, units and contracts are editable inline and
     persisted via `POST /api/runs/{id}/plan` with a live cost/time preview before launch.
   - **Lens 2 — Filtered Activity Stream (`Flow.tsx`)**: upgraded narrative feed with smart
     filter chips (`All`, `Edits`, `Checks & Errors`, `Approvals`, per-worker).
   - **Lens 3 — Review & Verification Studio (`ReviewLens.tsx`)**: full-bleed unit-grouped
     diff viewer with click-a-line inline commenting routed directly to the owning worker,
     split Live Preview toggle, and ship controls (`Draft PR`, `Undo Merge`).
   - **Lens 4 — Telemetry & Flight Recorder (`TelemetryLens.tsx`)**: parallel Gantt
     execution waterfall, prompt cache-hit & cost economics breakdown, Checkpoint
     Time-Travel scrubber with 1-click **Fork Run**, and `.tmrun.json` bundle export.

6.3 **Ambient Floating Approval Dock (`ApprovalDock.tsx`).**
   - Surfaces pending risky tool approvals across all 4 lenses with instant `A` (Approve) /
     `D` (Deny) keyboard shortcuts so approvals never get buried in a scrolled log.

6.4 **Smart New-Run Launcher (`NewRun.tsx`).**
   - Centered home composer enriched with live Execution Mode preview (`Direct Fast-Path` vs
     `Parallel Build` vs `Phased Sketch`), inline per-run USD Budget Cap selector, and a
     live Preflight Doctor health badge (`GET /api/doctor`).

6.5 **Unified Knowledge Hub (`Knowledge.tsx`).**
   - One tabbed workspace uniting the visual **Codebase Atlas** (`GET /api/atlas`),
     **Dictionary**, **Errorography**, **Skills**, and **Policy-as-Code** editor.

## Order

| Step | What | Size | Needs |
| --- | --- | --- | --- |
| 1 | Phase 0: commit in slices, one worktree per thread | S | — |
| 2 | 5.1 a first eval suite of 10 tasks | M | 0 |
| 3 | 2.9–2.10, 2.13, 3.5, 3.10–3.12: Compiled Memory, `run_code` v2, Atlas, Policy, Doctor, Checkpoints/Fork, Flight Recorder | L | 0 |
| 4 | Phase 6: Spatial Mission Control, 4-Lens Run Studio, Interactive Plan Workbench, Rich Visuals, Knowledge Hub | L | 3 |
| 5 | 1.1–1.3, 1.6: web tools, fetch pipeline, roles, injection framing | L | 0 |
| 6 | 2.1 LSP in the loop | L | 0 |
| 7 | 1.4–1.5, 1.7: research phase, local docs, UI | M | 5 |
| 8 | 2.3 red/green gate, 2.6 release gate, 2.11–2.12 dual-spec & contract RFCs | M | 6 |
| 9 | 3.1 secrets, 3.3 supply-chain gate, 3.4 merge queue | L | 5 |
| 10 | 2.4 plan review, 2.5 best-of-N, 2.2 context packs | L | 2, 6 |
| 11 | 3.6–3.9, 3.13 service, schema, observability, idempotency, remote mesh | M | — |
| 12 | Phase 4 workflow items, PR loop first | L | 9 |
| 13 | 5.2 gate, 5.3 learned routing | M | 2 |

## Risks

- **Prompt injection through the web** is the largest new risk. 1.6 ships with 1.1, never
  after it.
- **Warm language servers** cost memory per run tree. Cap them, share where possible, and fall
  back to the CLI checkers.
- **Best-of-N doubles spend on hard units.** It stays behind the budget and is off by default.
- **More gates mean more ways to stall.** Every gate refuses with a sentence and a way out
  (Retry, Switch, or an override), the same as typed limits.
