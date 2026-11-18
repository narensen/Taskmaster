# Benchmarks

Raw results of `scripts/bench.sh`, one JSON line per build in `results.jsonl`. The public
site's table is built from this file and nothing else.

`tipjar` (prompt: `tipjar.txt`, plan: `tipjar/plan.json`, harness for every role:
`tipjar/roles`) was measured on 2026-11-18 on one Linux machine with Antigravity (`agy`)
for every role, because Grok's balance was used up that day. Each Taskmaster row builds the
same locked plan from approval to done; planning is not timed. The single-agent row gives
one agent the prompt and the same confinement as a Taskmaster worker (its directory, no
shell).

| Build | Wall time | Calls | Output | Input | Cache read |
| --- | --- | --- | --- | --- | --- |
| Taskmaster, checker off | 835 s | 3 | 126,909 | 310,684 | 3,088,358 |
| Taskmaster, checker on | 527 s | 3 | 98,237 | 250,506 | 2,105,062 |
| Taskmaster, Code Mode | 404 s | 2 | 97,404 | 143,950 | 1,077,324 |
| Single agent | 324 s | 1 | 37,291 | 74,586 | 542,467 |

What one sample of each can and cannot say:

- On a two-page static site, one agent was faster and used fewer tokens than Taskmaster's
  plan, build, check and repair loop. The loop pays for itself on larger products; this
  prompt is too small to show that, and nothing here claims it does.
- The checker-off build needed one repair after the browser smoke check failed; the others
  passed first time. Most of the gap between the three Taskmaster rows is that repair and
  model variance, not the setting. With the checker on, verification waited its full
  20-second limit for the last check (about 4% of that build).
- With Code Mode on, the two workers made 12 `run_code` calls and used the browser tool on
  their own pages; that build had the fewest calls and the least fresh input of the
  Taskmaster rows. One run is not enough to make it the default, so it stays off.

Run them again with `scripts/bench.sh` (see the comment at its top).
