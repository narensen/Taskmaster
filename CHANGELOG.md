# Changelog

What changed in each release, for the people who use Taskmaster. The section for a version is
also its GitHub release notes (`scripts/release-notes.sh` takes it from here).

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
