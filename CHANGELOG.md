# Changelog

What changed in each release, for the people who use Taskmaster. The section for a version is
also its GitHub release notes (`scripts/release-notes.sh` takes it from here).

## 0.1.8

### Run modes and turns

- Agent and Code run modes, one-agent follow-up turns, and a mode picker with Review in it once a
  run has finished.
- Per-run permission modes, and goals that keep a run going until they hold.
- Hidden turn checkpoints, with Edit-from-here rewind and optional file restore.
- A run's providers can be switched between turns, with a handoff to the next agent.
- One request can be raced across providers, compared side by side, and the one picked merged.
- The master can answer a worker's low-risk approval first (opt-in).
- A suggested next message after a turn and Ctrl/Cmd+S draft stash (Labs).

### Engine

- Release gate, red/green proof, Best-of-N races for hard units and contract mutation sanity
  checks are wired into the run engine.
- Provider rate-limit usage feeds Auto routing; interrupted worker sessions resume on restart, and
  idle warm sessions hibernate.
- Builders start in a holder that survives a daemon update and are picked up by the next daemon.
- `taskmaster.yaml` worktree setup, shared dirs, copied files and trust-by-hash approval.
- Design Mode preview, batched diff review, an agent status line and a `run_code` wait helper.

### Connect and devices

- A Connect page, one-time pairing links with QR codes, named revocable devices, `--lan`, an MCP
  server for outside agents, prompt links and a reconnect banner.
- Paired machines' runs show in the sidebar, each with its own reconnect and Needs you.
- The taskmaster skill for outside agents, served at `/skill.md`.

### Faster and better looking

- A sleeker design, a faster home, run start and provider pickers.
- Smaller run-page bundle, fewer re-renders and less polling, with perf budgets.
- Per-run stream watchers, buffered transcript writers and an LRU transcript cache; cold-open
  transcripts load a recent window and tool bodies on demand.
- Search inside runs from Ctrl+K; a unified Needs-you state with unread tracking and queue-or-steer
  notes; Pin, Settle and Archive move to the daemon and finished runs settle on their own.

## 0.1.7

### Faster runs

- A unit the planner marks hard starts on the mid tier; an easy one builds at low effort.
- A page that merges while others still build has its own files type-checked and linted at once,
  and goes back to its worker straight away when they fail.
- `eslint --fix` and `ruff --fix` run before the checks, and what they fix is committed.
- A repair that fails exactly as the attempt before it goes up a tier at once.
- A test step that fails once and passes when run again is reported as flaky, and costs no repair.
- A long list of the same type error reaches a repair as three examples and a count.

### Better results

- Locked contract tests are every worker's acceptance check. When one fails, the units it imports
  repair their code; the master no longer rewrites the test.
- Errorography counts a check failure by its tool and code (`tsc TS2304`, `ruff F401`), and a fix
  found on a higher tier is handed to the model that kept making the mistake.
- A build or test step that is fixed becomes project memory every model is told on the next run.
- The model scorecard recommends a worker at 85% first pass over three units or more, and marks a
  model that needs a repair more often than not. A run that starts with such a worker says so.

### Control

- Undo takes a merged run back out of the project in one new commit, keeping later work. It is
  refused, with the reason, when the run's files were edited since.
- Cancel is graceful (finish what is running) or immediate, and a cancelled run can be resumed.
- A run can have a spending cap.
- The daemon log rotates at 8 MB.

### Also in this release

- Web access: daemon-mediated web search, an SSRF-safe fetch with pinned DNS, local docs, OSV
  advisories, native search passthrough, planner research, Web access settings and a per-run Web
  sources view.
- Operations panels, issue import, attachments, PR CI sync, and installing the daemon as a service
  from the web UI and `install.sh`.
- Compiled working memory, a codebase atlas, policy-as-code, a preflight doctor and the four-lens
  run studio.
- Repair playbooks, brief hashing, schema migrations and stratified auto-routing.
- Ops API routes, a CLI eval gate and a benchmark suite.

## 0.1.6

This release makes a run easier to follow while it streams, and gives the master a clearer picture
of the workers it hands work to.

### A run view you can read at a glance

- A headline at the top of a run says what it is doing now, or how it finished.
- The master and each worker show one line about their current step and a short glimpse of their
  reasoning, instead of a wall of raw output.
- Click an agent's headline to open its activity: the files it changed, the files it read, the
  commands it ran and its last error. Raw output stays folded until you ask for it.
- The master's card carries the plan's overview and the workers it delegated to, split units
  included. Inspect on any of them jumps straight to that worker's activity.
- Streamed entries are drawn on the next display frame, at most once a frame, and bursts of
  updates share one reload, so long runs stay smooth.
- The daemon wakes a run's live stream the moment a new entry arrives, instead of on a timer.
- Long run lists are drawn a screenful at a time, and only a run's clock ticks in the sidebar.
- Refreshed styles for the run view in both the dark and light themes, checked for 4.5:1 contrast.

### Planning and delegation

- The master is told how many parallel workers it has and which harness and model they use.
- A plan now has a one-paragraph summary, and every unit and part can carry its own context:
  interfaces, symbols, conventions and where its siblings' files are. Each worker's brief includes
  the summary, its context and the globs the other workers own.
- A run can split work across up to 16 units, instead of 4.
- Planners may write `workers` as well as `units` or `parts`.

### Fewer runs cut short

- Agents have no time limit by default (Settings can still set one).
- Thinking models get longer before a quiet stream counts as stalled: five minutes for small briefs
  and ten for larger ones.
- The planner has up to five minutes, and more tool calls, before it is asked to commit to a plan.

### Packaging

- The release check for the Linux package no longer fails after the upload has finished.

## 0.1.5

This release is about making the orchestration pay for itself. Workers test their own work, verify
cannot pass a failure, plans fit projects that are not websites, and a run is built a phase at a
time.

### Workers run their tests

- A builder gets a shell inside a sandbox, so it can run its tests, install what they need and
  update a lockfile.
- The sandbox is bubblewrap for Claude Code, Antigravity, Grok and OpenCode, and Codex's own sandbox
  for Codex.
- The whole machine is read-only to the builder except its worktree, its own scratch folder and the
  run's package caches.
- Fetch tools reach only PyPI, npm and crates.io.
- Both are switches in Settings, under Worker sandbox, and on by default.
- Where bubblewrap cannot run, workers have no shell, and the run log says so once.
- Each worker's brief ends with a short list to work down, which ends with running the tests.

### Verify really verifies

- Python tests are found from a `tests/` folder or `test_*.py` files; no config file is needed.
- Tests run through the project's own runner (poetry, uv or hatch), or else pip.
- They run in a fresh environment the run builds for itself, with the dev or test extras
  installed. A venv the daemon was started from no longer leaks in.
- A failure that names no file a unit owns goes to the master with its output.
- If it still fails, the run fails: it exits 1, and its branch is kept and not merged.
- A project whose tests could not run at all says `verify: nothing ran` and fails.

### Plans for libraries, tools, servers and migrations

- A plan names its kind: web, library, cli, server or migration.
- Only units a browser shows have routes, a design system and browser checks.
- Contracts are interfaces only, never a file a unit has to write.
- The rules the planner is shown are the rules its plan is checked against.
- A field with one sensible fix is corrected in place. A rejected plan is repaired by a short call
  instead of being planned again from nothing.

### Phased runs and PROJECT.md

- A bigger job is sketched first, in 2-6 phases, and only the first is planned in detail.
- Each phase builds, passes the project's tests and is recorded in `PROJECT.md`, with a checkpoint,
  before the next one joins.
- The next phase is planned while the current one builds.
- The run view shows the phases under the master, each with its state.

### Repairs that know what went wrong

- A repair carries the failing output, the calls that were refused and what the last attempt said.
- It goes back to the agent that built the unit, in the same session, so that agent keeps its context
  and the provider's cache.
- An attempt that needed a shell or the network it did not have is not retried.
- When every tier is the same model, there is no escalation ladder: two focused repairs, then a clear
  failure.

### Code Mode

- Every worker and every repair gets `run_code`.
- Scripts gain `test()`, which runs the project's tests in the sandbox and names the failures, and
  `readMany()`.
- Script time limits are settings.
- With Code Mode on, a Claude Code worker writes, runs and tests only through `run_code`.
- Code Mode is still off by default until benchmarks show it ahead.

### Costs, estimates and reports

- **Prices:** a price the provider reports is used; otherwise calls are priced from a price table in
  the settings file (Grok's prices are built in).
- **No fake zero:** a run with no prices says "unpriced", never `$0.00`.
- **Estimates:** sized to the plan, with cache reads counted apart.
- **`tm-daemon report`:** prints a run's stage table: planning, each unit, verify, idle and total
  time, the planning share, tokens, the cache-hit ratio, the cost, and tool and `run_code` calls.
  `--append bench/results.jsonl` collects it for benchmarks.
- **Ledger rows:** each names its agent, so the parts of a split unit are counted apart.
- **Transcripts:** they keep each brief whole, and each streamed message is one line.
- **Silent agents:** an agent that sends nothing for two minutes gets a line in the log.

### Limits and providers

- A provider that asks for payment (HTTP 402, "usage balance exhausted") is a typed limit like a
  quota or a sign-in: one sentence, with Switch and Retry in the app.
- The run is kept where it stopped. The command line exits 3, and the same command resumes it.
- `--timeout 0` means no time limit for an agent call.
- Units of a non-web plan are named `unit:` everywhere instead of `page:`.
- The OpenCode harness works end to end with a free model. A builder that kept its session open used
  to hang after its first turn.

### A smoother app

The run view no longer redraws its whole transcript for every streamed token. Measured with four
workers streaming at 4x CPU slowdown (a typical laptop):

| | 0.1.4 | 0.1.5 |
|---|---|---|
| Frame rate | 30.5 fps | 57.8 fps |
| Long tasks over 50 ms (in 15 s) | 72 | 0 |
| Main thread busy (of 15 s) | 13.4 s | 7.4 s |
| Memory growth (per 15 s) | 37 MB | 4.5 MB |
| First-load JavaScript (gzip) | 273 KB | 228 KB |
| Time to interactive, unthrottled | 206 ms | 149 ms |

Every screen but the gallery now loads when first opened and is fetched in the background once the
app is idle.

### Under the hood

- The version is kept in one place and checked in CI. Released binaries report their real version,
  not 0.1.0.
