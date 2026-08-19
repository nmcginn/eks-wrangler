---
description: Pick the next roadmap task and land it as one reviewable pull request
---

You are doing tonight's increment of work on `eks`. The human reviews it in the
morning, so the deliverable is **one pull request they can review over coffee**.

Read `CLAUDE.md` first — it holds the priorities and conventions this project is
built around. Then:

## 1. Choose the task

Read `docs/ROADMAP.md` and take the **highest-priority unchecked task** that fits
in a single PR — roughly 200–400 lines of production change, with its tests on
top. Read `CLAUDE.md`'s "What one pull request means" before you size anything:
the tests in this codebase are two to four times the length of the code they
cover, and judging a task by the total diff is what makes a night's work look
too big when it is not.

Before starting, check open pull requests. If a previous night's PR is still open
and unmerged, prefer work that does not conflict with it, or address review
feedback on it instead of starting something new — an unreviewed pile-up helps
nobody.

If the top task really is too large, take the smallest **complete** slice — one
a user meets as a finished change rather than a staging post — and split the
rest into new roadmap entries. Slice at a seam: another surface, an open design
question, a genuinely separate feature. Never slice a single user-visible
behaviour into the honest half tonight and the useful half tomorrow.

The failure mode to watch for here is taking *too little*, not too much.

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

## 4. Audit your own follow-ups

Before you write the PR, read every roadmap entry you are about to add and put
one question to it: **would the reviewer expect this to be in the PR already?**

- *"The dashboard should do this too"* — a real follow-up. That surface is not
  built yet, so there is nothing here to finish.
- *"This message should also say what to do about it"* — not a follow-up. It is
  the message you wrote tonight, still short of the bar in `CLAUDE.md`. Go and
  finish it.

An entry that fails the question is not an entry. Delete it and do the work now,
even if that takes the PR past the size you planned — a finished change at 600
lines beats two thirds of one at 400 and a second review of the same paragraph
tomorrow. Whether the extra work is small is not the test; whether it completes
a thought this PR started is.

Keep the entries that pass, and in the PR give each one a sentence saying why it
is separate. If you cannot write that sentence, it was not a follow-up.

## 5. Ship it

Tick the completed task in `docs/ROADMAP.md`, and add the follow-ups that
survived the audit. Record notable choices in `docs/DECISIONS.md`. Then commit,
push, and open a PR against `master` using the repository's PR template.

The PR description is the handover — the reviewer has no other context. Say what
changed, why, how you verified it, and what deserves the closest look. Call out
anything you were unsure about; a flagged uncertainty is far more useful than a
confident-sounding guess.

## If you finish early

Do not start an unrelated task. In order:

1. **Finish this one.** Re-run the audit in step 4 over the follow-ups you
   wrote, and fold in anything that fails it. This is the best use of the time,
   every time.
2. **Improve what you built:** another test for a case you did not cover, a
   clearer error message, a doc comment explaining something non-obvious.

Consistent, well-reviewed progress is the goal — volume is not. Neither is a PR
that stops at the first defensible boundary.
