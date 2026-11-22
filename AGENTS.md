# Taskmaster

Taskmaster plans a product, builds each page in its own git worktree, and reviews the result. It runs on this machine and drives the coding CLIs already installed.

## Layout

- `crates/tm-core` holds the run model, the event log, and Errorography. It does no I/O.
- `crates/tm-daemon` is the HTTP API, settings, git, and the drive loop. The binary is `tm-daemon`.
- `crates/tm-harness` adapts `claude`, `agy`, `grok`, `codex`, and `opencode`.
- `crates/tm-review`, `crates/tm-commons`, `crates/tm-token`, `crates/tm-verify`, `crates/tm-dictionary`, and `crates/tm-launcher` sit beside those.
- `web/` is the React and Vite app. The daemon serves `web/dist`, or `$XDG_DATA_HOME/taskmaster/web` (usually `~/.local/share/taskmaster/web`) when `--web` is omitted.
- `desktop/src-tauri` is the desktop shell. It is outside the Cargo workspace.
- `site/` is the public landing page.
- `scripts/install.sh` builds from source. `scripts/install-release.sh` is copied into the release tarball as `install.sh`.

## Commands

```bash
cargo clippy -p tm-daemon -p tm-core --offline --all-targets -- -D warnings
cargo test -p tm-core -p tm-daemon --offline --lib
npm run typecheck --prefix web
cd web && npm run e2e
```

Do not run `npx tsc` from the repository root. CI is `.github/workflows/ci.yml`: clippy with `-D warnings`, `cargo test`, the web build, the site tests, and e2e. `cargo fmt` is not required on a machine that does not have rustfmt.

## Working here

- Make small local commits. Do not push, and do not open a pull request, unless the person asks.
- One writer per checkout. A second thread working on this repository at the same time gets its own git worktree, so two agents never edit the same files.
- Dark mode stays monochrome. New UI uses `--ink`, `--muted`, and `--line`. Leave status colours and the diff panel alone.
- Append new CSS. Do not rewrite `web/src/styles.css`.
- The home composer stays centred. The sidebar name stays Taskmaster.
- Show a tool result or a diff only when the harness sent it. Do not invent a thought the harness did not send.
- A quota, rate limit, or sign-in stop is a typed limit: a sentence, a reset time when the reply has one, Switch, and Retry.
- A note on a finished run is a follow-up. The master reads it before any worker.
- Telemetry stays off until the settings file turns it on. An export clears `commons.token`. Host tokens stay in `~/.taskmaster/hosts.json`.
- The daemon log is `$XDG_STATE_HOME/taskmaster/daemon.log`, or `~/.local/state/taskmaster/daemon.log`.

## Skills

Procedures live in `.agents/skills`. `.claude/skills` is the same directory.

| Skill | Use it for |
| --- | --- |
| `triage` | Sorting a report before changing code |
| `test-the-app` | Daemon tests and the Playwright suite |
| `desktop-check` | The Tauri shell |
| `cut-a-release` | A `v*` tag and the Linux tarball |
| `add-a-harness` | A new provider CLI |
| `errorography` | A repeated model failure |
| `perf-check` | A timing or bundle question |
| `audit` | Secrets, tokens, and the approval gate |
| `ci` | The GitHub Actions workflows |
| `dependencies` | Cargo and npm updates |
| `pull-requests` | Describing a change |
| `tests` | A new Rust or Playwright test |
| `crate` | A new module in the workspace |
| `web-ui` | A screen in `web/` |

## Skills that were not brought over

T3 Code's own skills are contribution triage, the application test suite, and the mobile test suite. Those jobs are `triage`, `test-the-app`, and `desktop-check` here. There is no React Native client, so the mobile pairing script has no counterpart.

These names showed up in T3 Code's vendored trees and do not fit this stack:

- Effect-only: `ai-docs`, `changesets`, `effect-development`, `jsdocs`, `migration-guidance`, `vendored-assets`. This repo has no Effect packages and no changesets.
- Alchemy-only: `alchemy-domain`. A provider is `add-a-harness`, not an Alchemy domain.
- Their release queue: `release-blog`, `release-overnight`, `pr-queue`. A release here is a `v*` tag. There is no pull-request queue and no release blog.

`bundle-analysis`, `package-development`, `performance-analysis`, `prepare-release`, `audit`, `harden`, `ci-maintenance`, `dependency-maintenance`, `pull-requests`, and `test-development` are covered by the skills in the table. `harden` is part of `audit`. Bundle size is part of `perf-check`.
