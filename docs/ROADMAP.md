# Taskmaster roadmap, round 3: speed, then what T3 Code, Orca and herdr do that we don't

Written 2026-11-22. Round 2 (web access, pipeline intelligence, hardening, the four-tab run
view) is in [roadmap/round-2.md](roadmap/round-2.md); almost all of it shipped.

Each item says what it is, where it goes, how, the test that proves it, and the number to
measure before and after. Work top to bottom. One item, one local commit. Follow
`AGENTS.md`, the `perf-check` skill (measure first, report both numbers) and the `tests` skill
(a test that fails without the change).

## Where we stand

Landed this round (commits `b8fb298`..`c502d84`):

- Run modes: **Auto / Code / Agent** in every composer. Agent mode gives the whole project to
  one agent with no planning call. A note on a finished run in Agent or Auto mode becomes
  `Event::AgentTurn`: one agent, the note as its task, no page rebuilt.
- **Turns**: each follow-up starts below the last; earlier turns fold to one line.
- **Stop now** stops at once; calls cut off by a cancel end as `stopped`, with no spinner or clock.
- **Preview always opens**: `dev` → `start` → `preview` script, install missing packages,
  any HTML page, a file listing, and a static fallback when the dev server dies.
- **Speed**: the provider model list is cached and warmed. Home went from 49 to 10 requests
  and 4.1–4.6 s to 0.59 s; `POST /api/runs` from 0.5–1.3 s to 3–4 ms.
- Settings uses one settings source; the ops and web access panels are styled.

What the research below found missing, in one line each:

- **Performance**: the run stream does work that scales with every other run's traffic, and a
  transcript is 80% repeated metadata (Phase P).
- **Written but not wired**: red/green proof, contract mutation sanity, best-of-N and the
  release gate exist in `gates.rs` with no caller (Phase W).
- **Agent experience** (T3 Code): queue vs steer, rewind a turn with file checkpoints,
  per-run permission modes, goals, message search, server-owned settling, an MCP server for
  outside agents (Phase A).
- **Workspace** (Orca): a project file for worktree setup and shared folders, Design Mode,
  batched diff review, usage meters, racing agents, an agent status line (Phase O).
- **Runtime** (herdr): a blocked/working/idle/unseen state model with rollups, provider
  session resume after a daemon restart, `wait` tools (Phase H).

## What the three projects do, and where we are

| Feature | T3 Code | Orca | herdr | Taskmaster now | Item |
| --- | --- | --- | --- | --- | --- |
| Bounded cold open, history paged, tool bodies on demand | yes | – | – | whole transcript on open | P5 |
| Live command output only while a row is expanded, throttled | yes | – | – | every byte on the main stream | P5 |
| Queue or steer a message while an agent works | yes | – | – | notes steer; no queue | A1 |
| Edit an earlier prompt, revert files to that turn | yes (hidden git refs) | worktree checkpoints | – | undo of a whole merged run | A2 |
| Permission mode per thread in the composer | yes | – | – | one global approvals mode | A3 |
| Goals: keep going until a condition holds | `/goal` | – | – | no | A4 |
| Message search in the palette | yes | jump anywhere | – | run ids only | A7 |
| Server-owned settling (merged PR, inactivity) | yes | – | – | client-only flags | A8 |
| Outside agents drive the app (MCP / CLI / socket) | MCP server | Orca CLI | socket API + skill | Dictionary MCP only | A10 |
| Per-worktree setup script, shared dirs, copied ignored files | t3.json actions | `orca.yaml`, `.worktreeinclude` | – | `node_modules` symlink only | O1 |
| Click an element in the preview, send it to the agent | – | Design Mode | – | no | O2 |
| Batched line notes on a diff, sent once | – | yes | – | one note per line | O3 |
| Usage and reset times before a limit hits | yes | yes | – | typed limit after it hits | O4 |
| Same task raced across providers, pick the winner | – | yes | – | helper exists, unwired | O5 / W3 |
| Agent sets its own status line | – | worktree comment | custom status | derived headline only | O6 |
| Idle agent sessions released, resumed on return | – | hibernation | – | warm sessions kept | O7 |
| Blocked / working / idle / done-unseen with rollups | partial | yes | yes (strict) | working/done/failed | H1 |
| Resume provider sessions after a restart | – | yes | yes | fresh brief | H2 |
| Agents wait on servers, logs or other agents | – | `terminal wait` | `wait output` | no | H4 |

## Phase P: performance bugs

Found by reading the hot paths and measuring on this machine. Each one names the cost, the fix,
the test and the number to report.

**Measured and fine (no action):** the transcript builder (`SurfaceBuilder`): 43,248 synthetic
entries build in 18 ms cold, and a streaming frame costs 0.05–0.08 ms late in the run. Opening a
finished run on the fixture shows the stream in 82–211 ms.

### P1. The run stream re-reads files on every other run's traffic (daemon, M)

- **Where:** `run_stream` in `crates/tm-daemon/src/api.rs`, and `Live::subscribe`.
- **Bug:** each loop pass reads and parses the run's whole `events.jsonl` and reads the
  transcript, even when only transcript entries are new. The wake channel is one broadcast for
  all runs. A wake for another run does `continue`, which goes back to the top and reads both
  files again. A 250 ms tick re-reads them on every open run page. Cost grows as
  (open run pages) × (pushes per second across all runs): a 3-tab session with 8 streaming
  workers is hundreds of full reads per second.
- **Fix:**
  - Per-run `tokio::sync::watch` sequence counters (one for events, one for agent entries)
    in `Live`, replacing the global broadcast for feeds.
  - Read `events.jsonl` only when its counter (or `Store::stat`) moved.
  - On a wake for another run, never re-read.
  - Drop the 250 ms tick, or keep it only for writers in another process, gated by `stat`.
- **Test:** a counting `Store` wrapper: one open feed while another run pushes 1,000 entries
  does zero reads of the first run's files.
- **Measure:** reads per second with 3 feeds and the fixture streaming (`FIXTURE_DELAY_MS`).

### P2. Transcripts are 80% repeated agent tags (daemon + web, M)

- **Where:** `Live::push` in `live.rs` (writes the full `agent` tag on every entry); the
  readers in `model.ts` (`SurfaceBuilder.apply`) and `Tabs.tsx`.
- **Measured:** the largest transcript on this machine is 1.7 MB in 3,531 entries. 3,446 of
  them are `thought` fragments with an average tag of 340 B and average text of 82 B. Real
  transcripts: median 469 KB, max 1.7 MB.
- **Fix:**
  - Write the full tag once, on the call's `start`; later entries carry `{id, call}` only.
  - The reader keeps a map from call to tag.
  - Coalesce consecutive `text`/`thought` deltas of one call inside a ~50 ms window before
    writing.
  - The API expands old-format files the same way, so existing runs still open.
- **Test:** a transcript written in the new form opens with the same surfaces as the old form
  (fixture of each). Old files still read.
- **Measure:** bytes and entries for the fixture bookshop run before and after; target ≥3×
  smaller and ≥5× fewer SSE events.

### P3. Every streamed fragment is a synchronous open–append–close under one global mutex (daemon, M)

- **Where:** `Live::push` (`live.rs`), `FileStore::append` (`store.rs`), and
  `secrets::redact_json` → `policy::redact_text`.
- **Bug:**
  - One `std::sync::Mutex` (`write`) serializes every run's pushes and every note change.
    It holds file I/O on a tokio worker thread.
  - `redact_text` copies each string about 10 times per entry (two marker passes plus one
    per secret prefix), even when nothing matches.
  - A secret split across two deltas is not redacted at all.
- **Fix:**
  - A writer task per run, fed by an `mpsc` channel. It keeps a `BufWriter` open and flushes
    every 50 ms or 64 KB, or on `end`.
  - Notes get their own lock.
  - Redaction gets a fast pre-check (`memchr`/Aho–Corasick over the markers, prefixes and
    registered secrets) and returns the input untouched on a miss.
  - Redact after coalescing (P2), with a small carry window so split secrets are caught.
- **Test:** a secret split across two `text` deltas shows as `•••`. 10,000 pushes from 8
  tasks finish without blocking a concurrent `GET /api/runs`.
- **Measure:** push throughput and p99 latency of `GET /api/runs` while 8 workers stream.

### P4. The transcript cache never forgets (daemon, S)

- **Where:** `Live.cache` in `live.rs`.
- **Bug:** every entry of every run read since the daemon started stays in memory as
  `serde_json::Value`, about 5–10× its JSON size, for the life of the daemon.
- **Fix:** an LRU over runs (keep 8, drop runs idle more than 10 minutes), or keep line offsets
  and read lines on demand.
- **Test:** reading 20 runs keeps at most 8 cached.
- **Measure:** RSS after opening 20 large fixture runs.

### P5. Cold open sends the whole transcript; tool output rides the main stream (daemon + web, L)

- **Where:** `GET /api/runs/{id}/agents` (`api/desk.rs`), `useRun` in `web/src/run/model.ts`,
  `Step` in `Flow.tsx`.
- **Fix** (T3 Code's rules, `docs/internals/performance-regressions.md` in their repo):
  - **Bounded window.** `?window=recent` returns the newest entries up to about 1 MiB, never
    splitting a call, plus a one-line summary per older call. Older calls load on scroll or
    when a past turn is expanded.
  - **Bodies on demand.** `output` bodies over 2 KB leave the wire as `{id, bytes}`.
    `GET /api/runs/{id}/agents/{index}` returns one body when a step expands.
  - **Live output per row.** A running command's output streams only to a client that
    expanded that row: frames every 100 ms, a 16 KB append cap, then a tail replace
    (T3's `CommandOutputHub`).
- **Test:** a 900-call fixture run opens with at most 1.2 MiB over the wire. Expanding a step
  fetches its body. A 50 KB command output does not grow the main stream.
- **Measure:** bytes and time to first paint of the stream for the largest real transcript
  (1.7 MB) and for a synthetic 10 MB one.

### P6. The files panel's 483 KB chunk loads on every run open (web, S)

- **Where:** `web/src/files/FilesPanel.tsx` imports `PatchDiff` from `@pierre/diffs/react` at
  the top. The side panel opens by default on Files, so every run page pays for it (137 KB
  gzip) before anyone asks for a diff.
- **Fix:**
  - Import `PatchDiff` lazily, only in diff mode.
  - Mount the panel's body after the stream has painted (`requestIdleCallback`).
- **Measure:** bytes and script time on run open, before and after (Playwright `page.on("response")`).

### P7. The run page re-binds listeners and forces layout every frame (web, S)

- **Where:** `RunView.tsx`.
  - The global `keydown` effect depends on `list`, so it is removed and added on every
    streamed frame.
  - The auto-scroll effect sets `scrollTop = scrollHeight` on every `entriesVersion`, which
    forces a layout each frame.
- **Fix:**
  - Keep `list` and `focus` in refs, and bind once.
  - Pin to the bottom with a sentinel and `overflow-anchor`, or scroll only when
    `scrollHeight` changed, in one `requestAnimationFrame`.
- **Measure:** long tasks over 50 ms per second in a Performance trace while 8 fixture workers
  stream.

### P8. Expanded agent activity renders everything (web, S)

- **Where:** `AgentActivity` in `Flow.tsx`.
  - The "Raw model output" `<details>` renders and parses (`Rich`) every text block on every
    update, even while it is closed.
  - "Show earlier" renders every step with no limit.
- **Fix:**
  - Render the `<details>` body only when it is open.
  - Virtualize steps with `@tanstack/react-virtual` (already used by `CodeView`).
- **Measure:** commit time of one frame with a 3,000-step agent expanded.

### P9. The run list re-renders every row every 3 seconds while anything runs (web + daemon, S)

- **Where:** `useLoad(api.runs, …, 3000)` in `App.tsx` and `useLoad` in `ui.tsx`.
- **Bug:** while any run is active, each poll returns a new array. Every row is a new object,
  so every memoized row re-renders, and the whole list is `JSON.stringify`'d to compare.
- **Fix:**
  - Structural sharing by run id: keep the old object when its JSON is unchanged.
  - Better: `GET /api/runs?since=<seq>` returns only changed rows, and later an SSE list
    stream (T3's shell deltas).
- **Measure:** row renders per poll with 30 runs and one active (React Profiler).

### P10. Continuous animations (web, S)

- **Where:** `styles.css` has 8 `infinite` animations: spinners on each working agent, the
  sidebar and the header, plus shimmer, sheen, pulse and the caret. T3 Code treats a
  continuously repainting animation as a bug, because it pegs the GPU on high-refresh
  displays.
- **Fix:**
  - At most one animated indicator on screen.
  - Elsewhere, a static working glyph that changes on state change.
  - Pause animations of off-screen elements (`content-visibility: auto` on cards).
  - Check that `prefers-reduced-motion` and the Motion setting stop all eight.
- **Measure:** GPU and compositor time in a 10 s trace with 8 working agents.

### P11. Every run event refetches the whole ledger (web + daemon, S)

- **Where:** `useRun`. `reload()` fetches the run and the full ledger on every run event, and
  the ledger is fetched again on every agent `end`.
- **Fix:**
  - `GET /api/runs/{id}/ledger?after=n` returns new entries only.
  - Coalesce reloads within 100 ms.
- **Measure:** requests per minute on a busy run.

### P12. Guardrails so speed stays (S)

- A Playwright `perf.spec.ts` with budgets, running in CI:
  - home ≤ 12 API requests with 16 runs;
  - `POST /api/runs` ≤ 50 ms with a cached provider list;
  - run cold open ≤ 1.2 MiB;
  - ≤ 20 SSE events per second per text-streaming agent;
  - no request over 300 ms on home or run open.
- A daemon test asserting a feed's reads do not grow with other runs' pushes (P1).
- In `AGENTS.md`, a short "multiplicative paths" rule taken from herdr: before adding work to
  a per-entry, per-frame or per-feed path, name its frequency × cardinality.

## Phase W: wire up what is already written

These exist in `crates/tm-daemon/src/gates.rs` with no caller outside that file. Cheap, and
each one raises quality.

- **W1. Release gate** (`transcript_ran_tests`, `release_gate_nudge`).
  - When a worker that had a shell and tests available releases without running them, send
    the nudge once as a follow-up turn on its open session.
  - Call it where a worker's turn ends, before its checks (the `WorkerFinished` path in
    `engine.rs`).
  - Test: the fake harness that skips tests is sent back exactly once.
- **W2. Red/green proof** (`verify_red_green`), for Agent-mode turns and bug-fix prompts.
  - When the worker adds a test, run it on the base commit (it must fail) and on the result
    (it must pass).
  - A test that passes on the base is refused with a sentence.
  - Test: the fixture where the new test already passes on the base is refused.
- **W3. Best-of-N on hard units** (`pick_best_candidate`).
  - For `difficulty: hard` and within budget, two providers from the scorecard build in
    sibling worktrees. Checks, then tests, then the smaller diff pick the winner; the loser's
    worktree is removed.
  - Off by default.
  - Test: the winner is merged, the loser is gone, and the ledger has both calls.
- **W4. Contract mutation sanity** (`contract_mutation_sanity`).
  - Stub the implementation; a contract test that still passes is vacuous and goes back to
    its author.
  - Test: the fixture with a vacuous contract test is flagged.

## Phase A: agent experience (from T3 Code)

### A1. Queue or steer while a run works (M)

- **Today:** a note on an active run steers (the master reads it before the next worker). A
  note on a finished run starts a turn.
- **Add:** a `queued` flag on `Note` (`live.rs`). A queued note waits until the run settles,
  then all queued notes open one turn together, in their order and with their own mode.
- **Composer:** Enter uses the person's default (Settings → Chat: Queue or Steer), and
  Ctrl/Cmd+Enter does the other.
- **Thread:** queued notes show dashed at the end of the thread, with Edit, Remove and
  Send now. Alt+Up edits the last one.
- **Test:** two queued notes on a running fixture run start one turn after it finishes, and
  a steer note reaches the master before the next worker.

### A2. Edit from here: rewind a turn, with or without its files (L)

- **Checkpoints.** Before each turn starts (the person's `turn` entry and the first turn),
  capture the checkout into a hidden ref, `refs/taskmaster/checkpoints/<run>/turn/<n>`.
  Write a tree from a temporary index (`GIT_INDEX_FILE`, `git add -A`, `write-tree`,
  `commit-tree`), so untracked files are included and the person's branch gets no commit.
  This is T3's `CheckpointStore`.
- **Edit from here.** On a turn's message, this puts the message back in the composer and
  drops that turn and later ones from the run's view, with two choices:
  - **Keep changes** leaves the files as they are.
  - **Revert files too** restores the checkpoint.
- Refuse a file restore while another run builds in that checkout.
- Builds on the turns from this round and on `undo.rs`.
- **Test:** a run with two turns rewinds to turn 2: the files match the checkpoint, and turn
  3's agents are gone from the view and from the next brief.

### A3. Permission mode per run, in the composer (M)

- **Today:** `approvals.rs` has one policy, set in Settings.
- **Add:** a per-run mode stored on `RunRequest` (Supervised, Auto-accept edits, Auto, Full
  access), mapped onto the gate (`tm-harness/src/gate.rs`) and each provider's own flags.
  Where a provider cannot honour a mode, say so in the picker.
- The composer shows the mode next to the Auto/Code/Agent picker; the Settings value is the
  default.
- **Test:** an Auto-accept-edits run approves an edit without asking and still asks for a
  shell command.

### A4. Goals: keep going until it is true (M)

- `/goal <condition>` or a Goal field: "all tests pass", "the preview opens", "`cargo test`
  is green".
- After each turn, the daemon checks the condition: `test()`, the verify step, a command or a
  preview probe. If it is not met, it starts another Agent turn with the failure as the task,
  up to N turns or the budget.
- The run header shows the goal and its progress.
- **Test:** a fixture whose first agent turn leaves a failing test gets a second turn and ends
  green, and the cap stops a goal that never holds.

### A5. Suggested next prompt (S, Labs, off by default)

After a turn ends, one cheap call that reuses the prompt cache suggests the next message.
Tab fills it in.

### A6. Prompt stash and recall (S)

Ctrl/Cmd+S stashes the draft with its attachments; the same key on an empty composer restores
it.

### A7. Search inside runs from Ctrl+K (M)

- An inverted index of transcript text and notes per project, built incrementally from
  `agents.jsonl` (words of three letters or more).
- Palette results under "In runs" open the run at the matching entry.
- **Test:** a word from a worker's text finds that run and scrolls to the entry.

### A8. Server-owned settling (M)

- **Today:** Settle, Pin and Archive are browser-only (`prefs.runFlags`).
- **Add:** move them to the daemon. Auto-settle after a merged run or PR, or after N days idle,
  unless the run is set to Never.
- Settling stops the run's preview and removes its leftover worktrees.
- The sidebar reads the server's state, so every device agrees.
- **Test:** a merged fixture run settles itself, and a run set to Never does not.

### A9. Switch the model mid-run, with a handoff (M)

- Change a role's provider on a finished or paused run.
- The next agent gets a compact handoff (plan, decisions, diff stat, open failures) instead of
  the old provider's session.
- Uses the compiled working memory (`memory.rs`).

### A10. Let outside agents drive Taskmaster (M)

- An orchestrator MCP server beside the Dictionary MCP: `start_run`, `run_status`,
  `wait_run` (until done, failed or needs-you), `add_note`, `read_transcript` (bounded),
  `preview_url`. It uses a scoped token.
- Ship `skills/taskmaster/SKILL.md` (as herdr ships its skill) so Claude Code or Codex knows
  how to use it, and refuse when the token is missing.
- **Test:** an MCP client starts a fixture run, waits, and reads the result.

## Phase O: workspace (from Orca)

### O1. A project file for worktrees: `taskmaster.yaml` (M) — do this first in this phase

- Read from the project root, committed with the project:
  ```yaml
  setup: npm ci            # runs once in each new worktree, before its agent starts
  preview: npm run dev -- --port $PORT
  test: npm test
  shared: [node_modules, .venv, .cache]   # linked, not copied (generalizes borrow_node_modules)
  copy: [.env, .env.local]                # ignored files copied into new worktrees
  archive: ""                             # runs before a worktree is removed
  ```
- `setup` runs with `wait-for-setup` semantics, and its output goes to the run's terminal
  transcript.
- `preview` feeds `Previews` ahead of package.json scripts. `test` feeds `runtime.rs`.
- The person approves a shared file's commands once (trust by hash), like Orca.
- Settings → Project shows the file's state, with "Create from this project" to generate it.
- **Test:**
  - a fixture with `setup: touch .ready` has `.ready` in every worker's tree before its brief;
  - `copy: [.env]` puts the ignored `.env` in each worktree;
  - an unapproved changed `setup` asks first.

### O2. Design Mode in the preview (M)

- A toolbar toggle in the Preview tab. Hover highlights an element, and a click captures its
  CSS selector, trimmed outer HTML, key computed styles and a cropped screenshot. Use the
  existing browser (`browser.rs`) for the screenshot, or the iframe when same-origin.
- The capture lands in the composer as a chip and is routed to the worker that owns the
  source file (owned globs plus a source-map lookup when the dev server has one).
- **Test:** clicking the fixture's `<h1>` inserts a chip with its selector, and sending routes
  a note to the page's worker.

### O3. Batched diff review (S)

- Line comments in the Diff and Review lenses collect as drafts with a count.
- "Send N notes" sends one note per owning worker with every line anchor, as one turn.
- Comments stay pinned after the revision and can be resolved.
- **Test:** three comments on two pages become two notes and one turn.

### O4. Usage and limits before they bite (M)

- A status-bar meter per provider: the 5-hour and weekly windows and their reset times.
  Sources are each CLI's own reports (Claude's rate-limit status data, Codex rate-limit
  events, ACP usage); OAuth polling is opt-in and off by default.
- A warning at 80%.
- `Roles::resolve_auto` routes away from a provider near its limit.
- Typed limits stay as the backstop.
- **Test:** a fixture provider reporting 85% shows the warning and is skipped by Auto.

### O5. Race a request across providers (L)

- A "Race" choice in the composer: the same prompt runs as 2–3 runs on sibling branches
  (one per provider).
- A compare view shows checks, tests, diff size, cost and the preview side by side. Merge the
  winner; the others are removed.
- Builds on `POST /api/runs/{id}/fork` and W3.
- **Test:** a race of two fixture providers shows both results and merges only the chosen one.

### O6. Agents set their own status line (S)

- `status(text)` in `run_code` (and an MCP tool for harnesses without Code Mode) sets a
  one-line "what I am doing" shown on the agent card and the mission node, ahead of the
  derived headline.
- Kept in the transcript as a `status` entry.

### O7. Release idle warm sessions and resume them (S)

- Warm sessions (`Driver.warm`) idle for more than N minutes are closed.
- A repair that needs one resumes the provider session by id (`claude --resume` is already
  supported in `tm-harness/src/claude.rs`; Codex and ACP have session ids) instead of
  starting cold.

### O8. Let the master answer low-risk worker approvals (S, opt-in)

The master may approve a worker's approval request with a reason. The person still sees Allow
and Deny, and the first answer wins.

## Phase H: runtime (from herdr)

### H1. One state model, rolled up, with unread (S–M)

- Agent states: `working`, `blocked` (waiting on an approval, a question or a limit), `idle`,
  `done` (unseen until viewed), `failed`, `stopped`.
- A blocked agent makes its run show "Needs you" in the sidebar and on gallery cards. A run
  that finished while you were elsewhere stays bold until opened.
- An "explain" tooltip says why a state was chosen (for example "waiting on approval
  `bash: npm i`").
- Use strict rules: blocked only on a real pending approval, question or limit, never a
  guess.
- **Test:** a fixture approval turns the run "Needs you", and approving clears it. A finished
  run is unread until opened.

### H2. Resume provider sessions after a daemon restart (M)

- Record each call's provider session id (Claude session, Codex thread, ACP session) on its
  `start`/`end` entries.
- When `resume.rs` respawns an in-flight unit, it resumes that session with a short "the
  daemon restarted; continue" turn instead of sending the whole brief again. It falls back to
  a fresh brief.
- **Test:** a fixture worker interrupted by a restart is resumed with its session id, and the
  brief is not resent.

### H3. Keep agents alive across a daemon update (L, later)

- A small process holder owns agent processes and PTYs. A new `tm-daemon` reattaches to them
  after an update instead of killing them (herdr's `--handoff`, Orca's daemon).
- Only after H2.

### H4. Agents can wait (S)

- `wait({url|port|log|agent, timeout})` in `run_code`: wait for the dev server to answer, a
  log line to appear, or a sibling unit to pass.
- It replaces sleep loops and makes the preview check reliable.

### H5. Several machines, one run list (M)

- The sidebar merges runs from paired daemons (today they are only gallery cards), each with
  its own reconnect and its own "Needs you".

**Not needed:** herdr's screen-detection rules. Our agents speak structured protocols
(`stream-json`, the Codex app server, ACP). The PTY screen reader (`tm-harness/src/screen.rs`)
is only for installers and sign-in screens.

## Order

| Step | Items | Size | Why now |
| --- | --- | --- | --- |
| 1 | P1, P3, P4 | M | Daemon cost that grows with every run and tab |
| 2 | P2 | M | Transcripts 3–4× smaller, fewer SSE events; P5 builds on it |
| 3 | P6–P11, P12 | M | The run page and sidebar stay smooth; budgets stop regressions |
| 4 | P5 | L | Big runs open fast |
| 5 | W1–W4 | M | Quality from code already written |
| 6 | O1 | M | Worktree setup, shared deps and `.env`: preview and tests work in every worktree |
| 7 | H1, A1 | M | Never hunt for the stuck run; queue or steer |
| 8 | A2 | L | Rewind a turn, with files |
| 9 | O4, H2, O7 | M | Limits before they bite; resume instead of re-briefing |
| 10 | O2, O3, O6, H4 | M | Design Mode, batched review, status line, wait |
| 11 | A3, A4 | M | Per-run permissions, goals |
| 12 | A10, A7, A8 | M | Outside agents, search, server settling |
| 13 | O5, A9, A5, A6, O8, H5 | M–L | Race, model switch, conveniences, multi-machine list |
| 14 | H3 | L | Live handoff across daemon updates |

## How to run each item

1. Measure the "before" number named in the item. Write it in the commit message.
2. Write the test that fails without the change.
3. Make the change.
4. Run the checks:
   `cargo clippy -p tm-daemon -p tm-core --offline --all-targets -- -D warnings`,
   `cargo test -p tm-core -p tm-daemon --offline --lib`, `npm run typecheck --prefix web`,
   and `cd web && npm run e2e`. Two tm-daemon tests fail without `python3-venv`; that is the
   baseline.
5. Check the screen in the browser against the fixture daemon (the `web-ui` skill).
6. Measure "after" and commit. Do not push.

## Risks

- **Changing the transcript format (P2, P5)** touches every reader: the web stream, the
  terminal tab, `close_open_calls`, tests and exports. Keep reading the old form, and add a
  format version on `start` entries.
- **Checkpoints (A2)** write hidden refs in the person's repository. Prune them when a run is
  removed, and never restore files while another run uses the checkout.
- **`taskmaster.yaml` (O1)** runs commands from a file in the repository. Ask once per file
  hash, as Orca does, and never run it for a cloned repository before the person approves.
- **Racing and best-of-N (O5, W3)** multiply spend. They stay behind the budget cap and are off
  by default.
- **Usage polling (O4)** must not send tokens anywhere new. Read what the CLIs report; OAuth
  polling is opt-in.

## Sources

- T3 Code: [pingdotgg/t3code](https://github.com/pingdotgg/t3code). Read:
  `docs/internals/overview.md`, `docs/internals/performance-regressions.md`,
  `docs/internals/web-ui.md`, `docs/user/composer.md`, `docs/user/permission-modes.md`,
  `docs/user/thread-sidebar.md`, `apps/server/src/checkpointing/CheckpointStore.ts`,
  `apps/server/src/orchestration-v2/CommandOutputHub.ts`.
- Orca: [stablyai/orca](https://github.com/stablyai/orca). Read: `README.md` and
  `docs/site/content/docs/` (`cli/orchestration`, `model/orca-yaml`,
  `cli/worktree-checkpoints`, `browser/design-mode`, `review/annotate-ai-diff`,
  `recipes/parallel-agents`, `agents/hibernation`, `agents/usage-tracking`,
  `model/session-restore`, `agents/hooks-memory`).
- herdr: [ogulcancelik/herdr](https://github.com/ogulcancelik/herdr). Read: `README.md`,
  `AGENTS.md` (multiplicative performance paths), and docs 0.7.3 (`agents`, `agent-skill`,
  `session-state`, `integrations`, `socket-api`).
