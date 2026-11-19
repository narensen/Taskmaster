---
name: errorography
description: Record a repeated model failure in Taskmaster's Errorography. Use when a run should remember a mistake and a fix for one provider.
---

# Errorography

`crates/tm-core/src/errorography.rs` keeps a `Book` of `Mistake`s per model. `record(model, failure)` groups by `signature()`, which drops paths, line numbers, and bare numbers. `PROMOTE_AT` is 3. A promoted mistake can store a fix of at least 20 characters via `learn_fix`. Warnings for one model are never shown to another.

The daemon writes the book through its records. A quota sentence is recorded against the provider id from `record_outcome` in `crates/tm-daemon/src/api.rs`. Do not also record that same sentence as a planning mistake.

The page reads `GET /api/errorography`. A new failure shape needs a unit test in `tm-core` that records the same text twice with different paths and asserts one signature.
