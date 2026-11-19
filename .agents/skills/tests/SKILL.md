---
name: tests
description: Add a Rust or Playwright test in Taskmaster. Use when a bug fix or a new route needs a check that fails without the change.
---

# Tests

A daemon behaviour gets a `#[test]` or an async test next to the code, or in `crates/tm-daemon/src/tests.rs` when it needs the HTTP client. Assert the sentence a person would read, not a panic debug string. `Resolution` has no `Debug`. Match on `Err`.

`ApiError` has no `Display`. Log `err.1`.

A screen gets a Playwright test in `web/e2e/`. Use `open` for the real daemon and `openFixture` for the scripted one. `CHOSEN` in `fixtures.ts` is every role set to `grok`. The fixture project has a `grok` stub on `PATH` that prints `* grok-4 (default)` for `models`.

Shared state: tests run with one worker. Put the daemon's project back if the test opened another one. A run id that already exists returns 409. Handle that, or use a fresh id.

Run the new test by name before the wider suite. See `test-the-app`.
