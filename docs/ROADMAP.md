# Roadmap

The backlog for the nightly loop. Tasks are ordered by priority — take the
highest unchecked one that fits in a single pull request.

Each task lists acceptance criteria. A task is done when those hold, tests cover
them, and `make check` passes. Tick the box in the same PR that implements it.

If a task turns out to be larger than one PR, land the smallest useful slice,
tick nothing, and split the remainder into new tasks here.

---

## Milestone 1 — Live cluster data

The tool currently reads kubeconfig only. This milestone makes it talk to a
cluster.

- [x] **Async runtime and Kubernetes client bootstrap.**
  Add `tokio`, `kube`, and `k8s-openapi`. Build a client from the selected
  context. Add `eks nodes` printing name, status, version, age.
  *Acceptance:* works against a real EKS cluster; a missing/expired credential
  produces a clear message naming the context and suggesting
  `aws sso login`, not a raw HTTP error; node-row formatting is a pure function
  tested against fixture `Node` objects; no live-AWS test.

- [x] **Quantity parsing, and node capacity and allocatable.**
  Parse the Kubernetes resource-quantity grammar; show CPU and memory capacity
  and allocatable per node.
  *Acceptance:* Kubernetes quantity parsing (`100m`, `1.5`, `2Gi`, `1e3`) is its
  own tested function covering each suffix and rejecting garbage.

- [x] **Pod requests per node, and utilisation percentages.**
  Sum each node's pod requests and show them against allocatable as a
  percentage. Split out of the task above, which was two PRs' worth.
  *Acceptance:* the effective pod request — `max(sum of containers, max of init
  containers)` plus pod overhead, with sidecars counted in the sum — is its own
  tested pure function; percentages use `theme::Severity` thresholds; a node
  running no pods reads as `0%` rather than as an error.

- [x] **`eks pods` listing.**
  Pods for a namespace (or `--all-namespaces`) with status, ready count,
  restarts, age, and node.
  *Acceptance:* phase derivation matches `kubectl` for the awkward cases —
  `CrashLoopBackOff`, `Terminating`, `Init:0/2`, `Completed`; each has a test.

- [ ] **metrics-server integration.**
  Live CPU/memory usage for nodes and pods where `metrics.k8s.io` is available.
  *Acceptance:* absent metrics-server degrades to showing requests with a note,
  never an error; the API call is behind a trait so it can be faked in tests.

### Follow-ups from the pod listing

- [ ] **Label and field selectors for `eks pods`.**
  `-l app=api` is the first thing anyone reaches for after seeing a pod list,
  and the second is "only the ones that are not Running".
  *Acceptance:* the selector is passed to the API server rather than filtered
  client-side; an unparseable selector is rejected with the offending text
  quoted, before any request is made.

- [ ] **How long ago the last restart was.**
  `kubectl` shows `9 (5m ago)`, taking the newest `lastState.terminated
  .finishedAt` across the containers. A restart count with no recency behind it
  cannot distinguish a pod that crashed nine times last Tuesday from one
  crashing now, which is the whole question.
  *Acceptance:* the timestamp chosen is a pure function over the container
  statuses, following `kubectl`'s rule that only sidecar restarts survive
  initialisation; a pod that has never restarted shows the bare count.

- [ ] **A `--wide` mode for `eks pods`.**
  Pod IP, and the nominated node for a preempting pod — the two columns
  `kubectl -o wide` adds that answer questions the current table raises.
  *Acceptance:* the column set is a pure function over the flag, tested both
  ways; the default table is unchanged to the byte.

### Follow-ups from the capacity columns

- [ ] **Extended resources in the node table.**
  GPUs and other device-plugin resources (`nvidia.com/gpu`) are parsed correctly
  but never shown; a node table that hides the reason a pod will not schedule is
  doing half a job.
  *Acceptance:* a column appears only when some node in the listing reports the
  resource, so a CPU-only cluster gains no empty columns.

- [ ] **A narrow mode for the node table.**
  `eks nodes` is now eight columns and around 120 characters wide, which wraps
  on an 80-column terminal. The request columns made this worse, and they are
  also the ones most worth keeping when space is short.
  *Acceptance:* columns are dropped in a documented order to fit the terminal;
  the choice is a pure function over an available width, tested at 80, 100, and
  1 column.

### Follow-ups from the request columns

- [ ] **A pod count per node.**
  The request totals are computed from a full pod listing that is then thrown
  away; `PODS` is one more column and the number people ask for next, alongside
  the node's `maxPods` limit, which is the *other* reason a pod will not
  schedule.
  *Acceptance:* the count excludes finished pods, matching the requests total it
  sits beside; `maxPods` comes from the node's `allocatable["pods"]`.

- [ ] **Paginate the pod listing.**
  `eks nodes` now fetches every pod in the cluster in one request to total the
  requests, and `eks pods -A` does the same to list them. On a large cluster
  that is the biggest response the tool asks for, and it shares the paging
  problem the node listing has.
  *Acceptance:* pages with the same tested continue-token function the node
  listing uses; the two listings still run concurrently.

- [ ] **Severity colour in the CLI table.**
  `Requested::severity` classifies each percentage on the shared thresholds, and
  the CLI table then prints it in plain text. A node at 97% deserves to look
  like it, at least when stdout is a terminal. `PodRow::severity` is now in the
  same position — a `CrashLoopBackOff` reads exactly like a `Running` — so one
  change should light up both tables.
  *Acceptance:* colour is suppressed when stdout is not a TTY and when `NO_COLOR`
  is set; the decision is a pure function, tested both ways.

### Follow-ups from the client bootstrap

- [ ] **Paginate node listings.**
  `eks nodes` fetches every node in one request. A cluster with thousands of
  nodes deserves `limit`/`continue` paging, and eventually a spinner while the
  pages arrive.
  *Acceptance:* paging is driven by a tested pure function over the continue
  token; a fixture with three pages is covered.

- [ ] **Move `eks contexts` onto the shared table renderer.**
  `format::table` now renders `eks nodes`; `commands::contexts` still has its
  own copy, which also owns the `*` active-cluster gutter.
  *Acceptance:* one renderer, existing `contexts` output unchanged to the byte.

- [ ] **Global flags before a subcommand.**
  `eks --kubeconfig x contexts` is rejected because `args_conflicts_with_subcommands`
  treats the flag as the bare-dashboard form. Flags after the subcommand work,
  which makes the failure look arbitrary.
  *Acceptance:* both orders parse; a test covers each global flag in both
  positions.

- [ ] **A `--timeout` for cluster requests.**
  A hung API server currently leaves `eks nodes` waiting forever with no way out
  but Ctrl-C.
  *Acceptance:* the default is documented; timing out names the cluster and
  suggests checking VPN or endpoint access.

## Milestone 2 — The dashboard

- [ ] **Node pane with live data.**
  Replace the placeholder overview with a node list: utilisation bars, pod
  counts, conditions.
  *Acceptance:* first paint happens before data arrives; loading state is
  visible; `TestBackend` tests for loading, loaded, and error states.

- [ ] **Background refresh.**
  Move fetching onto a background task feeding the UI over a channel. Refresh on
  an interval and on demand (`r`).
  *Acceptance:* the render loop never awaits a network call; a hung API call
  leaves the UI fully navigable; refresh interval is configurable.

- [ ] **Pod browsing.**
  Drill from a node into its pods, and from a pod into its containers. Namespace
  filter. Breadcrumbs showing where you are.
  *Acceptance:* every view is reachable and escapable by keyboard alone;
  navigation state transitions are unit-tested without a terminal.

- [ ] **Pod detail view.**
  Containers, images, resource requests/limits, restart reasons, recent events.
  *Acceptance:* long values wrap rather than truncate; tested at 80 columns.

- [ ] **Log viewing.**
  Stream logs for a container, with follow, scrollback, and wrap toggle.
  *Acceptance:* streaming never blocks input; leaving the view cancels the
  request; a 10k-line burst does not stall the UI.

- [ ] **Fuzzy search.**
  `/` filters the current view. Fuzzy, case-insensitive, ranked.
  *Acceptance:* the matcher is a pure, tested function; filtering 10k rows stays
  under one frame — cover it with a benchmark.

## Milestone 3 — Polish

- [ ] **Config file.**
  `~/.config/eks/config.toml` for theme, refresh interval, default namespace.
  CLI flags override the file; the file overrides defaults.
  *Acceptance:* precedence is tested; a malformed file warns and falls back to
  defaults rather than exiting.

- [ ] **Light theme and auto-detection.**
  Detect terminal background where possible, with a config override.
  *Acceptance:* both themes meet WCAG AA contrast for body text; a test asserts
  the contrast ratios.

- [ ] **Startup budget and benchmarks.**
  Add `criterion` benchmarks for kubeconfig parsing and first paint. Document
  the budget from CLAUDE.md and measure against it.
  *Acceptance:* `make bench` runs; CI reports regressions rather than failing.

- [ ] **Shell completions and a man page.**
  Generate from the clap definition via `clap_complete` and `clap_mangen`.
  *Acceptance:* `eks completions bash|zsh|fish` emits valid output; generation
  is covered by a test; `make dist` includes them.

- [ ] **Golden-file rendering tests.**
  Use `insta` snapshots over `TestBackend` for the main views.
  *Acceptance:* snapshots committed; `cargo insta` workflow documented in
  CONTRIBUTING-level detail in `docs/ARCHITECTURE.md`.

## Milestone 4 — Distribution and hardening

- [ ] **More release targets.**
  Add `aarch64-unknown-linux-gnu` and `x86_64-unknown-linux-musl` to the release
  workflow.
  *Acceptance:* both cross-compile in CI; musl binary is verified static.

- [ ] **Supply-chain checks in CI.**
  `cargo-deny` for advisories, licences, and duplicate versions.
  *Acceptance:* runs on PRs; `deny.toml` committed with rationale for any
  allowance.

- [ ] **MSRV verification job.**
  Build against the `rust-version` in `Cargo.toml` so it stops being a claim.
  *Acceptance:* CI job pinned to that toolchain.

- [ ] **Install story.**
  An install script and a Homebrew tap formula.
  *Acceptance:* `README.md` documents install for macOS and Linux; the script
  verifies checksums.

---

## Done

- **`eks pods` listing** (2026-08-18) — pods for one namespace or for every
  namespace, with `kubectl`'s own `STATUS` derivation reimplemented as a pure
  function in `k8s::pods::row`: the backwards walk over app containers, sidecar
  handling, `Init:<n>/<total>` progress, `Terminating`, and a pod on a lost node
  reading `Unknown`. Asking for one namespace and all of them at once is an
  error rather than one flag quietly winning.

- **Pod requests per node, and utilisation percentages** (2026-08-18) —
  `k8s::pods` totals the effective requests of the pods on each node, following
  the scheduler's own arithmetic for init containers, sidecars, and pod
  overhead; `eks nodes` gained `CPU REQ` and `MEM REQ` columns showing the total
  and its share of allocatable. The pod listing runs concurrently with the node
  listing, and failing it costs two columns rather than the command.

- **Quantity parsing, and node capacity and allocatable** (2026-08-18) —
  `k8s::quantity` parses the full resource-quantity grammar; `eks nodes` gained
  `CPU` and `MEMORY` columns showing allocatable against capacity. The pod
  requests and utilisation half of the original task was split into its own
  entry above.

- **Async runtime and Kubernetes client bootstrap** (2026-08-17) — `tokio`,
  `kube`, and `k8s-openapi` added; `eks nodes` lists name, status, version, and
  age; cluster failures are translated into advice rather than HTTP statuses.

## Ideas, not yet scheduled

Pull these up into a milestone when they become the most valuable next thing.

- Multi-account support (the current design assumes one AWS account).
- `eks exec` into a container, and port-forwarding.
- Resource editing — scale a deployment, delete a pod, cordon/drain a node.
  Needs a confirmation-and-undo design first; destructive actions deserve care.
- Cost attribution per namespace or workload.
- CloudWatch and control-plane log integration.
- Watch-based incremental updates instead of polling.
- A `--json` output mode across every read command for scripting.
