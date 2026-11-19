---
name: audit
description: Review a Taskmaster change for secrets, tokens, and the approval gate. Use before a commit that touches auth, git remotes, settings export, logs, or telemetry.
---

# Audit

Check the change for these, and fix the ones it introduces:

- A token in a git remote, a log line, an error string, or an HTTP response. Host tokens live in `~/.taskmaster/hosts.json` mode `0600` and are not returned to the browser. `git` receives them as one `http.extraHeader`, and errors are scrubbed.
- `commons.token` in an export. `redact_export` in `crates/tm-daemon/src/api/desk.rs` clears it. The test is `an_export_drops_a_commons_token`.
- Telemetry sending a prompt, a path, or a token. `telemetry.rs` sends `kind` and `version` only, and only to an `https://` URL in `TASKMASTER_TELEMETRY_URL`, and only after settings turn it on.
- A new route that is not `GET` or `POST`. The API's CORS methods are GET, POST, and OPTIONS.
- A shell, a write outside the worktree, or a network fetch that skips the approval policy. The default mode is `never`.
- `config.example.json` gaining a real token. Placeholders contain `example.invalid` or `replace-me`. The test `the_example_file_is_all_placeholders_and_changes_nothing` must still pass.

`cargo clippy -p tm-daemon --offline --all-targets -- -D warnings` is the compile check. Do not add a secret to a fixture to make a test pass.
