---
name: triage
description: Sort a Taskmaster bug report or feature request before changing code. Use when an issue, a mail, or a note needs a first pass.
---

# Triage

Read the report and the code it names. Do not start a fix during triage.

1. Say whether it is a defect, a change, or a question.
2. Name the crate or the screen. `web/` for what a person sees, `crates/tm-daemon` for a run, `crates/tm-harness` for a provider CLI, `desktop/src-tauri` for the shell.
3. Write the smallest check that would fail today: a `cargo test` filter, or a Playwright test name.
4. Note what must stay: monochrome dark mode, the centred home composer, no invented reasoning, telemetry off by default, tokens out of exports.

Stop there unless the person asks for the change. A report about GitHub Actions not starting is often the account billing limit, not the workflow file. See `AGENTS.md`.
