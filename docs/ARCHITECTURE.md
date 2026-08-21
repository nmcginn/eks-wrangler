# Architecture

## Shape

```
src/
  main.rs              Entrypoint: parse, log, dispatch, exit code. Nothing else.
  lib.rs               Library root — everything testable lives below here.
  cli.rs               clap definitions and argument-derived settings.
  kubeconfig.rs        Reading and safely rewriting kubeconfig files.
  cluster.rs           Turning kubeconfig entries into human-facing views.
  format.rs            Ages and aligned tables — pure string formatting.
  theme.rs             The entire colour palette and severity thresholds.
  k8s/                 The Kubernetes client, paging and request budgets,
                       quantities, selectors, nodes, pods, and metrics.
  commands/            One module per user-facing command.
  ui/                  The interactive dashboard.
```

The binary is a shell around the library. If a behaviour only exists in
`main.rs`, it cannot be tested — so it does not go there.

## The rule: separate computation, I/O, and rendering

Three layers, and the boundaries are load-bearing:

**Computation** — parsing, formatting, filtering, state transitions. Pure
functions over plain data. `ClusterIdentity::from_arn`, `render_table`,
`App::on_key`. All of it is tested directly.

**I/O** — the filesystem and the Kubernetes API. Confined to narrow functions
that do nothing but fetch and hand back data: `KubeConfig::load_from`,
`k8s::connect`, `k8s::nodes::fetch`.

**Rendering** — `ui::draw` and friends. Takes `&App` and paints. It makes no
decisions that are not about layout, and it never fetches anything.

This is why `App::on_key` returns a `Flow` rather than calling `exit`, and why
commands return `String` rather than calling `println!`. Every interesting path
is reachable from a test without a terminal, a cluster, or a network.

## Data flow today

```
kubeconfig files ──► KubeConfig ──► ResolvedContext ──► ClusterView ──► table / TUI
   (I/O)              (parse)         (join)           (present)        (render)
```

`KubeConfig` is the literal file contents. `ResolvedContext` joins a context to
the cluster it references. `ClusterView` is the presentation layer — it is what
decides that a user should see `prod (us-east-1)` rather than
`arn:aws:eks:us-east-1:111122223333:cluster/prod`.

Live cluster data enters as a second pipeline, joined to the first by the
selected context:

```
                                          ┌─► nodes::fetch ────┐
ClusterView ──► k8s::connect ──► Client ──┼─► pods::fetch ─────┼─► NodeRow ──► table
 (choose)         (build)                 └─► metrics::usage ──┘  (present)  (render)
                                                (I/O)
```

The three requests are issued concurrently — `eks nodes` should cost one round
trip's wait, not three — and they fail independently. A node listing that fails
ends the command. The other two only cost columns: a failed pod listing empties
the request columns and the counted half of `PODS`, and an absent metrics API
drops the usage columns altogether, each adding a footnote saying why. Both failures are ordinary rather
than exceptional. A role that grants nodes but not pods across every namespace
is a normal thing to have, and metrics-server is an add-on that EKS does not
install for you, so on a fresh cluster there is simply nothing to show.

The metrics request has a third outcome that is neither of those: it succeeds and
carries nothing. That costs the same two columns a failure does, so it earns a
footnote of its own rather than the silence a successful request used to buy.

`NodeRow::from_node` takes an explicit `now`, so ages are computed rather than
observed and every row in a listing shares one instant.

What a node has *booked* is a second computation on that pipeline:
`pods::effective_requests` reduces one pod to the CPU and memory the scheduler
reserved for it, and `pods::by_node` totals those by node. Both are pure
functions over `Pod` values, so the awkward parts — init containers, sidecars,
pod overhead, a pod that has finished — are fixtures rather than a cluster you
have to arrange.

`by_node` returns a `Placed` rather than a bare `Requests`: the count of the
pods it totalled, beside their totals. The count is the `PODS` column's
numerator and it comes out of that one loop deliberately, because whatever rule
decides a pod is occupying a node has to decide both numbers. A cell saying 12
beside a request total covering 14 pods is two plausible figures with nothing
on screen to say they disagree. `Quantity::from_count` then makes the count the
same type as the `allocatable["pods"]` it is divided by, which is what lets the
column reuse `Share` entire — the ratio, the severity thresholds, the cell, and
the sort key — instead of a second division written in integers.

`effective_requests` has two callers, deliberately: `pods::by_node` totals it per
node for `eks nodes`, and `PodRow::from_pod` keeps it per pod as the denominator
`eks pods` shows live usage against. One function rather than two sums is what
stops the two commands from disagreeing about what a single pod booked.

`eks pods` is a third computation on the same pipeline, and the one where the
computation/rendering split earns the most. The `STATUS` column `kubectl` prints
exists nowhere in the API: `status.phase` holds one of five words and none of
them is `CrashLoopBackOff`, `Init:0/2`, `Terminating`, or `Completed`. Those are
derived from the container statuses underneath, by an order-dependent walk with
a dozen special cases. `pods::row` reimplements that walk as a pure function over
a `Pod` and an explicit `now`, so each case is a fixture — including the ones
nobody can arrange on demand, like a pod on a node that stopped answering.

Ordering is the last hop on that pipeline and the one furthest from the cluster:
`k8s::pods::order` sorts finished `PodRow`s, so `--sort` needs neither a clock nor
a request — `PodRow` already carries the instant each restart finished and the
instant the pod was created, each beside the rounded age its cell is rendered
from. Sorting rows rather than pods is also what will let the dashboard reorder a
listing it is already holding.

Each order maps a row to a `Rank`, which is either something to compare or a
marker that this row has nothing to rank. `--sort-reverse` flips only the first
kind, so the rows an order cannot rank stay in the tail whichever way round the
listing runs — reversing the whole comparison would open every reversed listing
on its blank rows. That split is why the tail tiers of `restarts` (an undated
restart is not the same blank as no restart) survive reversal too.

`Rank`, `Direction`, the comparison that keeps those two halves apart, and the
two notes under a reordered table all live in `k8s::order`, one level above both
listings, because `eks nodes --sort` follows exactly the same rules and a second
copy of them would be a second chance to drift. The notes are generic over the
`Order` enums and take their spelling from `clap::ValueEnum`, so an ordering is
always called what the flag calls it. What they cannot know is the rows, so each
listing hands in the two facts they turn on: a `ranks_any` predicate — asked
about every ordering, not just the one that failed, since the answers for the
others are the "sort by … instead" advice — and a `cause`, which is that
listing's own map from an ordering to the footnote that would already have
explained an empty column. What is *not* shared is the keys: `k8s::nodes::order` ranks by the share of
allocatable a node's figures represent, where `k8s::pods::order` ranks by the
figure itself. That was once for want of a denominator; now that a pod's usage
has one, it is a choice — a node's denominator is the machine, so 95% of a small
one is comparable to 30% of a large one, while a pod's is whatever a manifest
asked for, and 400% of a 10m request is 40m of anybody's cluster. `NodeRow`
gained a `created_at` beside its rendered `age` for the same reason `PodRow` has
one — two nodes can both read `3d` and be nearly a day apart. `--sort pods` is
the newest of the node orders and ranks a share too: the node worth looking at
is the one with two slots left, not the one running the most pods.

Which columns a table has is the same shape of decision, one hop later, and it is
settled the same way: `k8s::nodes::columns` and `k8s::pods::row::columns` are
pure functions from a listing's conditions — the scope, whether any row carries
live usage, whether any row shows that usage against a request, and
`format::Width` — to a `Vec<Column>`, and each `Column` answers for both its
heading and its cell. The pod table's usage columns carry that last condition as
data, because it decides a heading rather than a column: `CPU/REQ` over a column
of `262m/500m (52%)` pairs, plain `CPU` where no row has one. Two parallel lists of headers and cells is the
alternative, and it has a failure that type-checks: a heading added under one
condition and its cell under a subtly different one shifts every figure to the
right of it under the wrong heading, and the table still renders. `format::Width`
sits with `format::table` rather than in `k8s`, because `--wide` decides nothing
about what is fetched — everything the extra columns show already arrived with
the nodes and pods. Its `Narrow(u16)` variant is the other end of `--wide`:
the same one type carries "widen" and "fit this many characters" so both
listings agree on what the flag means, and each is free to decide its own drop
order. Each table's lives beside its `columns` function — `k8s::nodes::DROP_ORDER`
and `k8s::pods::row::DROP_ORDER` — and the two are different lists because the
tables hold different things: a node's `VERSION` goes first, a pod's `AGE` does.
What they share is the measurement: `format::column_widths` and
`format::row_width`, the same pair `format::table` pads and separates by, so a
drop rule cannot stop at a width the renderer does not print at. Each rule
measures the listing once and then drops columns by arithmetic over the widths,
because a column's width does not depend on which of its neighbours are still
there. The
terminal-size lookup lives in one function in `main.rs`, so the arithmetic that
picks columns has no ioctl to fake in a test. `format::list` sits beside it for the same reason: writing a
set of names out as prose, serial comma and all, is a rule that belongs in one
place whether the sentence around it is offering orderings to sort by or naming
the columns a failed pod listing emptied.

Selectors take the same shape in reverse: `k8s::selector` parses the label and
field selectors a user types (`app=api`, `status.phase!=Running`) into a
canonical string, rejecting a malformed one — with the offending text quoted —
before `eks pods` connects. It is another pure parser with no Kubernetes types
in its signature, so the whole grammar is a fixture table; the command layer's
`selectors_for` is where that validation is wired ahead of any request.

Live usage is a fourth computation on that pipeline, and the one that needed a
type the API does not give us. `metrics.k8s.io` is an aggregated API served by an
optional add-on, so `k8s-openapi` — which only generates the core API — has no
`NodeMetrics`. `k8s::metrics` hand-writes it: a serde struct plus a
`kube::Resource` impl whose group, version, and plural are what put
`/apis/metrics.k8s.io/v1beta1/nodes` on the wire. Fetching sits behind the
`Source` trait, so the paths worth testing — no metrics-server at all, a node the
sampler has not reached, a reading that will not parse — are fixtures rather than
a cluster somebody has to break. `nodes::Share` then carries requests and usage
in one shape, because they answer different questions against the same
denominator and neither should be able to disagree with the other about what
counts as hot.

The pod half of that API is the same idea in a different shape. `PodMetrics` is
namespaced, so the listing follows `--namespace`/`--all-namespaces` like the pod
listing beside it, and it reports per *container* rather than per object, so
`metrics::pod_usage` sums the containers — all of them or none, since a partial
sum reads on screen exactly like a complete one. Usage is joined onto the rows by
namespace and name in the command layer, which is also what makes the columns
follow the selectors: only pods the API server already returned have a row to be
given a figure.

A sample is more than its figures. metrics-server stamps each one with when it
was taken and the window it was averaged over, and `metrics::Sample` carries both
through the join so a table can be dated from the samples that actually reached
it rather than from whatever the endpoint returned. `metrics::freshness` reduces
those to the age of the oldest sample and the longest window — a pure function
over samples and one instant, so staleness is a fixture rather than a wait — and
`metrics::Outcome` is the three-way answer the command layer needs afterwards:
the columns are shown and want dating, the read failed, or the read answered with
nothing and the table owes an explanation nobody was giving it. Which of the
three a listing is in is asked of the rendered rows, not of the reply, so the
footnote and the columns cannot disagree.

Which resources a node has is a fifth computation, and the one whose columns are
not known until the nodes arrive. `k8s::resource::is_extended` decides whether a
name in a node's capacity map is a device the cluster added or one Kubernetes
defines, by Kubernetes' own rule — qualified, and outside the `kubernetes.io`
domain — so `nvidia.com/gpu` earns a column and `hugepages-2Mi` does not.
`NodeRow` carries a `Device` per extended resource it reports, and
`nodes::columns` takes the union across the listing, which is what lets a
cluster with one GPU node group show the column on every row and a `-` on the
nodes without the hardware. `Column` borrows the resource name from the rows for
exactly this reason: it is the one column whose identity is data.

The same names reach the other end of the pipeline through `pods::Requests`,
which keeps `cpu` and `memory` as fields and everything else in a map, so
`effective_requests` charges a pod's GPU by the scheduler's rules rather than by
a second sum written for devices.

Every one of those listings reaches the wire through one function. `k8s::page`
asks for `page::SIZE` objects at a time and follows the `continue` token, so a
cluster with ten thousand pods is a series of ordinary responses rather than one
enormous one — and an ordinary cluster is still the single request it always
was, because a first page that comes back short carries no token. The loop is
four lines of I/O in `page::collect`; everything that decides whether there is
another page lives in `page::Listing`, over a page of items and a token, which
is what makes a three-page listing a fixture. It also spots the one shape of
answer that could hang the tool for ever — a server handing back the token it
was given — and stops rather than following it.

`page::Budget` is `--timeout`, and it lives there because paging is what
decides its unit. A listing is now several requests, so the budget is spent per
request: a cluster large enough to need four pages should not be cut off for its
size, only for going quiet. Cluster failures are therefore `page::Error` rather
than `kube::Error` — the API failures and the one `kube` has no opinion about,
since waiting for ever is a reasonable thing for a library to do and not for a
command-line tool. The budget covers requests and not `k8s::connect`, and that
is a limitation rather than a choice: `kube` runs a kubeconfig's credential
helper with a blocking `std::process::Command`, so a hung `aws eks get-token`
blocks the thread and no timeout around it would fire.

Resource quantities get their own hop: the API server reports capacity as
strings in a small grammar (`3920m`, `7134420Ki`, `1e3`), and `k8s::quantity`
turns those into numbers before anything formats or divides them. It is a pure
parser with no Kubernetes types in its signature beyond the newtype it unwraps,
which is why the whole suffix table is covered by tests rather than by whatever
instance types happen to be in the cluster you tried it on.

Only the async commands build a Tokio runtime, and they build it themselves —
see `commands::block_on`. `eks contexts` still starts with nothing but a file
read. When the dashboard grows live data (see `docs/ROADMAP.md`), fetching moves
onto a background task feeding `App` over a channel; the render loop will never
await it.

## Testing

Run `make test`. The suite needs no cluster, no credentials, and no network, and
that is a property to defend rather than a coincidence.

- **Pure logic** — assert directly on returned values.
- **Filesystem behaviour** — `tempfile` for real files in a temp dir. The
  kubeconfig writer is tested for durability and for preserving fields we do not
  model.
- **Terminal rendering** — `ratatui`'s `TestBackend` renders into an in-memory
  buffer; assert on the text that lands on screen. Always include a
  tiny-terminal case; a panic mid-render leaves a real user in raw mode.
- **Input** — construct `KeyEvent`s and feed them to `App::on_key`.

Tests that need a fake cluster get fixtures, never live AWS.

## Error handling

`thiserror` for typed errors at module boundaries (`kubeconfig::Error`,
`k8s::client::Error`), `anyhow` at the command layer where the caller is a
human, not code. `main` prints the whole context chain with `{:#}`.

Errors from the cluster get one extra step: `k8s::client::explain` turns a
`page::Error` into a sentence naming the cluster and what to do next, and the
raw error goes to `tracing::debug` instead of the user's terminal. It is a pure
function over a classified failure, so each message is asserted on in a test.
Two of those classifications exist because listings are paged and budgeted: a
`410` is a page marker that expired mid-listing, which is not the user's fault
and wants "run it again"; and a `Slow` names the budget it overran and the
larger one to type, spelled through `Budget` itself so the advice cannot suggest
a value the flag would reject.

`k8s::metrics::explain` is the one place that wraps it. Two failures dominate the
metrics endpoint and a core-API caller never sees either — a `404` because
nobody registered the API group, and a `503` because metrics-server is up but has
not finished its first scrape — and both have concrete advice behind them.
Everything else falls straight through to the shared explanation rather than
growing a second vocabulary for an expired SSO session.

`k8s::metrics::unsampled` sits beside it and is deliberately not one of its
branches: no request failed, so there is no `kube::Error` to explain. It is the
sentence for a reply that arrived empty, and it says metrics-server is installed
precisely because the footnote it replaces would have said the opposite.

`unwrap`, `expect`, and `panic!` are denied by lint in library code. Ask what
should happen instead and return a `Result` saying so.
