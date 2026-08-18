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

### 17. `eks pods` reimplements `kubectl`'s STATUS derivation, faithfully

The word in `kubectl`'s `STATUS` column is not a field. `pod.status.phase` only
ever holds `Pending`, `Running`, `Succeeded`, `Failed`, or `Unknown`, and none of
those is what a person is looking for — `CrashLoopBackOff`, `Init:0/2`,
`Terminating`, `Evicted` and `Completed` are all derived from the container
statuses underneath by a specific, order-dependent walk.

We copy that walk rather than invent a clearer one. People read a `STATUS`
column by habit, and a tool that says something subtly different from the
`kubectl` next to it makes them stop and check — which costs more than any
improvement in wording would save. The parts that look like bugs are kept on
purpose and each carries a test: the app containers are walked *backwards* so
the first container in the spec is the one named; a started sidecar is skipped
by the init walk but still counts towards the ready fraction; a plain init
container's restarts are dropped from the total once initialisation is over,
while a sidecar's survive; and `Initialized` being true ends the init phase even
when an init container is still reporting.

Two places where a judgement was needed rather than copied:

- **Severity.** `theme::Severity` has to come from somewhere, and the mapping
  lists the calm words and the settling ones and treats *everything else* as a
  failure. That way round on purpose: the set of things that can go wrong with a
  pod grows with every Kubernetes release, and a reason this tool has never
  heard of should arrive coloured as a problem rather than quietly as fine.
  `1/2 Running` is a warning, not a success, for the same reason.
- **An empty status.** A pod caught between admission and its first kubelet
  report derives to an empty string, where `kubectl` prints nothing. We print
  `Unknown`, because a blank cell in a table reads like a rendering bug rather
  than a fact about the pod.

### 18. `eks pods` lists finished pods; the node totals do not

`k8s::pods` now has two fetches. `fetch` filters the terminal phases out
server-side, because it exists to total what is *booked* on a node and a
completed Job holds nothing. `fetch_scope` filters nothing, because the
`Completed` Job that ran an hour ago and the `Evicted` pod that explains the
morning are exactly what someone runs `eks pods` to find.

`--namespace` and `--all-namespaces` are rejected together rather than one
silently winning. kubectl lets `-A` override, which leaves a user reading a list
they did not ask for and believing they did; a one-line error naming both flags
costs less than that. And a `403` on `--all-namespaces` adds a sentence
suggesting `-n <namespace>`, because access bound to a single namespace is the
usual cause and the cluster-wide list is the only call it cannot serve.

### 19. Selectors are parsed here, not handed to the API server raw

`eks pods -l` and `--field-selector` could each be a one-line pass-through:
take the string, call `ListParams::labels`, let the API server judge it. We
parse and validate them ourselves in `k8s::selector` first, for the same reason
`k8s::quantity` reimplements the quantity grammar rather than trusting a parse
downstream.

A selector is a request, not a guarantee. A malformed one comes back as a `400`
whose body talks about parse offsets in a string the user cannot see, arriving
*after* the credential helper has run and a round trip has happened. Validating
before connecting turns that into an instant, local error that quotes the part
that is wrong — `"env in"` is missing its value list — which is the whole
acceptance criterion for the task and, more to the point, the difference between
a typo you fix in a second and one you debug against a cluster.

Parsing also lets us emit a *canonical* form: `==` folded to `=`, whitespace
normalised, so `app == api` and `app=api` reach the wire identically. The parser
is a pure function with no Kubernetes types in its signature, so the two
grammars — label selectors with set membership and existence, field selectors
with equality only — are covered by a fixture table rather than by provoking a
cluster.

Two smaller calls:

- **kube's `Selector` type is not reused.** `kube::core::Selector` models a
  parsed selector but has no parser from the string form a user types, and its
  `Display` sorts and de-duplicates set values. We keep our own tiny
  representation so `env in (prod, staging)` round-trips in the order it was
  written, which makes a canonicalised selector recognisable rather than
  reshuffled.
- **A blank selector is absent, not empty.** `-l ''` folds to `None` rather than
  being sent as an empty label selector, because an empty selector string is a
  thing some servers treat differently from no selector at all, and "filter by
  nothing" is what the user meant. An empty *filtered* listing says which
  selector emptied it, so a live namespace a filter cleared does not read like an
  empty one.

### 20. `NodeMetrics` is hand-written, and the fetch sits behind a trait

`metrics.k8s.io` is not part of Kubernetes. It is an aggregated API served by
metrics-server, an optional add-on that EKS does not install for you, and
`k8s-openapi` only generates the core API — so there is no `NodeMetrics` type to
import. We write one: a serde struct plus a `kube::Resource` impl whose group,
version, and plural are the whole content of the decision, because they are what
put `/apis/metrics.k8s.io/v1beta1/nodes` on the wire. Get them wrong and every
cluster looks like it has no metrics-server, which is why there is a test that
asserts the URL rather than trusting the four strings to be read carefully.

Only `metadata` and `usage` are modelled. `serde` ignores the rest by default, so
a newer metrics-server adding a field cannot break a listing.

Fetching goes behind a `Source` trait. Not for indirection's sake — there is
exactly one real implementation, on `Client` — but because the answers worth
testing are ones a cluster will not give on demand: no metrics-server at all, a
node the sampler has not reached, a reading that will not parse. A fake source
makes each of those a fixture. The trait returns `impl Future + Send` rather than
using `async fn`, so the future can go into the `tokio::join!` beside the node
and pod listings.

### 21. Absent usage costs two columns, not two columns of dashes

A failed *pod* listing leaves `CPU REQ` and `MEM REQ` reading `-` (decision 16).
A missing metrics API drops `CPU USE` and `MEM USE` from the table entirely.

The asymmetry is about which case is normal. A pod listing failing is unusual —
a specific RBAC shape — so the columns stay and say they could not be filled.
metrics-server being absent is the *default* on a fresh EKS cluster, and a
default that permanently adds two dead columns to everyone's node table is a tax
on the common case to explain the uncommon one. The columns appear when there is
something to put in them, and a footnote naming what to install carries the news
otherwise.

The columns are all-or-nothing across a listing, not per row: one node the
sampler has not reached yet reads as `-` beside its neighbours rather than
collapsing the columns for everybody. So the decision is `any`, and it is a pure
function over the rows.

### 22. Usage and requests share one type, and one denominator

`nodes::Share` — what was `Requested` — now carries both what the pods on a node
have booked and what the node is actually burning. One type rather than two
near-identical ones, because the thing that would drift if they were separate is
the *classification*: `Severity::from_utilisation` deciding that 90% is alarming
has to mean the same thing in both columns or the table teaches the user nothing.

Both divide by **allocatable**, not capacity. For requests that is simply
correct — allocatable is what the scheduler hands out. For usage it is a choice:
a container can and does burn into the kubelet's reservation, so usage can read
above 100%, and it is measured against a number that is not a hard ceiling. It is
still the right denominator here, because the two columns sit side by side and a
reader comparing 80% booked against 40% used is comparing fractions of the same
thing. A utilisation *bar* asks a different question and wants capacity
underneath it; that is on the roadmap rather than smuggled in here.

Usage that cannot be read stays `None` rather than folding to zero, unlike a
missing container request — which really is zero, because a container that asked
for nothing has asked for nothing. A node with no usage reading is a node we have
not heard from, and drawing that as an idle machine would be an invention.

### 23. A pod's usage is all of its containers or none of them

`metrics.k8s.io` reports node usage as one map and pod usage as a *list* of
per-container maps, so `metrics::pod_usage` has to add the containers up. The
decision is what to do when one of them cannot be read — absent, or a figure
that will not parse.

We give up on the whole pod for that resource. The alternative, summing the
containers that did report, produces a number that is smaller than the truth and
completely indistinguishable on screen from a correct one: `250m` beside a pod
whose sidecar was not counted looks exactly like `250m`. A `-` is a worse-looking
cell and a better answer, because it is the only one that cannot mislead. The
resources are decided independently, so a pod missing a CPU reading still shows
its memory.

A pod with *no* containers in the sample is unknown for the same reason it is not
zero elsewhere (decision 22): that is what metrics-server sends for a pod it has
registered but not yet scraped, and a fresh pod drawn as idle is exactly the
wrong answer during an incident.

### 24. The pod metrics listing takes the label selector but not the field one

`eks pods -l app=api` narrows the metrics request too — the aggregation layer
filters on labels like any other API server, and on a large cluster that is worth
the payload. `--field-selector` is deliberately not passed on: metrics-server
does not implement field filtering, and the fields people select on
(`status.phase`, `spec.nodeName`) are not on a `PodMetrics` in the first place.
Sending one would ask a server to filter on something it cannot see.

The columns still follow both selectors, because usage is *joined* onto the pod
rows by namespace and name rather than being a listing in its own right. Only
pods the API server already returned — after both selectors — have a row that a
figure can land on. Extra samples in the response simply have nowhere to go.

That join is keyed on namespace *and* name, not name alone: `kube-system/coredns`
and `payments/coredns` are ordinary, and `-A` puts them in one table.

### 25. Live usage is not fatal to `eks pods`

Same rule as the node table (decision 21), for the same reason: metrics-server is
an add-on EKS does not install for you, so its absence is the default rather than
an error. The pod listing is the fatal request; a failed metrics request costs
`CPU` and `MEMORY` and earns a footnote saying what to install. The two requests
go out together in a `tokio::join!`, so the columns cost one round trip's worth of
waiting rather than a second one.
