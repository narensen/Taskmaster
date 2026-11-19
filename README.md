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

<table>
  <tr>
    <td><b>A locked plan</b></td>
    <td>Contracts are committed before any page is built, so workers can't drift apart.</td>
  </tr>
  <tr>
    <td><b>Parallel pages</b></td>
    <td>Each page builds in its own worktree and merges back only the files it owns.</td>
  </tr>
  <tr>
    <td><b>Independent review</b></td>
    <td>The reviewer and verifier are a different model from the one that wrote the page.</td>
  </tr>
  <tr>
    <td><b>Free checks first</b></td>
    <td>Type checks, linters, the project's own build and tests, and headless-browser checks run before any model is asked to grade.</td>
  </tr>
  <tr>
    <td><b>A token ledger</b></td>
    <td>Every call records input, output and cache tokens, time, and price where the provider reports it.</td>
  </tr>
  <tr>
    <td><b>Resumable runs</b></td>
    <td>The run is an event log, so a crash picks up where it stopped.</td>
  </tr>
</table>

## Install

One short line per platform. Verify the download with the checksum file that ships with every release.

| Platform | Get it |
| --- | --- |
| Ubuntu / Debian | `Taskmaster_<version>_amd64.deb` from [Releases](https://github.com/narensen/Taskmaster/releases/latest), then `sudo apt install ./Taskmaster_<version>_amd64.deb` |
| Any Linux | `Taskmaster_<version>_amd64.AppImage`, then `chmod +x` and run it |
| Linux server (headless) | `taskmaster-<version>-x86_64-linux.tar.gz`, then `./install.sh` inside it. The app is copied to `~/.local/share/taskmaster/web`, so `tm-daemon serve <project>` finds it without `--web`. |
| macOS | Build from source with `scripts/install.sh` (needs `cargo` and `git`) |

Then install and sign in to at least one provider. `tm-daemon providers` prints the install and sign-in commands for each one.

Verify a download:

```bash
sha256sum -c SHA256SUMS.txt
```

## Use

```bash
tm-daemon serve .                      # the app at http://127.0.0.1:8737
tm-daemon . --prompt "A notes app with a list and an editor."
tm-daemon . --yes                      # approve the plan and build it
```

The [guide](docs/GUIDE.md) covers roles, configuration, the API, Commons, and reviewing any repository with `tm-review`.

## Recursive self-improvement

Taskmaster gets better the more it runs, because every mistake feeds back into the system that made it.

- **Errorography.** Each model's repeated mistakes are recorded per model (`errorography.json`). The same mistake, three times for that model, is handed back to it on its next attempt — with the fix that was learned — so a model doesn't repeat itself.
- **Citations.** Each page records the Dictionary hash it was built against. A later run skips a page whose cited hashes still match, so the system builds on work it already proved.
- **Reviews that improve reviews.** Reviewer and verifier are different models, so one model never grades its own page. Confirmed and rejected findings are appended to the review history, and the next review reads what earlier ones found.
- **The event log is the run.** `events.jsonl` is the source of truth. Replaying it rebuilds the state, so a crashed run resumes from the log instead of from a model's memory.

Every run leaves the system sharper than it found it.

## Code Mode (optional)

Workers can run sandboxed JavaScript instead of only writing files. Code Mode gives each worker `run_code`: one async call with `read`, `write`, `edit`, `grep`, `ls`, `sh`, `check` and `browser`, run in Node behind the same permission gate as the worker's own tools (no files, processes or workers of its own, and under bubblewrap no network). Claude Code workers use Claude Code's own native code mode instead.

It's off by default. Turn it on in Settings (`code_mode.enabled`) when a worker benefits from computing, scraping its own preview, or transforming data in code.

## Develop

```bash
cargo test --workspace
cd web && npm ci && npm run build && npm run e2e
```

## License

MIT.