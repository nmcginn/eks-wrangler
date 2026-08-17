# eks — working agreement

`eks` is a terminal tool for exploring and operating AWS EKS clusters. It should
be the thing people reach for instead of stringing together `kubectl` commands,
and it should be a pleasure to look at while they do it.

Most work here is done by Claude, one pull request per night, reviewed by a human
in the morning. That cadence shapes everything below: each change must stand on
its own and be verifiable without the reviewer running a cluster.

## Priorities

In this order, when they conflict:

1. **Performance.** Startup is a feature. `eks` should be usable before the user
   notices it launched — target **under 50 ms** to first paint, and never block
   first paint on a network call. Render from cached or empty state, then fill in.
2. **Responsiveness.** The UI thread never waits on I/O. Every API call happens
   off the render path, with a visible loading state and a way to cancel. A key
   press must be acknowledged within one frame, always.
3. **User experience.** Discoverable without a manual: visible keybindings, real
   error messages that say what to do next, no raw ARNs where a name will do.
   Vim keys and arrow keys both work, everywhere.
4. **Testing and validation.** Logic is tested without a cluster. Anything that
   can be a pure function should be, precisely so it can be tested.

Being fast and being beautiful are not in tension here — both are the point.

## Architecture

`src/main.rs` is a thin shell; everything real lives in the library so it can be
tested. See `docs/ARCHITECTURE.md` for the module map and `docs/DECISIONS.md`
for why things are the way they are.

The rule that matters most: **separate computation from I/O and from rendering.**

- Parsing, formatting, filtering, and state transitions are pure functions.
- Terminal I/O is confined to `ui::run`.
- Input handling is a method on `App` that takes a key and returns a state
  change, so navigation is tested by feeding it key events, not by driving a
  terminal.

## Testing standards

- Every bug fix starts with a failing test.
- Test behaviour, not implementation. Test names are sentences describing the
  guarantee: `selection_wraps_at_both_ends`, not `test_select_next_2`.
- Rendering is tested through `ratatui`'s `TestBackend` — assert on the text
  that lands on screen.
- Always include the awkward cases: empty lists, a 1x1 terminal, missing
  kubeconfig fields, a context that points at a cluster that does not exist.
- Never write a test that needs live AWS credentials. Cluster responses are
  fixtures.
- `make check` must pass before anything is pushed. CI runs the same thing.

## Conventions

- **Rust 2024**, stable toolchain. MSRV is declared in `Cargo.toml`.
- **No panics in library code.** `unwrap`, `expect`, and `panic!` are denied by
  lint. A panic in a TUI leaves the user's terminal wedged. Return `Result`.
  Tests may opt out locally with `#![allow(clippy::unwrap_used)]`.
- **No `unsafe`.** Forbidden crate-wide.
- Errors: `thiserror` for typed library errors, `anyhow` at the command layer.
  Error text is written for a user having a bad day — say what failed and what
  to do about it.
- All colour goes through `theme::Theme`. Never hardcode a `Color` in a widget.
- Comments explain *why*. The code already says what.
- Add a dependency only when it earns its place; note anything notable in
  `docs/DECISIONS.md`.

## The nightly loop

Each night a fresh session picks up the next task. The procedure is in
`.claude/commands/nightly.md` — run `/nightly`. In short:

1. Read `docs/ROADMAP.md` and take the highest-priority unchecked task that fits
   in **one reviewable pull request**.
2. Branch from `master` as `nightly/<yyyy-mm-dd>-<slug>`.
3. Build it, with tests. Run `make check`.
4. Open a PR that explains what changed, how it was verified, and what a
   reviewer should look at. Tick the task in `docs/ROADMAP.md` in the same PR.

### What "one pull request" means

Roughly 200–500 lines of diff. If a task turns out bigger, split it: land the
smallest useful piece and add the remainder to the roadmap as follow-ups. A PR
that a human can review with coffee in hand beats a PR that does more.

Never leave `master` broken, never merge your own PR, and never weaken a test to
get CI green — if a test is wrong, fix the test deliberately and say so in the PR.
