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

### 9. The async runtime is built per command, not around `main`

`#[tokio::main]` would build a runtime for `eks contexts` and `eks use`, which
never await anything — pure cost against a 50 ms startup budget. Instead
`commands::block_on` builds a current-thread runtime for the commands that talk
to a cluster. A one-shot command spends its life waiting on a single request, so
worker threads would only add startup time.

### 10. `kube` with `rustls`, `ring`, and `http-proxy`

`rustls` over OpenSSL because a single static binary is the install story, and
linking the system OpenSSL undoes that. `ring` has to be named explicitly: with
`default-features = false` no crypto provider feature reaches `rustls`, and it
then *panics* at the first TLS handshake asking to be told which provider to
use. `http-proxy` because `kube` refuses to build a client at all when
`HTTPS_PROXY` is set without it, and corporate proxies in front of EKS are
common enough that failing there would be a support burden.

### 11. Cluster failures are translated at the boundary

`kube` reports an expired SSO session as `ApiError: ... (Status { code: 401 })`.
Correct, and useless to the person who needs to run `aws sso login`.
`k8s::client::explain` classifies the error and returns a sentence naming the
cluster and the next action; the raw error goes to `tracing::debug`, so `-v`
still has it. The classification is deliberately coarse — five kinds — because
an arm only earns its place if it leads to advice worth printing, and a
plausible-but-wrong suggestion costs more time than an honest raw error.

For the same reason the default log filter turns `kube_client` off: it logs
every failed request at ERROR, and printing that above our own sentence means
the user reads the unhelpful one first.

### 12. Times come from `jiff`, via `k8s-openapi`

`k8s-openapi` 0.28 exposes timestamps as `jiff::Timestamp` and re-exports the
crate. We use that re-export rather than depending on `jiff` directly, so the
version can never drift from the one the API types are built against.

`k8s-openapi`'s `latest` feature picks the newest Kubernetes API version it
knows. The node fields we read — conditions, `nodeInfo`, object metadata — have
been stable for many releases and are all optional in the generated types, so an
older EKS control plane deserialises fine. Pin an explicit `v1_NN` feature if we
ever reach for an API that only exists in newer releases.

### 13. Quantities are integer thousandths in an `i128`

`k8s-openapi` models a resource quantity as a newtype over `String`, so the
arithmetic is ours. `k8s::quantity::Quantity` holds the value as thousandths of
a unit — millicores for CPU, thousandths of a byte for memory — in an `i128`.

Integer thousandths rather than an `f64` because a millicore is the smallest
unit anyone schedules against, and a quantity that survives a round trip
unchanged is far easier to reason about than one that is `3.9199999999999995`.
`i128` because thousandths of an exbibyte overflow an `i64`, and `Ei` is in the
grammar whether or not anyone has the hardware.

The cost is that values finer than a thousandth — a `1n` extended resource —
round to the nearest thousandth. Nothing displayed is measured that finely, and
carrying arbitrary precision to hide a rounding nobody can see is machinery
without a payer.

Two things are deliberately strict. A capital `K` is rejected: the grammar only
has the lowercase one, and so does `kubectl`, so accepting it would make us the
odd tool out. And a number too large to represent is `TooLarge` rather than
`Malformed`, because telling a user their perfectly well-formed value is not a
quantity would send them looking for a typo that is not there.

### 14. Memory is shown in binary units, unlike `kubectl`

`kubectl` prints allocatable memory exactly as the node reported it —
`7134420Ki`. Precise, and unreadable at a glance, which is the only thing a
capacity column is for. `quantity::memory` picks the largest binary unit that
leaves a legible number and shows one decimal: `6.8Gi`. Everything is shown in
binary units even when the node used a decimal suffix, because a column mixing
`1G` and `1Gi` is worse than one that is consistently approximate.

The node table shows `allocatable/capacity` in one cell rather than two columns.
The gap between the two is the kubelet's reservation, which on a small EKS node
is a surprisingly large slice, and putting the numbers next to each other is
what makes that visible without a second column of arithmetic.

### 15. Pod requests follow the scheduler, not the obvious sum

`pods::effective_requests` is `max(sum of app containers and sidecars, the peak
init container)` plus pod overhead, per resource. Every term is there because
the scheduler reserves it, and the naive "add up every container" is wrong in
both directions: it double-counts init containers that have already exited, and
it misses the sandbox overhead a RuntimeClass declares.

The subtle one is sidecars — init containers with `restartPolicy: Always`. They
never exit, so they belong in the steady-state sum *and* in the footprint of
every init container that starts after them. Order matters: an init container
listed before a sidecar never overlaps with it. That ordering is what the
`an_init_container_before_a_sidecar_is_not_charged_alongside_it` test pins down,
and it is the clause most likely to be broken by a well-meaning simplification.

The maximum is taken per resource rather than by picking one "largest"
container, because a pod with a CPU-hungry init container and a memory-hungry
one needs the peak of each.

Terminating pods still count. They hold their place on the node until the
kubelet confirms they are gone, and a draining node that reads as empty is a
worse lie than one that reads as full for a few seconds longer than it is.

### 16. A failed pod listing empties two columns rather than the command

`eks nodes` now issues two listings. They are concurrent, so the command costs
one round trip rather than two, and they are not equally fatal: a node listing
that fails ends the command, while a pod listing that fails leaves `CPU REQ` and
`MEM REQ` reading `-` with a footnote explaining why.

The asymmetry is deliberate. Read-only roles that cover nodes but not pods in
every namespace are common, and throwing away a node table that we already have
in hand would be a worse answer than an honest partial one. `-` and `0 (0%)` are
kept visibly different for the same reason: "we could not find out" and "nothing
is running here" are different facts, and a shared rendering would quietly turn
one into the other.
