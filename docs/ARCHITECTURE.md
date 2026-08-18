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
  k8s/                 The Kubernetes client, quantities, nodes, and pods.
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
ClusterView ──► k8s::connect ──► Client ──┬─► nodes::fetch ─┐
 (choose)         (build)                 │     (I/O)       ├─► NodeRow ──► table
                                          └─► pods::fetch ──┘  (present)  (render)
                                                (I/O)
```

The two listings are issued concurrently — `eks nodes` should cost one round
trip's wait, not two — and they fail independently. A node listing that fails
ends the command; a pod listing that fails only empties the request columns and
adds a footnote saying why, because a role that grants nodes but not pods across
every namespace is a normal thing to have.

`NodeRow::from_node` takes an explicit `now`, so ages are computed rather than
observed and every row in a listing shares one instant.

What a node has *booked* is a second computation on that pipeline:
`pods::effective_requests` reduces one pod to the CPU and memory the scheduler
reserved for it, and `pods::by_node` totals those by node. Both are pure
functions over `Pod` values, so the awkward parts — init containers, sidecars,
pod overhead, a pod that has finished — are fixtures rather than a cluster you
have to arrange.

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
`kube::Error` into a sentence naming the cluster and what to do next, and the
raw error goes to `tracing::debug` instead of the user's terminal. It is a pure
function over a classified failure, so each message is asserted on in a test.

`unwrap`, `expect`, and `panic!` are denied by lint in library code. Ask what
should happen instead and return a `Result` saying so.
