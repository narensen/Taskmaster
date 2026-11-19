---
name: ci
description: Change Taskmaster's GitHub Actions workflows. Use when a job, a cache, or the e2e runner needs an update.
---

# CI

`.github/workflows/ci.yml` runs on pushes to `main` and on pull requests. Jobs: `test` (clippy `-D warnings`, `cargo test`, the Commons Postgres load check), `web` (the Vite build and the site tests), and `e2e` (Chrome plus `ui_fixture`). The Rust toolchain in the workflow is 1.93.1.

`.github/workflows/release.yml` runs on tags `v*`. It is described in `docs/RELEASING.md` and the `cut-a-release` skill.

A private repository on this account can fail at startup with an empty job when Actions billing is spent. That is not a broken workflow. Do not rewrite the YAML to work around it.

Dependabot is `.github/dependabot.yml` for cargo, the `web/` and `site/` npm trees, and GitHub Actions. A dependency bump still needs the checks in the `dependencies` skill.
