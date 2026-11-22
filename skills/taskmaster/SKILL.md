---
name: taskmaster
description: Hand a build, a fix, or a whole product to Taskmaster on this machine through its MCP server, wait for it, read what it did, and steer it with notes. Use when the person asks to have Taskmaster build or change something, or to check on a Taskmaster run.
---

# Taskmaster

Taskmaster runs coding agents on the person's machine. A master plans the work and parallel
workers build it (Code mode), or one agent owns the whole project (Agent mode). Every run
shows in the Taskmaster app, so the person can watch, approve, and stop it.

## Before you start

Use only the `taskmaster_*` tools from the `taskmaster` MCP server.

- If those tools are missing, or a call answers 401 or 403, **stop**. Do not reach the
  daemon another way (no curl, no reading `~/.taskmaster`, no guessing a token). Tell the
  person: "Make an agent token in Taskmaster → Connect → Outside agents, then add the
  `taskmaster` MCP server with it."
- Never print, log, or commit the token.

## Tools

| Tool | What it does |
| --- | --- |
| `taskmaster_list_runs` | Recent runs: id, phase, whether one needs a person, and its request. |
| `taskmaster_start_run` | Start a run from a `prompt`. `mode` is `agent`, `code` or `auto`. `approve: true` builds without waiting for the person to approve the plan. |
| `taskmaster_run_status` | One run's phase, pages, spend, error and link. |
| `taskmaster_wait` | Wait until a run is done, failed, cancelled, or waiting on a person. `timeout_secs` is at most 600. |
| `taskmaster_read_run` | What the run did, kept short: the request, the plan, each agent's last words and files, errors, notes. |
| `taskmaster_send_note` | Steer a running run (the master reads it before the next worker), or start a new turn on a finished one. |
| `taskmaster_approve` | Approve a run's plan. Only when the person asked you to. |
| `taskmaster_cancel` | Stop a run now. Only when the person asked you to. |
| `taskmaster_preview` | Start a run's live preview and return its address. |

## How to use them

1. Pick the mode. `agent` for a fix or a change across a few files, `code` for a new product
   or many pages, `auto` when unsure.
2. `taskmaster_start_run` with the person's request in their words. Leave `approve` false
   unless they said to build without review.
3. Give the person the run's link at once, so they can watch.
4. `taskmaster_wait`, then `taskmaster_read_run`. If the wait ends with the run waiting on a
   person (a plan to approve, a risky command), say so and stop: that answer is theirs.
5. A follow-up goes through `taskmaster_send_note` on the same run, not a new run.

## Limits

- A `/goal` line is refused: goals run checks on this machine, so only the person sets one,
  in the app.
- The run's permission mode is the person's Settings. You cannot raise it.
- Taskmaster answers only for the project open in the app. Ask the person to open another
  one there if the request is for a different project.
