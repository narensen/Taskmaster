# Taskmaster Gemini benchmarks

Taskmaster is a local, multi-model coding harness and run studio. You describe a web app, library, CLI, server, or migration once (or import a GitHub/GitLab issue with screenshots and files). The master researches unfamiliar APIs, locks shared contracts and executable contract tests, and splits the build across up to 16 parallel workers in isolated git worktrees. Fast checks run as each unit merges, and a separate reviewer and verifier grade the result before a run is called done. It can drive Claude Code, Codex, Grok Build, Antigravity, and OpenCode.

This note records the Gemini comparison runs: tinydb greenfield and brownfield across Taskmaster generations n5, n7, and n8/n8b, plus the first three greenfield tasks in the TB-15 suite. A later Grok Build comparison was started and then stopped; those runs are not reported here.

## Setups

| Name | What it is |
| --- | --- |
| **tmh** | Taskmaster, standard mode. Workers use `write_file`, `edit_file`, and `run_command`. |
| **tmhcode** | Taskmaster Code Mode. Workers are expected to edit and test through `run_code`. |
| **T3** | T3 baseline, same tinydb prompt as Taskmaster. Model: `gemini-4-neon`. |
| **Agy** | Antigravity alone, same tinydb prompt. Model: `gemini-4-neon-high`. |

Taskmaster roles in these runs:

- Planner, reviewer, and verifier: Antigravity `gemini-4-neon-high`.
- Worker and mid: requested as Antigravity `gemini-4-hydrogen-high`.
- From n8 onward, effort routing is on by default (normal → `-medium`, easy → `-low`). The original n5, n7, and n8 ledgers record unit calls as `gemini-4-hydrogen-high`. The n8b and TB-15 ledgers record the routed model (`gemini-4-hydrogen-medium`, and on the n8b brownfield Code Mode run also `-low` and `-high`).

Binaries: n5 from `/workspace/tm-fix-neon-run/n5/bin/tm-daemon`; n7 `tm-daemon` sha256 `c129f6c63dce391121c38f88bbe7f6d66da8a6a4baebda0dfefe26d217d33953`; n8 `074118c81dc26bea5346b80661475676d4405e4444ca94ebf454c42353782dfe`; n8b `aa85da7be96402756c61c2bf4d133e3ca64b3d9ac22660a82e0bf896028bf1e1`. TB-15 Gemini runs used the n8b binary. The n8 tree includes a coherence pass documented in `docs/coherence-review.md` (shared Dictionary MCP prefixes, centralized `safe_rel` / `is_flow_path`, and related cleanups).

## Methodology

- **Prompt.** Every tinydb greenfield setup used the same frozen prompt (`tinydb.txt`, sha256 `07e77072263e0b43db7005dd0f66db313b7bf48771a85298b6d9a65b70ddc442`). Brownfield used `brown/tinydb-feature.txt` (sha256 `d7e728f09def696294f927c92178e3c07e09015fe0c922a38bce8f3da160906e`) on a frozen base tree. TB-15 tasks used the suite `prompt.txt` for that task.
- **Hidden checks.** Scores come from hidden graders and, where present, hidden reference solutions. Those files are not part of the prompt. Tinydb greenfield uses `new_tinydb.py` (sha256 `f0ecd92e77ea7f40b4fc8e93ba43e54a4fe1898040bc23b7878ea23187dcc153`, frozen 2026-11-20). Brownfield uses `grade_feature.py` (sha256 `62d2a6db1aa8f7206a9e5e489009c69ec0c2c1ddcede80376a42292399326589`) and reports a feature score plus a 24-check regression against the original tinydb grader. TB-15 graders and prompts are listed in `suite/FROZEN.sha256`. The v2 grader freeze is dated 2026-11-26 22:25 IST (`AUDIT-v2.md`).
- **v2 regrade.** After the v2 audit, the mdtodo grader started accepting the required `Error:` line on stderr as well as stdout. The mdtodo prompt was not changed, so the finished tmh and tmhcode runs were regraded in place (v1 21/24 → v2 24/24). calcx scores were unchanged. notesapi ran after the v2 freeze. Official TB-15 scores in the table below are v2.
- **One run per cell, with two exceptions.** n7 greenfield was run twice (n7a and n7b). After a Code Mode bug on n8 brownfield, tmhcode was rerun as n8b for tinydb greenfield and brownfield. n8 standard-mode brownfield was not rerun.
- **Tokens.** For Taskmaster, `in` is uncached input from the run ledger (Antigravity reports this with cached tokens already excluded), `out` is output, and `cache` is cache reads. The same three fields are taken from `result-n1.json` for T3 and Agy (`in`, `out`, `cache_read`). Do not add `in` and `out` and call the sum uncached input. T3 and Agy also report a separate `thinking` count, listed under the tinydb table. These runs were logged as unpriced: there was no price table for these models, so no USD cost is given.
- **Time.** Wall-clock seconds from each run's `.time` record or metrics JSON, in Asia/Calcutta (IST). End timestamps in the sources already carry a `+05:30` offset.
- **n5 scores.** n5 did not have a saved metrics JSONL in `/workspace/tm-flow-run`. Token totals match the n5 run log and ledger. Scores are from the same frozen `new_tinydb.py` grader used for n7 and n8, applied to the saved n5 trees.

## Results

### tinydb greenfield

All of these setups scored 24/24 on the hidden grader. n7a and n7b are both listed because the n7 uncached-input range depends on how the planner split the work (4 units on n7a tmh, 6 on n7b tmh).

| Setup | Generation | Score | Time (s) | Uncached in | Output | Cache read | Agent calls |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| T3 | n1 | 24/24 | 1,279.0 | 766,109 | 87,544 | 574,194 | — |
| Agy alone | n1 | 24/24 | 1,432.0 | 109,684 | 91,827 | 743,124 | — |
| tmh | n5 | 24/24 | 614.8 | 268,344 | 177,009 | 1,183,460 | 5 |
| tmhcode | n5 | 24/24 | 620.9 | 267,608 | 162,815 | 736,879 | 7 |
| tmh | n7a | 24/24 | 545.2 | 289,746 | 159,339 | 1,685,735 | 5 |
| tmhcode | n7a | 24/24 | 866.6 | 340,727 | 167,978 | 1,863,527 | 7 |
| tmh | n7b | 24/24 | 641.4 | 439,872 | 171,536 | 2,212,033 | 7 |
| tmhcode | n7b | 24/24 | 519.0 | 342,158 | 147,521 | 1,693,405 | 7 |
| tmh | n8 | 24/24 | 410.3 | 125,174 | 79,554 | 455,279 | 4 |
| tmhcode | n8 | 24/24 | 409.6 | 125,859 | 80,975 | 438,154 | 4 |
| tmhcode | n8b (rerun) | 24/24 | 464.8 | 125,260 | 81,992 | 496,171 | 4 |

Notes:

- T3 also reported 18,703 thinking tokens; Agy reported 31,330. Baseline `calls` in `result-n1.json` is 1 for both and is not comparable to Taskmaster agent calls, so it is omitted.
- Agy uncached input is 109,684. Agy `in` + `out` is 201,511, which is the ~202k figure that was sometimes quoted earlier. That 201,511 is a total, not uncached input. T3 uncached input is 766,109, not ~279k.
- On n8, Taskmaster standard mode finished in 410.3s with 125,174 uncached input. That is faster than T3 (1,279.0s) and Agy (1,432.0s), and lower uncached input than T3, but higher uncached input than Agy alone. Taskmaster n8 tmh `in` + `out` is 204,728, close to Agy's 201,511 total.

### tinydb brownfield

Feature checks are 23 hidden tests for the new work; regression is the original 24 tinydb checks. There are no T3 or Agy brownfield baselines.

| Setup | Generation | Feature | Regression | Time (s) | Uncached in | Output | Cache read | Agent calls |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| tmh | n7 | 23/23 | 24/24 | 445.0 | 368,105 | 104,449 | 2,223,723 | 6 |
| tmhcode | n7 | 23/23 | 24/24 | 495.3 | 372,240 | 92,596 | 2,056,699 | 5 |
| tmh | n8 | 23/23 | 24/24 | 371.4 | 231,748 | 66,288 | 1,152,326 | 4 |
| tmhcode | n8 | 5/23 | 24/24 | 373.7 | 219,037 | 75,510 | 440,941 | 4 |
| tmhcode | n8b | 23/23 | 24/24 | 472.9 | 245,747 | 78,609 | 807,514 | 4 |

The n8 Code Mode run failed on the feature grader (ALTER and related checks were missing) after a unit whose `run_code` edit errored, whose `run_command` was refused in Code Mode, and that was then counted as done with no file changes. That was fixed in n8b; the n8b Code Mode rerun scored 23/23 + 24/24. n8 standard mode already had a full score and was not rerun.

### TB-15 suite (Gemini, v2 graders)

These are Taskmaster-only runs on calcx (24 checks), mdtodo (24), and notesapi (25). T3 and Agy were not completed for this suite. tinydb2, calcx-plus, and mdtodo-plus were not run.

| Task | Setup | Score | Time (s) | Uncached in | Output | Cache read | Agent calls |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| calcx | tmh | 24/24 | 244.7 | 67,801 | 35,704 | 158,463 | 4 |
| calcx | tmhcode | 24/24 | 198.2 | 83,194 | 36,996 | 100,466 | 4 |
| mdtodo | tmh | 24/24 | 329.2 | 95,665 | 48,809 | 345,366 | 4 |
| mdtodo | tmhcode | 24/24 | 342.5 | 88,172 | 45,533 | 260,929 | 6 |
| notesapi | tmh | 25/25 | 272.8 | 85,956 | 47,382 | 228,764 | 4 |
| notesapi | tmhcode | 25/25 | 278.8 | 104,254 | 45,802 | 146,386 | 4 |

calcx and mdtodo used almost only `gemini-4-hydrogen-medium` for workers (mdtodo Code Mode also had one `gemini-4-hydrogen-high` call). notesapi used `gemini-4-neon-high` for the planner and `gemini-4-hydrogen-medium` for the three worker calls.

### Token-cut journey (n7 to n8)

n5 tinydb greenfield was already around 268k uncached input (268,344 tmh, 267,608 tmhcode). n7 greenfield uncached input was higher: 289,746 on n7a tmh, up to 439,872 on n7b tmh, and 340,727 / 342,158 on the two n7 Code Mode runs.

The n8 token-cut work brought greenfield uncached input down to 125,174 (tmh) and 125,859 (tmhcode), with the n8b Code Mode rerun at 125,260. In short, n7's roughly 290–440k uncached input fell to about 125k. Cache reads on those n8/n8b greenfield runs were 438,154–496,171, down from 1,685,735–2,212,033 on n7. Output fell from 147,521–171,536 on n7 to 79,554–81,992 on n8/n8b. Wall clock on n8 was 410.3s / 409.6s, against 519.0–866.6s on n7.

Brownfield uncached input also fell, but by less: n7 was 368,105 / 372,240; n8 standard mode was 231,748; the successful n8b Code Mode run was 245,747.

## Limitations

- **Single runs, small sample.** Most cells are one run. n7a/n7b and the n8b Code Mode rerun are the only repeats. There are no medians, error bars, or timeout retries in these tables.
- **Missing baselines.** T3 and Agy were measured only on tinydb greenfield. There is no brownfield or TB-15 baseline. TB-15 itself covers only three of six planned tasks.
- **Runs were not filesystem-isolated.** The sandbox made other paths read-only; it did not hide them. A later Grok Build pass on the same bench tree showed agents reading hidden graders, reference solutions, and (in one case) a sibling run. A scan of the Gemini tinydb and TB-15 logs found no evidence that those runs did the same, but they were not isolated either, so a 24/24 on Gemini does not rest on a hard guarantee that the grader was unseen.
- **Estimates and cost.** Planner token estimates were often high (for example n5 tmh estimated 479,985 tokens plus 1,330,992 cache reads, and used 268,344 uncached input, 177,009 output, and 1,183,460 cache reads). Actual runs are unpriced, so this document does not report USD cost.
- **Model-name comparison.** Effort routing and ledger `model` strings are not uniform across n5–n8b, even though every Taskmaster launch requested the same role models. Treat routed `-medium` / `-low` rows as the same requested setup, not as a different model choice.
- **Code Mode purity.** Some Code Mode ledgers still include `read_file` or `edit_file` next to `run_code` (for example n8 tinydb tmhcode and both mdtodo Code Mode tool lists).

## Sources

- Taskmaster description: `/workspace/tm-flow/README.md`
- T3 and Agy tinydb results: `/workspace/t3setup/h7/bench/logs/result-n1.json`
- n7 metrics: `/workspace/tm-flow-run/n7/n7a.jsonl`, `n7b.jsonl`, `n7brown.jsonl`
- n8 / n8b metrics: `/workspace/tm-flow-run/n8/n8.jsonl`, `n8brown.jsonl`, `n8b.jsonl`
- Progress notes: `/workspace/tm-flow-run/progress-n8.md`, `/workspace/tm-flow-run/progress-tb15.md`
- TB-15 Gemini v2 metrics: `/workspace/t3setup/h7/bench/tb15/tm-v2.jsonl` (complete, including notesapi). The copy at `/workspace/tm-flow-run/tb15-logs/tm-v2.jsonl` has calcx and mdtodo only; it was snapshotted before notesapi finished. notesapi figures also match `progress-tb15.md`.
- n5 tokens: `/workspace/t3setup/h7/bench/logs/n5-tinydb-tmh.log` and `n5-tinydb-tmhcode.log`, plus the run ledgers; scores from frozen `new_tinydb.py` on the saved trees
- Token-cut analysis (n7 breakdown; T3/Agy comparison targets in that file are superseded by the correction above): `/workspace/tm-flow-run/token-breakdown.md`
- Coherence pass on the n8 tree: `/workspace/tm-flow/docs/coherence-review.md`
- Freezes: `/workspace/t3setup/h5/grader/README.md`, `/workspace/t3setup/h7/bench/brown/FROZEN.sha256`, `/workspace/t3setup/h7/bench/suite/FROZEN.sha256`, `/workspace/t3setup/h7/bench/suite/AUDIT-v2.md`
