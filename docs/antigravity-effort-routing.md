# Antigravity Effort-Based Thinking Routing

Antigravity's ACP server (`agy acp`) selects thinking depth through the model variant suffix (`-low`, `-medium`, `-high`, `-xhigh`) rather than a CLI `--effort` flag. When `effort_routing` is enabled (the default), Taskmaster routes each Antigravity worker turn based on its unit `difficulty` and role while keeping explicit `--worker-model` choices safe.

## Role & Difficulty Mapping

| Caller | `task.effort` | Effect on a `*-high` / `*-xhigh` Antigravity model |
| --- | --- | --- |
| Worker (`difficulty: "easy"`, first pass) | `"low"` | Maps to `*-low` |
| Worker (`difficulty: "normal"` or unset, first pass) | `"medium"` | Maps to `*-medium` |
| Worker (`difficulty: "hard"`) | `"high"` | Keeps configured `*-high` / `*-xhigh` |
| Worker repair (`errors.is_some()`) | `None` | Keeps configured `*-high` / `*-xhigh` |
| Planner (`ask_planner`, `ask_repair`, `phase_request`) | `None` | Keeps configured `*-high` / `*-xhigh` |
| Reviewer / Verifier / Master repair | `None` | Keeps configured `*-high` / `*-xhigh` |

## Verified Antigravity Model Families (`agy models`)

Taskmaster never silently upgrades an explicit `-low` or `-medium` model to a higher tier, and only steps down when the target variant is verified to exist:

1. **Live ACP catalog check (primary):** If `session/new` advertises `models.availableModels` (or `configOptions` for `model`), the mapped variant is chosen **only** when that exact id appears in the server's catalog.
2. **Verified allowlist (fallback when `session/new` omits a catalog):** Only families verified against `agy models` to offer `-low`, `-medium`, and `-high` are mapped:
   - `gemini-4-neon` (`-low`, `-medium`, `-high`, `-xhigh`)
   - `gemini-4-carbon` (`-low`, `-medium`, `-high`)
   - `gemini-4-argon` (`-low`, `-medium`, `-high`)
   - `gemini-4-hydrogen` (`-low`, `-medium`, `-high`, `-xhigh`)
   - `gemini-3.8-flash` (`-low`, `-medium`, `-high`)
   - `claude-opus-5-5` (`-low`, `-medium`, `-high`)
   - `claude-sonnet-5-5` (`-low`, `-medium`, `-high`)
3. **Preserved non-tiered models:** Models without matching lower variants—such as `gpt-oss-120b-medium` and `gemini-pro-orchestrator*`—are always passed verbatim.
4. **Runtime fallback:** If `session/set_model` ever rejects the mapped variant, Taskmaster immediately retries with the caller's explicit requested model so `--worker-model` never breaks.

## Configuration & Opt-Out

Effort routing is enabled by default and can be toggled in three ways:

- **Settings file (`.taskmaster/config.json` or `~/.taskmaster/config.json`):**
  ```json
  { "agents": { "effort_routing": false } }
  ```
- **Environment variable:** `TASKMASTER_EFFORT_ROUTING=0` (or `false`/`off`/`no`) pins explicit `--worker-model` / `--master-model` ids verbatim; `TASKMASTER_EFFORT_ROUTING=1` re-enables routing.
- **CLI flag:** `tm-daemon ... --no-effort-routing` disables effort routing for a single run.
