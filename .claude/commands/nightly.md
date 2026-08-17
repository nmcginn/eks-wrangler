---
description: Pick the next roadmap task and land it as one reviewable pull request
---

You are doing tonight's increment of work on `eks`. The human reviews it in the
morning, so the deliverable is **one pull request they can review over coffee**.

Read `CLAUDE.md` first — it holds the priorities and conventions this project is
built around. Then:

## 1. Choose the task

Read `docs/ROADMAP.md` and take the **highest-priority unchecked task** that fits
in a single PR (roughly 200–500 lines of diff).

Before starting, check open pull requests. If a previous night's PR is still open
and unmerged, prefer work that does not conflict with it, or address review
feedback on it instead of starting something new — an unreviewed pile-up helps
nobody.

If the top task is too large, take the smallest genuinely useful slice of it, and
split the rest into new roadmap entries rather than half-finishing it.

## 2. Build it

```sh
git fetch origin master
git checkout -b nightly/$(date +%Y-%m-%d)-<short-slug> origin/master
```

Work to the task's acceptance criteria. Non-negotiables from `CLAUDE.md`:

- Tests come with the change, and they cover the acceptance criteria plus the
  awkward cases — empty input, tiny terminals, missing fields, absent credentials.
- No `unwrap`/`expect`/`panic!` in library code; the lint will stop you.
- Nothing blocks first paint on the network. Nothing blocks the render loop on I/O.
- Colours come from `theme::Theme`.
- Error messages tell the user what to do next.

## 3. Validate

```sh
make check
```

Formatting, clippy, tests, and docs must all pass. If a test fails, fix the
cause. Never weaken or delete a test to get to green — if a test is genuinely
wrong, change it deliberately and say so in the PR.

Where a change is visible in the terminal, run it and confirm it looks right:
`make run ARGS="contexts"`.

## 4. Ship it

Tick the completed task in `docs/ROADMAP.md`, and add any follow-ups you
discovered. Record notable choices in `docs/DECISIONS.md`. Then commit, push, and
open a PR against `master` using the repository's PR template.

The PR description is the handover — the reviewer has no other context. Say what
changed, why, how you verified it, and what deserves the closest look. Call out
anything you were unsure about; a flagged uncertainty is far more useful than a
confident-sounding guess.

## If you finish early

Do not start a second task. Instead, improve what you just built: another test
for a case you did not cover, a clearer error message, a doc comment explaining
something non-obvious. Consistent, well-reviewed progress is the goal — volume
is not.
