<p align="center">
  <img src=".github/readme/icon.png" width="80" height="80" alt="Taskmaster logo" />
</p>

<h1 align="center">Taskmaster</h1>

<p align="center">One master plans the product. Many workers build it in parallel. A different model checks every page.</p>

<p align="center">
  <a href="https://github.com/narensen/Taskmaster/releases/latest"><strong>Download</strong></a> ·
  <a href="docs/GUIDE.md">Guide</a> ·
  <a href="docs/RELEASING.md">Releasing</a> ·
  <a href="https://www.taskmaster-commons.duckdns.org">Website</a>
</p>

<p align="center">
  <img src=".github/readme/hero.webp" width="100%" alt="The Taskmaster home screen with the composer and a finished run" />
</p>

Taskmaster is a multi-model coding harness that runs on your machine. You describe the product once. The master locks the plan and the shared contracts, workers each build one page in their own git worktree, and a separate reviewer and verifier check the work before a run is called done.

It drives the coding CLIs you already use: Claude Code, Codex, Grok Build, Antigravity and OpenCode. You pick a provider and model for each role.

## What you get

- **A locked plan.** Contracts are committed before any page is built, so workers can't drift apart.
- **Parallel pages.** Each page builds in its own worktree and merges back only the files it owns.
- **Independent review.** The reviewer and verifier are a different model from the one that wrote the page.
- **Free checks first.** Type checks, linters, the project's own build and tests, and headless-browser checks run before any model is asked to grade.
- **A token ledger.** Every call records input, output and cache tokens, time, and price where the provider reports it.
- **Resumable runs.** The run is an event log, so a crash picks up where it stopped.
- **Desktop app.** It starts the local daemon for you and updates itself from GitHub Releases.

## Install

| Platform | How |
| --- | --- |
| Ubuntu, Debian | `.deb` from [Releases](https://github.com/narensen/Taskmaster/releases/latest), then `sudo apt install ./Taskmaster_*_amd64.deb` |
| Any Linux | `.AppImage` from [Releases](https://github.com/narensen/Taskmaster/releases/latest) |
| Linux server | `taskmaster-*-x86_64-linux.tar.gz`: the daemon, the review tool and the built app |
| macOS, from source | `scripts/install.sh` (needs `cargo` and `git`) |

Then install and sign in to at least one provider. `tm-daemon providers` prints the commands for each one.

## Use

```bash
tm-daemon serve .                      # the app at http://127.0.0.1:8737
tm-daemon . --prompt "A notes app with a list and an editor."
tm-daemon . --yes                      # approve the plan and build it
```

The [guide](docs/GUIDE.md) covers roles, configuration, the API, Commons, and reviewing any repository with `tm-review`.

## Develop

```bash
cargo test --workspace
cd web && npm ci && npm run build && npm run e2e
```

## License

MIT.
