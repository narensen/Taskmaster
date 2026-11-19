---
name: pull-requests
description: Describe a Taskmaster change. Use when writing a commit message or a pull request body. Do not open or push unless asked.
---

# Pull requests

Commits are one sentence, with no conventional-commit prefix. Group them by behaviour: a run-view change, a settings change, the release installer, the repo skills.

A pull request body says what a person can do now, the tests that were run, and what was not run. Link a screenshot when a screen changed.

Do not push, and do not open the pull request, unless the person asks. Do not force-push. `gh` on this machine is a personal account. Do not put a token from that account into the repo, a fixture, or a host file.

There is no pull-request queue in this repository. Local commits on the working branch are the stack.
