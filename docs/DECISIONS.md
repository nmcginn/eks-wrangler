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
is now 200–400 lines of production change with tests on top, and a slice has to
be complete before it is asked to be small. Deferral needs a seam to happen at:
an untouched surface, an open design question, or a night's work of its own.

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
