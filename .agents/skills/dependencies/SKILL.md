---
name: dependencies
description: Update Cargo or npm dependencies in Taskmaster. Use when a crate, a web package, or a GitHub Action is being bumped.
---

# Dependencies

Cargo changes go through `Cargo.lock`. After a bump:

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

`--locked` is what CI uses. A lockfile that does not match the manifests fails there.

npm changes are `web/package-lock.json` and `site/package-lock.json`. After a bump, `npm run typecheck --prefix web` and `npm run build --prefix web`. The site job is `cd site && npm test`.

Do not bump a package only to silence a warning in code this change does not touch. Read the changelog line that affects this repo. GitHub Actions majors are the `uses:` lines in `.github/workflows/`.
