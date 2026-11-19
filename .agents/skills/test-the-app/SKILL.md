---
name: test-the-app
description: Run Taskmaster's daemon tests and the Playwright suite. Use when checking a run, the web app, or an end-to-end path.
---

# Test the app

Daemon and core:

```bash
cargo test -p tm-core -p tm-daemon --offline --lib
cargo clippy -p tm-daemon -p tm-core --offline --all-targets -- -D warnings
```

One test at a time. `cargo test` takes a single filter. A pipe hides the exit code unless `set -o pipefail` is on.

The web suite builds the app, builds `tm-daemon` and `examples/ui_fixture`, and runs Playwright against system Chrome:

```bash
cd web && npm run e2e
```

`npm run e2e:only` skips the build when those binaries and `web/dist` are already current. `TASKMASTER_CHROME` overrides `/usr/bin/google-chrome`.

`web/e2e/setup.ts` starts a real daemon and, when the example binary exists, a scripted one. The scripted harness streams a plan when the brief contains "Plan this product and lock its contracts." or "Plan this small product quickly." A prompt that contains `USAGE LIMIT` returns a quota sentence and no plan. `FIXTURE_DELAY_MS` is 5 in that setup. Tests share one daemon, so a test that opens another project puts the original project back before it ends.

`Ctrl+J` is covered by `web/e2e/shell.spec.ts`. Home, a full run, the quota sentence, git init, clone, and the five provider menus are `production.spec.ts` and `menus.spec.ts`.
