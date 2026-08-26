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

Work lands as a nightly PR sized for a human to review over coffee. The
constraint is the point: it forces tasks to be split into independently valuable
pieces, keeps `master` releasable, and keeps a human in the loop on every change.
See `CLAUDE.md`.

*Amended after #15.* The size was stated as 200–500 lines of diff, which measured
the wrong thing. Tests here run two to four times the length of the code they
cover, so a 500-line ceiling on the *total* left barely a hundred lines for the
change itself, and the loop began splitting on the line count rather than at a
seam. #15 is the example: it landed a note saying an ordering had ranked nothing
and deferred *pointing that note at the fix* to a second PR — one thought cut in
half, and half of it below the bar priority 3 sets for error messages. The budget
was set to 200–400 lines of production change with tests on top, and a slice had
to be complete before it was asked to be small. Deferral needs a seam to happen
at: an untouched surface, an open design question, or a night's work of its own.

*Amended again.* The replacement number turned out to be the same failure mode
one size down: PRs were still being shaped to land under a ceiling rather than
to finish the thought, and the remainder kept going back onto the roadmap
instead of into the PR — the roadmap grew faster than it shrank. There is no
line target now, in this decision or in `CLAUDE.md`. A PR is sized by whether
it is a complete, reviewable change; nothing here caps how large that is
allowed to be.

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

### 26. Restart recency is carried by the count, not gathered beside it

`kubectl` prints `RESTARTS` as `9 (5m ago)`, and the parenthesis is the half that
answers the question — a count with no recency cannot tell a pod that crashed
nine times last Tuesday from one crashing right now.

The date is the newest `lastState.terminated.finishedAt`, and the interesting
decision is *which* containers it is taken across. It is exactly the set whose
restarts survived into the count: during initialisation, every init container
walked; afterwards, the sidecars plus the app containers, with the plain init
containers' history dropped. So `Init.last_restart` and `Init.sidecar_last_restart`
shadow `Init.restarts` and `Init.sidecar_restarts` line for line, and the
assignment that discards the init counts discards their timestamps in the same
statement. Gathering the timestamp separately — a `max` over all the statuses —
would be shorter and would let a finished init container date a count it is no
longer part of, showing `3 (1m ago)` for a pod whose last real restart was
twenty minutes back.

A pod that has never restarted keeps a bare `0` rather than gaining a `(— ago)`.
Most rows in a healthy listing are that row, and there is genuinely nothing to
date. So is a restart with no `finishedAt`: the count is real, and inventing a
moment for it would not be.

The formatted age lives on `PodRow` rather than the `Timestamp` it came from, for
the same reason `age` does — every row in a listing is rendered against the one
instant handed to `PodRow::from_pod`, so rendering never reaches for a clock.

### 27. Ordering is a function over rows, and the count is only the tie-break

`--sort restarts` sorts `PodRow`s, not `Pod`s. That is what lets the whole
ranking be a fixture table — the awkward cases are two `i32`s and two
`Option<Timestamp>`s rather than container statuses arranged to produce them —
and it is also what will let the dashboard reorder a listing it already has in
memory without going back to the API server.

The key is *when*, not *how many*. Sorting by restart count reads like the
obvious thing and answers the wrong question: it puts the pod that failed two
hundred times last week above the one that started crashing a minute ago, which
during an incident is precisely backwards. The count survives as the tie-break,
where it decides between two containers killed by the same node problem at the
same instant.

Three ranks, not two. A restart with no `lastState.terminated.finishedAt`
(decision 26) is neither dated nor absent: the count is real, so sorting it in
among the healthy pods would bury a genuine crash, but there is no moment to
rank it against the dated ones, and inventing one — treating it as the epoch, or
as now — would put a pod somewhere it has not earned. It sits between the two,
in its own rank.

Every ordering is total, ending in namespace-then-name. An ordering that is only
*nearly* total shows up as a listing that changes shape between two runs of the
same command against an unchanged cluster, which reads as a bug in the cluster
rather than in the sort.

`PodRow` therefore carries both `restart_age` (formatted, for rendering) and
`last_restart` (the instant, for ordering). They are redundant on purpose:
`restart_age` rounds, so two pods that crashed forty seconds apart both read
`5m` and cannot be ordered by what is on screen, while rendering still must not
reach for a clock of its own.

### 28. `--sort` is a `clap::ValueEnum` on the domain type

`k8s::pods::Order` derives `ValueEnum` itself rather than `cli.rs` defining a
parallel enum and converting. The alternative is a translation table that can
drift, guarding a boundary that is not really there: which order to print a
listing in is a presentation choice in both directions, not a Kubernetes concept
being adapted for the command line. Deriving it there also means clap owns the
"that is not one of the orders" message, listing the ones that are.

### 29. `eks pods`'s flags travel as a struct

`commands::pods::list` takes a `Request` rather than eight positional arguments.
Past seven, `clippy::too_many_arguments` is denied here, and the lint is right
for the usual reason: four of the flags are `Option<&str>` and a mistake that
swaps two of them type-checks. The struct is raw command-line text — validating
it is still `list`'s first job, before it connects to anything.

### 30. Reversing an order does not reverse its unrankable tail

`--sort-reverse` flips the comparison between rows an order can rank, and leaves
everything else where it was. A pod that has never restarted, one with no
`creationTimestamp`, one metrics-server has not sampled — none of these move out
of the tail when the listing is reversed.

Reversing the whole comparison would be simpler and is wrong. "Least CPU" is a
question about which pod is idle; the pod with no sample is not idle, it is
unmeasured, and putting it first answers a question nobody asked while burying
the one they did. The same argument holds for `restarts`, where it would open a
reversed listing on ninety-nine healthy pods.

So each order maps a row to a private `Rank`, either `By(key)` or
`Unranked(tier)`. Only `By` is flipped; `Unranked` always sorts after `By` and
its tiers keep their own order. The tiers exist because `restarts` has two kinds
of blank — a restart the kubelet recorded no `finishedAt` for is a real crash
with no moment attached, and belongs above a pod that has never restarted at all
— and reversal must not collapse them either.

The alphabetical tie-break at the end is never reversed. It exists to make every
order total rather than to say anything, and reversing it would mean the two
directions of one order disagreed about rows they both consider equal.

### 31. `--sort age` puts the youngest pod first

The opposite way round from `kubectl --sort-by=.metadata.creationTimestamp`,
which prints oldest first. The rule this tool follows is that every order but
`name` leads with the row the person went looking for — the newest restart, the
largest usage figure — and during an incident the question behind `AGE` is "what
changed", which the youngest pod answers. One rule across five orders is worth
more than matching another tool on one of them, and `--sort-reverse` is the
`kubectl` reading for anyone who wants it. Both the `--help` text and the README
say so, because a sort that runs the way you did not expect is indistinguishable
from a broken one.

### 32. `--sort-reverse` rather than a `-` prefix on the order

`--sort -cpu` was the other candidate. It loses: clap has to be told to allow
hyphenated values before it will accept it, `--sort -cpu` and `--sort=-cpu`
behave differently once it is, and the accepted-values list clap prints on a typo
stops matching what the flag really takes. A separate boolean flag composes with
every order for free, shows up in `--help` next to the one it modifies, and needs
no parsing at all.

### 33. `Direction` and `Rank` moved up to `k8s::order`; the keys did not

`eks nodes --sort` needs the same two rules `eks pods --sort` already had — which
way round an order runs, and that a row an order cannot rank stays in the tail
under either direction — and those rules are worth exactly nothing if the two
tables can drift apart on them. So `Direction`, `Rank`, and the comparison that
keeps ranked and unranked rows apart now live in `k8s::order`, with the rule
written down once in its module docs and asserted on the primitive rather than
on one listing's rows.

The keys stayed put. A node has no restart count and a pod has no allocatable
capacity, so each listing keeps its own `Order` enum, its own `sort`, and its own
rank functions. A shared trait over "things that can be sorted" would have bought
nothing here: the only code it would have deduplicated is the four-line `sort`
that appends the alphabetical tie-break.

### 34. The node orders rank by share; the pod orders rank by the figure

`eks pods --sort cpu` ranks by the figure a pod is using rather than by its share
of what it asked for. That began as "there is nothing else" — a pod's usage had
no denominator then — and decision 40 has since given it one, so the choice is
now a choice, and it stands. A node's denominator is the machine, which makes
95% of a small node genuinely comparable to 30% of a large one. A pod's
denominator is whatever somebody typed into a manifest, so a pod at 400% of a 10m
request is burning 40m and is nobody's problem, while one at 60% of four cores is
eating the node. The share is a real question about that pod's own sizing, and
it is a different question from the one `--sort cpu` is asked — which is why a
share ordering is a roadmap entry rather than a correction to this one.

Doing the same for a node would answer the wrong question: the node table already shows every figure
as a percentage of allocatable, and a two-core node at 95% is closer to trouble
than a sixty-four-core node burning twenty times as much at 30%. So the node
orders rank by `Share::ratio`.

That gives node usage a second kind of blank, and the `Rank::Unranked` tiers
`restarts` needed are what carry it: a node with a figure but no allocatable to
divide it by (one still registering) sorts ahead of a node with no figure at all
(one metrics-server has not reached), and both stay behind every node that has a
percentage. A node reporting zero allocatable is in the first tier rather than at
the top of the listing as an infinity, because `Quantity::ratio_of` refuses to
divide by zero.

`Share::ratio` is an `f64`, and a sort key has to be `Ord`, so the key is a
private newtype ordered by `f64::total_cmp`. That is total over every `f64` there
is, including the ones a nonsensical reading from the API server could produce —
a strange figure then sorts strangely instead of making the comparison
inconsistent and the whole sort meaningless.

### 35. `--sort status` puts the unknown node above the cordoned one

Node health has four states and `--sort status` has to put them in some order.
`NotReady` leads, and `Ready` is last; the argument is about the middle. A node
whose kubelet has stopped reporting (`Unknown`, usually a node that has only just
registered — or one that is about to be a problem) sits above a cordoned one,
because a cordoned node is a node somebody deliberately took out of service and
the accident belongs above the intention. Nothing is unranked under this order —
every node has a status — so unlike the usage orders it reverses completely.

### 36. A reordered listing names its order; the default one stays silent

`eks nodes --sort cpu` and `eks nodes` print the same columns, the same widths
and the same rows, and to anyone who did not type the command they are the same
table. `--sort cpu --sort-reverse` is worse: the unrankable tail stays at the
bottom under either direction, so a reversed listing looks like the ordering
running the other way with a few odd rows at the end. So a reordered listing now
carries a line under the table saying which order it is in.

It is silent for the default order in its natural direction, which is what keeps
every existing command's output unchanged to the byte — a promise worth more than
the note, since it is what lets anyone paste `eks nodes` into a script or a
ticket without the tool having an opinion about it. Both halves matter:
`--sort-reverse` on its own reverses the *default* order and prints Z-to-A, which
is not the default listing and is the one most easily mistaken for it, so it
speaks.

`k8s::order::note` is generic over the two `Order` enums rather than written once
per listing, taking the name from `clap::ValueEnum::to_possible_value`. That is
the text the user typed after `--sort`, so the note cannot start spelling an
ordering differently from the flag that produced it — `cpu-requested`, never
`CpuRequested`. A variant `clap` will not name is one hidden from `--help`; there
is nothing honest to call it, so the note is dropped rather than guessed at.

It is a *note*, not a header: it joins the existing footnote list that carries
"no metrics-server" and "could not list pods", which means it lands under the
table, after the notes about what went wrong — a table nobody could fill in is
more urgent than the order it came out in — and it disappears entirely on an
empty listing, where the renderer already drops footnotes because "there is
nothing here" is the only thing worth reading.

What the note deliberately does not say is whether the ordering actually ranked
anything. `eks nodes --sort cpu` on a cluster with no metrics-server sorts by a
column that is not in the table, and every row lands in the tail. That is a
second, sharper thing to say, it depends on the rows rather than on the flags,
and it is its own roadmap entry.


### 37. "Nothing ranked" is a second note, computed by the listing

Decision 36 ends by naming the gap it left: the note says which ordering was
*asked for*, and says nothing about whether that ordering managed to rank a
single row. `eks nodes --sort cpu` on a cluster with no metrics-server is the
case — no `CPU USE` column, every row in the tail, the alphabet deciding the
whole listing — and `Sorted by cpu.` underneath makes it worse rather than
better, because it names an ordering over rows it did not arrange. The footnote
above explains the missing columns and says nothing about the flag the user
typed, so `--sort` reads as broken.

So there is a second line, `Nothing here has cpu to sort by.`, and it is a
second function rather than a third argument to `note`. The two questions have
different inputs: which order was asked for is a fact about the flags, and
whether it ranked anything is a fact about the rows. Keeping them apart is what
lets `note` stay pure in the flags alone and testable without a fixture row.

The rows half is `k8s::nodes::ranks_any` and `k8s::pods::ranks_any`, because the
keys are the part of an ordering `k8s::order` deliberately does not know. Both
are `any`, not `all`, following the rule the usage columns already use: one
unsampled row is not a listing the ordering failed to order, and one ranked row
is enough to put the row somebody went looking for at an end of the table.

Rankability is a second exhaustive `match` over `Order`, sitting beside the
comparison rather than being derived from it. Two matches are a drift risk, and
exhaustiveness is the answer — adding an ordering without saying what makes a
row rankable under it will not compile. They are also not the same function:
under the pod `restarts` order, a restart the kubelet recorded no `finishedAt`
for is `Rank::Unranked` — there is no moment to rank it against the dated ones —
but the count is a key under that ordering as well as a tie-break, so the
ordering did lift that pod clear of the healthy rows. Deriving rankability
mechanically from `Rank` would print "nothing here has restarts to sort by" over
a listing with a crashing pod near the top of it.

The note stops at the diagnosis and does not name the order the rows came out in
instead. Unranked rows keep their tail tiers, so a listing split across two of
them is grouped by *something* even when nothing in it ranked, and "this is in
name order" would be a guess dressed up as an explanation. It is also silent for
the default ordering: nobody typed a flag, so there is no flag to explain, and
the byte-for-byte promise of decision 36 holds unchanged.

### 38. The "nothing ranked" note advises, and never advises twice

Decision 37 stopped at the diagnosis: `Nothing here has cpu to sort by.` is
honest, and it is not yet advice. Two things were missing, and they are
different problems.

The first is repetition. On a cluster with no metrics-server the footnote two
paragraphs up has already named the cause and linked to metrics-server, so
saying it again in different words would be the same paragraph twice, a line
apart. `k8s::order::Cause` is how the listing says which it is, and the note then
either points back — `…, for the reason above.` — or stands on its own. It is the
listing's answer rather than `k8s::order`'s because which of a table's footnotes
covers which column is exactly the knowledge that module does not have:
`k8s::nodes::cause` and `k8s::pods::cause` are third exhaustive matches over
`Order`, beside the comparison and the rankability one, so an ordering added
without saying which failure could account for it will not compile.

The wording is "for the reason above" rather than "the note above says why"
because the paragraph *directly* above is decision 36's `Sorted by cpu.` line,
which gives no reason at all. A reason is the one thing up there that can only be
the failure footnote.

The second is that under `--sort restarts` in a healthy namespace nothing is
above the note at all — nothing failed, the pods simply have not crashed — so
pointing upwards would point at nothing. What that listing is owed is the flag
that *would* have worked, and the note can work it out: `unranked_note` takes the
`ranks` predicate rather than a bare `ranked: bool` and asks it about every
variant of the `Order` enum, so the suggestions come from the rows in front of
the user. It cannot name an ordering that would have failed the same way, and it
cannot drift as orderings are added.

Two variants are left out. The default is what dropping `--sort` altogether
gives you, so "sort by name instead" is advice to type a flag in order to get the
listing you would have had anyway; and a variant hidden from `--help` is a flag
value the user cannot find any other way. When that leaves nothing, the advice
line is dropped rather than invented — on a table where no ordering ranks,
silence says "there is nothing else here to sort by" better than a suggestion
that would fail identically.

The bar for a suggestion is *rankable*, not *tells the rows apart*, which is the
same `any`-not-`all` rule decision 37 settled. It has a visible cost: on a
cluster where every node is `Ready`, `status` is suggested and reorders nothing.
The alternative bar is a different question about an ordering from the one this
module has been answering, and it is left as a roadmap entry rather than guessed
at.

### 39. `--wide` lands on both listings, and its columns are a `Column` list

`kubectl -o wide` is where people already go for a pod's IP or a node's AMI, so
`eks` spells it `--wide` and copies both column sets to the letter, including
the headings and their order. Two departures, both deliberate:

`NODE` stays in the default pod table, where `kubectl` holds it back for wide,
because a pod listing that will not say which machine a pod is on answers half
the question people came with. So `--wide` adds three columns around it rather
than the four `kubectl` adds. And the node table's wide columns go on the *end*,
after `AGE`, rather than between `VERSION` and the capacities where `kubectl`
puts them: the default table is then the wide one with its tail cut off, and
someone comparing the two does not have to re-find the columns they were already
reading.

The flag lands on `eks nodes` at the same time as on `eks pods`, though only the
pod half was on the roadmap. `eks nodes --wide` failing as an unknown argument
while its twin accepted it would read as a bug rather than as a decision, and
the node columns raised no question the pod ones had not already settled — this
is the rule about a flag honoured by one listing and not its twin, in `CLAUDE.md`.

The two tables now build their columns as a `Vec<Column>` from a pure function
over the listing's conditions, where before each kept parallel lists of headers
and cells assembled under matching `if`s. That pairing has a failure mode that
type-checks: a heading pushed under one condition and its cell under a subtly
different one puts every figure to the right of it under the wrong heading, and
the table still renders perfectly. A `Column` answers for both halves of itself,
so the two cannot drift, and the whole layout becomes one value a test can
assert on rather than a table someone has to read in a terminal.

`format::Width` is a two-variant enum beside `format::table`, not a `bool`, for
the reason `k8s::order::Direction` is one: one type shared by both listings is
what keeps `--wide` meaning the same thing on each. It sits in `format` rather
than in `k8s` because it decides nothing about what is fetched — every field the
extra columns show already arrived with the nodes and pods, so `--wide` costs no
request and cannot fail.

Where `kubectl` prints `<none>`, these columns print `-`, which is what every
other empty cell in the tool prints. Matching `kubectl` mattered for the column
names and their order, where a reader's habits are; a second spelling of "empty"
inside one table would cost more than the resemblance is worth.

The wide columns appear whatever is in them, unlike the usage columns, which are
dropped when no row has a figure. The conditions look alike and are not: usage
columns arrive unasked for, so an empty pair is clutter charged to someone who
never wanted them, while `--wide` was typed. A column of `-` under
`NOMINATED NODE` is the answer "nothing here is being preempted", and dropping it
would leave the user unable to tell that from a flag that did nothing.

`READINESS GATES` is in the pod set even though the roadmap entry named only two
columns. The default table cannot explain a pod whose `READY` reads `1/1` while
the cluster still calls it unready — every container up, an external controller
withholding its condition — and that is a question the table itself raises. A pod
with no gates reads `-` rather than `0/0`, which would suggest something
unsatisfied on nearly every row where there is nothing to satisfy.

### 40. A pod's usage is shown against its request, in one cell

`eks pods` printed `262m` and left it there. The figure is unreadable on its own:
a quarter of a core is fine, throttled, or a mistake depending entirely on what
the pod asked for, and a reader who wants to act on the number has to go and find
that request in a manifest. The node table has never had this problem — every
figure in it is a share of allocatable — and the pod table now says
`262m/500m (52%)`.

The denominator is the **request**, and it comes from `pods::effective_requests`
— the same function `eks nodes` totals per node. A pod has no allocatable of its
own, so its request is the only honest denominator available; it is also the
number somebody would go on to change. Calling the function rather than summing
the containers again is what keeps `eks pods` and `eks nodes` from quietly
disagreeing about one pod: that sum is `max(containers + sidecars, peak init
container)` plus pod overhead, and a second implementation of it would be wrong
in a way nobody could see.

It is one cell rather than a usage column and a request column beside it, and the
two are alternatives rather than halves — a `CPU REQ` column beside a
`262m/500m (52%)` cell would print the same number twice. The pair won because
its halves are read together or not at all, because the pod table is already the
wider of the two listings, and because `READY`'s `1/2` and the node table's
`3800m/4` make `a/b` this tool's existing spelling for a part and its whole.

The other design has one thing this one does not: a request column would show
what a pod booked on a cluster with no metrics-server, which is the EKS default
and where this cell shows nothing at all. That is a second question — "what did
this ask for", not "how is it doing against it" — and answering it means columns
on every listing, which is a decision about the default table's width rather than
about a denominator. It is a roadmap entry, and the reviewer who prefers that
design should say so there.

The heading follows the cell: `CPU/REQ` over a column of pairs, and plain `CPU`
where no row in the listing has one, so the table itself says what the percentage
is a share of instead of a manual saying it. A `/REQ` over a column of bare
figures would name a denominator that is not there, which is the same rule the
usage columns already follow by being absent rather than empty.

A pod that asked for nothing keeps its bare figure. `Quantity::ratio_of` declines
a zero denominator, so "asked for nothing" and "cannot be divided" are one branch
rather than two that could drift; a percentage of zero would be an invention, and
the pod really did ask for nothing. This is deliberately not the treatment a
missing *usage* reading gets — that stays `-`, because nobody measured it, and a
request is not a measurement.

Rounding moved to `format::percentage`, shared with `nodes::Share::cell`. A node's
share of allocatable and a pod's share of its request are the same kind of figure
printed in two tables people read one after the other, and them differing by a
digit would be a bug nobody could explain.

The share is not classified into a `Severity`, unlike the node table's. The
thresholds would not carry: 90% of a node's allocatable is alarming, while 90% of
a pod's own request is a well-sized pod. What "hot" means for a pod against its
request is a decision worth making deliberately, and nothing renders colour yet,
so this waits for the roadmap entry that lights up both tables.

### 41. A usage figure is shown with its age, and an empty sample set says so

The usage columns had two states on screen and three in reality. metrics-server
missing produced a footnote saying what to install; metrics-server answering
produced the columns. metrics-server answering *with nothing* — a fresh install,
a node that joined a minute ago, a namespace whose pods have only just started —
produced neither: the columns vanished exactly as they do when it is absent, and
nothing was printed, because the footnote was written on the error path and there
was no error. From the reader's chair that is indistinguishable from the missing
case, and the advice for the two is opposite. `metrics::Outcome` makes the third
case a value rather than a gap, and `usage_unsampled` gives it the footnote it
was owed.

Which of the three a listing is in is asked of the **rendered rows**, not of the
reply. `eks pods --field-selector spec.nodeName=…` narrows the rows but not the
metrics request — metrics-server does not implement field filtering — so a reply
can be full of readings for pods the table does not contain. Asking the reply
would call that listing sampled while showing no figures at all.

The other half is that a figure which *is* shown carries no date. A number with
nothing beside it cannot be told from an instantaneous reading, and — the reason
this matters — metrics-server going quiet does not fail the request that asks it
for a sample. The same table keeps rendering, with figures that are minutes old
and look exactly like fresh ones. So every table with usage in it now ends with
`Usage is up to 12s old, averaged over 20s.`

The age is the **oldest** sample in the listing. The note is a guarantee about
the whole table and is only as good as its worst row; "up to" is what makes that
readable. The window is the **longest** any sample reported, since samples that
disagree mean two scrapers or one being reconfigured, and the slower of them
decides how long "up to date" lasts.

Stale is more than two windows. metrics-server publishes about one reading per
window, so one window of lag is what a working scraper looks like and two means a
scrape did not happen. A listing whose window we could not read is never accused:
without a window there is no scale to judge an age against, and "your figures are
stale" is not a sentence to print on a guess. A window of `0s` is treated the same
way, since it would call a listing taken half a second ago stale.

`metav1.Duration` reaches the wire as a Go duration string — `20.04s`, `1m0s`,
`500ms` — which is not anything `jiff` parses, so `metrics::parse_duration` is a
small grammar of its own. Integer arithmetic throughout: `20.04s` is exact in
nanoseconds and is not exact in binary floating point, and this value is compared
against another duration rather than merely printed. The unit table is ordered
longest-spelling-first, because `ms` read as `m` turns half a second into half an
hour and would call every listing fresh forever. Anything outside the grammar is
`None` rather than a guess — a wrong window either accuses a healthy cluster or
excuses a scraper that has stopped.

One consequence worth naming: `Missing::usage`, which decides whether the
"nothing ranked" note points at a footnote above or explains itself, now means
"the columns are gone" rather than "the read failed". Both ways of losing them
leave a footnote for it to point at, so the old reading would have printed the
same advice twice, a paragraph apart — the thing decision 38 exists to prevent.

### 42. A device is one column, and its shape is the pod table's, not this one's

`nvidia.com/gpu` had been parsed correctly since decision 13 and shown nowhere,
which made `eks nodes` unable to answer the one question a GPU cluster is ever
asked: is there a card free. A column now appears for every extended resource
some node in the listing reports, on the `any`-not-`all` rule the usage columns
already follow, so a cluster of m5.xlarges prints exactly the table it printed
before and a mixed cluster shows the CPU nodes a `-`.

What counts as "extended" is a naming rule rather than a list of vendors, which
is the point — the whole reason extended resources exist is that a cluster can
invent one. `k8s::resource::is_extended` is Kubernetes' own definition: a
fully-qualified name outside the `kubernetes.io` domain. That leaves `cpu`,
`memory`, `pods`, `ephemeral-storage`, `hugepages-2Mi`, and the
`attachable-volumes-*` limits sitting in the same capacity map alone. They are
not devices, they have native meanings a table should state in native words, and
a column headed `HUGEPAGES-2MI` reading `0` on every node is exactly the noise
the condition above exists to avoid.

The cell is `2/4 (50%)` — booked over allocatable — which is the **pod** table's
usage cell rather than either of this table's two, and that is a deliberate
inconsistency. `Capacity`'s pair prints allocatable over capacity, and for a
device those are the same number on every healthy node; the gap between them is
not the kubelet's routine reservation but a fault, so it belongs in a sentence
rather than in a pair of figures a reader has to notice are different. `Share`'s
`2 (50%)` hides the total, and for a device the total is the fact people came
for: "this node has eight A100s" is not something to work back out of a
percentage. So the device column shows both numbers, and the denominator is
still allocatable — the same one CPU REQ and CPU USE divide by, so a percentage
means one thing across a row.

That choice hides one thing, and it is the thing this column was added for. A
card the kubelet has and will not hand out — a plugin that marked one unhealthy,
most often — shrinks allocatable and leaves the cell reading `0/3 (0%)` on a node
with four. From the table that is a node with three free cards and a pod that
will not schedule onto any of them. `devices_withheld` says so, names the node
with the widest gap, counts the rest, and says where to look. It is a footnote
rather than a third number in the cell because it is not routine: the ordinary
node offers everything it has and earns no line.

Requests grew the same shape one level down. `pods::Requests` keeps `cpu` and
`memory` as fields — every caller wants them, every container may have them —
and carries everything else in a `BTreeMap` keyed by the name the cluster
invented. `plus` and `max` fold over the union, so a GPU asked for by one init
container and not the next does not vanish from the pod's footprint, and the
scheduler's arithmetic from decision 15 applies to devices without a second
implementation. The cost is that `Requests` is no longer `Copy`; the call sites
that felt it now borrow the totals rather than cloning a map per row.

Two `-` characters can appear in a device column and they say different things.
`-` alone is a node that does not report the resource: no such hardware, which is
a different answer from having none free. `-/4` is a node that has four and a pod
listing that failed, so only the numerator is unknown — the count came back with
the nodes and is still good. The footnote that explains the failure names the
device columns it emptied, for the reason decision 38 gives: a message that
diagnoses without saying which columns it is about makes the reader find them.

### 43. Every listing is paged, through one function

`eks nodes` fetched every node in one request, and `eks pods -A` every pod —
twice over on the node table, which totals the pods to fill in its request
columns. On a cluster of any size that is the largest thing this tool asks for,
and the API server has to hold the whole answer in memory before it sends a byte
of it. Kubernetes' answer is `limit` and `continue`, and `k8s::page::collect` is
now the single door every listing goes through: nodes, pods, scoped pods, and
both metrics endpoints.

`page::SIZE` is 500, which is `kubectl`'s own chunk size. It also settles the
compatibility question, because a first page that comes back short carries no
continue token: an ordinary cluster is exactly the one request it always was,
and only a big one pays for a second.

The loop is deliberately tiny and the decisions are all outside it.
`page::Listing` holds the items, remembers the token the last request carried,
and answers `Next::Page`/`Done`/`Stalled` — so a three-page listing, an empty
one, an empty-string continue token, and a server that repeats its token are
fixtures, and `collect` itself is four lines of I/O with nothing to get wrong.

`Stalled` is the one case Kubernetes never produces and we handle anyway. A
server that hands back the token it was given would page for ever, fetching the
same objects, with the command never returning and nothing on screen to say why.
It ends the listing with a `tracing::warn!` rather than an error, because the
pages that did arrive are real: half a listing with a warning beside it beats no
listing. It is not a footnote under the table — the shape the rest of this tool
words such things in — because carrying "this may be short" up from the fetch
would mean every listing function returning a pair, and this is a case a
conformant server cannot reach.

metrics-server does not chunk its replies: `limit` is a parameter it ignores, so
those two listings finish after one page regardless. They go through `collect`
anyway, because what that buys them is the *budget* below. A metrics endpoint
that has gone quiet should cost the same wait as any other request, not an
unbounded one, and it is the request whose columns the tool can most afford to
lose.

### 44. `--timeout` is spent per request, and cannot cover the credential helper

A hung API server left `eks nodes` waiting for ever with no way out but Ctrl-C,
and the shape of that failure is why it needs a flag rather than a constant: a
private EKS endpoint reached from outside its VPC does not refuse the
connection, it simply never answers. The default is 30 seconds — long enough
that a busy API server is not cut off mid-answer, short enough that a wrong
network is a sentence rather than a hang — and `--timeout 0` restores the old
behaviour for anyone who wants it.

Per *request* rather than per command, and decision 43 is why: a listing is now
several requests, and a cluster large enough to need four pages would be cut off
for its size rather than for being unreachable. The same reasoning puts `Budget`
in `k8s::page` rather than in a module of its own — the unit of the budget is
the unit of the paging.

`Budget` parses `30s`, `500ms`, `2m`, `1h`, and a bare number of seconds, which
is narrower than the Go duration grammar `kubectl` takes. The missing piece is
the compound `1m30s`, and it is missing on purpose: `Display` has to print a
spelling `from_str` reads back, because the timeout message ends with ``allow it
longer: `--timeout 1m` `` and advice that names a value the flag would reject is
worse than no advice. One unit in, one unit out, and a test asserts the round
trip.

What the flag could not promise at first was the part before the first request:
`kube` resolves a kubeconfig's auth eagerly and runs the exec plugin with a
blocking `std::process::Command`, so an `aws eks get-token` that hangs blocked
the thread rather than the future, and a `tokio::time::timeout` wrapped around
`k8s::connect` would never have fired. Decision 50 covers it, and the flag's
help now says "step" rather than "request".

### 45. `eks contexts` renders through `format::table`, gutter and all

`format::table` came out of `eks nodes` and `commands::contexts` kept its own
copy of the same column-width arithmetic, which is two chances to decide
differently what "aligned" means. The table now comes from the shared renderer
and the output is unchanged to the byte, which a test asserts in full rather
than by probing with `contains`.

The `*` marker did not move into it. It is not a column: a column would be
padded to its width and followed by the standard two-space separator, so rows
would read `*  prod`, and it is the only such marker in the tool — no other
listing has a row that is more current than its neighbours. So the table is
rendered without it and each line is prefixed afterwards, header included. That
keeps the gutter's two characters a fact about this listing rather than a
feature of every table.

### 46. `Width::Narrow` carries a target width, and the drop rule is the listing's

`eks nodes` at ten columns and around 140 characters wide wraps on the terminal
every laptop lid narrows to under a docked browser, which is where the request
and usage columns land — the ones most worth keeping. The other end of `--wide`
was always going to be a third `Width` variant rather than a listing-specific
flag; the type already existed as one place both tables agreed on how much to
show.

`Width::Narrow(u16)` carries the target width, not "narrow yes/no". Deciding
"which columns fit" from a hidden ambient terminal size would put the answer
somewhere a test could not name it, and the acceptance criterion for this task
was a pure function over an available width. So the ioctl lives in one
`stdout_terminal_cols` function in `main.rs` and the arithmetic lives in
`k8s::nodes::narrow_to_fit`, which two tests hit at 80, 100, and 1 column with
no terminal in sight.

`--wide` beats `Narrow` at the type gate: `Width::for_terminal(true, _)`
returns `Wide`. A `--wide` that widened when asked to and then narrowed itself
would be a flag that meant nothing on the terminals it exists for. A pipe is
not a "narrow terminal": `Width::for_terminal(false, None)` returns `Default`,
so `eks nodes | grep foo` is unchanged to the byte and no script parsing the
output breaks. Pods is passed the same width value for consistency; the pod
table has no drop rule yet, and `is_wide()` reads `false` on a `Narrow`
variant, so the pod table lands on its default columns rather than losing
some in a way this PR did not design.

The drop order for `k8s::nodes` is a `DROP_ORDER` list of predicates, in this
sequence: `VERSION`, `AGE`, the `REQ` pair, the `USE` pair, `CPU` and
`MEMORY`, every device column, `STATUS`. Two rules shaped it:

- **Partner columns leave before their base.** `CPU REQ` is a percentage of a
  capacity, and dropping the capacity while keeping the percentage leaves a
  figure of nothing. So `REQ` and `USE` drop before `CPU` and `MEMORY`, and
  the pair columns drop together — an eye reading `CPU REQ` next to `MEMORY`
  with no `MEM REQ` pairs the wrong numbers.
- **The interesting column outlasts the ordinary one.** A device column only
  exists because somebody installed the plugin that surfaces it; every
  cluster has `CPU` and `MEMORY`, and only the GPU cluster has `NVIDIA.COM/GPU`.
  So devices drop after `CPU` and `MEMORY` rather than before them. On a
  general cluster this step is a no-op. On a GPU cluster with a very narrow
  terminal, the row keeps the card count and loses `CPU`, which is the right
  trade — the user typed `eks nodes` for the card, not for the ordinary
  columns that were going to show up anyway.

`NAME` never drops. A row we cannot fit at all is still a row with a name; the
terminal wraps it, and dropping the name would leave a listing that has no
answer for "which node is this".

### 47. The pod table's drop order, and one measurement for both tables

`Width::Narrow` landed with the node table (decision 46) and the pod table
treated it as `Default`: a `Narrow` reaches `k8s::pods::row::columns`, falls
through the `is_wide()` check, and prints the full row on a terminal too small
for it. `eks pods` is the wider of the two listings on a cluster with
metrics-server, so it wanted the same treatment; what it could not take was the
node table's list, because none of the columns in it are the same columns.

The pod order is `AGE`, `NODE`, the usage pair, `RESTARTS`, `READY`, `STATUS`,
with `NAME` and `NAMESPACE` never dropped. Three rules shaped it:

- **The table's own repetition goes first.** `AGE` is the cheapest column on
  the row and the least of it, and it is the one fact the table already says
  twice: `RESTARTS` carries `9 (5m ago)`, so "when did this last change"
  survives it leaving.
- **A follow-up question goes before a first one.** `NODE` is the widest cell
  in the table on EKS, where a node is a forty-character DNS name, and which
  machine a pod is on is what you ask *after* you know which pod you are
  looking at. Every column that stays is there to find that pod, and dropping
  `NODE` lands on `kubectl get pods`'s own column set, where a reader's habits
  already are.
- **The health columns outlast the rest, and `STATUS` outlasts them.** A
  listing down to a name and one word keeps the word that names the problem;
  `READY`'s `0/1` is the detail under `CrashLoopBackOff` rather than a fact of
  its own, and `RESTARTS` is the widest of the three. This is the one step of
  the order that is a judgement rather than a deduction — `Running 0/1` is a
  real pod, and an argument for `READY` outlasting `STATUS` could be made.

`NAMESPACE` never dropping is the pod table's own rule, and it is not the node
table's `NAME` rule wearing a hat. The column is in the table only under `-A`,
and there a name is not an identity: `coredns-abc` in `kube-system` and a copy
of it in another namespace are two pods, and the column the user widened the
scope to get is the only thing telling them apart. Under `-A` the pair is the
name, so it drops when `NAME` does, which is never.

The `--wide` columns are not in the list, because they cannot be in the table:
`Width::for_terminal(true, _)` answers `Wide`, so a `Narrow` listing never
carried `IP`, `NOMINATED NODE`, or `READINESS GATES` in the first place. A step
for them would be a step that never fires.

The measurement moved. `k8s::nodes::row_width` mirrored `format::table`'s
arithmetic by hand, with a comment saying the drop rule was its only caller —
which was true for one night. A second caller is the condition that changes the
answer, so the rule lives in `format` now, as the pair `column_widths` (as wide
as the widest cell, or the header) and `row_width` (two spaces between, and
none after the last), with `format::table` and both drop rules going through
them. Two copies of that would be free to drift, and a listing measuring rows
the renderer disagreed with would drop a column to fit a width nothing prints
at. A test ties the two to the renderer: a row measured from `column_widths`
is exactly the longest line `table` actually prints, over a cell wider than its
header, a ragged row, a header-only table, and a single column with no
separators to count. Both listings assert the same thing from the other end —
every line of a `Narrow(80)` render is at most 80 characters — so the guarantee
is checked against rendered output rather than against the arithmetic that
chose the columns.

Splitting the two is also what makes narrowing one pass over the listing rather
than one per drop step. A column is as wide as its own widest cell whatever its
neighbours do, so dropping one changes which widths are in the sum and not what
any of them are: each rule measures once, zips the widths onto the columns, and
then does arithmetic over a dozen numbers. The obvious loop — re-render every
cell, re-measure, drop, repeat — costs a listing's worth of string formatting
per step, seven of them on a ten-thousand-pod table, for an answer that cannot
have changed.


### 48. The pod count rides with the request totals, and is a share like everything else

`PODS` is a count, and every other figure on the node row is a measured
quantity. Two choices follow from refusing to let that difference matter.

**One walk over the pods, not two.** `pods::by_node` already decided, pod by
pod, which pods are occupying which node — the terminal phases out, a
`Terminating` pod still in, an unscheduled one charged to nobody. Counting in a
second pass would be a second chance to answer that differently, and the failure
would be invisible: a `PODS` cell saying 12 beside a `CPU REQ` cell totalling 14
pods' requests is two plausible numbers, and nothing on screen says they
disagree. So `by_node` returns a `Placed` — a count and a `Requests` — from the
one loop, and `NodeRow::from_node` takes that one value rather than two
parameters that could arrive out of step. It is also why a failed pod listing
empties both halves together: they are the same `Option`.

**The count becomes a `Quantity`.** The denominator, `allocatable["pods"]`, came
off the wire as a quantity string, and the numerator is an integer we counted
ourselves. `Quantity::from_count` makes them the same type, which buys the
column `Share` entire: the ratio, `theme`'s severity thresholds, the cell
format, and `nodes::order`'s `busiest` key, none of them written twice. The
alternative — a bespoke pair of integers with its own division — would have been
a second rule for what counts as hot and a second place to get "the API server
reported no allocatable" wrong.

The cell is `Share::pair`, `12/58 (21%)`, and not the `12 (21%)` the request
columns use. `CPU REQ` can leave its denominator out because `CPU` is the column
next to it; `PODS` has no such neighbour, and the limit is half of what the
reader came for. It varies by instance type and by CNI configuration, so a bare
`21%` names a fraction of a number nobody in front of the table knows. That is
the same argument the device columns made, so `Device::cell` now delegates to
`Share::pair` rather than keeping its own copy of the formatting.

`allocatable`, not `capacity`, is the denominator. Both are reported and they
differ: the kubelet's `--max-pods` and the VPC CNI's address budget land in
allocatable, and it is the number the scheduler counts against. Dividing by
capacity would flatter exactly the nodes whose CNI is the binding constraint,
which on EKS is most of them.

The column is unconditional, where the device columns appear only when some node
reports the resource. `CPU REQ` is the closer analogue: every node has a pod
limit, so there is no cluster the column would be a row of dashes on, and the
one state that empties it — a failed pod listing — leaves it reading `-/58`,
which still carries the limit. That is also why the footnote for that failure
now names it: `requests_unavailable` says the *booked half* of `PODS` and the
device columns is empty, because saying the columns were empty would be visibly
untrue on screen.

`--sort pods` comes with it, and ranks the share rather than the headcount, as
every other node order does (decision 34). A node with 80 pods out of 234 is not
the node to look at; the one with 50 out of 58 is. Its unrankable tail is the
nodes with no count, under `k8s::order`'s existing two-tier rule, and
`nodes::cause` maps it to the request footnote — the count is the pod listing's,
so one failure explains all three of `cpu-requested`, `memory-requested`, and
`pods`.

In `DROP_ORDER` it goes third, after `VERSION` and `AGE` and ahead of the `REQ`
pair. Two reasons, and the second is the one that settled it. A node runs out of
CPU or memory long before it runs out of pod slots unless the CNI's address
budget is what is short — so of the three booked figures this is the least often
the binding one. And an 80-column node table has been keeping `CPU REQ` and `MEM
REQ` since decision 46; a column added afterwards should not be what takes them
away. The existing test asserting that 80 columns keep the request pair failed
when `PODS` was placed later, which is exactly the signal that test was written
to give.

### 49. Colour is spent on the rows worth looking at, and on nothing else

`nodes::Share::severity` and `PodRow::severity` had classified every percentage
and every pod status since they were written, and the CLI table then printed the
answer in plain text. A node at 97% looked exactly like a node at 4%.

The obvious implementation was to reuse `Theme::severity`, which the dashboard
already draws bars with, and paint each graded cell in it. That is wrong, and the
reason is what most of this change turns on: `Theme::severity` writes
`Severity::Ok` in green. A dashboard draws a severity as a *shape* — a bar filled
green along its length is a quantity, and the green is the fill. A table draws it
as ink on a line somebody is scanning, and on a healthy cluster nearly every cell
is `Ok`. Painting all of them green would put the strongest signal a terminal has
on the rows with nothing to say, and leave the one broken node competing with two
hundred green neighbours for the eye.

So there is a second mapping, `Theme::severity_ink`, and `Ok` maps to `None`:
the absence of an escape sequence, so the cell prints in whatever colour the
user's terminal was already using — which is what the whole table printed in
before this existed. `Warn` and `Critical` take the theme's warning and danger
colours. `Unknown` is muted rather than alarming, because it is an absence: a `-`
where a figure could not be read, and greying it out says so without shouting.
The consequence, and the point: on a healthy cluster `eks nodes` emits no escape
sequences at all, and every byte of colour on screen is a row somebody should
look at.

What that mapping is *not* is a second opinion about what counts as hot. The
thresholds stay `Severity::from_utilisation`'s, one rule for both surfaces; a
test asserts the two mappings agree variant for variant, differing only in that a
table leaves `Ok` alone. `Column::severity` on each table likewise only says
which cells carry a reading — it re-reads `row.severity` and `Share::severity`
and invents nothing.

**The severity travels in the cell.** `format::Cell` is a `String` and an
`Option<Severity>`, rather than `table` taking a parallel grid of colours. Both
listings narrow themselves to a terminal by `retain`ing over their columns, and a
parallel grid would be one `retain` away from colouring the wrong column with
nothing on screen to say so.

**Ink never moves a column.** Every width comes from the cell's text and the
escapes are wrapped around it after the padding is decided, so a coloured table
and a plain one have their columns in the same places, character for character.
Both listings assert it directly: strip the escapes back off and the plain table
is underneath, at every narrow width as well as the default. It is also why an
empty graded cell is left alone — a zero-width cell in escapes is a sequence
`table`'s trailing-space trim cannot see, and it would leave a line ending in ink
with nothing inside it.

**Headers and footnotes stay plain.** A heading names a column; it is not a
reading off one. A footnote is prose under the table, and it is usually
explaining something the table has already coloured — saying it a second time in
red is shouting.

**The pod table colours `STATUS` and nothing else, on purpose.** `READY` is not a
second column to colour: `0/1` is *why* a `Running` pod grades `Warn`, so
colouring it would paint one judgement across two columns. `CPU/REQ` and
`MEMORY/REQ` are a gap rather than a rule, and the gap is deliberate.
`Severity::from_utilisation`'s thresholds are about a node's allocatable, where
90% booked is nearly full; a pod at 90% of the CPU it asked for is a well-sized
pod, and one at 400% of a 10m request is burning 40m and is nobody's emergency.
Colouring those cells on the node's thresholds would tell the reader something
untrue, in red, on most of their rows. What "hot" means for a pod against its own
request is a decision, and it is on the roadmap rather than in this change.

**`--color`, and who wins.** `auto` is the default and is the rule the tool
already followed for narrowing: a terminal gets colour, a pipe or a file does
not, so `eks nodes | grep NotReady` is the bytes it always was. `auto` also
honours [`NO_COLOR`](https://no-color.org/) — set and *not empty*, because an
empty value is the spec's own way of saying "not set" and a shell that exports
`NO_COLOR=` into every process must not silently disable colour everywhere — and
`TERM=dumb`, the one value that promises no escape sequences are understood.
`--color always` overrides all three, which is what makes `eks nodes --color
always | less -R` work; `--color never` overrides them the other way. The flag is
global rather than per-listing because it describes the output stream and not one
table, and it is a `clap::ValueEnum` on the domain type for the reason `--sort`
is one (decision 28): `--color sometimes` is rejected with the three that exist
listed, before anything connects.

`eks contexts` renders through `format::table` (decision 45) and gains nothing
from any of this, because none of its cells carries a severity — a context is a
name, a region, and a namespace read out of a file, and there is nothing about it
to be alarmed by. `--color always` there is a flag with nothing to do, which is
honest. The one mark that does single a row out, the `*` gutter, is not a
severity either, and whether it deserves a colour is a separate question on the
roadmap.

**The escape sequences are written here rather than taken from `crossterm`.**
`theme::foreground` maps a `ratatui::style::Color` to an SGR sequence, every
variant spelled out with no catch-all arm — so a colour added to `ratatui` in a
future release stops the build rather than silently printing plain — and a test
asserts the exact bytes for each. A sequence with a typo in it is a column five
characters out of place on somebody else's terminal, and only an assertion on the
bytes catches that. The theme's own colours are 24-bit; a terminal that does not
understand `38;2;r;g;b` ignores the sequence, which leaves the same table in the
colour it had before, and `NO_COLOR` or `--color never` is there for anything
stranger.

**The dashboard is out of scope for `NO_COLOR`.** The TUI keeps its colours
whatever the environment says. `NO_COLOR` is about software that adds colour to
text it prints; a full-screen interface made of borders, panes, and a selected
row is not that, and honouring the variable there would mean a monochrome theme
rather than a switch — which is the light-theme task's problem, not this one's.

**`commands::nodes` grew a `Request` struct** to carry the palette, mirroring
`eks pods` (decision 29). It was going to have to: `list` was already at seven
parameters, four of them describing the same one request.

### 50. `--timeout` covers the credential helper, on a task that is left behind

`--timeout` bounded every request to the cluster and nothing before them, and
the gap was the loudest failure the tool had left: a laptop that has lost its
route to an SSO endpoint runs `aws eks get-token`, the helper sits there, and
`eks nodes --timeout 5s` waited for ever with no way out but Ctrl-C. Decision 44
named that as a limitation and this is the other half of it.

The reason it was a limitation and not an oversight is that `kube` runs the exec
plugin inside `Client::try_from`, with a blocking `std::process::Command::output`,
on whatever thread asked. Wrapping `k8s::connect` in `tokio::time::timeout`
compiles and does nothing: the timer and the future it is racing are on the same
thread, and the thread is in `waitpid`. So the build moves onto
`tokio::task::spawn_blocking`, and the timeout races the `JoinHandle` instead —
which is a future that a running timer can actually beat.

**The task is abandoned, not cancelled**, because a blocking task cannot be
cancelled: dropping its handle stops anyone waiting on it and stops nothing
else, and the subprocess belongs to `kube` rather than to us, so there is no
child to kill from here. That makes the shutdown the second half of the fix.
Dropping a Tokio runtime waits for its blocking tasks, so `commands::block_on`
now calls `Runtime::shutdown_background` and returns: the thread finishes on its
own if the helper ever exits, and returning from `main` ends the process either
way. Without that line the timeout fires, the message prints, and the tool hangs
at the door — which is the same hang, one frame later. A test in
`commands::block_on` asserts it with a thirty-second blocking sleep and no
kubeconfig at all.

**Per step, like the requests.** The helper gets the same `--timeout` value each
page of the listing after it gets, rather than a share of one command-wide
budget — the reasoning of decision 44, applied one step earlier. A helper that
spends twenty seconds refreshing an SSO token has not used up the listing's time,
any more than one page uses up the next page's.

**It is its own message.** `Failure::Slow` names the budget and then talks about
VPCs, private endpoints, and VPNs, and none of that is true of a subprocess on
the user's own laptop. `k8s::client::stalled_helper` sits beside `explain`
rather than inside it, because there is no `kube::Error` behind this failure to
classify — nothing has been asked of the cluster yet. It names the command the
context runs and says to run it by hand, which is the only thing the user can
usefully do: that is how they find out it is sitting on a browser prompt.
`helper_command` builds that line out of the `AuthInfo` the kubeconfig produced
and quotes what a shell would need quoted, since an EKS `exec` block routinely
carries a profile or a role ARN with a space in it, and a command line the user
has to repair before it runs is worse than none. The block's `env` comes out in
front of the command as `NAME=value`, for the same reason and a sharper one: an
entry that sets `AWS_PROFILE`, pasted without it, runs against whatever profile
the shell already had and may answer instantly — which sends the user to look
for a problem somewhere that does not have one. What is *not* in the sentence is
a guess at which of the several things that hang `aws eks get-token` is hanging
this one — a blackholed metadata address, an SSO endpoint with no route to it, a
`credential_process` of the user's own that prompts — because naming the wrong
one confidently costs more than naming none.

The message also offers `--timeout 0`, which no other message does. It is the
one failure where "wait for as long as it takes" is a reasonable answer rather
than a way to reinstate a hang: an interactive helper waiting on a human is
doing its job, and the tool now has a default that would cut it off after thirty
seconds. That is the one behaviour change a user could dislike, and it is the
reason the escape hatch is named in the sentence rather than left in `--help`.

### 51. The dashboard fetches on a plain OS thread, not a shared `tokio` runtime

The node pane needed a way to fetch without the render loop ever awaiting a
network call — CLAUDE.md's rule that a key press is acknowledged within one
frame leaves no other option. `commands::spawn` answers it with
`std::thread::spawn` around a current-thread `tokio` runtime, built and shut
down exactly the way `commands::block_on` already does for a one-shot command,
delivering its result over a plain `std::sync::mpsc::Receiver` instead of
returning it.

The alternative was enabling `tokio`'s `sync` and `rt-multi-thread` features
now and spawning the fetch as a task on a shared runtime built once at
startup. That is the more conventional shape for async Rust, and it loses
here on cost for what it buys: it adds two features and a second
runtime-lifecycle model to a codebase that has deliberately kept exactly one
(decision 9), for a task that only ever has one fetch in flight. The worry
that motivated a second look — that "Background refresh" would need real
cancellation to discard a stale, still-running fetch when the user asks for
another — turns out not to bite. `page::Budget` already bounds `gather`'s
worst-case wall-clock time, covering `k8s::connect` and every paged request
behind it (decisions 44 and 50), and discarding a stale *result* is free
either way: replacing a `Receiver` with a new one for the next fetch drops the
old one, and the abandoned thread's `tx.send` on a disconnected channel simply
fails silently, exactly as `spawn`'s own doc comment says it will.

`spawn` is deliberately not `block_on` with a different return type. They
answer different questions — return a value, or promise one later — and nudging
one to grow into the other for the sake of a shared implementation would blur
which contract a caller is relying on. What *is* shared is the one invariant
that actually matters: `shutdown_background` rather than a dropped or joined
runtime, so a credential helper `kube` left running is abandoned once, not
waited for a second time on whichever thread happens to be shutting it down.

### 52. A dashboard bar divides by allocatable, matching the CLI's percentage

`nodes::Share::ratio()` — what `CPU USE`/`MEM USE` already divide by — is the
denominator the node pane's utilisation bars use too, rather than capacity.
Decision 22 left this open on the CLI side, naming capacity as the more
literal reading of "is this machine busy" and putting it on the roadmap for
whichever surface asked first: that surface is the dashboard bar, now, and it
answers the other way.

The reason is the same one decision 40 gives for the pod table's usage cell:
the two surfaces sit side by side and a reader moving between them must not
find the same node reading two different numbers. A bar and a CLI column that
divided by different things could both be defensible in isolation and still
teach the user something false the first time they compare a hot node in the
dashboard against the table they just ran — worse than either reading alone,
because it looks like a bug rather than a choice. Allocatable also costs
nothing new: `Share` already carries it, so the bar is `Share::ratio()` and
`Share::severity()` reused whole, not a second computation over
`Capacity::allocatable_ratio()`, which stays unused and available for the
"usage against capacity for the dashboard's bars" roadmap entry if a future
change decides a bar answering "how busy is the box" is worth a second
reading beside this one.

Superseded by decision 53: that roadmap entry came due and was taken.

### 53. The dashboard bar now divides by capacity; the CLI table still divides by allocatable

Decision 52 picked allocatable for the bar too, on the strength of "a bar and
a table that divide by different things teach the user something false the
first time they compare them." The roadmap task that decision left open —
"usage against capacity for the dashboard's bars" — asks for exactly that
divergence anyway, on the grounds decision 22 gave first: a bar answers "is
this machine busy", a capacity question, and a table cell answers "will
another pod fit", an allocatable one. Two different questions answered
honestly is not the same failure as two answers to one question that
disagree.

`Share::ratio()`/`severity()` are unchanged — they still divide by
`allocatable`, so `CPU USE`/`MEM USE` and the request columns read exactly as
before. The choice moved to the call site rather than staying baked into
`Share`: `ratio_of`/`severity_of` take an explicit denominator, and
`ui::nodes::bar` is the one caller so far that passes `Capacity::capacity`
rather than `Share::allocatable`. A future column or pane that wants the
capacity reading gets it without a second field on `Share` or a second type.

Worth flagging for review rather than assuming settled: decision 52's warning
about the two surfaces disagreeing still holds in spirit — a node pinned at
100% of allocatable now draws a bar that is not full, and a reader comparing
the two side by side has to know why. It is outweighed here by the roadmap
task's explicit ask, but if a reviewer would rather the two readings match,
the fix is `bar`'s call site, not `Share`.

### 54. The node pane's usage note is worded bare, not through the CLI's wrapper

The CLI table's third usage outcome — a metrics read that answered with
nothing sampled yet — earns `k8s::nodes::usage_unsampled`, which names `CPU
USE` and `MEM USE` because those are the columns going missing. The node
pane's bars have no such headings; they are just labelled `CPU` and `MEM`
already, in the row itself. So `k8s::nodes::usage_note`, the pane's reading of
the same three-way `metrics::Outcome`, calls `metrics::unsampled` and
`metrics::freshness_note` directly rather than through the CLI's wrapper,
which is what the roadmap task asked for by naming those two functions
specifically rather than their CLI-side callers.

The fourth outcome the CLI table has — a read that failed outright, footnoted
via `usage_unavailable` — is deliberately not carried over. The task was
scoped to the freshness and unsampled notes, and the pane already says
"nothing here" for a failed read the only way it currently can: every bar
reads `-`. Wiring the explanation through would mean giving the pane
something like the CLI's footnote list, which is the shape question the
"nothing ranked" and sort-note follow-ups are already waiting on the reviewer
to settle — bundling it into this change would be answering that question by
accident rather than on purpose.

The note lives on `NodesState::Loaded` as a second field beside the rows,
built once by `commands::nodes::spawn_gather` from the same `Gathered` the CLI
table's footnotes come from — `k8s_nodes::usage_note(&rows, &usage, &samples,
now, &label)` — so the pane and the table read one classification, never two.
The transfer type, `commands::nodes::NodesFetch`, is a struct rather than a
`(Vec<NodeRow>, Option<String>)` tuple over the channel, so the note is a
named field at both ends instead of a position to keep straight.

`ui::nodes::draw` splits the note on `\n` into one `Line` per sentence before
handing it to the pane: `ratatui` does not treat an embedded newline as a
break the way a terminal printing the same string does, and the stale
reading's second sentence — telling the reader to check metrics-server's pod
— would otherwise run together with the first. The note sits between the
`NODES` heading and the rows, in the header a pane has and a footnote list
does not, and it is silent on an empty node list: there is no usage to date
when nothing is running, whatever the read answered.

### 55. Background refresh: an immediate refetch on selection, a quiet one on
the interval and on `r`

The node pane's fetch used to be one-shot: `main` started it before the
terminal took over and nothing ever asked again. The roadmap task left two
triggers besides the interval for this change to design — `r`, and the
sidebar selecting a different cluster — and both needed an answer before
either could be built.

**Selection change refetches immediately and resets to `Loading`.** The
alternative — waiting for the interval, or leaving the previous cluster's
rows on screen until it elapses — reads as a bug: a user who switches
clusters in the sidebar is looking at the new one's name over the old one's
node list, which is worse than a blank pane with "Loading nodes…" in it.
`App::start_loading_nodes` is the pure half of that (a fourth transition
tested the way `apply_nodes` and `on_key` are); the event loop pairs it with
an immediate call through `spawn_nodes`, the same closure `r` and the
interval use.

**`r` and the interval do *not* reset to `Loading`.** These refresh a cluster
the pane is already showing, and blanking a working table to redraw the same
rows a second later is the flicker background refresh exists to avoid — the
whole point is that the pane goes on being readable while a request is in
flight. The old rows stay up until the new fetch answers.

That choice has a consequence `apply_nodes` had to absorb: a failed
background poll can no longer be presented as "no data at all," because there
usually *is* data — the last good listing. `apply_nodes` therefore only moves
to `NodesState::Error` on a failure with nothing loaded yet (the original,
one-shot case); a failure after a successful load keeps the rows and adds
`refresh_error`, a message the pane shows as a line under `NODES` without
touching the table beneath it. A transient blip reads as a transient blip
instead of as the cluster losing every node.

**The interval is its own type, not a reused `Budget`.** `--timeout` and the
new `--refresh` parse and print the same grammar — `Budget`'s — so
`RefreshInterval` is a one-line wrapper delegating both directions rather
than a second grammar. It stays a distinct type anyway: the two flags mean
opposite things by `0` on the same underlying number (a timeout of zero would
never finish; a refresh interval of zero is simply "don't," and `r` still
works), and a field typed `Budget` at the call site would read as a request
timeout to a reviewer skimming `ui::mod.rs`. It is global on the CLI, like
`--color` and `--timeout`, because the flag has to reach the bare `eks`
invocation, the common case, and `Command::Dashboard` has no arguments of its
own for a subcommand nobody types to carry.

**Fetching moved from a receiver to a closure.** `ui::run` used to take the
one `mpsc::Receiver` `main` had already started; it now takes that same
initial receiver *and* `spawn_nodes: impl Fn(&str) -> mpsc::Receiver<...>`,
built once in `main` over the config, kubeconfig paths, and budget the CLI
commands use, so every fetch after the first — `r`, the interval, a new
selection — goes through `commands::nodes::spawn_gather` the same way the
startup one did. The event loop stays generic over the closure rather than
depending on `commands::nodes` directly, which is what keeps `ui::event_loop`
testable in principle without a `KubeConfig` in scope, even though the loop
itself is the I/O layer and untested today for the same reason it always was
— the render loop's job here is only to decide *when* to call the closure.
A caller who replaces `nodes_rx` while an older fetch is still running simply
stops listening for it, per `commands::spawn`'s existing contract; the
abandoned thread finishes on its own and nothing needed a cancellation
mechanism.

Amended by decision 56: the closure stopped being generic once a second pane
needed its own, for the reason given there.

### 56. Pod browsing needed a focus model first, and the fetch closures became
boxed rather than generic

"Pod browsing"'s first slice — `Enter` on a node opens its pods — turned out
to have a prerequisite the roadmap task had not named: the node list had no
selection of its own to drill *from*. Only the sidebar could be navigated;
the detail pane was a fixed list nothing pointed a highlight at. Two
interaction questions sat behind that gap — how keyboard focus should move
between the sidebar and the detail pane, and whether `Esc` should keep
meaning "quit" once there was somewhere to back out to first — and both were
put to the user rather than guessed at, the same way a reviewer would have
been asked on a task this shaped. The answers: `Tab` toggles focus, and `Esc`
backs out one level before it quits.

`Focus` is a two-variant enum, `Sidebar`/`Detail`, toggled by `Tab` and read
by `App::on_key` to decide which of two sets of movement methods `j`/`k`/
`Home`/`End` call, and by `draw_cluster_list`/`draw_detail` to decide which
border gets `Theme::pane_border`'s focus colour — the mechanism the sidebar
already used alone, now shared rather than duplicated for a second pane.
`detail_selected` is one `usize` on `App` rather than a field per view,
because exactly one list is ever on screen in the detail pane at a time; it
is reset to `0` on every view change and every fresh load specifically so it
cannot point past the end of a list that just arrived shorter than the one
before it.

`View` is `Overview | NodePods { node: String }` rather than a stack, on the
same reasoning `RefreshInterval` got its own type over reusing `Budget`: a
`Vec<View>` would be guessing at a shape one more case cannot justify, and
today there is exactly one level of drill-down. The roadmap's own next slice
— a pod's containers — is what a stack is *for*, and is left to add one
rather than have this change build it against a single example.

`Esc`'s rule is `back_or_quit`: back out of `View::NodePods` when there is
one, quit otherwise. `q` and `Ctrl-C` are unconditional either way, so a
user who wants out does not have to remember how deep they are.

Row highlighting reuses the pattern `Theme::selected` already had for the
sidebar's `List` widget, adapted for the node and pod panes' hand-built
`Line`s: `Line::style(theme.selected())` sets the line's own style, which
`ratatui` patches *underneath* each span's — so a row's severity colouring
(a `CrashLoopBackOff` in red, say) survives being highlighted, because the
span's foreground is more specific than the line's and wins, while the
line's background shows through wherever a span left one unset. The
highlight itself only appears while `Focus::Detail` holds focus, unlike the
sidebar's own selection, which stays visible under either focus — the
sidebar's highlight answers "what is this dashboard showing", a question
that does not stop being true when `Tab` moves the keys elsewhere, while the
detail pane's answers "what would `Enter` open right now", which is not true
the moment focus leaves it.

The pod-browsing pane fetches once per node rather than joining the node
pane's background refresh: `commands::pods::spawn_gather_for_node` filters
`Scope::All` on `spec.nodeName`, across every namespace, and carries no
usage figures — metrics wiring for a third pane, and whether it should
refresh on the same interval the node pane does, are both weighed better
once there is a pane to review them against than guessed at alongside the
navigation this change exists to add. `PodsState`'s `apply_pods` therefore
always overwrites on failure, unlike `NodesState::apply_nodes`: there is no
earlier good listing for *this* node to protect, only for whichever one was
open before it.

`ui::run` and `ui::event_loop` took a `spawn_pods: impl Fn(&str, &str) ->
...` alongside `spawn_nodes` for exactly as long as it took to notice the
shape: two closures now, generic on both, and a third pane would make three.
`NodesFetcher` and `PodsFetcher` are boxed trait objects instead, so `run`'s
signature does not grow a type parameter every time a pane gains its own
fetch trigger — a distinction (which closure a function happens to be)
nothing outside `main` needs at the type level. The dynamic dispatch this
costs is one call per keypress or refresh tick, not per frame.

### 57. `-l`/`--field-selector` became global flags, and the dashboard combines
them with its own scoping rather than replacing it

"Carry the pod selectors into the dashboard" asked for one thing the
pod-drilldown pane did not have: `Selectors` reused rather than the pane
growing its own filter. The pane had no flags of its own to grow one from —
`Command::Dashboard` carries no fields — and `Command::Pods` had the only
`-l`/`--field-selector` definitions in the parser. Two ways to give the
dashboard the same flags: a second, dashboard-only pair, or moving the
existing pair up to `GlobalArgs`, beside `--namespace`, which already sits
there accepted by every command and acted on by fewer than all of them. The
second was the smaller change and the more honest one: `--namespace` had
already established that a global flag some commands ignore is this
project's answer to "a flag that means the same thing everywhere it applies,"
rather than a design question this task needed to reopen. `eks pods -l
app=api` still parses exactly as before — clap resolves a global arg from a
subcommand the same way — and `eks nodes -l app=api` now parses too, doing
nothing, which is what `eks nodes -n payments` already did.

Validating them is `main::run`'s job, once, before either `dashboard` or
`pods::list` is reached: `commands::pods::selectors_for` is the same function
both call, so a malformed `-l` is rejected in the same words whichever
surface it was typed for, and the dashboard's terminal never initialises on
one that cannot parse — first paint stays free of a request `ratatui::init`
would otherwise have to unwind out of.

The pane's own `spec.nodeName` filter could not simply be replaced by the
user's field selector, because it is not optional: it is the reason this is
*this node's* pane and not the whole cluster's. `commands::pods::
scoped_to_node` is the pure function that composes the two — a comma joins
two field requirements as an `AND` the same way it already joins two label
ones, so `--field-selector status.phase!=Running` narrows what a node's pane
shows rather than being silently overridden by the node scoping, or silently
dropped in favour of it. Pulling the combination out of the async
`gather_for_node` and into its own function is what let the rule be a
fixture — three cases, no cluster — instead of something only a live fetch
could exercise.

An empty pod list is ambiguous without knowing why it is empty, which the CLI
table already had an answer for: `k8s::pods::row::selector_note`, private
until now, phrases whichever of a label and a field selector are active. Made
`pub` and re-exported, it is what lets the pane say "No pods here match label
selector `app=api`." instead of "This node has no pods." — sharing the
phrase itself, not just the shape of the fix, with the CLI's `empty()`. The
note travels from the *user's* `Selectors`, computed once in `gather_for_node`
before `scoped_to_node` folds in the node filter, because that filter is
implicit in "this is the node's pane" and would be a strange thing to explain
back to someone as a reason their list came up empty. `PodsFetch` and
`PodsState::Loaded` both carry it as an `Option<String>`, the same shape
`NodesFetch::usage_note` already established for a fact a fetch computes once
and a pane reads without recomputing.

Left for later, deliberately: editing the selector without restarting `eks`.
The dashboard has no text-input mechanism yet — fuzzy search (`/`) is the
first roadmap task that will need one — and guessing at its shape to serve
this task alone would very likely guess wrong. Tracked in the roadmap as its
own entry, to be built once there is an input mechanism to hang it on.

### 58. Two roadmap entries merged: a pane cannot say which order it is in
before it can be put in one

The roadmap carried "Carry `--sort` into the dashboard" and, listed as
higher priority, "Carry the sort note into the dashboard's panes" as
separate entries. Reading them together made the ordering an accident
rather than a priority call: the note entry's own acceptance criterion —
"the default order is as silent in a pane as it is on the command line" —
presupposes a pane that has *an* order to be silent about, which is exactly
what the other entry builds. Taking the note task first would have meant
wiring `k8s::order::note` to a call site that could only ever hand it
`Order::default()`, passing its own test by construction and proving
nothing. This is decision 55's shape again — background refresh and the
freshness note it dates were two entries with the same dependency — so the
same answer applies: build the mechanism, and the note that explains it
lands in the same PR rather than describing a flag that does not exist yet.

Sorting is client-side and costs no request, same as `--sort` on the CLI
costs no second listing: `App` gained `node_order`/`node_direction` and
`pod_order`/`pod_direction`, and `k8s_nodes::sort`/`k8s_pods::sort` run
over whatever rows are already in `NodesState::Loaded`/`PodsState::Loaded`
— on a fresh fetch (`apply_nodes`/`apply_pods`), and again whenever the
ordering itself changes. `commands::nodes::spawn_gather` and
`commands::pods::spawn_gather_for_node` are untouched: the fetch closures
main builds know nothing about ordering, exactly as the acceptance
criterion asked, and a fetch already in flight when the user reorders is
sorted by whatever is active the moment it lands rather than the moment it
was requested.

Two independent orderings, not one shared field, because the node pane and
the pod-drilldown pane hold different rows and are never both on screen at
once — `View::Overview` shows one, `View::NodePods` the other. `s` and `S`
dispatch on `App::view()` rather than on which pane holds keyboard focus,
matching `r`: refreshing and reordering are both about *what the detail
pane is showing*, not about where `j`/`k` currently move a highlight.

`s` cycles through `O::value_variants()` — the same list `--sort`'s
`--help` prints, read as a ring instead of parsed from text — one key press
at a time, wrapping back to the default; `S` flips `Direction`, mirroring
`--sort-reverse`. A single key for "reverse" rather than a second flag: a
pane has no argv to add one to, and `Shift+<letter>` beside a plain
`<letter>` is the closest a keybinding gets to the same relationship
`--sort-reverse` has to `--sort`. The footer hint reads `s/S  sort` rather
than spelling both out — `sort` and `shift+s reverse` together overflowed
an 80-column footer, clipping `quit` off the end of a 90-column test
terminal, and the shorter form follows `j/k`'s own precedent for "two keys,
one hint" already sitting beside it.

The highlighted row in the detail pane is deliberately left where it is
across a reorder, index and all, rather than reset to the top. That reads
backwards next to `leave_node_pods` and `drill_into_pods`, which do reset
it — but those change *which* rows are on screen; a reorder changes only
what order the same rows print in, the same situation `apply_nodes` is
already in on every background refresh, and that path was decided (see
decision 55) to hold the index steady rather than jump the selection around
under the user. Resetting only on a reorder would make the pane's own two
list-changing operations disagree about which counts as "the same list."

### 59. Huge-page columns are conditioned on being nonzero, not merely reported

`ephemeral-storage` and `hugepages-*` sit in a node's `capacity`/`allocatable`
maps beside `cpu` and `memory`, but they are not shaped alike. Every real
node reports `ephemeral-storage`, so its column follows the same `any`-row
condition [`shows_usage`] already uses — the ordinary case gains it, and a
node still registering does not cost everyone else the column. `hugepages-*`
is different: the kernel reports an entry for every size it was built with,
almost always at `0`, whether or not an administrator ever reserved a pool.
Conditioning the column on *presence*, the way a device column is, would put
`HUGEPAGES-2MI` on every EKS listing, full of zeroes — exactly the noise
`resource::is_extended` already excludes `hugepages-*` from the device
treatment to avoid. So `hugepage_names` conditions on the pair being
*nonzero* in at least one row instead: the same `any`-not-`all` shape, one
level further in.

Both columns are shaped like `Capacity` — an `allocatable/capacity` pair
formatted with `quantity::memory` — rather than like `Device`'s
`booked/offered` count. Neither resource has a request tracked against it
yet, so there is no numerator to pair a capacity against; a `REQ` column for
either is the same undecided question the roadmap already leaves open for a
device's own request ("What a pod asked for, when nothing has measured it"),
not one this change answers by inventing a shape for it.

Placement: both sit after `PODS` and before `AGE`, ephemeral storage first as
the one every node has, then the device columns, then whichever huge-page
sizes qualified — grouped with `PODS` and the devices as "what this machine
can give out" rather than spliced between `MEMORY` and its `REQ` column, so
the existing pairing of a capacity with the share beside it is undisturbed.
In `DROP_ORDER` they are the very first columns to go on a narrow terminal —
ahead of even `VERSION` — because they are the newest facts on the row and
were not visible at all before tonight; a reviewer resizing a terminal on an
existing listing should see no difference until it is genuinely tight.

### 60. `View` grew a third variant instead of becoming a stack

The pod-browsing task that added `View::NodePods` left its own doc comment a
prediction: a pod's containers, the next drill-down level, was "the natural
place this grows into a `Vec<View>`". Building that here would have been
guessing at a shape for a depth this tool still does not have — the roadmap's
next drill-down candidate, a container's logs or its own detail view, is not
scheduled, and nothing today asks how a third level backs out or what happens
if a fetch fails partway down a stack two levels deep. A fixed three-variant
enum answers the actual question — `Overview`, `NodePods { node }`,
`PodContainers { node, namespace, pod }` — and keeps `back_or_quit` and
`draw_detail` each one exhaustive `match`, so the compiler catches a missing
arm the way an unbounded stack never would. A stack earns its keep once a
third level is a real task rather than a hypothetical one; two known levels
were not that point.

The consequence worth a reviewer's attention: `App::leave_node_pods` split
into two distinct operations that a `Vec<View>` would have collapsed into
one. `App::leave_detail_view` (its new name) resets the detail pane all the
way to `Overview` and discards both the pods and the containers panes in one
call — the shape a cluster switch needs, since a stale drill-down at either
depth is equally wrong under a newly selected cluster. `Esc`'s one-level-at-
a-time backing out is a separate path in `back_or_quit`, and deliberately so:
going from `PodContainers` back to `NodePods` does not refetch the pod
listing, because nothing about it changed — it is exactly the pods pane that
was already on screen a moment ago. Collapsing the two into a single "pop the
stack" operation would have made that distinction one `if` statement's worth
of ceremony instead of two clearly-named methods, at the cost of hiding the
fact that they answer different questions: "the user asked to back up" and
"the ground moved out from under them" are not the same event, and do not
deserve the same amount of state thrown away.

`ContainerRow` lives beside `PodRow` in `k8s::pods`, not inside `row.rs`
itself: `row.rs`'s whole point is deriving one `STATUS` for a pod by picking
which of several unhappy containers gets to speak for it, and a per-container
state is the opposite operation — every container reports for itself, in
spec order, nothing chosen on its behalf. The two modules share one thing,
`exit_reason`, which moved from private to `pub(super)` so a `Terminated`
container and an app-container's contribution to a pod's own `STATUS` are
guaranteed to use the same word for the same termination rather than two
independent spellings drifting apart over time.

The fetch is a plain `kube::Api::get` on the one pod's name, not a second
listing: the node-pods pane already read every field a container row needs
out of the `Pod`s it fetched to build `PodRow`s, but keeps none of that
around once reduced — a `PodRow` sized for a listing of many pods has no
room for one pod's full container list, and carrying every pod's raw
containers through a pane that almost never needs them would cost more than
asking again for the one the reader actually drilled into. It goes through
the same `Budget::wrap`/`k8s::explain` path every other fetch in the tool
does, so a pod that was deleted between the listing and the drill-down reads
as an ordinary API failure rather than a special case invented for this one
pane.

### 61. A container's requests and limits are its own spec, not `effective_requests`

The pod-detail task asked for "resource requests/limits" per container, and
`k8s::pods::effective_requests` already turns a pod's containers into
numbers — but it answers a scheduling question (what does this *pod* have
booked, sidecars and the init peak and pod overhead folded in) that nobody is
asking of one row in a container list. `ContainerRow` reads each container's
own `resources.requests`/`limits` directly instead, reusing `Requests::read`
for the request half so a container that declared none reads the same real
zero every other request figure in this tool gives an absent entry.

Limits do not get the same treatment, on purpose. `Requests::read` defaults
an absent entry to zero because that is the right reading for a request — the
scheduler reserves nothing for one nobody made — but a limit nobody set means
nothing bounds the container, which is a different fact and one Kubernetes
does not even let a manifest spell as "limit: 0". `cpu_limit` and
`memory_limit` are `Option<Quantity>`, and the sentence built from them says
`unlimited` for `None` rather than `0`, so the two absences read as the two
different things they are. `resources_summary` builds both sentences as
plain text in `k8s::pods::containers` — computation, not rendering — and the
pod-containers pane prints them as a second, dimmed line under each
container's identity, wrapped by the pane's existing `Paragraph` rather than
a new widget.

"Recent events", the other half of the task's original wording, is not here:
nothing in this tool reads the `Event` API yet, and it wants its own fetch, a
dedup/count rule, and a decision about what "no events" should say next to a
pod too young for the API server to have kept any — a night of its own
rather than a fact to bolt onto a container's row. See `docs/ROADMAP.md`'s
follow-up entry.

### 62. Arrow keys joined `Tab`/`Esc`, and quitting at the top level needs two presses

Live use against real clusters turned up two problems with the scheme
decision 56 settled on: `Left`/`Right` did nothing at all, which reads as
broken once `j`/`k` have already trained a user that arrow keys work here
too, and a single `Esc` at `View::Overview` quit immediately, which is too
easy to trigger by accident. Both were reported directly rather than found
as a roadmap task, and the fix touches the same keys decision 56 chose, so
it revises that decision's rule rather than adding beside it.

`Right`/`Tab` are now two names for one motion (`App::advance`): switch
focus from `Sidebar` to `Detail` if the sidebar has it, or drill in
(`drill_in`) if the detail pane already does. `Left`/`Esc` are two names
for the reverse (`App::retreat`), but not the exact mirror — drilling out
of the current view wins over moving focus back to the sidebar, regardless
of which pane is focused, so backing out of a two-level drill-down is still
one press per level exactly as before. Only once the view is already back
at `Overview` does a `Left`/`Esc` press move focus to the sidebar first,
and only once focus is *also* already on the sidebar does it reach the quit
step. Making focus-switching win first, the more literal reading of
"`Left`/`Esc` do pane-switching too", was tried and rejected: `Focus`
usually still points at `Detail` right after drilling in, since drilling
never changes it, so that ordering would have cost a spare `Esc` on every
"back out immediately" press — the exact interaction decision 56 was
written to preserve — in exchange for a pane-switch nobody was reaching for
in the same breath as backing out.

The quit step itself changed too: decision 56's "`q` and `Ctrl-C` are
unconditional either way" no longer holds for `q`. `Esc` and `q` at the top
level now arm a pending quit (`App::quit_or_arm`, backed by one
`Option<Instant>` field, `quit_armed_at`) rather than quitting outright, and
only a second `Esc`/`q` within `QUIT_CONFIRM_WINDOW` (600ms) confirms it —
either key confirms the other's arm, so a user reaching for whichever one
comes to mind first does not have to remember which one they already
pressed. `q` stays gated on `View::Overview` alone, unlike `Esc`, since `q`
never had focus-aware behaviour to preserve; it is a no-op while drilled
into anything, rather than the unconditional quit it used to be. Any key
that is not `q`/`Esc`/`Left` clears a pending arm, so a navigation press in
between two quit-family presses cancels it instead of letting a much later,
unrelated `Esc` confirm a stale one. `Ctrl-C` is untouched and still
unconditional — the one part of decision 56's rule this change keeps
exactly, since an immediate, no-questions-asked way out was the point of
having it at all.

The footer (`draw_footer`, now taking `&App` instead of `Theme` so it can
read the new `App::quit_pending`) replaces its hint line with "press esc/q
again to quit" for as long as a quit stays armed, styled with
`theme.severity(Severity::Warn)` rather than a new `Theme` method. Shipping
the double-press rule without this would leave a user's first `Esc`/`q`
looking like it did nothing.

---

### 63. A pane's `Cause::Explained` is narrower than the CLI's, and honestly so

The roadmap task for carrying the "nothing ranked" note into the dashboard's
panes flagged one thing the CLI never had to decide: what `order::Cause::
Explained` means when the thing it points at — "the reason above" — is a
footnote list the pane does not have.

The CLI table has two footnotes that can explain an empty usage column (a
failed read, and one that answered with nothing) and one that explains a
failed pod-requests listing, and `k8s_nodes::cause`/`k8s_pods::cause` point
the relevant orderings at whichever applies. Both dashboard panes have far
less to point at. The node pane has exactly one note, `usage_note`, and it is
only ever printed for the *unsampled* case — its own doc comment already
left a failed read silent, out of an earlier task's scope, because there was
no footnote list yet to add `usage_unavailable`'s explanation to. The
pod-drilldown pane has no usage note of any kind, because it does not sample
usage for its rows at all yet.

Rather than widen either pane tonight to make more of `Cause::Explained`
true, this change makes it true only where a note the reader can already see
actually says why: `k8s_nodes::usage_missing_explained(rows, usage_note)` is
`true` only in the unsampled case, `Missing::requests` is unconditionally
`false` in the node pane (no note there explains a failed pod listing), and
the pod pane passes `Missing::default()` outright. A pane that has not
explained something must not claim it has — the module's own docs call this
out as the whole point of `Cause::Unexplained` being the default — so the
narrower reading was the only honest one available without inventing a new
footnote surface neither pane has earned yet. Both gaps are now their own
roadmap entries, so the two Missing.requests being uniformly `false` and the
pod pane's uniform `Missing::default()` are not left looking like a wrong
answer: they are today's honest one, waiting on surface that has not been
built.

`usage_missing_explained` reads `rows` and `usage_note` rather than
threading a new field through `NodesFetch`/`NodesState` end to end: `usage_
note`'s own three-way match already makes the answer recoverable — the
unsampled and unreadable cases both leave every row's `shows_usage` false,
and only the unsampled branch of `usage_note` is ever `Some` — so the
existing field carries enough information without a second one duplicating
it.

### 64. Log streaming gets real cancellation; decision 51's "discard is free" does not apply

Every other background fetch answers once, and stopping early is free: drop
the `Receiver`, and the abandoned thread's `tx.send` on a disconnected
channel fails silently (decision 51). A `follow`ed log is not that shape — it
is a live connection that keeps costing the API server a request for as long
as nobody tells it to stop, so leaving the pane has to actually end it, not
merely stop listening to it.

`commands::spawn_stream` is `spawn`'s counterpart for that: the task gets a
`tokio::sync::oneshot::Receiver<()>` alongside the sender, and
`commands::pods::stream_logs` races it against `lines.next()` inside a
`tokio::select!` loop. Dropping the `StreamHandle` the caller is handed back
fires the oneshot, the `select!` never polls the read again, and the
underlying `AsyncBufRead` — and with it the HTTP request — drops on the spot.
This works *because* a log stream is ordinary async I/O, unlike the one thing
in this tool that genuinely cannot be cancelled: the credential helper `kube`
runs as a blocking `std::process::Command` inside `Client::try_from`, which
is why `--timeout` covering it (decision 50) could only ever abandon it
rather than stop it. `stream_logs` has no such blocking step in its own read
loop, so the oneshot signal has something to interrupt.

`Inflight` in `ui::mod` is where the event loop keeps the handle alive —
`logs_handle` is never read, only held and eventually dropped or overwritten,
which is the cancellation. Every place a drill-down view changes away from
`ContainerLogs` clears it unconditionally rather than only when backing out,
because drilling *forward* past a level that never had a stream running
finds nothing there to clear either way, and writing the two cases
separately would be two chances for one of them to forget.

### 65. A paused log view is pinned by a hidden-line count, not a remembered index

The scrollback buffer is a bounded `VecDeque` (decision-adjacent to nothing
before this — the first bounded buffer in the tool), and the awkward
question is what "scrolled up two lines" should still mean once the buffer
keeps moving underneath it: new lines keep arriving at the bottom while
paused, and past `MAX_LINES` every arrival also evicts one from the front.
An absolute index into `lines` survives neither: a `VecDeque` index means
different content after either kind of change.

`Log::hidden_below` is a *count* — how many of the newest lines are below the
bottom of the view — rather than a position, and `Log::push` increments it by
one on every arrival while the view is paused, regardless of whether that
arrival also evicted the oldest line. That one rule is what makes both cases
come out right: while the buffer is still growing, `len` and `hidden_below`
grow together, so `len - hidden_below` — the window's bottom edge — stays put
and a paused reader keeps looking at exactly the lines they were looking at.
Once the buffer is at capacity, `len` stops growing but `hidden_below` still
climbs, so the bottom edge retreats by one exactly when eviction has shifted
every surviving line's true index back by one. Two situations that move the
underlying deque in opposite senses are handled by the same increment,
because the quantity being counted was never a position to begin with.

`Log::visible` still has to decide what "there is nothing older to reveal"
means, and that answer needs the pane's row count, which only the renderer
has — so `hidden_below` is never clamped at scroll time. `scroll_up` and
`jump_to_start` simply record how far the reader asked to go, including past
the oldest line that exists; `visible(rows)` is where the window's bottom
edge is floored at `len.min(rows)`, which is what stops a `PageUp` on a short
log from blanking the pane and is exactly the reading `jump_to_start` relies
on to show everything rather than one line clinging to the top.

### 66. `View` grew a fourth variant, still not a stack

Decision 60 picked a fixed enum over a `Vec<View>` because a third level was
hypothetical at the time — "a container's logs or its own detail view" was
named as the next candidate and explicitly not scheduled. Tonight scheduled
it. The reasoning that decided against a stack was about the *cost* of one
outrunning its benefit at a known, small depth, not about three being some
natural ceiling, and nothing about a fourth `View::ContainerLogs { node,
namespace, pod, container }` arm changes that trade: `back_out_one_level`,
`next_view`, and `draw_detail` are still one exhaustive `match` apiece, and
the compiler still catches a level added to one without the others. A stack
starts paying for itself once a *fifth* level is a real task rather than a
guess at one — nothing on the roadmap names one yet.

The one new wrinkle a stack would not have had either: `View::ContainerLogs`
is the first variant whose detail pane is not a list of selectable rows.
`App::detail_row_count` reads `0` for it and `j`/`k`/`Home`/`End` are
special-cased in `on_key` to scroll the log instead of moving a highlight
that does not exist there — `PageUp`/`PageDown` are new for the same reason,
since a highlight-based pane never needed a "move by more than one" key. The
footer hints branch on the same distinction, showing `f`/`w` in place of
`enter`/`s`/`S`, which have nothing to do in a view with no rows to open
further and no ordering to change.

### 67. The `/` filter narrows what a pane draws, not what its footnotes reason about

`fuzzy::rank` reduces a pane's rows to the ones the query matches, and the
question it raised was whether `order::unranked_note`, `cause`, and the node
pane's `usage_note` should read that same narrowed set or the full listing
each pane already had in `NodesState`/`PodsState`. They read the full
listing. `--sort cpu` on a node pane filtered down to two nodes still says
"Nothing here has cpu to sort by" on the strength of the other eight the
filter is hiding, which can look wrong at a glance — the note is answering
"could this ordering ever rank anything here", not "did it rank one of the
rows currently on screen", and those are different questions once a filter
exists to ask the second one.

The alternative — threading the filtered subset into `ranks_any`/`cause` too
— was rejected for the same reason `ranks_any`/`cause` take `&[NodeRow]`/
`&[PodRow]` today: changing that to accept whatever shape a filtered `Vec<&
NodeRow>` is would touch the CLI table's call sites for a question the CLI
table does not have, since `eks nodes`/`eks pods` have no live filter to
narrow by. Keeping the footnotes over the whole pane means one clear rule —
"this note is about the listing, the rows below it are about what you
typed" — rather than a note whose meaning silently changes depending on
whether a filter happens to be active. Worth a second look if it reads as
confusing in practice; the fix, if so, is `ranks_any`/`cause` taking an
iterator rather than a slice, which the CLI callers already satisfy for
free (`&[T]` is `IntoIterator<Item = &T>`) without changing their call
sites at all.

### 68. Clearing an applied filter is its own `Esc` press, ahead of backing out

Decision 62 made `Esc` at the top level a two-press confirm, and mid-drill an
`Esc` already backs out one level rather than jumping straight to `Overview`
— both readings of the same rule, that one press should undo the single
most recent thing rather than everything at once. A filter is one more thing
that can be "the most recent" state a press should undo first: `Esc` while
`Filter::Applied` clears the filter and leaves the view exactly where it
was, and only a second `Esc` — with no filter left to clear — backs out of
the drill-down. Making the two presses do both at once (clear and retreat
together) was the other option, and was rejected because it makes `Esc`'s
meaning depend on whether a filter happens to be set, which is exactly the
kind of surprise decision 62 was written to avoid: a user mid-search who
presses `Esc` expecting to leave the search box, and instead finds
themselves a level shallower in the dashboard too, has no way to tell the
two apart afterwards. `Filter::Editing` — text still being typed — is
different again: `Esc` there cancels outright rather than clearing-then-
backing-out, since there is nothing committed yet to "back out of" one step
at a time.

### 69. The previous-log toggle lives on `View`, and always flips

`p` switches the container-logs pane between a container's current log and
its previous instance's (`kubectl logs -p`). The obvious place to keep
"which one" is a field on `App`, the way `Filter` and the sort orders are —
but every one of those is read by a `draw` function and nothing else decides
whether to fetch on its account. A log's mode decides *what gets fetched*,
and `event_loop` already has exactly one trigger for that: "the view just
changed, so ask `start_drill_fetch` what it needs now." Putting `previous`
on `View::ContainerLogs` itself means flipping it is a view change like
drilling in or backing out, and reuses that wiring outright rather than
teaching `event_loop` a second reason to refetch.

The harder call was what happens when there is nothing to switch to. A
container that has never restarted has no previous instance, and opening a
connection that could only ever answer "not found" would read as a hung
fetch until it did — so `toggle_log_previous` checks the restart count
(read from `App::containers`, the listing this pane's own drill-down already
left in place, rather than a second copy carried on `View`) and refuses
before anything is sent. The first version of that refusal left `previous`
unchanged and only overwrote `self.logs` with a message — which meant a
second `p` re-ran the identical refusal forever, because nothing about the
state that decides "am I trying to switch" had moved. `p` now always flips
`previous`, refusal or not; a switch that lands on "previous" with nothing
there sets `LogsState::Unavailable` instead of fetching, and a second press
flips back to `false` and fetches the current log exactly as it would from
any other starting point. `Unavailable` is deliberately not `Error`: an
`Error` is a connection that was attempted and failed, styled to match, and
this is neither — the connection was never opened, and the message is
information, not a fault. Flipping `previous` unconditionally also means the
refusal is a real view change, so `start_drill_fetch`'s unconditional drop
of the previous fetch still runs and the stream that was showing the current
log is not left running unread behind the message.

`k8s::pods::logs::params` answers the one question the roadmap entry left
open, whether opening a previous log should force `follow: false`: yes,
unconditionally, because a terminated container's log has already stopped
growing and a `follow`ed read of one would sit open waiting for a line that
is never coming. The current log's own `follow: true` is untouched.

### 70. `--sort` advice is filtered by `distinguishes`, a second and stricter
predicate than `ranks_any`

`k8s::order::unranked_note`'s "sort by X instead" line used to suggest any
ordering `ranks_any` said yes to — has at least one row it can rank. `--sort
status` on a cluster where every node is `Ready` exposed the gap: every node
has a status, so `ranks_any` is trivially true for it, and offering it as the
fix for a failed ordering sends the reader to a table that looks exactly like
the one that just told them nothing worked. The roadmap entry behind this
posed two questions and left both to whoever picked the task up.

The first: whether "the rows differ under this ordering" is the right bar for
a *suggestion*, given "one row ranked" is deliberately the bar everywhere
else `k8s::order` asks it — whether the flag itself is honoured, whether the
diagnosis fires, which tail tier an unranked row lands in. It is, and only
there: `unranked_note` now takes a second closure, `distinguishes`, asked
only inside `alternatives`, and ANDed with the existing `ranks` rather than
replacing it — an ordering has to both have something to rank a row by *and*
actually put two rows in a different arrangement before it earns a place in
the advice. Every other use of `ranks_any` — deciding whether the user's own
chosen ordering counts as unranked, deciding a row's tail tier, deciding
`cause` — is untouched. The advice line is the one place in the tool that
promises the reader something will look different if they type this, so it
is the one place held to a promise the rest of `--sort` does not make.

The second: whether an ordering that puts every row in one group is really
saying nothing, given "everything here is `Ready`" is an answer of a kind.
It is not treated as one here. The advice list exists to name a flag worth
typing next, and an ordering that provably rearranges nothing is not that,
whatever true thing it could be read as saying about the cluster instead. A
note that answered "is everything healthy" would be a different, useful
feature — closer to a summary line than to sort advice — and it is not this
one; building it would have been guessing at a feature the task never asked
for under the cover of answering the one it did.

`distinguishes(rows, order)` is implemented once per listing, beside its
`ranks_any`, by comparing every row's `rank` against the first row's rather
than comparing every pair: `rank` is already a total order (it is what `sort`
itself uses before the alphabetical tie-break), so if every row compares
equal to the first they compare equal to each other, and one pass is enough.
`ranks_any` and `distinguishes` genuinely diverge, not only on the uniform-
status case the roadmap entry named: two nodes tied at the same share of
allocatable both rank under `cpu` and distinguish nothing between them,
proven directly in `k8s::nodes::order`'s tests.

The consequence worth flagging rather than discovering later: a listing of
exactly one row can never be distinguished by anything, so a single-node or
single-pod cluster now gets the bare diagnosis with no "sort by X instead" at
all, where before it got whichever alternatives `ranks_any` allowed regardless
of how many rows were on screen. Sorting one row was always a no-op, so the
new note is arguably the honest one — but it is a real behaviour change
beyond the literal example the roadmap entry gave, and several existing tests
in `k8s::nodes` that used single-row fixtures needed a second, contrasting row
added to keep testing what they were written to test rather than quietly
starting to test the single-row case instead.

Left open, and its own roadmap entry: `unranked_note`'s gate on the user's
*own* chosen ordering is still `ranks_any` alone, so `--sort status` typed
directly against a uniform cluster still prints `Sorted by status.` with
nothing said about the fact that it changed nothing. That is a different
question — whether a working-but-vacuous ordering deserves a note at all,
given the line is not lying — and answering it was not this decision's to
make.

### 71. A vacuous ordering the user actually typed gets its own diagnosis, in
`unranked_note` rather than a second function

Decision 70 left one gap on purpose: `unranked_note`'s gate on the order the
user typed was still `ranks_any` alone, so `--sort status` against a cluster
where every node is `Ready` printed only `Sorted by status.` — true, and
silent about the fact that the table looks exactly like it did before the
flag. This decision closes it.

The two questions worth separating were whether a working-but-vacuous
ordering deserves a note at all, given `note`'s line is not lying, and — if
so — what it should say that is not `unranked_note`'s existing wording
reused for a different reason. Both are answered yes and "something
different": `note` stays honest about *what was asked for*, and a second
line, alongside the one that already exists for "nothing to rank", answers
the question `note` cannot — *whether it mattered*. Silence there reads as
success, and for a vacuous ordering it is not one.

The mechanical choice was whether that second line is a new function or a
second branch on `unranked_note`. It is the latter. `unranked_note` already
takes `ranks` and `distinguishes` as closures — `distinguishes` was, before
this decision, read only inside `alternatives`, to build the advice — so the
guard `if order == O::default() || ranks(order) { return None; }` became `if
order == O::default() { return None; }` followed by a diagnosis chosen from
three cases: nothing to rank (unchanged, `Cause` still applies), ranked but
`!distinguishes(order)` (new; `Cause` does not apply — the column is not
missing, so nothing above the table could be pointing at it), and both
(silent, unchanged). A second function would have needed its own copy of
`alternatives`'s advice-building, for advice that is identical either way —
an ordering has to clear `ranks` and `distinguishes` to be offered whichever
diagnosis is asking. The four call sites (`commands::nodes`,
`commands::pods`, `ui::nodes`, `ui::pods`) needed no changes at all: they
already pass `distinguishes` as the fourth argument, so the new case reaches
every listing and every dashboard pane in one change rather than four.

The wording is deliberately not `unranked_note`'s existing "nothing here has
X to sort by" reused: that sentence is false when `ranks(order)` is true —
there *is* something to sort by, and every row has it. The new line —
`Every row here ranks the same under {name}, so sorting by it changed
nothing.` — names the actual fact: not an absent column, but one where
every value ties.

The consequence worth flagging, an extension of the one decision 70 already
named: a listing of exactly one row can never be *distinguished* by
anything, so a single-row cluster now gets the "changed nothing" diagnosis
for any non-default ordering it ranks under, where before it printed only
`Sorted by cpu.` and stopped. `k8s::nodes::mod` and `ui::nodes`/`ui::pods`
each had one existing test built on a single ranked row that asserted
silence beyond the "Sorted by" line; each needed a second, contrasting row
to keep testing what it was written to test — the same shape of fix decision
70 needed for `k8s::nodes`'s own single-row fixtures — plus a new test
against the single-row case to cover what actually changed. Sorting one row
was always a no-op, so the new line is the honest one, and it is now said in
the one place `--sort` speaks rather than left to the reader to notice.

### 72. `eks` does not own credential resolution; it owns knowing when it will fail

The open question in the roadmap's "Stop the credential helper" entry — whether
`eks` wants to own credential resolution at all — is answered *no*, and this
change is what makes the no affordable.

Owning it would mean implementing the `client.authentication.k8s.io` exec
protocol, or the IAM Identity Center OIDC device flow over `aws-sdk-ssooidc`,
and handing `kube` a resolved `Config`. Both were weighed and both lost to the
same two objections. The first is cost: `aws-config` and the smithy stack pull a
second hyper/rustls tree into a binary whose startup time is priority one in
`CLAUDE.md`, and the commands that touch nothing but the filesystem currently do
not even build a runtime. The second is worse: a native device flow has to write
the token cache in the format `aws eks get-token` reads back, and that format is
`botocore`'s private contract. We would be pinning ourselves to an
implementation detail of the tool we are trying to cooperate with, and a change
to it would show up as "logging in silently does nothing".

So `aws::login` shells out to `aws sso login --profile X`. The AWS CLI is
already a hard requirement of every EKS context this tool opens — the `exec`
block runs it — so this adds no dependency the user did not already have, and it
inherits the browser handling, the device-code flow, and the cache format for
free. Always the `--profile` spelling and never `--sso-session`: it works for
both spellings of an Identity Center profile and on older CLI v2 builds, and one
form is one thing to print in the message offering it.

What `eks` does own is the *question*. That half needs no SDK at all, because
the answer is on disk.

### 73. The session check reads two files, and matches on `startUrl` rather than a hash

`aws::config` reads `~/.aws/config` for four keys, and `aws::sso` reads the AWS
CLI's token cache for two. Both are pure functions over file contents with an
explicit `now`, which is what lets the check run *before* connecting instead of
in reaction to a `401` the user has already waited for. Measured against a
31-entry cache the whole pre-flight is under a tenth of a millisecond, so it
sits before the dashboard's first paint without troubling the 50 ms budget.

Two choices inside it are worth writing down.

**The config reader is hand-written, not a dependency.** AWS's format looks like
INI and is not: a value may be empty and continue as an indented block beneath
it (`s3 =` followed by `addressing_style = path`), which a general-purpose
reader either rejects or folds into the section as bare keys. We want four keys,
none of which is ever written that way, so the parser skips indented lines
rather than modelling sub-properties. Eighty lines, and it earns its place under
`CLAUDE.md`'s dependency rule where a crate would not. `serde_json` was promoted
from a dev-dependency for the cache, which is genuinely JSON and somebody
else's; hand-rolling a reader for that would have been the worse trade.

**Cache entries are matched on the `startUrl` inside them, not on the
filename.** The AWS CLI names each file after the SHA-1 of the session name or
start URL. That is a `botocore` implementation detail rather than a documented
contract, and a tool that recomputed the hash would break silently the day it
changed — while reading a hash of a value it is already holding. Matching the
field costs a scan of a few small files, cannot drift, and skips the
`botocore-client-id-*.json` registrations in the same directory for free, since
they carry no `startUrl`. Every failure in that scan is a skip: a directory that
does not exist means nobody has logged in yet, and a file we cannot parse
belongs to a newer CLI than the one we were written against. The worst case of
being wrong is offering a login that was not needed.

A token with under sixty seconds left counts as expired. It would be refused
partway through a paged listing otherwise, and a credential error about a
session that was alive when the user pressed Enter is the worst of both answers.
That folds two readings into one `Session::Expired`, so the wording function
says "signs out in 40s" as readily as "signed out 9h ago".

### 74. A browser never opens without a yes, and the policy is a pure function

`--login` is `auto`, `always`, or `never`, spelled to match `--color`'s three
rather than inventing a second vocabulary for the same shape of choice.

`aws::decide` is the entire policy as one pure function over the session, the
flag, and whether there is a human at the terminal — the last passed in rather
than asked for, so the table is a test. The row that matters is `auto` with no
terminal: it proceeds, and the user gets the message this tool has always
printed. A listing being redirected into a file has nobody to answer a question,
and a tool that opened a browser there — or worse, sat waiting on a keystroke
nobody would type — is one people work around rather than use. "Interactive"
means both stdin *and* stderr are terminals, since that is where the answer
comes from and where the question goes.

Everything user-facing goes to stderr, and when stdout is not a terminal the
child's stdout is redirected there too: `eks nodes | column -t` prints the same
bytes it printed before. `--login never` keeps that promise literally — it does
not read `~/.aws` at all, and the dashboard does not even build the runtime the
pre-flight would need.

The offer is made at most once per command. The retry after a cluster refuses is
real and worth having — a token revoked centrally still reads as live in the
cache until something tries to use it — but it is gated on `Outcome::NothingToDo`
rather than on "no login has run yet". Three outcomes rather than a `bool`,
because a user who has just answered "no" and a pre-flight that found nothing to
say mean opposite things to the retry. Asking the identical question twice in
one command is how a tool teaches people to reach for `--login never`.

`aws sso login` is deliberately outside `--timeout`. That budget is about a
cluster that will not answer; this is a human at a browser, and bounding it
would recreate the hang-versus-give-up problem decision 50 solved, on the one
path where waiting is the correct behaviour.

### 75. The dashboard asks before it opens, and offers `L` after

A one-shot command can ask a question wherever it likes. The dashboard cannot:
its fetches run on background threads that do not own the terminal, and a login
offered from one would be shouting over the pane it was trying to fill. So the
two halves are split by *who owns the screen*.

Before `ui::run` opens the alternate screen, `credentials::preflight` puts the
question with ordinary stdio. Every fetcher built after that point is pinned to
`LoginMode::Never` — not the user's flag — so a worker thread can never reach
the prompt at all. That is a construction-time guarantee rather than a rule
somebody has to remember.

A session that dies while the dashboard is open is `L`, and deliberately not an
automatic suspend: seizing the terminal from under somebody who is reading a
container's log, to open a browser they did not ask for, is worse than the
failure. `App::on_key` returns a new `Flow::Login` — its own variant for the
reason `Flow::Quit` is one, the state machine decides and the event loop acts —
and only when the failure on screen is credential-shaped. `credentials_lost` is
one flag on `App` rather than a field threaded through four pane states: the
session belongs to the cluster, not to whichever pane happened to be the one
that asked, so a refusal from any of them offers the key and a success anywhere
withdraws it.

Carrying that flag across the thread boundary is why `commands::FetchError`
replaced the bare `String` the fetchers used to hand back. The classification
exists while the typed `k8s::client::Error` is still in hand and is gone by the
time the receiving end has a message to print, so it is asked once, at the
boundary, rather than re-derived by matching on English prose later.
`k8s::client::Error::Cluster` grew a `failure` field for the same reason, and
`Error::explained` gives the listings that run *after* a client exists the same
pairing — an expired token is refused by the API server as readily as by the
credential helper, and which of the two noticed is not something the user should
have to care about.

`ui::run` owns the suspend, because it is the only function here that knows a
real terminal is involved. It hands `event_loop` a closure; `event_loop` stays
generic over the backend and a test passes one that does nothing, so every
keypress test still runs against `TestBackend`. The closure leaves raw mode and
the alternate screen and re-enters them around the login rather than calling
`ratatui::restore()`/`init()`, which would hand back a *new* `Terminal` the loop
would have to swap in mid-iteration.

Two smaller things fell out of touching that pane. The credential footer is its
own short hint list, beside the ones `/` and the log view already have, rather
than one more hint appended to the default: prepending `L log in` pushed `q
quit` off an 80-column terminal, which the default list is explicitly ordered to
protect, and when the session is gone `j/k` and `s/S` are moving around a
listing nothing can refill anyway. And `NodesState::Error` is now drawn as one
line per sentence: every message from `explain` diagnoses and then advises, and
`ratatui` draws an embedded newline as one unbroken line, which had been putting
the half that says what to do next off the right-hand edge of the pane.
