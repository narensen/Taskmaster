---
name: web-ui
description: Change a Taskmaster screen. Use when editing web/src, layout, copy, or client state.
---

# Web UI

The app is `web/`: React 19, Vite, TypeScript. `npm run typecheck --prefix web` is the check. The dev server proxies `/api` to `TASKMASTER_DAEMON`.

Dark mode is monochrome. Use `--ink`, `--muted`, and `--line`. Do not recolour status or the diff. Append CSS in `web/src/styles.css`. Inter for chrome, the mono stack for code.

Copy is original. A provider menu fades opacity only. Do not animate `transform` on a Radix popover. The five roles on Settings are Master, Mid, Worker, Reviewer, and Verifier. The home deck's checker seat sets reviewer and verifier together.

A fresh install calls `GET /api/setup` for one line per CLI and does not block the role buttons on that call. Settings can export, import, and reset. Reset keeps the git project. The log viewer reads `GET /api/logs`.

Verify a screen in the browser: do the action, and check the other page that reads the same state. A screenshot of the first paint is not the check.
