---
name: crate
description: Add a module or a crate to the Taskmaster workspace. Use when new Rust code needs a home.
---

# Crate

The workspace is the crates listed in `AGENTS.md`. Prefer a module in the crate that already owns the behaviour. `tm-core` stays free of I/O. HTTP, files, and processes belong in `tm-daemon` or `tm-harness`.

A package with both `lib.rs` and `main.rs` does not let the binary see private modules. Call new helpers from the library, as `log` and `telemetry` are called from `api.rs`.

Register a module in the crate's `lib.rs`. Keep the doc comment on the module, and mention a new HTTP route in the block at the top of `crates/tm-daemon/src/api.rs` and `api/desk.rs`.

`cargo test -p <crate> --offline --lib` and clippy with `-D warnings` are the checks. Follow the surrounding style. rustfmt may be absent.
