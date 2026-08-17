# Decisions

Short records of choices that would otherwise get re-litigated. Append as they
are made; amend rather than delete when one is reversed.

---

### 1. Rust, with `ratatui` for the interface

Startup time and steady-state CPU are features here, and a garbage-collected
runtime makes a sub-50 ms budget a fight. Rust also ships a single static binary,
which makes the install story trivial. `ratatui` is the mature immediate-mode TUI
library and its `TestBackend` lets rendering be unit-tested — decisive, given how
much weight this project puts on validation.

### 2. Thin binary, fat library

`main.rs` parses arguments and dispatches; everything else lives in the library.
Code in a binary crate is awkward to test, so keeping `main.rs` trivial is what
makes the rest testable at all.

### 3. Panics are denied by lint

`unwrap`, `expect`, and `panic!` are `deny` in `Cargo.toml`. A panic inside a TUI
leaves the terminal in raw mode with no echo — the user's shell appears broken.
Every panic is a bug we ship to someone's terminal, so the compiler stops them.
Tests opt out locally.

### 4. Kubeconfig writes go through the untyped YAML tree

We model only the handful of kubeconfig fields we display, but a real config also
holds exec credential plugins, extensions, and proxy settings. Deserialising into
our types and re-serialising would silently delete all of it and break the user's
authentication. So reads use the typed view and writes mutate
`serde_yaml_ng::Value` in place, touching only `current-context`.

Writes go to a sibling temp file and are renamed into place, because a partial
write to a kubeconfig is a genuinely bad afternoon.

### 5. Show cluster names, not ARNs

`aws eks update-kubeconfig` names contexts after the cluster ARN. It is precise
and unreadable. `ClusterView` derives a short name, region, and account from the
ARN; the UI shows `prod (us-east-1)` and keeps the ARN for when it is asked for.
`eks use` accepts either, and refuses to guess when a short name is ambiguous.

### 6. No async runtime until something awaits

`tokio` was added during scaffolding and removed again before the first commit:
nothing in the tool awaits yet, and an unused runtime is build time and binary
size for nothing. The first task on the roadmap adds it back deliberately,
together with the Kubernetes client that needs it.

### 7. `serde_yaml_ng` instead of `serde_yaml`

`serde_yaml` is archived and unmaintained. `serde_yaml_ng` is the maintained fork
with the same API.

### 8. One reviewable pull request per night

Work lands as a nightly PR sized for a human to review over coffee — roughly
200–500 lines. The constraint is the point: it forces tasks to be split into
independently valuable pieces, keeps `master` releasable, and keeps a human in
the loop on every change. See `CLAUDE.md`.
