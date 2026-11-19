---
name: perf-check
description: Measure a Taskmaster timing or bundle question before changing it. Use when a screen, a stream, or the web build feels slow.
---

# Perf check

Measure first, then change, then measure again. Report both numbers.

Budgets for the web app: first paint under a second on a warm daemon, input under 100ms, a route change under 150ms, and scrolling while several agents stream.

Likely costs are an SSE update per token, a transcript that is not virtualised, and eager Shiki, xterm, GSAP, or Lottie. `useRun` already batches agent entries. Confirm the current path before adding another cache.

For the bundle, build with `npm run build --prefix web` and read the Vite output sizes. A dependency that is only used on one screen should be imported from that screen.

Daemon time belongs in a `cargo test` or a timed `tm-daemon` run against `examples/ui_fixture`, with `FIXTURE_DELAY_MS` set, so the number is not a model call.
