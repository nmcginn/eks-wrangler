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

- [x] **Usage against what the pod asked for.**
  `eks pods` now shows what a pod is burning, and the node table shows every
  figure as a share of something. A pod's `CPU`/`MEMORY` has no denominator, and
  the one that answers "is this limit wrong?" is the pod's own request — the
  number that decides whether it is throttled or about to be OOM-killed.
  *Acceptance:* the request comes from the existing `effective_requests`, so the
  two commands cannot disagree; a pod with no request shows the bare figure
  rather than a percentage of zero.
  Landed as one cell — `262m/500m (52%)` — with the heading following it:
  `CPU/REQ` over a column of pairs, plain `CPU` where no row in the listing has
  one, so the table says what the percentage is a share of.

- [ ] **What a pod asked for, when nothing has measured it.**
  A pod's request reaches the table only as the denominator of a usage figure, so
  on a cluster with no metrics-server — the EKS default — `eks pods` still says
  nothing about what anything booked. "What did this ask for?" is a different
  question from "how is it doing against it?", and it is asked of pods nobody has
  sampled: the `Pending` one, the one on a cluster with no add-ons installed.
  Separate because it is the reviewer's call, and it is a call *between* designs
  rather than an addition to this one: a `CPU REQ` column beside the existing
  `262m/500m (52%)` cell would print the same number twice, so taking this means
  either a column and a plainer usage cell — the node table's shape — or a column
  that appears only where the usage pair does not. It also puts two more columns
  on every listing, which is the default table's width — less of an objection now
  that `k8s::pods::row::DROP_ORDER` exists and a new column can say where it
  drops, but still a decision about what the default table is before it is one
  about what a narrow table keeps.
  A pod's *device* request is the same question with the same undecided answer
  and one extra reason to wait: `eks nodes` now shows what each node has booked
  of a GPU, so "which pod is holding the fourth card" is the follow-on question,
  and it wants whatever column shape this task settles rather than a GPU-shaped
  exception to it. `Requests::extended` already carries the figure.
  *Acceptance:* the figure is `effective_requests`'s, as the usage denominator
  already is; whichever shape it takes, no listing shows one pod's request twice;
  a device column on `eks pods` appears on the condition the node table's does —
  some row in this listing asked for one — rather than on a second rule.

- [ ] **Sort a pod listing by its share of what it asked for.**
  `--sort cpu` ranks the figure, which is the right key for "what is eating this
  node" and the wrong one for "whose request is wrong" — the question the new
  percentage puts in front of the reader. A pod at 400% of a 10m request is
  burning 40m; the ordering that finds it is not the one that finds the pod
  burning a core. Separate because it turns on a decision the reviewer should
  make rather than one this PR could guess: whether a share is two more `--sort`
  values (`cpu-share`, `memory-share`), a modifier that re-reads the existing
  ones, and whether the node orders — which already rank by share — should gain
  the opposite reading at the same time for symmetry.
  *Acceptance:* the unrankable tail is the pods with no request or no sample,
  under `k8s::order`'s existing rule rather than a second one; `--sort cpu` is
  unchanged.

- [x] **Usage against capacity for the dashboard's bars.**
  `nodes::Share` divides usage by *allocatable*, which is the right denominator
  for "will another pod fit". A utilisation bar is asking a different question —
  "is this machine busy" — and wants capacity underneath it, so a node at 100%
  of allocatable does not draw as a full bar when a tenth of the machine is
  still kubelet reserve.
  *Acceptance:* the choice of denominator is explicit at the call site rather
  than baked into `Share`; both readings are tested on one fixture node.

- [x] **Show the sampling window beside the usage columns, and say when there
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
  Landed on both listings, in one wording, as `Usage is up to 12s old, averaged
  over 20s.` — the age is the oldest sample so the line covers every row, and the
  window is the longest any sample reported. Past two windows it says the figures
  are stale and names the pod to look at. `metrics::Sample` carries the two
  stamps through the join, so the note dates the samples that reached the table
  rather than the ones the endpoint returned; `metrics::Outcome` makes "answered
  with nothing" the third case it always was.

- [ ] **A row whose sample is old, rather than a listing that is.**
  The freshness note is one line about the whole table, and it takes its age from
  the oldest sample in it — so a single node whose kubelet stopped reporting
  makes a listing of otherwise-current figures read as stale, and the reader has
  no way to tell which row dragged it. Discovered while writing the note, and
  separate because the fix is a shape this PR could only have guessed at: an age
  per row is a column, and the pod table is already the wider of the two
  listings, so it belongs with the narrow-mode decision rather than arriving
  ahead of it; a marker on the stale rows instead is the other design, and it
  needs `Severity` colour to exist before it reads as anything. The narrow-mode
  half of that is settled now that both tables have a drop order: a column
  taking this on arrives with a place in `DROP_ORDER` rather than as one more
  thing nothing can drop.
  *Acceptance:* whichever shape it takes, the staleness rule stays
  `Freshness::is_stale`'s rather than a second reading of "a couple of windows";
  a listing where every sample is current gains nothing.

- [x] **Carry the freshness and unsampled notes into the dashboard's panes.**
  The third of the notes that will want a pane's header rather than a footnote
  under a scrolling list, and it pairs with the two sort-note entries above. A
  pane refreshing in the background makes it matter more than it does on the
  command line, where a listing is as old as the moment you typed it: a pane
  whose figures stopped moving looks exactly like a pane on an idle cluster.
  *Acceptance:* the pane words both through `k8s::metrics::freshness_note` and
  `k8s::metrics::unsampled`, not a second wording, and asks
  `k8s::metrics::Outcome::of` which of the three cases it is in rather than
  testing the request result itself.

### Follow-ups from the capacity columns

- [x] **Extended resources in the node table.**
  GPUs and other device-plugin resources (`nvidia.com/gpu`) are parsed correctly
  but never shown; a node table that hides the reason a pod will not schedule is
  doing half a job.
  *Acceptance:* a column appears only when some node in the listing reports the
  resource, so a CPU-only cluster gains no empty columns.
  Landed as one column per resource, `2/4 (50%)` — booked over allocatable —
  with `k8s::resource::is_extended` deciding which names qualify by Kubernetes'
  own rule, so `hugepages-2Mi` and the `attachable-volumes-*` limits beside it
  are left alone. `pods::Requests` grew a map so the scheduler arithmetic that
  totals CPU totals devices too. A node that does not report the resource reads
  `-` rather than `0/4`: no such hardware is a different answer from none free.
  The count alone would have hidden the case the column exists for — a card the
  kubelet has and will not offer — so `devices_withheld` names the node with the
  widest gap and says to check its device plugin.

- [ ] **Sort the node table by an extended resource.**
  `eks nodes --sort cpu` finds the node closest to full and there is no way to
  ask the same of the GPU column, which on a training cluster is the only column
  anyone is reading. Separate because it turns on a decision the reviewer should
  make rather than one this PR could guess: `--sort` is a `clap::ValueEnum` on
  the domain type (decision 28) precisely so a bad value is rejected with the
  valid ones listed, and a resource name is not one of a fixed set — it is
  whatever the cluster invented, and is not known until the nodes have been
  fetched, which is after the flag has been parsed. Taking this means either a
  free-form `--sort` value validated against the rows instead of by `clap`, or a
  second flag, and the answer applies to `eks pods` at the same time.
  *Acceptance:* whichever shape it takes, the ordering ranks by share as the
  other node orders do, and a node that does not report the resource sorts into
  the unrankable tail under `k8s::order`'s existing rule rather than as a zero.

- [ ] **Native resources that still have no column: `ephemeral-storage` and
  `hugepages-*`.**
  `is_extended` names them explicitly as the things a device column is *not*,
  which makes their absence a decision rather than an oversight. `pods` has left
  this list — the pod-count task below gave it a heading and a denominator of its
  own. Separate because each of the two remaining wants a heading and a
  denominator a reader recognises rather than the device treatment:
  `ephemeral-storage` is a capacity pair like memory, `hugepages-2Mi` reads `0`
  on almost every node and so wants the `any`-not-`all` condition, and whether a
  cluster that uses none of them should see any of this is the question. Not
  urgent: neither was ever visible.
  *Acceptance:* each column appears under a stated condition and reads in the
  units of the thing it counts, not as a device count.

- [x] **A narrow mode for the node table.**
  `eks nodes` is now ten columns and around 140 characters wide on a cluster
  with metrics-server, which wraps on an 80-column terminal. The request and
  usage columns made this worse, and they are also the ones most worth keeping
  when space is short. `format::Width` now exists as the other end of this —
  `--wide` is the opt-in direction — and `k8s::nodes::columns` is the one place
  the column set is decided, so this is a third variant and a drop order rather
  than a new mechanism.
  The device columns are the newest thing it has to place, and the hardest: a
  `NVIDIA.COM/GPU` heading is fourteen characters over a cell of nine, and it is
  the column nobody on a GPU cluster wants dropped.
  *Acceptance:* columns are dropped in a documented order to fit the terminal;
  the choice is a pure function over an available width, tested at 80, 100, and
  1 column.
  Landed as `Width::Narrow(u16)`, applied when stdout is a terminal and the
  user did not type `--wide`. A pipe is not a "narrow terminal": stdout piped
  gets the default table, byte for byte, so scripts parsing it do not break.
  The drop order for nodes is a `DROP_ORDER` list of predicates in `k8s::nodes`
  — VERSION, then AGE, then the REQ pair, then the USE pair, then CPU+MEMORY,
  then every device column, then STATUS — and NAME never drops. Devices go
  after the pair columns rather than before, so a GPU cluster keeps `GPU`
  even after `CPU` has left; that was the reviewer's warning the task's
  wording carried.

- [x] **A narrow mode for the pod table.**
  `Width::Narrow(u16)` now exists on both listings, and the pod-table code path
  treats it as `Default` — a Narrow variant reaches `k8s::pods::row::columns`
  and falls through the `is_wide()` check. The pod table is the wider of the
  two on a cluster with metrics-server and `--wide`, so it wants the same
  treatment, but it wants its own drop order: NAMESPACE, IP, READINESS GATES,
  and NOMINATED NODE all live there and nothing about node-table's list applies.
  The main-loop plumbing is done: `stdout_terminal_cols` is already passed to
  both listings.
  *Acceptance:* the drop rule lives in `k8s::pods::row` beside `columns`,
  matching the node table's shape; a piped `eks pods` is unchanged to the
  byte; the choice is a pure function over an available width, tested at 80,
  100, and 1 column.
  Landed as `k8s::pods::row::DROP_ORDER`: AGE, NODE, the usage pair, RESTARTS,
  READY, STATUS, with NAME never dropped and NAMESPACE never dropped either —
  under `-A` the pair is the pod's identity rather than a column beside its
  name. The wide columns the task worried about turned out not to be a
  question: `--wide` wins at the type gate, so a narrowed listing never carried
  IP, NOMINATED NODE, or READINESS GATES to drop, and the order the task asked
  the reviewer to settle is an order over the default columns only. AGE goes
  first because RESTARTS already says `9 (5m ago)`; NODE second because it is
  the widest cell in the table on EKS and answers the question you ask after
  you have found the pod. The measurement both tables narrow by is now
  `format::column_widths` and `format::row_width`, the pair the renderer itself
  pads and separates by, rather than a copy of its arithmetic per table.

- [ ] **A sort note that names a column the terminal dropped.**
  `eks nodes --sort cpu` and `eks pods --sort cpu` print `Sorted by cpu.` under
  the table, and on a terminal narrow enough the column that ordering ranks is
  one of the ones the drop rule took away — so the listing names an ordering
  over a column the reader cannot see, which is the complaint the "nothing
  ranked" note was written to answer in the other direction. True of `eks nodes`
  since narrow mode landed there and of `eks pods` from tonight; noticed while
  writing the pod drop order. Separate because it is one answer for both tables
  and the answer is the reviewer's: either the ordering's column is exempt from
  `DROP_ORDER` — which means deciding what `--sort cpu` protects on a node,
  where `CPU USE` cannot stay without the `CPU` it is a share of, and so drags a
  second column back into a row that did not fit — or the note says the column
  is hidden and the drop rule stays as it is. The two read very differently on
  an 80-column terminal, and building either would be guessing at which.
  The column-naming footnotes have the same fault and want the same answer:
  `nodes::requests_unavailable` says which columns a failed pod listing emptied,
  and on a narrow terminal some of those are columns the drop rule already took
  away. True of `CPU REQ` and the device columns since narrow mode landed, and
  of `PODS` from the night it arrived — which is the night it stopped being
  hypothetical, since `PODS` is on every listing where a device column is not.
  *Acceptance:* whichever shape it takes, one wording and one rule for both
  listings, through `k8s::order::note` as now; the same answer reaches
  `requests_unavailable`, which knows the rows but not the width today; a
  listing wide enough to keep every column says exactly what it says today.

### Follow-ups from the request columns

- [x] **A pod count per node.**
  The request totals are computed from a full pod listing that is then thrown
  away; `PODS` is one more column and the number people ask for next, alongside
  the node's `maxPods` limit, which is the *other* reason a pod will not
  schedule.
  *Acceptance:* the count excludes finished pods, matching the requests total it
  sits beside; `maxPods` comes from the node's `allocatable["pods"]`.
  Landed as `12/58 (21%)`, the device columns' shape, because the limit varies
  by instance type and by CNI configuration and a bare percentage names a
  fraction of a number nobody knows. `pods::by_node` returns a `Placed` — the
  count beside the totals, out of one walk — so the two halves cannot be about
  different sets of pods, and one failed pod listing empties both.
  `Quantity::from_count` makes the count the same type as the allocatable it
  divides by, which buys `Share`'s ratio, thresholds, cell, and sort key
  without a second division. `--sort pods` came with it, ranking the share as
  the other node orders do.

- [x] **Paginate the pod listing.**
  `eks nodes` now fetches every pod in the cluster in one request to total the
  requests, and `eks pods -A` does the same to list them — twice over now, since
  the metrics listing beside it is unpaged too. On a large cluster that is the
  biggest response the tool asks for, and it shares the paging problem the node
  listing has.
  *Acceptance:* pages with the same tested continue-token function the node
  listing uses; the two listings still run concurrently.
  Landed with the node half, through one `k8s::page::collect`.

- [x] **Severity colour in the CLI table.**
  `nodes::Share::severity` classifies each percentage on the shared thresholds,
  and the CLI table then prints it in plain text. A node at 97% deserves to look
  like it, at least when stdout is a terminal. `PodRow::severity` is now in the
  same position — a `CrashLoopBackOff` reads exactly like a `Running` — so one
  change should light up both tables.
  The pod table's `262m/500m (52%)` is the third figure wanting this and the one
  that needs a decision first: it carries no `Severity` deliberately, because
  `Severity::from_utilisation`'s thresholds do not transfer — 90% of a node's
  allocatable is alarming and 90% of a pod's own request is a well-sized pod. Say
  what "hot" means for a pod against its request before colouring it.
  *Acceptance:* colour is suppressed when stdout is not a TTY and when `NO_COLOR`
  is set; the decision is a pure function, tested both ways.
  Landed on both tables, with `Severity::Ok` deliberately *uncoloured*: a table
  is ink on a line rather than a bar with a fill, and a healthy cluster is nearly
  every cell, so painting all of them green would spend the strongest signal a
  terminal has on the rows with nothing to say. `Theme::severity_ink` is that
  second mapping — `None` for `Ok`, warning, danger, and muted for the rest — and
  it decides only how a severity is drawn, never which one it is; a test asserts
  it agrees with the dashboard's variant for variant. `format::Cell` carries the
  severity beside the text so a drop rule cannot `retain` the two out of step,
  and every width is measured from the text, so a coloured table and a plain one
  have their columns in the same places at every narrow width. `--color` came
  with it — `auto`, `always`, `never`, global, and `--colour` too — because
  `NO_COLOR` and a TTY check between them have no way to get colour into a
  pager. The pod usage pair stayed uncoloured, as the wording above asked; it is
  the first entry below.

### Follow-ups from the CLI colour

- [ ] **What "hot" means for a pod against its own request.**
  `eks pods` now colours `STATUS` and nothing else. `CPU/REQ` and `MEMORY/REQ`
  carry a percentage and no `Severity`, because `Severity::from_utilisation`'s
  thresholds are about a node's allocatable: 90% booked is nearly full there, and
  a pod at 90% of the CPU it asked for is a well-sized pod. Colouring them on the
  node's numbers would tell the reader something untrue, in red, on most of their
  rows. Separate because it is the decision the entry above deliberately left
  standing, and it is a decision rather than an implementation: whether a share
  of a request has thresholds at all, whether the interesting direction is *over*
  the request (throttled, about to be OOM-killed) rather than merely high, and
  whether a limit — which this tool does not read yet — is the denominator that
  would make the question answerable. "Sort a pod listing by its share of what it
  asked for", above, is the same question asked of an ordering rather than of a
  colour, and one answer should settle both.
  *Acceptance:* whatever the rule is, it is a function beside
  `Severity::from_utilisation` rather than a second set of numbers at the call
  site, and `Column::severity` in `k8s::pods::row` reads it; the node columns are
  unchanged.

- [ ] **Whether anything in a table with no severities deserves colour.**
  `--color` is global, and on `eks contexts` it has nothing to do: none of that
  table's cells is a reading off a cluster. The one mark that does single a row
  out is the `*` gutter, which the dashboard already draws in
  `Severity::Ok` green — so the tool is inconsistent about the same marker across
  two surfaces. Discovered while threading the palette through the third caller
  of `format::table`. Separate because the answer is a design question this
  change had no business guessing: a selection marker is not a severity, and
  deciding it should be green pulls in the wider question of whether headings,
  the active row, or a cluster's name are colour's business at all — which is the
  light-theme task's territory as much as this one's.
  *Acceptance:* whatever it turns into, `eks contexts` goes through the palette
  it is given rather than a hardcoded `Palette::Plain`; a listing under
  `--color never` is unchanged to the byte.

### Follow-ups from the client bootstrap

- [x] **Paginate node listings.**
  `eks nodes` fetches every node in one request. A cluster with thousands of
  nodes deserves `limit`/`continue` paging, and eventually a spinner while the
  pages arrive.
  *Acceptance:* paging is driven by a tested pure function over the continue
  token; a fixture with three pages is covered.
  The spinner half became its own entry below; it is a surface this tool does
  not have yet rather than the rest of this task.

- [ ] **Say that a listing is still arriving.**
  Paging turned a slow listing into several requests, and `eks nodes` on a very
  large cluster now spends that time as silently as it spent it before. A
  spinner, or a `read 1,500 nodes…` counter on stderr, was the other half of the
  paging task. The credential helper is the step that wants this most, and it
  wants it first: it now runs under the same budget as a request, and a user who
  waits thirty seconds for `aws eks get-token` has no way to know that is what
  they are waiting for until it fails. Separate because it is the first thing this tool would ever write
  to a terminal mid-command. Half of what it was waiting on is now answered: the
  severity-colour task settled what `eks` does when stdout is a pipe — nothing
  that was not there before, byte for byte — and `theme::Palette` and
  `--color` are the mechanism, so `Palette::is_colour` is a ready-made
  "is anyone watching this". What is still open is the other half, and it is a
  decision rather than a gap: this writes to *stderr*, where that answer does not
  transfer — a piped table with a spinner still on the terminal beside it is
  fine, and possibly the point — and whether `NO_COLOR` is the switch for
  movement as well as for ink is nobody's settled convention.
  *Acceptance:* nothing is written when stdout is not a terminal, so a piped
  listing is unchanged to the byte; the progress line goes to stderr and is
  erased before the table is printed.

- [x] **A timeout that covers the credential helper.**
  `--timeout` bounds requests to the cluster, and cannot bound what happens
  before the first one: `kube` resolves a kubeconfig's auth eagerly and runs the
  exec plugin with a blocking `std::process::Command`, so an `aws eks get-token`
  that hangs — a laptop that has lost its route to the SSO endpoint — blocks the
  thread rather than the future, and a `tokio::time::timeout` around
  `k8s::connect` would never fire. Discovered while writing the flag, and
  separate because the fix is a different mechanism on a different surface, not
  a wider version of this one: the helper has to run on a blocking task, and
  abandoning one does not kill the subprocess, so the runtime then has to be
  shut down without waiting for it. It also wants its own message — a hung
  credential helper is advice about the AWS CLI, not about a VPC.
  *Acceptance:* `eks nodes --timeout 5s` gives up in five seconds against a
  credential helper that never exits, and the process exits rather than waiting
  for it; the message names the helper rather than the API server.
  Landed as the two halves the wording predicted. `Client::try_from` — which is
  where `kube` runs the exec plugin — moves onto `tokio::task::spawn_blocking`,
  so the budget races a `JoinHandle` rather than a thread sitting in `waitpid`;
  and `commands::block_on` calls `Runtime::shutdown_background` rather than
  dropping the runtime, because dropping one waits for exactly the blocking task
  that was just given up on. `k8s::client::stalled_helper` is the message, beside
  `explain` rather than inside it since no request failed, and it names the
  command out of the kubeconfig's own `exec` block — `helper_command`, quoted so
  the user can paste it, because running it by hand is the only way to see what
  it is waiting on. The helper is spent per step like every request after it, and
  the message names `--timeout 0` as the way back to the old behaviour, which is
  the answer for a helper that is legitimately waiting on a human.

- [ ] **Stop the credential helper, rather than only stopping waiting for it.**
  `--timeout` now ends the hang, and it ends it by abandonment: a blocking task
  cannot be cancelled, so `eks` prints its message and exits while the `aws eks
  get-token` it started keeps running. Usually harmless — the orphan exits or
  dies with its network — but a helper `kube` left interactive still holds the
  terminal's stdin, so it can sit there taking keystrokes from the shell that
  gets its prompt back. Discovered while writing the abandonment, and separate
  because the only way to kill a child is to own it: that means running the
  `exec` block ourselves against the `client.authentication.k8s.io` protocol —
  token and client-certificate credentials both — and handing `kube` a resolved
  `Config` rather than a kubeconfig. That is a night on its own, and it turns on
  a question this change had no business answering: whether `eks` wants to own
  credential resolution at all, which is also what decides whether a token can
  be refreshed partway through a paged listing.
  *Acceptance:* the helper is gone by the time the process is; whatever runs it,
  the failure still words itself through `k8s::client::stalled_helper` and names
  the command through `helper_command` rather than a second spelling.

- [ ] **Make a listing's footnotes a pure function.**
  Every footnote's *wording* is a tested pure function; the list they are
  assembled into is not, because assembly happens inside `commands::nodes::list`
  between the requests, and that function needs a cluster. So the order the
  notes come out in — which matters, since one of them points at "the reason
  above" — is guaranteed by reading ten lines rather than by a test. Noticed
  while adding a fifth note to that list and having to hold two failures back
  until the rows existed. Separate because it is the same shape of work on
  `commands::pods::list`, which assembles its own, and because the parameter
  list is the design question: a function taking eight arguments is not obviously
  better than the ten lines it replaces, and the alternative — a small
  `Footnotes` builder both commands push into — is the reviewer's call.
  *Acceptance:* the order of the assembled notes is asserted in a test with no
  client; both listings assemble through the same thing.

- [x] **Move `eks contexts` onto the shared table renderer.**
  `format::table` now renders `eks nodes`; `commands::contexts` still has its
  own copy, which also owns the `*` active-cluster gutter.
  *Acceptance:* one renderer, existing `contexts` output unchanged to the byte.
  The gutter stayed behind: it is not a column, and a padded one would read
  `*  prod`.

- [x] **Global flags before a subcommand.**
  `eks --kubeconfig x contexts` is rejected because `args_conflicts_with_subcommands`
  treats the flag as the bare-dashboard form. Flags after the subcommand work,
  which makes the failure look arbitrary.
  *Acceptance:* both orders parse; a test covers each global flag in both
  positions.

- [x] **A `--timeout` for cluster requests.**
  A hung API server currently leaves `eks nodes` waiting forever with no way out
  but Ctrl-C.
  *Acceptance:* the default is documented; timing out names the cluster and
  suggests checking VPN or endpoint access.
  Spent per request rather than per command, because paging made a listing
  several of them. What it could not cover at first was the credential helper;
  that was its own entry, and is now ticked above.

## Milestone 2 — The dashboard

- [x] **Node pane with live data.**
  Replace the placeholder overview with a node list: utilisation bars, pod
  counts, conditions.
  *Acceptance:* first paint happens before data arrives; loading state is
  visible; `TestBackend` tests for loading, loaded, and error states.

- [x] **Background refresh.**
  Move fetching onto a background task feeding the UI over a channel. Refresh on
  an interval and on demand (`r`).
  *Acceptance:* the render loop never awaits a network call; a hung API call
  leaves the UI fully navigable; refresh interval is configurable; the fetches
  go through `k8s::page::collect` with the same `Budget` the CLI uses, so a
  request that never answers ends rather than pinning a background task for the
  life of the session.
  The node pane's one-shot fetch (`commands::spawn`, `commands::nodes::gather`,
  `App::apply_nodes`) is the mechanism this builds on rather than replaces.
  It also has to settle the case the one-shot fetch left open: switching the
  sidebar to a different cluster does not refetch today, so the node pane
  keeps showing whichever cluster's nodes were fetched at startup even after
  the "Overview" summary above it has moved to a new context. Whether a
  selection change should trigger a fetch immediately, wait for the interval,
  or something in between is exactly the kind of trigger this task already
  has to design for `r`.

- [x] **Pod browsing: drill from a node into its pods.**
  `Enter` on a highlighted node opens the pods placed on it; `Esc` backs out to
  the node list rather than quitting, and only quits once there is nowhere
  left to back out to. Breadcrumbs are the detail pane's own title: `
  Overview › <node> `.
  *Acceptance:* every view is reachable and escapable by keyboard alone;
  navigation state transitions are unit-tested without a terminal.
  Landed with a decision the task's wording had not settled: the node list had
  no selection of its own before now, so drilling into a row needed a focus
  model between the sidebar and the detail pane first. `Tab` toggles focus
  between `Focus::Sidebar` and `Focus::Detail`; `j`/`k`/`Home`/`End` move
  whichever one is focused, and the focused pane's border switches to
  `Theme::pane_border`'s focus colour, the mechanism the sidebar already had
  and the detail pane had never used. `App::view` is a `View` enum rather than
  a stack, since there is exactly one level of drill-down today; a pod's
  containers is the next one and the natural place it grows into a `Vec<View>`
  instead of a third variant. Split from the rest of the original task at the
  seam its own wording drew: a pod's containers and a namespace filter are
  both a second fetch this change did not need to build, so they are their own
  entries below rather than a guess at their shape.

- [ ] **Drill from a pod into its containers.**
  The other half of "Pod browsing", split out because it needs a second fetch
  the node-drilldown pane does not: containers, images, and restart reasons
  live nowhere in `PodRow` yet, which is also what "Pod detail view" below is
  waiting on. `View` gains a sibling to `NodePods` here, and `App::view` most
  likely becomes a small stack rather than a two-variant enum once there are
  two levels to back out of one at a time.
  *Acceptance:* `Esc` backs out one level at a time rather than straight to
  `Overview`; the same focus model drills in and out without a second
  mechanism.

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

### Follow-ups from the node pane

- [ ] **A node's full condition list, not just its derived status.**
  The node pane reads "conditions" as `NodeRow::status`/`severity` — the same
  derived `Ready`/`NotReady`/`Unknown`[,`SchedulingDisabled`] the CLI's
  `STATUS` column shows — because that is the data `NodeRow` already carries.
  `MemoryPressure`, `DiskPressure`, `PIDPressure`, and `NetworkUnavailable`
  are a different, wider fact about a node that nothing in this tool parses or
  shows yet. Separate because it is new data-model surface on `NodeRow`
  neither listing has today, and because four more conditions do not fit a
  one-line row — it wants a detail view for the selected node, and `Enter` on
  one now drills into its pods rather than such a view. Whether a node's own
  detail belongs behind a different key, or the pods list gains a header
  section for it, is the same kind of question "Pod browsing" left the
  reviewer once already.
  *Acceptance:* whichever shape it takes, the condition list is a pure
  function over a `Node`'s `status.conditions`, tested the way `status_text`
  and `severity` already are; a node reporting none of the four pressure
  conditions is not distinguished from one that has not reported at all.

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

- **Pod browsing: drill from a node into its pods** (2026-08-22) — `Enter` on
  a highlighted node in the `Overview` pane opens the pods placed on it;
  `Esc` backs out to the node list, and only quits the dashboard once there
  is nowhere left to back out to. The block's own title carries the
  breadcrumb: ` Overview › worker-3 `. This needed a focus model the
  dashboard did not have before tonight — the node list had no selection of
  its own, only the sidebar did — so `Tab` now toggles `Focus` between
  `Sidebar` and `Detail`, `j`/`k`/`Home`/`End` move whichever one is
  focused, and the focused pane's border switches to
  `Theme::pane_border`'s existing focus colour, which only the sidebar used
  before. `App::view` is a two-variant `View` rather than a stack, since a
  pod's containers — the next drill-down level the roadmap asks for — is
  the natural point to grow it into a `Vec<View>` instead of a third case
  guessed at ahead of time. Fetching is
  `commands::pods::spawn_gather_for_node`, filtering on `spec.nodeName`
  across every namespace; unlike the
  node pane it does not refresh in the background or carry usage figures,
  both left as follow-ups once a second pane exists to weigh them against.
  See decision 56.

- **Background refresh** (2026-08-22) — the node pane no longer fetches once
  at startup and stops: `--refresh` (default `15s`, `0` to disable) starts a
  new fetch on an interval, `r` refreshes on demand, and selecting a
  different cluster in the sidebar refetches immediately rather than leaving
  the previous cluster's rows on screen under the new one's name. `main`
  builds one `spawn_nodes` closure over the config, kubeconfig paths, and
  `--timeout` budget, and hands it to `ui::run` alongside the same
  `commands::nodes::spawn_gather` receiver it always started before the
  terminal took over — every fetch after that first one goes through the
  closure instead of a second mechanism. The render loop still never awaits a
  network call: it polls the channel non-blockingly once a frame, exactly as
  the one-shot fetch did. `App::start_loading_nodes` is the new pure
  transition for a selection change; `r` and the interval deliberately do not
  call it, so the pane keeps showing its current rows while a fetch for the
  *same* cluster is in flight rather than flashing back to `Loading` every
  refresh. `NodesState::Loaded` gained `refresh_error`, because that same
  choice means a failure can no longer be presented as "nothing loaded" —
  `App::apply_nodes` now only moves to `NodesState::Error` when nothing had
  loaded yet; a failure after a successful fetch keeps the last good rows and
  notes the failure under the heading instead of blanking a working pane over
  one bad poll. `RefreshInterval` wraps `k8s::page::Budget` for its grammar
  and round trip rather than growing a second duration parser, staying its
  own type because `0` means something different for the two flags. See
  decision 55.

- **Carry the freshness and unsampled notes into the dashboard's panes**
  (2026-08-22) — the node pane now says how old its usage bars are, in a line
  under the `NODES` heading: `Usage is up to 8s old, averaged over 20s.`, or
  the stale wording past two windows, or a sentence saying metrics-server has
  not sampled anything here yet. `k8s::nodes::usage_note` is the pane's own
  reading of the same three-way `k8s::metrics::Outcome` the CLI table already
  classifies its footnotes by, worded through `metrics::freshness_note` and
  `metrics::unsampled` directly rather than the CLI's `usage_unsampled` —
  that wrapper names `CPU USE`/`MEM USE`, columns the pane's bars do not have
  headings for. A failed metrics read is left silent, out of scope for this
  task: the pane has no footnote list yet to hand `usage_unavailable`'s
  explanation to, and every bar already reads `-`. `commands::nodes::gather`
  is unchanged; `spawn_gather` now reduces it to a `NodesFetch { rows,
  usage_note }` instead of a bare `Vec<NodeRow>`, so the pane and the CLI
  table read one classification off one fetch rather than two. See decision
  54.

- **Usage against capacity for the dashboard's bars** (2026-08-22) — the node
  pane's CPU and memory bars now fill and colour against the node's raw
  `capacity`, not the `allocatable` the CLI's `CPU USE`/`MEM USE` columns
  divide by: a node pinned at 100% of allocatable no longer draws a full bar
  when a slice of the machine is still kubelet reserve nothing can schedule
  into. `nodes::Share` gained `ratio_of`/`severity_of`, taking the denominator
  as a parameter rather than always reading `self.allocatable`, so the choice
  is explicit at each call site instead of baked into the type; `ratio()` and
  `severity()` are unchanged wrappers, and every existing reading — the CLI
  table, the request columns — is unaffected. `ui::nodes::bar` is the one
  caller so far that asks for the capacity reading, passing the node's
  `Capacity` alongside the `Share` it already had. See decision 53: this
  deliberately reopens decision 52, which had picked allocatable for the bar
  too to keep the two surfaces in agreement, and the tradeoff is worth a
  reviewer's second look.

- **Node pane with live data** (2026-08-21) — the dashboard's "Overview" pane
  grows a real node list under the cluster summary: name, status, a CPU and a
  memory utilisation bar, and a pod count. `commands::nodes::gather` is the new
  seam underneath both `eks nodes` and the pane — resolve the cluster, fetch
  nodes/pods/metrics concurrently, reduce to `NodeRow`s — so `list` and the
  pane's `spawn_gather` build rows through the one function and cannot drift
  about what a node's row means; only the renderers differ, an ANSI table for
  one and ratatui bars for the other. The fetch itself never touches the
  render loop: `commands::spawn` runs it on a plain OS thread with its own
  current-thread `tokio` runtime, shut down rather than joined for the same
  reason `commands::block_on` is (decision 51), and delivers its result over an
  `mpsc::Receiver` that `main` hands to `ui::run`. The loop polls it
  non-blockingly once per iteration, ahead of drawing, so first paint always
  shows `Loading nodes…` rather than waiting on the network, and a hung
  request leaves every key fully live. `App` gained one field and one pure
  transition, `apply_nodes`, tested the same way `on_key` is, with no channel
  or thread anywhere near a test. The bars divide by allocatable, the same
  denominator the CLI's `CPU USE`/`MEM USE` already do (decision 52), so the
  pane and the table never disagree about one node.

- **A timeout that covers the credential helper** (2026-08-21) — `--timeout` now
  bounds the step before the first request as well as every request after it, so
  an `aws eks get-token` that never comes back ends in a sentence rather than in
  Ctrl-C. `kube` runs the exec plugin inside `Client::try_from`, with a blocking
  `std::process::Command`, on whatever thread asked — so a timeout wrapped around
  `k8s::connect` compiled and did nothing, with the timer and the future it was
  racing on the same thread and the thread in `waitpid`. The build moves onto
  `tokio::task::spawn_blocking` and the budget races the `JoinHandle` instead.
  A blocking task cannot be cancelled, so giving up on one only stops waiting for
  it: `commands::block_on` therefore shuts its runtime down rather than dropping
  it, since dropping one waits for precisely the task that was abandoned — the
  same hang, one frame later. The message is its own, because `Failure::Slow`
  talks about VPCs and VPNs and none of that is true of a subprocess on the
  user's laptop; it names the command out of the kubeconfig's `exec` block,
  quoted so it can be pasted, and does not guess which of the several things that
  hang `aws` is the one hanging this. `--timeout 0` is named in the sentence, as
  the answer for a helper that is legitimately waiting on a human.

- **Paginate node listings**, **Paginate the pod listing** (2026-08-20) — every
  listing now goes through `k8s::page::collect`, which asks for 500 objects at a
  time and follows the `continue` token: the node listing, both pod listings,
  and both metrics endpoints. An ordinary cluster is unchanged, because a first
  page that comes back short carries no token and ends the listing after the one
  request it always took. The loop is four lines of I/O and every decision in it
  is outside it — `page::Listing` holds the items, remembers the token the last
  request carried, and answers `Page`/`Done`/`Stalled` — so three pages, an
  empty listing, and a `"continue": ""` are fixtures rather than a cluster
  somebody has to grow. `Stalled` is the case Kubernetes never produces and the
  only one that could hang the tool for ever: a server handing back the token it
  was given would page over the same objects until Ctrl-C, so it ends the
  listing with a warning instead. The two metrics listings do not chunk —
  metrics-server ignores `limit` and answers in one go — and go through the same
  door anyway, because what that buys them is the request budget below.

- **A `--timeout` for cluster requests** (2026-08-20) — `--timeout 30s` by
  default, `0` to wait for as long as it takes, and a message that names the
  budget it overran and the larger one to type: `did not answer within 5s … allow
  it longer: --timeout 10s`. It is spent per request rather than per command,
  which paging decided: a listing is several requests now, and a cluster large
  enough to need four pages would otherwise be cut off for its size rather than
  for being unreachable. `Budget` therefore lives in `k8s::page`, and prints a
  spelling it can parse back, so the advice can never suggest a value the flag
  would reject. Two failures that were raw HTTP became sentences on the way: a
  `410` is a page marker that expired mid-listing, which wants "run it again"
  rather than anything about credentials. What the flag could not cover on the
  night it landed was the credential helper, which `kube` runs with a blocking
  `std::process::Command` — its own entry then, and its own entry in this list
  now.

- **Global flags before a subcommand** (2026-08-20) — `eks --context prod nodes`
  parses, as `eks nodes --context prod` always did. `args_conflicts_with_subcommands`
  was buying nothing: every argument this parser has at the top level is a global
  one, meant to be legal beside a subcommand, so the setting only made the flag
  order arbitrary. Each global flag is now asserted in both positions, including
  the new `--timeout`.

- **Move `eks contexts` onto the shared table renderer** (2026-08-20) — one set
  of column-width rules in the tool rather than two, with the output asserted in
  full rather than probed, since "unchanged to the byte" was the whole
  requirement. The `*` gutter did not move into `format::table`: it is not a
  column — a padded one would print `*  prod`, since every column carries its
  own two-space separator — and no other listing has a row that is more current
  than its neighbours, so the table is rendered without it and each line is
  prefixed afterwards.

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
