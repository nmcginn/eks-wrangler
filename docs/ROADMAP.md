# Roadmap

The backlog for the nightly loop. Tasks are ordered by priority — take the
highest unchecked one that fits in a single pull request.

Each task lists acceptance criteria. A task is done when those hold, tests cover
them, and `make check` passes. Tick the box in the same PR that implements it.

If a task turns out to be larger than one PR, land the smallest **complete**
slice — one a user meets as a finished change — tick nothing, and split the
remainder into new tasks here. Split at a seam: a surface this PR does not
touch, or a decision the reviewer should make first. An entry that exists only
because the last change stopped short of the bar in `CLAUDE.md` is not a
follow-up; it is unfinished work, and it belongs in the PR that raised it.

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

- [x] **metrics-server integration for nodes.**
  Live CPU/memory usage per node where `metrics.k8s.io` is available.
  *Acceptance:* absent metrics-server degrades to showing requests with a note,
  never an error; the API call is behind a trait so it can be faked in tests.

- [x] **metrics-server integration for pods.**
  The other half of the task above, which was two PRs' worth. `eks pods` should
  gain `CPU` and `MEMORY` columns from `metrics.k8s.io/v1beta1/pods`, which is a
  namespaced listing summing per-container usage — a different shape from the
  node one, and the reason the two split.
  *Acceptance:* usage is summed across a pod's containers as a tested pure
  function; the columns follow `--namespace`/`--all-namespaces` and the existing
  selectors; a pod metrics-server has not sampled reads as unknown, not zero.

### Follow-ups from the pod listing

- [x] **Label and field selectors for `eks pods`.**
  `-l app=api` is the first thing anyone reaches for after seeing a pod list,
  and the second is "only the ones that are not Running".
  *Acceptance:* the selector is passed to the API server rather than filtered
  client-side; an unparseable selector is rejected with the offending text
  quoted, before any request is made.

- [ ] **Carry the pod selectors into the dashboard.**
  `k8s::selector` and `k8s::pods::Selectors` now filter `eks pods`; the dashboard
  pod views (Milestone 2) should take the same `-l`/`--field-selector` filters
  rather than growing their own, so a selector means one thing across the tool.
  *Acceptance:* the dashboard's pod fetch reuses `Selectors`; the parse-and-quote
  rejection path is shared, not duplicated.

- [x] **How long ago the last restart was.**
  `kubectl` shows `9 (5m ago)`, taking the newest `lastState.terminated
  .finishedAt` across the containers. A restart count with no recency behind it
  cannot distinguish a pod that crashed nine times last Tuesday from one
  crashing now, which is the whole question.
  *Acceptance:* the timestamp chosen is a pure function over the container
  statuses, following `kubectl`'s rule that only sidecar restarts survive
  initialisation; a pod that has never restarted shows the bare count.

- [x] **Sort the pod listing by how recently it restarted.**
  The restart column now carries a date, and the question behind it — "what is
  crashing *now*" — is a sort, not a scan. Alphabetical order buries the pod
  that restarted eight seconds ago among a hundred healthy ones.
  *Acceptance:* the ordering is a pure function over the rows; pods that have
  never restarted sort last rather than first; the default order is unchanged.

- [x] **More orderings for `--sort`, and a way to reverse one.**
  `--sort` now exists with two values, and `age`, `cpu`, and `memory` are the
  obvious next ones — the usage columns have exactly the same problem the
  restart column had, in that the pod burning a core is wherever its name puts
  it. A `--sort-reverse`, or a `-` prefix, would be the other half.
  *Acceptance:* each ordering is total, so a listing never shuffles between two
  runs of one command; reversing an ordering does not move the "nothing to rank
  this by" rows out of the tail, where they belong under either direction.

- [x] **Sort the node table too.**
  `eks nodes` has the same problem `eks pods` had before `--sort`: ten columns of
  numbers in alphabetical order. `k8s::pods::order` is generic in shape but not
  in type — `Rank`, `Direction`, and the reversal rule are the reusable half, and
  the keys are not — so this is a question of what to share rather than a copy.
  *Acceptance:* the rule that unrankable rows stay in the tail is stated once,
  not twice; a node with no metrics sorts last under `--sort cpu` in either
  direction.

- [x] **Say which ordering a listing is in.**
  `--sort cpu --sort-reverse` prints a table that looks exactly like `--sort cpu`
  to anyone who did not type it, and the tail of unranked rows makes a reversed
  listing look sorted the other way at a glance. A line under the table, beside
  the metrics footnote, would say. Now wanted by both listings, and worth writing
  once in `k8s::order` over an `Order`'s `clap::ValueEnum` name rather than twice.
  *Acceptance:* the default order says nothing, so the existing output is
  unchanged to the byte; the note is a pure function over the order and
  direction.

- [ ] **Carry the sort note into the dashboard's panes.**
  The note now sits in the footnote list of both CLI tables, and the dashboard
  panes will want the same line once they take `--sort` — but a pane has a title
  bar the CLI table does not, and a footnote under a scrolling list is a worse
  place for it than the header. Discovered while writing the CLI note.
  *Acceptance:* the note comes from `k8s::order::note`, not a second wording;
  the default order is as silent in a pane as it is on the command line.

- [x] **An ordering that ranked nothing should say so.**
  `eks nodes --sort cpu` on a cluster with no metrics-server sorts by a column
  that is not in the table: every row is unranked, and the alphabet decides the
  whole listing. The table now says `Sorted by cpu.` under it, which makes this
  *worse* rather than better — it names an ordering that did nothing, over rows
  the alphabet put in that order. The footnote beside it explains why the columns
  are missing but says nothing about the sort the user actually typed, so the
  flag still reads as broken. Discovered while sorting the node table.
  *Acceptance:* the note fires only when no row could be ranked, and names the
  ordering; a listing where even one row ranked says nothing extra.

- [x] **Point the "nothing ranked" note at what would fix it.**
  `Nothing here has cpu to sort by.` now sits under the table, and on a cluster
  with no metrics-server the footnote above it already says what to install — but
  the two are separate paragraphs that never refer to each other, and under
  `--sort restarts` in a healthy namespace there is nothing above it at all. The
  note is honest and it is not yet advice. Discovered while writing it.
  *Acceptance:* the wording is still one function over the `Order` name, not a
  sentence per ordering; a listing where the fixable cause is already explained
  above does not explain it twice.

- [ ] **Suggest orderings that tell the rows apart, not merely rank them.**
  The advice under an unranked listing suggests every ordering that can rank a
  row, which is the `any`-not-`all` rule the rest of `--sort` follows. On a
  cluster where every node is `Ready`, that suggests `status`, and `--sort
  status` there reorders nothing — the user is sent to a second listing in the
  same order as the first. Separate because the fix turns on a decision the
  reviewer should make rather than one this PR could guess: whether "the rows
  differ under this ordering" is the right bar for a *suggestion* when "one row
  ranked" is deliberately the bar everywhere else, and whether an ordering that
  puts every row in one group is really saying nothing — "everything here is
  Ready" is an answer too.

- [ ] **Carry the "nothing ranked" note into the dashboard's panes.**
  Pairs with the sort-note task above, and has the same shape: a pane that takes
  `--sort` can sort by a column its cluster does not populate, and the pane's
  header is the place to say so rather than a footnote under a scrolling list.
  Discovered while writing the CLI note.
  *Acceptance:* the pane words the answer through `k8s::order::unranked_note`,
  not a second wording, handing it the same two facts the CLI does — a `ranks`
  predicate over its own rows and a `k8s::nodes::cause`/`k8s::pods::cause`. The
  `Cause::Explained` half needs a decision the CLI did not have to make, since a
  pane has nowhere obvious for "the reason above" to point.

- [ ] **Carry `--sort` into the dashboard, alongside the selectors.**
  `k8s::pods::order` is deliberately a function over rows rather than over pods,
  so the dashboard's pod views can sort the rows they already have without
  refetching — and should, rather than growing an ordering of their own. Pairs
  with the selector task above; the three flags belong to the same listing.
  `k8s::nodes::order` is now the same shape, so the node pane gets this for free
  and the two panes should share the `Direction` key that reverses them.
  *Acceptance:* the dashboard sorts through `k8s::pods::sort` and
  `k8s::nodes::sort`; a key press changes the order, and another reverses it,
  without a request.

- [x] **A `--wide` mode for `eks pods`.**
  Pod IP, and the nominated node for a preempting pod — the two columns
  `kubectl -o wide` adds that answer questions the current table raises.
  *Acceptance:* the column set is a pure function over the flag, tested both
  ways; the default table is unchanged to the byte.
  Landed on `eks nodes` in the same change, since a `--wide` that one listing
  rejected as an unknown argument would read as a bug; and with `READINESS
  GATES` as a third pod column, because the default table cannot explain a pod
  whose `READY` reads `1/1` while the cluster calls it unready.

- [ ] **Decide what `--wide` means in a dashboard pane.**
  `format::Width` and the two `columns` functions are ready for a third caller,
  and the pod and node panes will meet the same question the CLI tables did.
  Separate because the answer may not be a wide mode at all: a pane is narrower
  than a terminal, not wider, and a pod's IP and readiness gates may belong in a
  detail view for the selected row rather than in five more columns nobody can
  fit. That is the reviewer's call to make before anything is built, and it
  needs the pane to exist first.
  *Acceptance:* whatever it turns into, the columns come from
  `k8s::pods::row::columns` and `k8s::nodes::columns` rather than a second list;
  a pane that shows the extra fields at all shows them under the same headings
  the CLI uses.

### Follow-ups from the usage columns

- [ ] **Usage against what the pod asked for.**
  `eks pods` now shows what a pod is burning, and the node table shows every
  figure as a share of something. A pod's `CPU`/`MEMORY` has no denominator, and
  the one that answers "is this limit wrong?" is the pod's own request — the
  number that decides whether it is throttled or about to be OOM-killed.
  *Acceptance:* the request comes from the existing `effective_requests`, so the
  two commands cannot disagree; a pod with no request shows the bare figure
  rather than a percentage of zero.

- [ ] **Usage against capacity for the dashboard's bars.**
  `nodes::Share` divides usage by *allocatable*, which is the right denominator
  for "will another pod fit". A utilisation bar is asking a different question —
  "is this machine busy" — and wants capacity underneath it, so a node at 100%
  of allocatable does not draw as a full bar when a tenth of the machine is
  still kubelet reserve.
  *Acceptance:* the choice of denominator is explicit at the call site rather
  than baked into `Share`; both readings are tested on one fixture node.

- [ ] **Show the sampling window beside the usage columns, and say when there
  was no sample.**
  metrics-server reports a `window` — typically `20s` — over which each sample
  was averaged, and a usage figure with no window behind it cannot be told apart
  from an instantaneous reading. It also goes stale silently when the scraper
  stops. The same silence covers the case where `metrics.k8s.io` answers with an
  empty list — metrics-server registered but not yet scraping — where the usage
  columns simply vanish and no footnote is printed, because the footnote is
  written on the error path and there was no error. Noticed while deciding what
  `k8s::nodes::cause` should call that case: `--sort cpu` now says something
  about it, and a table nobody sorted still says nothing.
  *Acceptance:* the age of the oldest sample in the listing is shown once, under
  the table; a sample older than a couple of windows says so; an empty sample set
  earns the same footnote a failed read does, worded for "not scraping yet"
  rather than "not installed".

### Follow-ups from the capacity columns

- [ ] **Extended resources in the node table.**
  GPUs and other device-plugin resources (`nvidia.com/gpu`) are parsed correctly
  but never shown; a node table that hides the reason a pod will not schedule is
  doing half a job.
  *Acceptance:* a column appears only when some node in the listing reports the
  resource, so a CPU-only cluster gains no empty columns.

- [ ] **A narrow mode for the node table.**
  `eks nodes` is now ten columns and around 140 characters wide on a cluster
  with metrics-server, which wraps on an 80-column terminal. The request and
  usage columns made this worse, and they are also the ones most worth keeping
  when space is short. `format::Width` now exists as the other end of this —
  `--wide` is the opt-in direction — and `k8s::nodes::columns` is the one place
  the column set is decided, so this is a third variant and a drop order rather
  than a new mechanism.
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
  requests, and `eks pods -A` does the same to list them — twice over now, since
  the metrics listing beside it is unpaged too. On a large cluster that is the
  biggest response the tool asks for, and it shares the paging problem the node
  listing has.
  *Acceptance:* pages with the same tested continue-token function the node
  listing uses; the two listings still run concurrently.

- [ ] **Severity colour in the CLI table.**
  `nodes::Share::severity` classifies each percentage on the shared thresholds,
  and the CLI table then prints it in plain text. A node at 97% deserves to look
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

- **Point the "nothing ranked" note at what would fix it** (2026-08-19) — the
  note now advises as well as diagnoses. Where a footnote above the table has
  already named the cause and linked to the fix, it points back at it —
  `Nothing here has cpu to sort by, for the reason above.` — rather than writing
  the same paragraph again a line later; `k8s::nodes::cause` and
  `k8s::pods::cause` are what decide, because which of a table's footnotes covers
  which column is exactly the knowledge `k8s::order` does not have. Where nothing
  is above it — `--sort restarts` in a namespace where nothing has crashed, which
  is not a failure at all — the diagnosis stands alone. Either way a second line
  says what to type instead: `Sort by age, cpu, or memory instead.`, worked out
  by asking the listing's own `ranks_any` about every variant of its `Order`
  enum, so the advice comes from the rows in front of the user and cannot name an
  ordering that would have failed the same way. The default ordering and any
  variant hidden from `--help` are left out — one is what typing no flag gives
  you, the other is a flag value nobody can find — and when that leaves nothing,
  the advice line is dropped rather than invented.

- **An ordering that ranked nothing should say so** (2026-08-19) — `eks nodes
  --sort cpu` on a cluster with no metrics-server now prints `Nothing here has
  cpu to sort by.` under the line naming the ordering, instead of leaving
  `Sorted by cpu.` to describe a listing the alphabet arranged. The split
  `k8s::order` already anticipated holds: `note` says which ordering was asked
  for and stays blind to the rows, and `unranked_note` is the separate answer to
  the separate question, taking the one fact the module cannot work out for
  itself — whether any row carried the key — from each listing's new
  `ranks_any`. `any` rather than `all`, matching the rule the usage columns
  already follow: one unsampled row is not a listing the ordering failed to
  order, because one ranked row puts the row somebody went looking for at an end
  of the table. Rankability is a second exhaustive `match` over `Order` beside
  the comparison, so an ordering added without saying what makes a row rankable
  under it fails to compile rather than quietly claiming every such listing
  ranked nothing — and the pod `restarts` arm is the reason the two matches are
  not one: a restart the kubelet recorded no `finishedAt` for is `Unranked`
  under `recency` but is still something this ordering sorted on, because the
  count is a key there as well as a tie-break. The note stops short of naming
  the order the rows came out in instead: unranked rows keep their tail tiers,
  so a listing can be grouped by something even when nothing in it ranked, and
  "this is in name order" would be a guess.

- **Say which ordering a listing is in** (2026-08-19) — a reordered listing now
  carries a line under the table naming its order: `Sorted by cpu, reversed.`
  `k8s::order::note` is one function over both listings' `Order` enums rather
  than a wording per table, and it takes the name from
  `clap::ValueEnum::to_possible_value`, so the note is the text the user typed
  after `--sort` — `cpu-requested`, never `CpuRequested` — and a renamed value
  cannot leave the note describing the old spelling. It is silent for the
  default order in its natural direction, which is what keeps every existing
  command's output unchanged to the byte; `--sort-reverse` on its own is *not*
  that case, because it reverses the default order and prints a Z-to-A listing
  that is the easiest of all to mistake for the default one. Joining the
  existing footnote list rather than growing a mechanism of its own also settles
  two questions for free: it sits under the notes about what went wrong, and it
  disappears on an empty listing, where "there is nothing here" is the only
  thing worth reading.

- **Sort the node table too** (2026-08-19) — `eks nodes --sort` takes `status`,
  `cpu`, `memory`, `cpu-requested`, `memory-requested`, and `age`, with
  `--sort-reverse` flipping any of them. The shared half moved up to
  `k8s::order`: `Direction`, `Rank`, and the one comparison that keeps ranked
  rows apart from unrankable ones, so the rule that a row with nothing to rank it
  by stays in the tail under either direction is now written down once and
  asserted on the primitive rather than on either listing's rows. The keys did
  not move — a node has no restart count — and they read differently on purpose:
  a node ranks by its *share* of allocatable, because a two-core node at 95% is
  closer to trouble than a sixty-four-core node burning twenty times as much at
  30%, while a pod ranks by the figure, having no denominator in its table. That
  gives node usage two kinds of blank, and `Rank`'s tiers carry them: a figure
  with no allocatable behind it sorts ahead of no figure at all. `NodeRow` gained
  `created_at` beside `age`, the pairing `PodRow` already had, so `age` ranks on
  an instant rather than on a rounded string.

- **More orderings for `--sort`, and a way to reverse one** (2026-08-19) —
  `--sort` gained `age`, `cpu`, and `memory`, and `--sort-reverse` flips any of
  them. The reversal is the part with a decision in it: it flips only the rows an
  ordering can rank, so a pod that has never restarted, or one metrics-server has
  not sampled, stays in the tail under either direction. Reversing the whole
  comparison would open every reversed listing on its blank rows, and "least CPU"
  asks which pod is idle rather than which pod nobody measured. Each ordering
  maps a row to a private `Rank` — either a key or a marker that there is nothing
  to rank — which is also what lets `restarts` keep its two distinct kinds of
  blank through a reversal. `PodRow` gained `created_at` beside `age`, the same
  pairing `last_restart` has with `restart_age`, so `age` sorts on an instant
  rather than on a rounded string.

- **Sort the pod listing by how recently it restarted** (2026-08-19) — `eks pods
  --sort restarts` puts the newest crash at the top. `k8s::pods::order` is a
  pure function over `PodRow`s with no clock and no cluster in its signature,
  which is what makes the awkward rankings fixtures: a restart the kubelet
  recorded no `finishedAt` for sorts between the dated restarts and the pods
  that have never restarted, because the count is real but there is no moment to
  rank it by. The count is only ever the tie-break — sorting by *how many* would
  put a pod that failed two hundred times last week above one that started
  crashing a minute ago. Every ordering is total, so one listing cannot render
  two ways.

- **How long ago the last restart was** (2026-08-18) — `eks pods` now prints
  `kubectl`'s `9 (5m ago)` rather than a bare `9`. The timestamp is the newest
  `lastState.terminated.finishedAt` across exactly the containers whose restart
  counts survived, so the two halves of the cell can never describe different
  sets: the assignment that drops a finished init container's restarts drops its
  date with them. A pod that has never restarted, and a restart the kubelet
  recorded no finish time for, both keep the bare count.

- **metrics-server integration for nodes** (2026-08-18) — `eks nodes` gained
  `CPU USE` and `MEM USE` columns from `metrics.k8s.io/v1beta1`, beside the
  request columns they should be read against. `k8s::metrics` hand-writes the
  `NodeMetrics` type `k8s-openapi` does not generate and puts the fetch behind a
  `Source` trait, so the case that matters — no metrics-server, which is every
  fresh EKS cluster — is a fixture rather than a cluster to uninstall from. That
  case costs two columns and a footnote naming what to install, never the table.
  The pod half of the original task was split into its own entry above.

- **Label and field selectors for `eks pods`** (2026-08-18) — `-l` and
  `--field-selector` push filtering onto the API server. `k8s::selector`
  reimplements both Kubernetes selector grammars as pure functions, so a
  mistyped selector is rejected with the offending text quoted before anything
  connects; an empty filtered listing names the selector rather than reading
  like an empty namespace. Set-based membership (`in`/`notin`), existence
  (`key`/`!key`), and `==`-folding are all covered.

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
