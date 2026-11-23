<p align="center">
  <img src="../assets/logo.png" alt="Taskmaster" width="420">
</p>

<h3 align="center">Describe the product once. Let many models build it, and have a different one check the work.</h3>

<p align="center">
  <img src="../assets/screenshots/01-gallery-desktop.png" alt="The Taskmaster gallery of runs" width="860">
</p>

<p align="center">
  <a href="https://github.com/narensen/Taskmaster/actions/workflows/ci.yml"><img src="https://github.com/narensen/Taskmaster/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/narensen/Taskmaster/releases/latest"><img src="https://img.shields.io/github/v/release/narensen/Taskmaster?label=release" alt="Latest release"></a>
  <img src="https://img.shields.io/badge/platforms-Linux%20%7C%20macOS-black" alt="Platforms">
  <img src="https://img.shields.io/badge/license-MIT-black" alt="MIT license">
</p>

<p align="center">
  <a href="https://github.com/narensen/Taskmaster/releases/latest"><b>Download</b></a> ·
  <a href="#quickstart">Quickstart</a> ·
  <a href="#features">Features</a> ·
  <a href="#the-app">The app</a> ·
  <a href="#configuration">Configuration</a>
</p>

## Download

| Platform | Get it |
| --- | --- |
| Ubuntu / Debian | `Taskmaster_<version>_amd64.deb` from [the latest release](https://github.com/narensen/Taskmaster/releases/latest), then `sudo apt install ./Taskmaster_<version>_amd64.deb` |
| Any Linux | `Taskmaster_<version>_amd64.AppImage`, then `chmod +x` and run it |
| Linux server (headless) | `taskmaster-<version>-x86_64-linux.tar.gz`: `tm-daemon`, `tm-review`, `tm-commons` and the built app |
| macOS | Build from source with `scripts/install.sh` (see [Install](#install)) |

Every release ships `SHA256SUMS.txt` and signed updater artifacts. The desktop app updates itself from GitHub Releases.

## What it is

Taskmaster is a multi-model coding harness. You describe the product once. A planner locks the contracts and the Dictionary. Workers build one page each, in parallel, and cite only those locked entries. A different model reviews the connections, and another confirms the findings before a page is asked to repair them.

The run is a state machine. The models do the writing. Taskmaster decides the next step and records it, so a crashed run resumes from the log instead of from a model's memory.

## Features

- **Locked plan.** Contracts and Dictionary entries are committed before any page is built, and each page is granted only the keys it cites.
- **Parallel pages.** Each page builds in its own git worktree on a run branch, `taskmaster/<run id>`. Only files matching the page's glob are merged. Up to four build at once.
- **Five harnesses.** `claude`, `agy`, `grok`, `codex` and `opencode`, each assigned per role (master, mid, worker, reviewer, verifier).
- **Independent review.** The reviewer and the verifier are different harnesses, so one model does not grade its own page.
- **Checks that cost nothing.** `tsc`, eslint, clippy, ruff, the project's own `build` and `test`, and headless-Chrome checks of a Next.js app, run before a run finishes.
- **Per-tool approval.** `approvals.mode` can ask a person before risky tool calls (shell, network, writes outside the worktree) instead of refusing them.
- **Live preview.** A run's dev server can be started from the app and is stopped with the run.
- **Token ledger.** Every agent call is recorded with input, output, cache-read and cache-write tokens, seconds, and price when the harness reports one; the run view splits them per agent.
- **Providers in the app.** Install, update and sign in to each harness CLI from the Providers page, with the installer's output and the sign-in code shown live. Node, Bun and Python install from there too.
- **One skills store.** SKILL.md skills are searched, installed, switched and updated in one store every harness shares; the planner attaches them to the pages they fit, and `$` in the composer names one.
- **A browser for workers.** A worker on a web page can open, click, type into, screenshot and read the console of its own page's preview, and nothing else.
- **Mid-tier checker (optional).** A second, cheaper look at each finished UI page: the diff, a screenshot, the brief and the done line, answered with pass or a short fix list. Runs beside the build.
- **Code Mode (optional).** Workers get `run_code`, one sandboxed JavaScript call for many steps; Claude Code workers use its own code mode.
- **Dictionary and Commons.** An immutable, content-addressed store of contracts, plus a shared Commons you can opt in to.
- **Errorography.** Each model's repeated mistakes are handed back to it on its next attempt.
- **Resumable.** `events.jsonl` is the run; replaying it rebuilds the state.
- **Review any repo.** `tm-review` reviews a diff, a range or a pull request without a Taskmaster plan.
- **Desktop and phone app.** A desktop app for macOS and Linux that starts and stops tm-daemon and updates itself from GitHub Releases, and the same React app served by the daemon; pair a phone over Tailscale.

## Screenshots

| | |
| --- | --- |
| ![Locked plan awaiting approval](../assets/screenshots/03-locked-plan-approval.png) | ![Live preview tab](../assets/screenshots/06-preview-tab.png) |
| **Locked plan.** Approve or reject with a note. | **Preview.** The run's dev server beside the run. |
| ![Diff with a comment on a line](../assets/screenshots/08-diff-gutter-comment.png) | ![Command palette](../assets/screenshots/14-palette.png) |
| **Diff.** Comment on a line in the gutter. | **Palette.** Jump to runs, pages and commands. |

<p align="center">
  <img src="../assets/screenshots/21-phone-run.png" alt="A run on a phone" width="300"><br>
  <em>The same app on a phone.</em>
</p>

## Quickstart

You need `cargo` and `git`. Install the CLIs of the harnesses you want to use; `tm-daemon providers` prints the install and sign-in commands.

```bash
scripts/install.sh                       # builds tm-daemon and tm-review, plus the app if npm is present
tm-daemon . --prompt "A small notes app with a list and an editor."
tm-daemon . --yes                        # approve the recorded plan and build it
```

Or use the app:

```bash
cd web && npm ci && npm run build
tm-daemon serve . --web web/dist         # http://127.0.0.1:8737, pair with the printed code or link
tm-daemon serve . --lan                  # reachable from the local network and the tailnet too
```

The Connect page (`/connect`) pairs other devices with a one-time link or QR code, lists and
revokes paired devices, adds other machines by their pairing link, and gives outside agents an
MCP server at `/mcp` with a token of their own. The API behind it:

```text
GET  /api/connect               how this daemon can be reached: one route per address
POST /api/pairing               {label?, kind?, ttl_secs?} -> a one-time code and a link per route
POST /api/pair                  {code, label?, kind?} -> {token}: the printed code or a link's code
GET  /api/devices               paired devices, the caller marked `current`
POST /api/devices/{id}/rename   {label}
POST /api/devices/{id}/revoke   its token stops working at once
POST /api/devices/agent         {label} -> a token for an outside agent, shown once
POST /mcp                       MCP (JSON-RPC over HTTP): taskmaster_list_runs, _start_run, _run_status,
                                _wait, _read_run, _send_note, _approve, _cancel, _preview
```

`tm-daemon` exits 0 when the run finishes clean, 1 when it fails, 2 when the plan is waiting for `--yes`, and 3 when a provider stopped it (a usage limit, no balance left, a sign-in); run the same command again to resume. `--timeout 0` lets an agent call run with no time limit.

## Architecture

```text
 web (React + Vite) ──HTTP/SSE──┐
 tm-daemon CLI ─────────────────┤
                                ▼
                         tm-daemon ── executor, ledger, API, git worktrees
                          │   │   │
          decides next ───┘   │   └── tm-dictionary (store, Commons, MCP)
          tm-core             │
          (no I/O)            └── tm-harness ──► claude · agy · grok · codex · opencode
                                  tm-verify (globs, import graph)
                                  tm-review (any repo)

 tm-commons ── shared Commons + sign-in service (tm-token: Ed25519 JWTs)
```

| Crate | Role |
| --- | --- |
| `tm-core` | The plan, the Dictionary rules, and the run state machine. No I/O. |
| `tm-harness` | Headless adapters for Claude Code, Antigravity, Grok, Codex, and OpenCode. |
| `tm-daemon` | The executor, the ledger, and the API. The binary is `tm-daemon`. |
| `tm-commons` | The Commons and sign-in service everyone shares. The binary is `tm-commons`. |
| `tm-dictionary` | The local store, the Commons client, and the MCP server. |
| `tm-verify` | Ownership globs and the import graph. |
| `tm-review` | Review of any git repository. The binary is `tm-review`. |
| `tm-token` | Sign-in tokens: Ed25519 JWTs and the published key. |
| `web/` | The app (React, TypeScript, Vite). Not a crate. |

## A run

1. **Plan.** `--prompt` asks the Taskmaster harness for the contracts, the pages, and the Dictionary entries. `--plan` uses a plan file and skips the model.
2. **Approve.** Without `--yes`, Taskmaster prints the plan and exits. Nothing is built.
3. **Lock.** The contracts are committed, and each page is granted the Dictionary keys it cites.
4. **Build.** The run works on its own branch, `taskmaster/<run id>`, in a checkout outside your working tree. Each page gets its own git worktree, and only files matching that page's glob are merged into the run.
5. **Review.** One reviewer reads each Dictionary connection the changed pages cite, then one security pass reads the dirty pages.
6. **Verify.** A second harness confirms the findings. Confirmed findings go back to the owning page. Then the project's own tools check what was built, and an optional `.taskmaster/verify` program checks the whole tree.

```bash
cargo build --release -p tm-daemon -p tm-review
tm-daemon . --prompt "A small notes app with a list and an editor."
tm-daemon . --yes
```

The first `tm-daemon` command records the plan and exits 2. The second approves that recorded plan and builds it. Pass `--yes` on the first command to approve immediately.

A rejected plan is repaired, not planned again: a short call with no tools gets the plan and the reasons and changes only what they name, up to two times. A reply with no plan in it at all is planned once more. An existing Dictionary entry may be cited. Different content for the same key is refused.

When the run finishes clean, its branch is merged into whatever the project has checked out and then deleted. A run that fails stays on its branch, and so does one git cannot merge cleanly; either way the project is left as it was. A repository with no commit yet has nothing to branch from, so its first run builds in place.

## Starting a project

`--template next` gives a new project its shared files: Next.js 15 with the App Router, React 19 and strict TypeScript. The planner is told the layout, so every page lands in `app/<page id>/` and shared code goes through contract files. A file the project already has is left alone. Any directory works as a template; a `TEMPLATE.md` in it is the note the planner reads.

```bash
tm-daemon . --template next --prompt "A small notes app with a list and an editor."
npm install
```

Taskmaster does not run `npm install`. Once the packages are there, later runs typecheck against them.

## Who does the work

Taskmaster picks no provider for you. Each role (`--master`, `--mid`, `--worker`, `--reviewer`, `--verifier`) takes the harness you name on the command line, or the one saved for the project in `.taskmaster/config.json` under `roles`; a run with a role left unchosen stops and says which. In the app, New Run and the gallery show a provider and model picker for each role. The first time every role is chosen, the answer is saved for the project; after that a choice in New Run applies to that run only, unless you tick "Remember for this project". Settings changes the saved choice.

```bash
tm-daemon providers
tm-daemon . --master grok --master-model grok-4.7 --prompt "A small notes app with a list and an editor."
```

`providers` asks every harness which models it offers right now. The JSON says whether the CLI is installed, the command that installs it, the command that signs in, and the model ids. Install and sign-in stay in your hands. Taskmaster prints the commands.

A `--master-model`, `--worker-model`, `--mid-model`, `--reviewer-model`, or `--verifier-model` must be an id that provider just returned. Leave the flag off and the harness keeps its own default.

The harnesses are `claude`, `agy`, `grok`, `codex`, and `opencode`.

## The brakes

A page that fails its checks is sent back with the errors: the failing output itself, the calls the gate refused with what they asked for, and, when the attempt changed nothing, that it changed nothing and its last words. It gets two repairs on the worker, then two on the mid model, then two on the master. After that, the page fails. When worker, mid and master are the same harness and model, climbing the ladder would only repeat the attempt, so there is none: two focused repairs, then the page fails. An attempt that changed nothing after a shell command or a fetch was refused is not retried at all: the same refusal would meet every retry, so the page fails at once and says what it needed. A Dictionary entry can be amended once in a run. Review runs for two rounds, and the round after the last one goes straight to verification.

The reviewer and the verifier are different harnesses, so one model does not grade its own page. Findings the verifier rejects are dropped. A write outside the page's glob fails the attempt, including a write to a locked contract. Up to four pages build at once. `--parallel` changes that for a new run. A resumed run keeps the parallelism it started with.

## Checks that cost nothing

Before a run finishes, the files it built are typechecked and linted with tools the project already has: `tsc` from `node_modules`, eslint when it is configured, clippy for Rust, ruff for Python. Only errors count. A type error, a compile error, or a lint rule the project set to `error` goes back to the page that owns the file, and into that model's Errorography. Style warnings do not. An error in a shared file the run wrote, a contract or a template file, goes to the master, which may edit those files and nothing else. It gets two such repairs in a run; if the file still fails, the run fails. An error in a file the run did not write was there before, so it is printed and left alone. Taskmaster installs nothing here.

When the types are clean, the project is run. Its own `build` and `test` scripts run through the package manager its lockfile names (npm, pnpm, yarn or bun). A Rust project gets `cargo test`. Any Python tests (a `tests/` directory, or `test_*.py` files) are run with pytest through the project's own runner when it has one (`poetry run`, `uv run`, `hatch run`), or else from pip. Either way they run in a venv the run makes for itself, `.taskmaster/runs/<id>/venv`, with the project and its dev or test extras (or its `requirements*.txt`) installed, and with `VIRTUAL_ENV`, `PYTHONPATH`, `CONDA_*` and any other venv on `PATH` stripped from the environment. The venv is kept for the rest of the run and refilled only when a dependency file changes.

Each failure goes back to the page whose file or route the output names. A failure that names a shared file the run wrote goes to the master. A failure that names neither also goes to the master, with its output, and the master may change any file to fix it. The run gets two such repairs; if the checks still fail after them, the run fails: it exits 1, and its branch is kept and not merged. A failing check never passes in silence. A project that has tests whose tests could not be run at all is not verified either: the run prints `verify: nothing ran` and fails.

A Next.js app that built is then started and opened in headless Chrome, one page per route, over the DevTools protocol. A dynamic segment such as `[id]` gets a sample value, and its 404 is not counted against it. The page gets back:
- an error status;
- an uncaught exception;
- a `console.error`;
- a resource that failed to load.

A screenshot of each page is kept, and the app shows it next to the page. Chrome is found on `PATH`, or at `$TASKMASTER_CHROME`. Only units a browser shows are opened: a plan for a library, a command-line tool, a server or a migration has no routes, no design system and no browser check. `--no-runtime-checks` skips all of this.

## What a plan is

A plan names its `kind`: `web`, `library`, `cli`, `server` or `migration`. Its `units` are the pieces of work, each one agent (or several, when it is split into `parts`) owning one glob. Only a unit a browser shows (every unit of a `web` plan, or one marked `"ui": true`) has a `route`, and only a plan with such a unit has a `design`. Contracts are interfaces that more than one unit imports, never implementations: signatures, types, constants and schemas with stub bodies, at most 120 lines each and 400 in all, and never a file a unit owns. The planner is shown the same list of rules the plan is checked against, and a field with one sensible fix (a route on a module, a design on a library, a path written `./x`) is fixed in place rather than sending the plan back. A unit whose files are already right and whose agent changes nothing is done, not failed.

## Phases and PROJECT.md

A job bigger than a small one is sketched first. The first planning call returns 2-6 phases in build order, each with its goal, the files it delivers, its interfaces as signatures only, and the command that runs its tests, and plans only the first phase in detail. Small jobs (a dependency upgrade, a fix, or fewer than about five files) are one phase with one or two units, as before.

One phase builds at a time. When it passes the project's checks, Taskmaster writes what it landed into `PROJECT.md` at the project root (from the change log, so it records what actually happened), keeps a checkpoint of the run's branch under `refs/taskmaster/<run>/phase-<n>`, and adds the next phase's units. A phase that needs only the interfaces before it is planned while that phase builds, so its workers start as soon as it passes. A project that has a `PROJECT.md` of its own keeps it; Taskmaster's is then not written. The run view lists the phases under the master, each with its state.

Every builder's brief opens with the same text, word for word: the project kind, `PROJECT.md`, the locked contracts in full in path order, the design for web units, and the shared rules. Its own task follows. Providers cache a prompt's leading text, so that shared part is served from their cache. The task ends with a short list to work down, which ends with running the tests. A planner lists only this project's Dictionary entries and the Commons entries the request names or shares two words with.

## Repairs go back to the same agent

A builder's session is kept open after its turn when its harness can (`agy`, `grok` and `opencode`), up to eight at a time. When that unit comes back for repair on the same tier, the failure is sent to it as a follow-up turn: it still has its task, its files and the provider's cache. Its worktree first gets every file the run's tree changed since, and a file left the same as the tree is never counted as the unit's work. If a repair by the master breaks units that had passed, its files are put back as they were at the checkpoint taken just before it.

## A worker's shell

A builder whose harness runs inside the sandbox (bubblewrap for `agy`, `grok` and `opencode`, Codex's own sandbox for `codex`) gets a shell. The whole filesystem is read-only to it except its worktree, `/tmp`, an empty home, its own scratch directory and the run's package caches under `.taskmaster/runs/<id>/`; toolchains under the home directory (`~/.local/bin`, `~/.cargo/bin`, `~/.rustup` and the like) are mounted read-only so they still run. It is told to run the tests for its files before it says it is done. Fetch tools reach only the package indexes (PyPI, npm and crates.io). Test caches, `__pycache__`, venvs and `node_modules` it leaves behind are never counted as its work. Where bubblewrap cannot run, or for Claude Code, workers have no shell, and the run log says so once. Settings, under Worker sandbox, turns either off (`sandbox.shell` and `sandbox.package_indexes` in the settings file). A plan that is not a web app names its agents `unit:<id>` rather than `page:<id>`.

## Handing over

Each attempt at a page is kept in `.taskmaster/runs/<id>/attempts/<page>.jsonl`: the model, what it was asked to fix, the files it changed, what it said when it finished, and why it was not kept. When a page goes back for repair, or up to a stronger model, the next attempt is given the last two. A change that was thrown away is included as a diff, since it is not in the tree. A kept one is already in the files, and the brief says to build on it.

## What a run cost

Every agent call is written to `.taskmaster/runs/<id>/ledger.jsonl`: the role, the unit and the agent (each part of a split unit apart), the harness, the model, the tokens, the seconds, the tool calls the gate refused, and the price with where it came from. A price the harness reports is used: over ACP, the reply's cost or what the session's running cost grew by. Otherwise the call is priced from the price table, by model and then by harness: `prices` in the settings file, dollars per million tokens for `input`, `cached_input` and `output`. Grok's are built in ($2, $0.50 and $6). A call with neither is "unpriced", never `$0.00`; a free model that reports no charge says so.

The estimate is not the planner's guess. On a first run it is sized from the plan: each unit, a module with its tests or a web page, the repairs a unit needs on average, every later phase of a sketch, and the contracts every builder carries. After that it is worked out from what build calls cost in this project's earlier runs. Cache reads are estimated and shown apart from fresh tokens: a harness re-sends its own prompt on every step, the provider serves that from a cache at a reduced rate, and it would otherwise swamp the tokens that reflect the work. The last line a run prints is the total next to that estimate. `status.json` has the same totals split by role, model and agent.

`tm-daemon report [<project>] [--run <id>]` prints where a run's time went, from its own records: planning, each unit's build with its repairs, the project's checks, idle time and the total, then the planning share, tokens, the cache-hit ratio, the cost, and the tool and `run_code` calls. `--json` prints it as JSON, and `--append <file>` adds it as one line to a results file such as `bench/results.jsonl`.

The run's log says when an agent has sent nothing for two minutes (`calc: no output for 120s`), and again every two minutes while it stays quiet. `TASKMASTER_ACP_TRACE=<file>` writes every line to and from an ACP agent (`agy`, `grok`, `opencode`) to that file. The transcript, `agents.jsonl`, keeps each brief whole and one line per streamed message.

**The event log.** `.taskmaster/runs/<id>/events.jsonl` is the run. Replaying it rebuilds the state. The default id is `current`.

## Optional: the checker and Code Mode

Both are off until you switch them on in Settings (or `checker.enabled` and `code_mode.enabled` in the settings file).

The **checker** gives a page that just passed its own checks one read-only call on the mid role's model, but only when the page changes UI files, is large, or needed a stronger tier. It reads `diff.patch`, a screenshot of the page as the worker left it, the brief, the done line and the worker's last words, and answers `pass` or up to five fixes. Those fixes join the verify step's errors, so the page is repaired the usual way. Each page is checked once, the check runs beside the rest of the build, and verification waits at most 20 seconds for a check still running.

**Code Mode** gives every worker and every repair `run_code`: an async JavaScript body with `read`, `readMany`, `write`, `edit`, `grep`, `ls`, `sh`, `test`, `check` and `browser`, run in Node with its permission model on (no files, processes or workers of its own) and, under bubblewrap, no network. `test()` runs the project's own tests in the worker's sandbox and returns `{ok, failed, output}`; `sh()` and `test()` run only inside the sandbox. Workers are told to batch their reads, edits and checks into a few scripts that each end with `return await test()`. A script may run 600 seconds and one `sh()` or `test()` call 300; `code_mode.script_secs` and `code_mode.sh_secs` change that. Every call goes to the same permission gate as the worker's own tools. Claude Code workers instead run Claude Code's native code mode (`CLAUDE_CODE_EXPERIMENTAL_CODE_MODE=1`) in an interactive session on a terminal.

`scripts/bench.sh` builds one locked plan under different settings, and runs a single-agent baseline, writing each result to `bench/results.jsonl`.

## Eval

`--eval` runs the same plan again on one baseline harness and writes `.taskmaster/evals/<id>.json`. Each side reports its pages, its check failures, its tokens, and its cost. A prompt is planned once, and the baseline reuses that plan. The baseline starts from the same commit. It is measured and then discarded: the project keeps the Taskmaster result. The baseline defaults to the master harness. The comparison runs after the plan is approved.

```bash
tm-daemon . --prompt "A small notes app with a list and an editor." --eval --baseline codex --yes
```

## The API

`serve` keeps the daemon up with no run of its own. A frontend starts, approves, rejects, resumes and cancels runs through it, reads the event log and the ledger, searches the Dictionary, and starts reviews.

```bash
tm-daemon serve .
tm-daemon serve . --tailscale
```

The first command listens on `127.0.0.1:8737`. `--tailscale` listens on this machine's Tailscale address, which is how a phone reaches it. The terminal prints a pairing code. A device posts the code once and gets a token; every other call sends that token. Paired devices stay paired across restarts, and only a hash of each token is stored.

**Updating while agents work.** Builders are started by a small holder process (`tm-daemon hold`, started by `serve` on its own), not by the daemon itself. Send `serve` `SIGUSR1` and it hands them to the next daemon: it stops reading at a line boundary, the holder keeps them working for up to ten minutes, and the daemon exits with status 75. The next `tm-daemon serve` for the same project picks each builder up where the last one stopped and carries the run on, with nothing in its transcript said twice and nothing already answered asked again. The desktop app does this when it installs an update. Under systemd, `systemctl --user reload taskmaster` does it, and the unit restarts on status 75. `Ctrl-C`, `SIGTERM`, and a crash still stop every agent the daemon started; a crashed run resumes each builder's provider session when it is resumed. The holder's socket is `$XDG_RUNTIME_DIR/taskmaster/hold.sock` (or `~/.local/state/taskmaster/hold.sock`); `TASKMASTER_HOLD=<path>` moves it, and `TASKMASTER_HOLD=off` starts every agent in the daemon as before.

```text
POST /api/pair                  {code} -> {token}
GET  /api/runs                  every run, with its phase, pages and spend
POST /api/runs                  {prompt | plan, id?, yes?, roles?, models?, parallel?, template?}
GET  /api/runs/{id}             one run, with its plan
GET  /api/runs/{id}/events      ?after=<n>
GET  /api/runs/{id}/stream      the same events as they happen
GET  /api/runs/{id}/ledger
GET  /api/runs/{id}/changes
POST /api/runs/{id}/approve
POST /api/runs/{id}/reject      {prompt?} plans again when a prompt is given
POST /api/runs/{id}/resume
POST /api/runs/{id}/cancel
GET  /api/providers
GET  /api/templates
GET  /api/dictionary            ?q=<text>
GET  /api/dictionary/{key}
GET  /api/errorography
GET  /api/reviews
POST /api/reviews               {target?, base?, head?, pr?, dirty?}
GET  /api/reviews/{id}
GET  /api/commons/entries       this daemon's Commons, for another daemon to pull
POST /api/commons/entries       an entry another daemon promoted
```

`stream` is server-sent events: one `event` per entry in the run's log, then `idle` with the phase once nothing is driving the run. A browser's `EventSource` cannot send a header, so this one route also takes `?token=`.

Each run is on its own branch, so several can build at once. `--pair` on a run serves the same API on the tailnet for as long as that run lasts.

## The app

`web/` is the frontend: React and TypeScript, built with Vite. The daemon serves it, so there is nothing else to run. It works the same on a phone and on a desktop, and a phone can add it to the home screen.

- Pair with the code, or sign in with an account when accounts are set up.
- Start a run: the prompt, a template, how many pages at once, and the harness and model for each role.
- Read the plan, then approve it, change it, or cancel. Follow a run as it happens.
- See what a run cost by role, by model, and by call, with cache reads apart.
- Read what each run committed, search the Dictionary, read the Errorography, and start and read reviews.

```bash
cd web && npm ci && npm run build
tm-daemon serve . --web web/dist
```

Without `--web`, the daemon serves `$TASKMASTER_WEB` or `~/.local/share/taskmaster/web`, where the install script puts the build. With neither, `/` is a small built-in page that pairs, lists runs, and approves a plan.

To work on the app, run `npm run dev` in `web/` next to `tm-daemon serve`. Vite sends `/api` to `127.0.0.1:8737`, or to `$TASKMASTER_DAEMON`. A build served from somewhere else can name the daemon on its sign-in screen; the API allows any origin, because it authenticates with tokens, not cookies.

## Configuration

Settings live in `.taskmaster/config.json` in the project, or `~/.taskmaster/config.json`. Without the file nothing changes. Copy `config.example.json` and fill in one part at a time; a placeholder counts as not set. Never commit real tokens.

| Key | Default | Meaning |
| --- | --- | --- |
| `store.kind`, `store.path` | `files` | `"sqlite"` keeps records in one SQLite file (`.taskmaster/taskmaster.db`). |
| `auth.issuer`, `auth.allow` | pairing code only | Also accept Commons sign-in; only accounts in `allow` (`github:<login>`, `google:<email>`) get in. |
| `commons.remote`, `commons.token`, `commons.share`, `commons.report` | local Commons | Connect to the shared Commons; `share` sends promoted entries, `report` reports pass results. |
| `approvals.mode`, `approvals.timeout_secs` | `never`, 600 | `never` refuses risky calls, `risky` asks first, `always` asks for every tool call. Unanswered questions are refused after the timeout. |
| `editor` | `$VISUAL`, then `$EDITOR` | The editor the app's Open action uses. |
| `preview.command` | the project's `dev` script | Command that starts the preview server; it gets the port as `$PORT`. |

Environment variables:

| Variable | Used by | Meaning |
| --- | --- | --- |
| `TASKMASTER_WEB` | `tm-daemon serve` | Built app to serve when `--web` is not given (else `~/.local/share/taskmaster/web`). |
| `TASKMASTER_DAEMON` | `npm run dev` in `web/` | Daemon the Vite dev server proxies `/api` to (default `http://127.0.0.1:8737`). |
| `TASKMASTER_CHROME` | runtime checks | Chrome binary, when it is not on `PATH`. |
| `TASKMASTER_GH` | pull-request actions | GitHub CLI program, default `gh` (mainly for tests). |
| `TM_COMMONS_URL`, `TM_COMMONS_BIND`, `TM_COMMONS_SESSION_SECRET`, `TM_COMMONS_SIGNING_KEY`, `TM_COMMONS_MODERATORS`, `TM_COMMONS_DAILY_LIMIT` | `tm-commons` | Server settings; see `deploy/commons/`. |

### The Taskmaster Commons

`tm-commons` is one service for everyone. It signs people in with GitHub or Google, and serves the entries people chose to share. Your Taskmaster pulls those entries before it plans, and deletes any entry the Commons has withdrawn.

Sharing works like this:
- With `share` on, your Taskmaster sends each entry your runs promote.
- Before anyone else gets an entry, the service refuses anything that looks like a credential, and a moderator approves it in the weekly review.
- `report`, on by default, says whether pages that used a Commons entry passed. That is how an entry earns its pass rate in other people's runs.
- Moderators withdraw entries that are reported or that keep failing, and can ban someone.

The service signs sign-in tokens with an Ed25519 key and publishes the public half. A Taskmaster checks tokens with that public key alone, and holds nothing that could make one.

`deploy/commons/` has what the server needs: how to set it up, the environment file with placeholders, a systemd unit and a Caddy config. A daemon can also act as a hub for a group, through `GET` and `POST /api/commons/entries`.

## Review any repository

`tm-review` does not need a Taskmaster plan. It checks out the commits under review and writes the context into that checkout: how the code fits together, what changed, the project's rules, and what earlier reviews of those files found.

```bash
tm-review . --dirty
tm-review https://github.com/org/repo --pr 12
```

Give it a repository or a request on GitHub, GitLab, Bitbucket, Gitea, Forgejo, Codeberg, Azure DevOps, or SourceHut. `--pr` reviews that request on the local `origin`. `--base` and `--head` select a range. The import graph is built from the repo. Clippy, `tsc`, eslint, and ruff run when the diff speaks those languages and the tool is there. `--install-lints` installs clippy when it is missing, and nothing else. A second harness verifies the findings. Confirmed and rejected findings are appended to `.taskmaster/reviews/history.jsonl`.

Rules for the whole project live in `.taskmaster/REVIEW.md`. A `REVIEW.md` further down the tree covers that directory and everything under it. The reviewer defaults to `claude` and the verifier to `agy`.

## Commands

```text
tm-daemon providers [--harness <h>]

tm-daemon serve <project> [--bind <addr>] [--tailscale] [--dictionary <dir>] [--commons <dir>] [--web <dir>]

tm-daemon <project> [--plan <file> | --prompt <text> | --prompt-file <path>] [--yes]
          [--worker <h>] [--mid <h>] [--master <h>]
          [--reviewer <h>] [--verifier <h>]
          [--worker-model <m>] [--mid-model <m>] [--master-model <m>]
          [--reviewer-model <m>] [--verifier-model <m>]
          [--parallel <n>] [--timeout <secs>] [--dictionary <dir>] [--commons <dir>]
          [--run <id>] [--template <next | dir>] [--eval] [--baseline <h>] [--pair]
          [--no-runtime-checks]

tm-commons serve | check | keygen      the Commons service; see deploy/commons

tm-review <repo | url> [--base <rev>] [--head <rev>] [--pr <n>] [--dirty]
          [--reviewer <h>] [--verifier <h>]
          [--reviewer-model <m>] [--verifier-model <m>] [--install-lints] [--keep]
```

`tm-daemon` exits 0 when the run finishes clean, 1 when it fails, 2 when the plan is waiting for `--yes`, and 3 when a provider stopped it (a usage limit, no balance left, a sign-in); run the same command again to resume. `--timeout 0` lets an agent call run with no time limit.

Commits the daemon makes need `user.name` and `user.email` on that repository. Taskmaster does not write git config.

## Install

```bash
scripts/install.sh
```

This builds both binaries and copies them to `~/.local/bin`, or to the directory you pass. It needs `cargo` and `git`. With `npm` present it also builds the app into `~/.local/share/taskmaster/web`, where `serve` finds it. Antigravity, Grok Build and OpenCode are confined with bubblewrap, so those three also need `bwrap`. A tag starting with `v` builds a Linux release on GitHub. The tarball's `install.sh` (this repository's `scripts/install-release.sh`) copies the prebuilt app to that same directory, so `tm-daemon serve <project>` finds it without `--web`.

To build without installing: `cargo build --release -p tm-daemon -p tm-review`. A harness is found on `PATH`, and also in `~/.local/bin` and `~/.opencode/bin`.

## Releases and the site

A `v*` tag builds the Linux server tarball and the signed desktop installers, with `latest.json` for the in-app updater and `SHA256SUMS.txt`; see [docs/RELEASING.md](RELEASING.md) for the secrets. `site/` builds the public landing page and releases page as static files for Caddy; see [deploy/site/README.md](../deploy/site/README.md).

## Contributing

```bash
cargo test --workspace        # Rust
cd web && npm ci && npm run typecheck && npm run build
cd web && npm run e2e           # Playwright, the built app on a real tm-daemon (system Chrome)
cd site && npm test            # the public site's generator
```

Run `npm run dev` in `web/` next to `tm-daemon serve .` to work on the app. `tm-core` does no I/O; keep it that way. Commits the daemon makes need `user.name` and `user.email` set on the repository.

## License

MIT.
