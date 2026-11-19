---
name: add-a-harness
description: Add a provider CLI to Taskmaster. Use when a new harness id, install command, or model list is needed.
---

# Add a harness

The ids live in `HARNESSES` in `crates/tm-harness/src/lib.rs`. `harness()` matches that list. `providers.rs` has one `Provider` per id, and a test that `PROVIDERS.len()` equals `HARNESSES.len()`. `setup.rs` reports install, version, and sign-in for the same ids.

A new id needs all of these, plus a label in `provider_label` in `crates/tm-daemon/src/reply.rs` and a logo in `web/src/providers/logos.tsx`. The web only offers models the providers API returned. An unknown id is rejected when settings are saved.

The adapter implements `Harness` and streams `HarnessEvent`: `Text`, `Thought`, `ToolCall`, `ToolResult`, `PermissionDecided`. A tool result is a separate event. Do not fold it into the answer text. Where the CLI sends no thought, send no thought.

Quota, rate-limit, and auth text is classified by `limit_of` in `reply.rs`. Add a phrase there only when it is unambiguous, and add a unit test with a captured reply. Do not treat a plan that happens to mention a limit as a limit.
