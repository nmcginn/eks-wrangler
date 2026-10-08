# Decisions

Short records of choices that would otherwise get re-litigated. Each says what
was decided and why, in a few lines; the PR that landed it has the detail.
Append as they are made; amend rather than delete when one is reversed. Code
comments cite these by number, so numbers are never reused or renumbered.

---

### 1. Rust, with `ratatui` for the interface

Startup time is a feature, and a GC runtime makes a sub-50 ms budget a fight.
Rust ships one static binary. `ratatui`'s `TestBackend` lets rendering be
unit-tested.

### 2. Thin binary, fat library

`main.rs` parses arguments and dispatches; everything else lives in the library,
because code in a binary crate is awkward to test.

### 3. Panics are denied by lint

`unwrap`, `expect`, and `panic!` are `deny`: a panic in a TUI leaves the terminal
in raw mode. Tests opt out locally.

### 4. Kubeconfig writes go through the untyped YAML tree

Reads use our typed view; writes mutate `serde_yaml_ng::Value` in place, touching
only `current-context`, so exec plugins, extensions and proxy settings we don't
model survive. Writes go to a sibling temp file and are renamed into place.

### 5. Show cluster names, not ARNs

`ClusterView` derives short name, region and account from the ARN; the UI shows
`prod (us-east-1)`. `eks use` accepts either, and refuses to guess when a short
name is ambiguous.

### 6. No async runtime until something awaits

`tokio` was removed from the scaffold until the first task that needed it (the
Kubernetes client) added it back deliberately.

### 7. `serde_yaml_ng` instead of `serde_yaml`

`serde_yaml` is archived; `serde_yaml_ng` is the maintained fork with the same API.

### 8. One reviewable pull request per night

Work lands as one nightly PR a human reviews over coffee, keeping `master`
releasable and a human on every change. *Amended twice:* line-count targets
(200–500, then 200–400) made the loop split on size instead of at a seam. There
is no line target; a PR is sized by being a complete change. See `CLAUDE.md`.

### 9. The async runtime is built per command, not around `main`

`commands::block_on` builds a current-thread runtime only for commands that talk
to a cluster; `eks contexts`/`eks use` never pay for one.

### 10. `kube` with `rustls`, `ring`, and `http-proxy`

`rustls` keeps the binary static. `ring` must be named explicitly or `rustls`
panics at the first handshake. `http-proxy` because `kube` refuses to build a
client when `HTTPS_PROXY` is set without it.

### 11. Cluster failures are translated at the boundary

`k8s::client::explain` turns a `kube` error into a sentence naming the cluster
and the next action; the raw error goes to `tracing::debug`. Kinds stay coarse:
an arm earns its place only with advice worth printing. The default log filter
silences `kube_client`, which logs failures at ERROR above our message.

### 12. Times come from `jiff`, via `k8s-openapi`

Use `k8s-openapi`'s `jiff` re-export so the version can't drift. The `latest`
feature is fine because the fields read are old and optional; pin `v1_NN` if we
ever need a newer-only API.

### 13. Quantities are integer thousandths in an `i128`

`Quantity` holds thousandths of a unit so values round-trip exactly; `i128`
because thousandths of an exbibyte overflow `i64`. Sub-thousandth values round.
Capital `K` is rejected (not in the grammar); an unrepresentable number is
`TooLarge`, not `Malformed`.

### 14. Memory is shown in binary units, unlike `kubectl`

`quantity::memory` picks the largest binary unit with one decimal (`6.8Gi`),
always binary. The node table shows `allocatable/capacity` in one cell so the
kubelet reservation is visible.

### 15. Pod requests follow the scheduler, not the obvious sum

`effective_requests` = max(app containers + sidecars, peak init container) +
overhead, per resource. A sidecar counts in every init container that starts
after it, not before (pinned by a test). Terminating pods still count.

### 16. A failed pod listing empties two columns rather than the command

Node and pod listings run concurrently. A failed node listing is fatal; a failed
pod listing leaves `CPU REQ`/`MEM REQ` as `-` with a footnote (RBAC often allows
nodes but not all pods). `-` and `0 (0%)` stay visibly different.

### 17. `eks pods` reimplements `kubectl`'s STATUS derivation, faithfully

People read STATUS by habit, so we copy kubectl's order-dependent walk,
including its quirks, each tested. Severity lists the calm and settling words and
treats everything else as a failure, so unknown reasons show as problems. An
empty status prints `Unknown`.

### 18. `eks pods` lists finished pods; the node totals do not

`fetch` (node totals) filters terminal phases server-side; `fetch_scope` (the
listing) keeps them. `-n` with `-A` is an error, not silent override. A `403` on
`-A` suggests `-n <namespace>`.

### 19. Selectors are parsed here, not handed to the API server raw

`k8s::selector` validates `-l`/`--field-selector` before connecting, so a typo is
an instant local error quoting the bad part, and emits a canonical form. `kube`'s
`Selector` isn't reused (no parser, reorders values). A blank selector is `None`.
An empty filtered listing names the selector that emptied it.

### 20. `NodeMetrics` is hand-written, and the fetch sits behind a trait

`metrics.k8s.io` isn't in `k8s-openapi`, so we write the type; a test asserts
the URL. Only `metadata`/`usage` are modelled. A `Source` trait exists so "no
metrics-server", "not sampled" and "won't parse" are fixtures.

### 21. Absent usage costs two columns, not two columns of dashes

No metrics-server is the EKS default, so `CPU USE`/`MEM USE` are dropped with a
footnote naming what to install, rather than showing dead columns. The choice is
`any` row across the listing, not per row.

### 22. Usage and requests share one type, and one denominator

`nodes::Share` carries both, so severity classification can't drift between
them. Both divide by allocatable (usage may exceed 100%). Unreadable usage is
`None`, never zero.

### 23. A pod's usage is all of its containers or none of them

If any container's reading is missing, that resource is `-` for the pod: a
partial sum is indistinguishable from a correct one. Resources are decided
independently. A pod with no containers sampled is unknown, not idle.

### 24. The pod metrics listing takes the label selector but not the field one

metrics-server filters on labels but not fields, so only `-l` is passed on.
Usage is joined onto pod rows by namespace and name, so both selectors still
apply to what's shown.

### 25. Live usage is not fatal to `eks pods`

Same rule as decision 21: a failed metrics request drops `CPU`/`MEMORY` and adds
a footnote. Both requests go out in one `tokio::join!`.

### 26. Restart recency is carried by the count, not gathered beside it

`9 (5m ago)` dates from the newest `finishedAt` across exactly the containers
whose restarts are in the count, so dropped init-container history drops its
timestamps too. Zero restarts, or no `finishedAt`, prints a bare count. The
formatted age is computed against one instant per listing.

### 27. Ordering is a function over rows, and the count is only the tie-break

Sorts operate on `PodRow`s, so rankings are fixture tables and the dashboard can
reorder in memory. `restarts` ranks by *when*, count as tie-break. An undated
restart has its own rank between dated and none. Every ordering is total,
ending in namespace-then-name. `PodRow` keeps both formatted and raw times.

### 28. `--sort` is a `clap::ValueEnum` on the domain type

`Order` derives `ValueEnum` itself; no parallel CLI enum to drift, and clap
writes the "not one of" message.

### 29. `eks pods`'s flags travel as a struct

`commands::pods::list` takes a `Request` struct; `too_many_arguments` is denied,
and swapped `Option<&str>`s would type-check.

### 30. Reversing an order does not reverse its unrankable tail

Each order maps a row to `Rank::By(key)` or `Rank::Unranked(tier)`. Only `By` is
flipped; unranked rows stay last in their tier order (an unmeasured pod is not
"least CPU"). The alphabetical tie-break is never reversed.

### 31. `--sort age` puts the youngest pod first

Every order but `name` leads with the row you went looking for; for age that's
"what changed". Opposite to kubectl; `--sort-reverse` gives kubectl's order.
Documented in `--help` and the README.

### 32. `--sort-reverse` rather than a `-` prefix on the order

`--sort -cpu` needs clap hyphen-value handling and breaks its value list; a
boolean flag composes with every order.

### 33. `Direction` and `Rank` moved up to `k8s::order`; the keys did not

The direction and unranked-tail rules are shared in `k8s::order`; each listing
keeps its own `Order` enum and rank functions (no shared "sortable" trait).

### 34. The node orders rank by share; the pod orders rank by the figure

A node's denominator is the machine, so node orders rank by `Share::ratio`. A
pod's is whatever a manifest says, so `--sort cpu` ranks by the figure (share
orderings came separately). Node usage has two unranked tiers: a figure with no
allocatable, then no figure. The `f64` key sorts by `total_cmp`.

### 35. `--sort status` puts the unknown node above the cordoned one

`NotReady`, `Unknown`, cordoned, `Ready`: the accident ranks above the
intention. Nothing is unranked, so it reverses completely.

### 36. A reordered listing names its order; the default one stays silent

A `Sorted by …` footnote appears for any non-default ordering or direction,
using the clap value name. The default listing is unchanged byte for byte. It
joins the footnote list after the failure notes and vanishes on empty listings.

### 37. "Nothing ranked" is a second note, computed by the listing

`Nothing here has cpu to sort by.` comes from a separate function fed by each
listing's `ranks_any` (an exhaustive match beside the comparison, `any` not
`all`). It diagnoses without naming a fallback order, and is silent for the
default ordering.

### 38. The "nothing ranked" note advises, and never advises twice

`k8s::order::Cause`: when a failure footnote already explains it, the note says
"for the reason above"; otherwise it suggests orderings that would rank these
rows, drawn from every `Order` variant (excluding the default and hidden ones).
No suggestion means no advice line.

### 39. `--wide` lands on both listings, and its columns are a `Column` list

`--wide` copies `kubectl -o wide`'s column names and order, except `NODE` stays
in the default pod table and the node table's wide columns go at the end. It
landed on both listings together (a flag honoured by one twin reads as a bug).
Tables build a `Vec<Column>` from a pure function so header and cell can't drift.
`format::Width` is an enum shared by both. Empty wide cells print `-` and wide
columns always appear because the user asked. `READINESS GATES` is included.

### 40. A pod's usage is shown against its request, in one cell

`262m/500m (52%)`, dividing by `pods::effective_requests` so `eks pods` and
`eks nodes` agree. Heading is `CPU/REQ` only when some row has a request; a pod
that asked for nothing keeps a bare figure. Rounding is shared in
`format::percentage`. No severity yet: node thresholds don't fit a pod's
request. (Superseded in part by decision 79: requests became their own columns.)

### 41. A usage figure is shown with its age, and an empty sample set says so

`metrics::Outcome` separates "no metrics-server", "answered with nothing" (own
footnote), and sampled, judged on the rendered rows. Tables end with `Usage is
up to 12s old, averaged over 20s.` (oldest sample, longest window). Stale means
older than two windows; an unknown or `0s` window never accuses. Go durations
are parsed exactly by `metrics::parse_duration`, longest unit spelling first.

### 42. A device is one column, and its shape is the pod table's, not this one's

A column appears for each extended resource any node reports, using Kubernetes'
own rule (`k8s::resource::is_extended`: fully qualified, outside
`kubernetes.io`). The cell is `2/4 (50%)`, booked over allocatable, showing the
total. A device withheld (allocatable below capacity) gets the `devices_withheld`
footnote. `pods::Requests` carries devices in a `BTreeMap`, folded by the same
scheduler arithmetic. `-` means no such hardware; `-/4` means the pod listing
failed.

### 43. Every listing is paged, through one function

`k8s::page::collect` pages every listing with `limit`/`continue`, size 500
(kubectl's). `page::Listing` is a pure state machine (`Page`/`Done`/`Stalled`).
A server repeating its token ends the listing with a warning and keeps what
arrived. metrics-server ignores `limit` but goes through `collect` for the
timeout budget.

### 44. `--timeout` is spent per request, and cannot cover the credential helper

Default 30s, `0` disables, applied per request so big clusters aren't cut off
for their size. `Budget` accepts single-unit durations only so `Display` round-
trips into the advice it prints. Coverage of the credential helper came in
decision 50.

### 45. `eks contexts` renders through `format::table`, gutter and all

One renderer, output unchanged byte for byte. The `*` marker is a prefix added
after rendering, not a column.

### 46. `Width::Narrow` carries a target width, and the drop rule is the listing's

`Width::Narrow(u16)` is applied when stdout is a terminal and `--wide` wasn't
typed; a pipe gets the default table. The terminal size is read once in
`main.rs`; `narrow_to_fit` is pure. Node `DROP_ORDER`: `VERSION`, `AGE`, `REQ`
pair, `USE` pair, `CPU`/`MEMORY`, devices, `STATUS`. Partner columns leave before
their base and in pairs; devices outlast the ordinary columns. `NAME` never drops.

### 47. The pod table's drop order, and one measurement for both tables

Pod order: `AGE`, `NODE`, usage pair, `RESTARTS`, `READY`, `STATUS`; `NAME` and
(under `-A`) `NAMESPACE` never drop. Width arithmetic lives once in `format`
(`column_widths`, `row_width`), shared with `table`, tested against rendered
output, and measured once per listing rather than per drop step.

### 48. The pod count rides with the request totals, and is a share like everything else

`pods::by_node` returns count and requests from one walk, so they can't
disagree and fail together. The count becomes a `Quantity` and reuses `Share`
whole. Cell `12/58 (21%)` over allocatable, always shown. `--sort pods` ranks the
share. In `DROP_ORDER` it goes after `VERSION`/`AGE`, before the `REQ` pair.

### 49. Colour is spent on the rows worth looking at, and on nothing else

CLI tables use `Theme::severity_ink`: `Ok` prints no escape at all, `Warn`/
`Critical` use warning/danger, `Unknown` is muted. Thresholds are the
dashboard's. Severity travels in `format::Cell`; ink never moves a column
(tested by stripping escapes). Headers and footnotes stay plain; the pod table
colours only `STATUS`. `--color auto|always|never` is global; `auto` honours
non-empty `NO_COLOR` and `TERM=dumb`. SGR bytes are written by
`theme::foreground` with an exhaustive match. The dashboard ignores `NO_COLOR`.

### 50. `--timeout` covers the credential helper, on a task that is left behind

`kube` ran the helper blocking inside `Client::try_from`, so the build moved to
`spawn_blocking` and the timeout races its handle; `block_on` uses
`shutdown_background` so an abandoned helper doesn't hang exit. The budget is per
step. `stalled_helper` gets its own message naming the exact command (with
shell-quoted `env`) to run by hand, and offers `--timeout 0` for interactive
helpers. (Narrowed by decision 110: we now run and kill the helper ourselves.)

### 51. The dashboard fetches on a plain OS thread, not a shared `tokio` runtime

`commands::spawn` runs a current-thread runtime on a `std::thread` and delivers
over `std::sync::mpsc`, keeping one runtime model (decision 9). A stale result
is dropped by replacing the receiver; `Budget` bounds the abandoned work.
`shutdown_background` is shared with `block_on`.

### 52. A dashboard bar divides by allocatable, matching the CLI's percentage

Superseded by decision 53.

### 53. The dashboard bar now divides by capacity; the CLI table still divides by allocatable

A bar answers "is this machine busy" (capacity); a table cell answers "will
another pod fit" (allocatable). `Share::ratio_of`/`severity_of` take the
denominator explicitly; `ui::nodes::bar` passes capacity. A node at 100% of
allocatable therefore draws a bar that isn't full; if that's unwanted, change
`bar`'s call site.

### 54. The node pane's usage note is worded bare, not through the CLI's wrapper

`k8s::nodes::usage_note` calls `metrics::unsampled`/`freshness_note` directly,
since the pane has no `CPU USE`/`MEM USE` headings. Built once in `spawn_gather`
from the same data as the CLI footnotes, carried in the `NodesFetch` struct,
split on `\n` into lines, silent on an empty list.

### 55. Background refresh: an immediate refetch on selection, a quiet one on the interval and on `r`

Selecting another cluster resets to `Loading` and refetches at once. `r` and the
interval keep the old rows until the answer lands. A failed refresh after a good
load keeps the rows and shows `refresh_error`. `--refresh` is a
`RefreshInterval` over `Budget`'s grammar, where `0` means "don't" and `r` still
works. Fetches go through closures built once in `main`. (Amended by decision 56:
the closures are boxed.)

### 56. Pod browsing needed a focus model first, and the fetch closures became boxed rather than generic

`Tab` toggles `Focus::Sidebar`/`Detail`; `Esc` backs out one level before it
quits; `q`/`Ctrl-C` always quit. One `detail_selected` index, reset on view
change and fresh load. The row highlight is a line style under span colours, so
severity survives it, and shows only while the detail pane has focus. The pods
pane fetched once per node. Fetch closures are boxed trait objects
(`NodesFetcher`, `PodsFetcher`) so `run` doesn't grow a type parameter per pane.

### 57. `-l`/`--field-selector` became global flags, and the dashboard combines them with its own scoping rather than replacing it

The selectors moved to `GlobalArgs`, as `--namespace` already was, and
`main::run` validates them once, before the terminal initialises.
`commands::pods::scoped_to_node` ANDs the user's field selector with the node's
`spec.nodeName`. An empty pane uses the CLI's `selector_note`, built from the
user's own selectors.

### 58. Two roadmap entries merged: a pane cannot say which order it is in before it can be put in one

Dashboard sorting and its note shipped together. Sorting is client-side, in
`App`, with separate node and pod orders. `s` cycles `value_variants()`, `S`
flips direction, keyed on the view (like `r`), not on focus. The footer reads
`s/S  sort`. The highlight index is kept across a reorder, as on refresh.

### 59. Huge-page columns are conditioned on being nonzero, not merely reported

`ephemeral-storage` shows when any row reports it; `hugepages-*` only when some
row is nonzero, since kernels report zero pools. Both are `allocatable/capacity`
pairs (no request tracked yet), placed after `PODS` and before `AGE`, and first
in `DROP_ORDER`.

### 60. `View` grew a third variant instead of becoming a stack

`Overview | NodePods | PodContainers` keeps every match exhaustive. Backing out
one level (`Esc`, no refetch) and resetting to `Overview` on a cluster switch
(`leave_detail_view`) are deliberately separate operations.

`ContainerRow` lives beside `PodRow` and shares `exit_reason` with it. The pane
fetches the one pod with `Api::get` through the usual `Budget`/`explain` path.

### 61. A container's requests and limits are its own spec, not `effective_requests`

Each container row reads its own `resources`: an absent request is zero, an
absent limit is `None` and reads `unlimited`. `resources_summary` builds the text
and the pane prints it dimmed under the container. Events were left for their own
task.

### 62. Arrow keys joined `Tab`/`Esc`, and quitting at the top level needs two presses

`Right`/`Tab` = `advance` (focus the detail pane, else drill in). `Left`/`Esc` =
`retreat`: drill out first, then move focus to the sidebar, then arm quit. At the
top level, `Esc`/`q` arm a quit that a second `Esc`/`q` confirms within 600 ms;
any other key clears it; `q` does nothing while drilled in. `Ctrl-C` always quits.
The footer shows "press esc/q again to quit" while armed.

### 63. A pane's `Cause::Explained` is narrower than the CLI's, and honestly so

A pane claims "for the reason above" only when a visible note says why: the node
pane only for unsampled usage (`usage_missing_explained`), never for requests
(until decision 98), and the pod pane never (until decision 99).

### 64. Log streaming gets real cancellation; decision 51's "discard is free" does not apply

A followed log holds an API request open, so `commands::spawn_stream` hands the
task a oneshot; dropping the `StreamHandle` ends the `select!` loop and the
request. `Inflight` holds the handle, and every view change away from logs drops
it.

### 65. A paused log view is pinned by a hidden-line count, not a remembered index

The log buffer is a bounded `VecDeque`. `Log::hidden_below` counts the newest
lines below the view and grows by one per arrival while paused, which holds the
view still both while the buffer grows and once it evicts. Clamping happens only
in `visible(rows)`.

### 66. `View` grew a fourth variant, still not a stack

`ContainerLogs` joins the enum: matches stay exhaustive, and a stack would earn
its place at a fifth level. It's the first view with no selectable rows, so
`j`/`k`/`Home`/`End`/`PageUp`/`PageDown` scroll and the footer shows `f`/`w`.

### 67. The `/` filter narrows what a pane draws, not what its footnotes reason about

Sort and usage notes judge the full listing, not the filtered rows: the note is
about the listing, the rows are about what you typed. If that confuses people,
make `ranks_any`/`cause` take an iterator.

### 68. Clearing an applied filter is its own `Esc` press, ahead of backing out

`Esc` with a filter applied clears it and stays put; the next `Esc` backs out.
While still typing, `Esc` cancels the edit.

### 69. The previous-log toggle lives on `View`, and always flips

`previous` lives on `View::ContainerLogs`, so `p` is a view change and reuses
the refetch wiring. `p` always flips; with no restarts it shows
`LogsState::Unavailable` (information, not an error) without fetching. Previous
logs use `follow: false`.

### 70. `--sort` advice is filtered by `distinguishes`, a second and stricter predicate than `ranks_any`

An alternative is suggested only if it both ranks some row and puts two rows in
a different order (`distinguishes`, one pass comparing each rank with the
first). All other uses keep `ranks_any`. A one-row listing never gets
suggestions.

### 71. A vacuous ordering the user actually typed gets its own diagnosis, in `unranked_note` rather than a second function

A ranked but non-distinguishing order prints `Every row here ranks the same
under {name}, so sorting by it changed nothing.`, with no `Cause`. It's a branch
of `unranked_note`, so every listing and pane got it with no call-site changes.

### 72. A dashboard pane's `--wide` facts move to the detail view they are about, rather than growing a wide mode

There's no wide mode in panes. The pod-containers pane shows `IP` always, and
`NOMINATED NODE`/`READINESS GATES` when present, as lines above the containers,
read through the same `pub(crate)` functions the CLI columns use, with no new
fetch. (Nodes: decision 78.)

### 73. `eks contexts` honours `--color`, and the `*` gutter still does not

`contexts` now takes the caller's `Palette`; a test asserts both palettes render
identically. Colouring the `*` identity marker was left to the theme work.

### 74. `eks` does not own credential resolution; it owns knowing when it will fail

No AWS SDK: it adds a second hyper/rustls tree to startup, and writing
`botocore`'s private token cache is fragile. `aws::login` shells out to `aws sso
login --profile X`. `eks` owns detecting the expiry, which needs only files on
disk. (Narrowed by decision 110: we now run the `exec` helper ourselves.)

### 75. The session check reads two files, and matches on `startUrl` rather than a hash

`aws::config` (a hand-written reader for four keys; AWS's format isn't INI) and
`aws::sso` (the JSON token cache) are pure over contents and `now`, and take
under 0.1 ms, so the check runs before connecting. Cache entries match on
`startUrl`, not botocore's filename hash; unreadable entries are skipped. Under
60 s left counts as expired.

### 76. A browser never opens without a yes, and the policy is a pure function

`--login auto|always|never`. `aws::decide` is pure over session, flag, and
interactivity (stdin and stderr both terminals). With `auto` and no terminal it
proceeds without asking. Prompts go to stderr, keeping stdout clean. `never`
doesn't read `~/.aws`. The offer is made at most once per command (three-way
`Outcome`). `aws sso login` isn't bounded by `--timeout`.

### 77. The dashboard asks before it opens, and offers `L` after

The pre-flight asks on plain stdio before the alternate screen opens; every
fetcher after that is pinned to `LoginMode::Never`. A session lost mid-dashboard
sets `credentials_lost` and offers `L` (`Flow::Login`), never an automatic
suspend. `commands::FetchError` carries the classification across threads.
`ui::run` owns the suspend closure (leave raw mode and alternate screen, then
re-enter). There's a separate credential footer; error text is drawn one
sentence per line.

### 78. The node's `--wide` facts land on the pod-drilldown pane, not a new `View`

`View::NodePods` already commits to one node, so its five wide facts draw
unconditionally above the pod list via `k8s::nodes::wide_facts`, through the
table's own `Column` accessors, from the `NodeRow` already in hand.
The node is looked up by name (`App::drilled_node`), so a refresh updates it and a
vanished node shows nothing. The facts draw while the pods are still loading.

### 79. A pod's request gets its own columns; the usage pair loses its half

`CPU REQ`/`MEMORY REQ` show the bare request whether or not anything is sampled;
the usage cell becomes `250m (50%)` under plain `CPU`. The two request columns
are gated together on any nonzero request. Device request columns come from the
union of the rows' `extended_requested`, and a pod that didn't ask reads `0`, not
`-`. Drop order: request pair before usage pair, then all devices together,
before the health columns.

### 80. Pod events land in the existing pod-containers pane, as a section that can fail on its own

`EVENTS` sits under `CONTAINERS`. It's fetched concurrently and has its own
`Result`, so missing `events/list` RBAC shows a dimmed `events_error` without
breaking the pane. Events are grouped client-side on `(reason, message)`, like
kubectl. An empty list is worded by pod age against an assumed 1 h retention
(`RETENTION_SECS`). The query sets both `involvedObject.name` and `.namespace`;
it reads `core::v1::Event`.

### 81. A pod's usage sorts by share too, and the wrapper for it moves to `k8s::order`

`--sort cpu-share`/`memory-share` are new `Order` variants, not a modifier. No
raw-figure sort for nodes. The `Ratio` (`total_cmp`) newtype moved to
`k8s::order`. Share has two unranked tiers: sampled with no request, then
unsampled.

### 82. A stale sample gets a text marker on its own cell, not a column or a colour

A cell's one `Severity` already means utilisation, so staleness is text:
`metrics::mark_stale` appends ` (stale)` to the cell. It's legible without colour
and passes no judgement on the figure.

### 83. The node pane's bars get the same text marker, on the figure they already print

`ui::nodes::bar` passes its trailing figure through `mark_stale` when
`usage_stale`: `1.5 (stale)`, with the fill and colour unchanged.

### 84. `--sort-resource` is a second flag, not a free-form `--sort` value

Device names aren't known until after fetching, so `--sort` stays a closed
`ValueEnum`. `--sort-resource <NAME>` conflicts with a non-default `--sort`,
checked by the pure `ordering_for` before connecting (not clap's
`conflicts_with`, because of the default value). Ranking reuses `busiest` over
`Device::share()`, and both blanks share the second tier. `device_note`/
`device_unranked_note` reuse `Cause` but offer no alternatives.

### 85. `--sort-resource` in the node pane is a second prompt, `R`, mirroring `/`'s life cycle — and `s` reclaims it

`/` isn't reused, so one key keeps one meaning. `R` opens `ResourceSort`
(`Inactive`/`Editing`/`Applied`, like `Filter`). `App::sort_nodes` resolves an
applied resource over the fixed order; the two share `node_direction`. `s` clears
an applied resource; `S` reverses without clearing. `ui::nodes::Sort` picks
which notes the header prints.

### 86. `eks pods --sort-resource` ranks a real zero directly, with no tail and no unranked note

A pod's missing device request is a real `0`, so `k8s::pods::order::
sort_by_device` ranks `Reverse<Quantity>` directly, with the usual tie-break
and no `Rank` tail. There's no `device_unranked_note`; `device_note` is kept.
`commands::pods::ordering_for` mirrors the node version.

### 87. `R` in the pod-drilldown pane answers the same as decision 85 did for the node pane

`pod_resource_sort` beside `node_resource_sort`, `ui::pods::Sort`, with the
same `s`/`S` rules. The footer's `R` hint shows over both panes. Keystroke
handling is factored into `advance_resource_sort`; `Filter` and `ResourceSort`
stay separate types.

### 88. A hidden column gets a second line under the sort note, not an exemption from `DROP_ORDER`

Exempting a sort column would cascade to its base column, so the drop rule
stands and `k8s::order::hidden_note` adds `That column is not shown at this
width; run with --wide or widen the terminal to see it.`, joined to the sort
note (CLI only). A column is hidden only if it's present at `Width::Default` and
absent at the actual width (`order_hidden`, an exhaustive `order_column`
match). `requests_unavailable` says whether some or none of its columns are
shown.

### 89. `device_note` gets the same hidden-column line, worded the same way, through its own function

The shared constant is `HIDDEN_COLUMN`; `device_hidden_note(hidden)` is the
device counterpart and `device_hidden` the per-listing check (one private
"ask twice, compare" helper). The joins `note_with_hidden`/
`device_note_with_hidden` live in `k8s::order`.

### 90. A pod's own request gets its own severity thresholds: `Warn` at 150%, `Critical` at 300%

`Severity::from_request_share`: a judgement call, not a measurement. Bursting
above a request is what headroom is for; 300% means the pod is living on borrowed
capacity. It grades upward only. `usage_severity` reads the same ratio as the
cell text and returns `None` (not `Unknown`) with no sample or request.

### 91. A known limit replaces the request-share reading once a pod is over its request, rather than adding a tier or a column

Below 100% of request, `from_request_share` is the whole answer. Above it, a
known pod-wide limit swaps the reading to `from_utilisation` against the limit.
With no limit, nothing changes. `effective_limits` mirrors `effective_requests`'
fold, but `Limits` is `Option`s, where any unbounded container makes the pod
unbounded, seeded with `Limits::zero()`.

### 92. The progress line is governed by the colour switches, not by one of its own

It draws only when both stdout and stderr are terminals; `--color always`
doesn't override that. Movement counts as ink: `--color never`, `NO_COLOR`, and
`TERM=dumb` turn it off too, so there's no separate `--progress` flag. `-v` and
`RUST_LOG` turn it off because log lines would shred it. A terminal reporting 0
columns is treated as 80. `progress::wanted` is pure and reuses `Palette::choose`.

### 93. A progress step is a handle that ends when it is dropped, and it is counted inside the paging loop

`page::collect` takes a `progress::Task` by value, so any exit path erases the
line on drop. `Progress::none()` costs nothing for the dashboard and pipes.
`Task::tick` redraws every 250 ms while awaiting (boxing the future) and skips
identical text. The wording counts (`reading 1,500 nodes, 12,000 pods… 4s`), and
the helper step is named (`running aws eks get-token`). Width is read once.

### 94. `SIGINT` is trapped only where `progress` draws — a new `block_on`, not a change to it

`block_on_interruptible` races the work against `ctrl_c()` for `eks nodes`/
`eks pods` only: a blocking `read_line` in the login prompts couldn't be
interrupted, so they stay on `block_on`. Dropping the loser erases the progress
line (tested via a generic `race`). A handler that can't install never fires.
Exit code 130; `main::run` returns `Result<ExitCode>`.

### 95. A listing's footnotes are assembled by a private function over a bundled params struct, not a shared `Footnotes` builder

The two tables' notes differ too much for a shared builder.
`commands::nodes::footnotes` and `commands::pods::notes` take
`FootnoteInputs`/`NoteInputs` structs of already-resolved values, and tests
assert the exact note order. Pod usage failure is held as a `Result` like the
node one.

### 96. The pod-containers pane's events section shares the container list's `/` query, matched against the event's reason

Nothing in `App` selects events, so one query can narrow both. `events_lines`
ranks through `fuzzy::rank` on `EventRow::reason`. The empty note and fetch
errors stay unfiltered; no match prints its own "No events match …".

### 97. A node's pressure conditions land on the pod-drilldown pane, unconditionally, coloured by `Theme::severity`

`k8s::nodes::Pressure` holds four `bool`s (Memory, Disk, PID,
NetworkUnavailable); absent counts as `False`. `pressure_facts` always prints
all four beside the wide facts: `Critical` for true, `Ok` otherwise. It uses
`Theme::severity` (dashboard) rather than `severity_ink`.

### 98. The node pane's failed-pod-listing note is worded for what the pane shows, not the CLI table's column names

`k8s_nodes::requests_note` names the `- pods` cell and the three booked
orderings, not CLI headings, from the same `Result` as `requests_unavailable`.
It's carried on `NodesFetch` → `NodesState::Loaded`, printed above
`usage_note`, and sets `Missing::requests`.

### 99. The pod-drilldown pane's usage fetch is scoped like `eks pods`'s, and its refresh cadence is left for its own decision

Metrics are fetched with `Scope::All` plus the user's selectors (metrics-server
can't filter by `spec.nodeName`) and joined onto the node's rows. `usage_cell`/
`usage_severity` became `pub(crate)` rather than copied. Cadence: decision 100.

### 100. The pod-drilldown pane shares the node pane's refresh triggers, and keeps its rows over a failed one

`refetch_pods` rides the node pane's interval, `r`, and login triggers, but only
while `View::NodePods` is showing (`pods_refresh_target`). `apply_pods` now keeps
the last good rows with `refresh_error`, like `apply_nodes`. Known gap: an empty
listing hides a later refresh error, as on the node pane.

### 101. `l`/`F` retype the dashboard's selectors as two independent prompts, and a commit refetches immediately

Selectors are server-side grammar, so they don't reuse `/`. One `SelectorEdit`
(`Inactive`/`Editing` with an `error` field) serves both keys. `Enter`
validates both selectors through `commands::pods::selectors_for`; a rejection
keeps the typed text and shows the reason. A committed change refetches at once
(`pod_selectors_before` is compared each loop). The applied selector shows as a
`Selector: …` header, reusing `selector_note`.

### 102. The container-logs pane's `/` is a plain substring jump-to-match, not a second `fuzzy::rank`

A log is never reordered or thinned, so `/` jumps like `less`: case-insensitive
substring, `n`/`N` to step. `logs::LogSearch` has `Filter`'s life cycle.
`Log::jump_to_match` is one `step` function (direction, inclusive on commit).
Matches are recomputed per step because eviction renumbers lines. The whole
matched line gets `Theme::match_highlight`. Note: `Line::styled` puts the style
on the `Line`, not its span.

### 103. The config file's path is `~/.config/eks/config.toml` on every platform, not a platform-varying one

The literal path matches `~/.kube/config` habits. Keys are `color` (alias
`colour`), `refresh`, and `namespace` (and later `theme`). A malformed file
discards all its settings, with one `tracing::warn!`; parsing is pure
(`(Config, Option<Warning>)`). Values reuse each flag's own `FromStr`. The flags
lost their clap defaults (`Option<T>`), and `GlobalArgs::effective_*` chains
flag → file → default.

### 104. Terminal-background detection reads `COLORFGBG`, not an OSC 11 query, and falls back to dark

`COLORFGBG` costs no I/O; an OSC 11 query would have blocked first paint with no
fixture to test it. Most terminals don't set `COLORFGBG`, so `auto` usually
falls back to dark, the safer wrong guess; `--theme light` and the config key
are the override. `Theme::light()` is picked for a light background, not derived
from dark, and neither theme paints a background. `Palette::choose` takes the
`Theme`, so CLI and dashboard agree; `App::set_theme` seeds it. (Amended by
decisions 111 and 115: the dashboard now also queries OSC 11 after first paint.)

### 105. Startup benchmarks measure the pure computation, not the process; CI reports through a branch-keyed cache, never fails

`benches/startup.rs`: `kubeconfig_parse` and `first_paint` (parse → views →
`App::new` → `set_theme` → one `TestBackend` draw) over a synthetic 50-context
kubeconfig, about 0.6–0.9 ms. They don't measure process startup (see decision
112). The CI `bench` job caches `target/criterion` by branch, falling back to
`master`, and writes the comparison to the step summary. `criterion` is trimmed
to `cargo_bench_support`.

### 106. Completions and the man page skip kubeconfig entirely; the man page is a hidden command; CI generates both only for native targets

`eks completions <shell>` and a hidden `eks man` render `Cli::command()` to a
`String`. They return before kubeconfig/config load, so a broken kubeconfig
can't break them. Release CI generates them only on `matrix.native` legs; the
cross-compiled darwin tarball ships without them. `make dist` includes both.

### 107. Golden snapshots are text first, colour by role name, and live beside `ui`'s own tests

Most `insta` snapshots are screen text only; four (one per drill-down level)
add styled runs, with colours written as `Theme` role names (`fg=muted`). Two
tests sweep every view in both themes: every colour must have a role, and the
light theme must use the same roles as dark (so there's no light snapshot).
`accent` and `border_focused` share a value, so the first match wins. Tests
live in `src/ui/tests/golden.rs`; `make snapshots` updates them, and
`cargo-insta` is optional. CI fails on any mismatch.

### 108. The new Linux targets cross-compile with the distribution's own toolchains, aarch64 runs under QEMU, and "static" is read from the ELF headers

`aarch64-unknown-linux-gnu` and `x86_64-unknown-linux-musl` build on
`ubuntu-latest` with Ubuntu's cross packages, not `cross` or zigbuild. The
aarch64 binary is smoke-tested and generates completions under
`qemu-aarch64`. `scripts/verify-static.sh` fails on any `PT_INTERP` or
`DT_NEEDED` (musl builds static-pie, so "no dynamic section" would be wrong).
The glibc floor was 2.39; fixed in decision 109.

### 109. The `-gnu` release binaries link against glibc 2.17 through `cargo zigbuild`, and the floor is read back out of the binary

`cargo zigbuild --target <triple>.2.17` on `ubuntu-latest`, with zig 0.16.0 and
cargo-zigbuild 0.23.4 pinned. 2.17 is Rust's own minimum and reaches Amazon
Linux 2 and 2023. `scripts/verify-glibc-floor.sh` reads the newest `GLIBC_x.y`
the binary needs (compared numerically) and names the symbols that raise it;
its tests use fixture readelf output. Each `-gnu` binary then runs
`--version`/`--help` in `amazonlinux:2023` (from `public.ecr.aws`), aarch64 under
QEMU.

### 110. `eks` runs the kubeconfig `exec` helper itself; it still does not own logging in

Decided by the reviewer on 2026-09-24. Narrows decision 74 without reversing it.

Decision 74's objections (SDK weight, writing botocore's cache) are about
logging in, not about running the `exec` block. The
`client.authentication.k8s.io` protocol is small: spawn the named command with
its env and args, and read one `ExecCredential` from stdout. No new crate, no
cache writes. Owning the child means `--timeout` kills the helper instead of
abandoning it, and a listing can re-run it near `expirationTimestamp`.
Unchanged: `aws sso login --profile X` is still a shell-out, the pre-flight
still only reads the cache, and failures are still worded through `explain`/
`stalled_helper`/`helper_command`. Token and client-certificate credentials are
both handled. An unparseable helper is an error naming the command, never a
fallback to `kube`.

### 111. The OSC 11 background query runs off the paint path, and a light answer re-themes after first paint

Decided by the reviewer on 2026-09-24. Lifts decision 104's deferral.

With `theme = auto` and nothing from `COLORFGBG`, the dashboard paints dark,
then sends OSC 11 on the input side. A light reply switches through
`App::set_theme` and redraws, spending none of the first-paint budget; one dark
frame before the flip is accepted. No reply or an unparseable one keeps dark
silently. The reply parser is pure and tested on fixture bytes; ordering is
asserted by events, not timers. A terminal `COLORFGBG` already answered isn't
queried. CLI tables keep `COLORFGBG` only.

### 112. Wall-clock startup is measured with `hyperfine` in CI's `bench` job, reported and never gated

Decided by the reviewer on 2026-09-24. Extends decision 105.

A pinned `hyperfine` times the release binary (`eks contexts`, `eks --version`)
against the same synthetic 50-cluster kubeconfig, and reports to the job summary
without ever failing CI. Terminal setup (raw mode) isn't measured, and the
report says so. Built as decision 116.

### 113. The Homebrew formula lives in this repository, not a separate tap

Decided by the reviewer on 2026-09-24.

`Formula/eks.rb` lives here, tapped by URL: `brew tap nmcginn/eks-wrangler
https://github.com/nmcginn/eks-wrangler`, then `brew install
nmcginn/eks-wrangler/eks`. A separate `homebrew-tap` repo would mean a second
repo to keep in step and a release token with push rights outside this one; if
more formulae appear, moving is a rename. The formula installs the release
tarballs (decisions 108, 109) with their published checksums, and ships the
completions and man page inside them. The install script verifies the same
checksums before installing anything.

### 114. The token is refreshed by a layer on every request, and a refused page is asked for once more

- **Where.** A `tower` layer (`k8s::auth`) supplies the token to every request
  (listings, log streams, metrics, the dashboard). `page::collect` retries a
  `401` once, but only on a page after the first.
- **One deadline.** `Budget::wrap` publishes its deadline in a task-local
  (`page::deadline`); a refresh uses the same instant and the helper is polled
  first, so "helper stalled" beats "cluster did not answer".
- **Killing.** `kill_on_drop` kills the helper process only, not a forked
  grandchild. No process group, because an interactive helper in a background
  group gets `SIGTTIN`.
- **`interactiveMode`.** Read as client-go does (`exec::interactive`):
  `IfAvailable` (the default) means "if stdin is a terminal". A non-interactive
  helper gets `/dev/null` stdin and captured stderr. The dashboard case is
  decision 119.
- **Parsing.** JSON first, then YAML only if it has a `status` (so `Enter MFA
  code:` isn't read as a map).
- **Certificates** are held for the client's life, not refreshed.
- **Dependencies:** `tower`, `http`, and `base64`, all already in the tree via
  `kube`; `tokio` gains `process`.

### 115. The OSC 11 reply is read back out of crossterm's key events, asked once with BEL, and judged by which theme reads better

- **Reading.** crossterm parses the reply as keys, so
  `ui::background::ReplyReader` sits before `App::on_key` and swallows exactly
  the keys that spell a reply. A user's own Alt+`]` is delayed one key, not
  lost; a broken-off reply is dropped, not replayed. The reader steps aside
  after one reply. This relies on crossterm internals that an upgrade could
  change.
- **BEL** terminator on the query; both BEL and ST accepted in the reply.
- **Not asked:** anything but `auto` with silent `COLORFGBG`; `TERM` unset,
  `dumb`, or `linux*`; non-Unix.
- **Verdict.** `Background::of_rgb` is light when `Theme::light().text` has
  more WCAG contrast against the colour than `Theme::dark().text` (crossover
  near `#777777`); a tie is dark.

### 116. The wall-clock benchmark is a tested shell script over a committed fixture, and it times the binary without a shell or the user's home

The kubeconfig fixture is the file `benches/fixtures/kubeconfig-50.yaml`,
shared by criterion (`include_str!`) and the script. `scripts/bench-startup.sh`
(`make bench-process`) takes `HYPERFINE` from the environment so
`scripts/tests/bench-startup.sh` can test it. It runs with `--shell=none` and an
empty `HOME`. The Markdown table goes to stdout for the job summary. A failing
command fails the job; a slow one never does. `hyperfine@1.20.0` is installed
via `cargo install --locked`.

### 117. `cargo-deny` denies by default, audits only the shipped targets, and its policy is tested against fixture crates

`multiple-versions = "deny"`; each current duplicate is skipped by its older
version with the responsible crates named. Flip to `warn` if dependabot makes
this noisy. Licences are an allow-list of permissive licences, with MPL-2.0
allowed for `option-ext` alone. `[graph] targets` is the five release triples;
add a target there if the matrix grows. `openssl-sys`/`native-tls`, `*`
requirements, and non-crates.io sources are denied.
`scripts/tests/deny-policy.sh` proves each rule fires against fixture
workspaces. It runs as CI's own `supply-chain` job (cargo-deny 0.20.2) and
`make deny`, not in `make check` (it needs network).

### 118. The MSRV job reads its toolchain from `Cargo.toml`, builds every target, and runs the tests

`scripts/msrv.sh --print` reads `rust-version` via `cargo metadata --no-deps`,
so `ci.yml` holds no second copy. The job runs `build --locked --all-targets
--all-features`, then `test --locked --all-features`, on that toolchain. The
script (`make msrv`, not in `make check`) takes `CARGO`/`RUSTUP` from the
environment for its tests, never auto-installs a toolchain, and says what to do
on failure. The README's "Requires Rust 1.90 or newer" is checked against
`Cargo.toml`. 1.89 probably works too (`kube` declares it); lowering the floor
is the reviewer's call.

### 119. The install script verifies before it writes, picks musl on x86_64 Linux, and the release job renders and commits the formula

- **POSIX `sh`.** `scripts/install.sh` targets whatever `sh` runs `curl | sh`
  (dash, BusyBox); its tests run under the system `sh` so a bashism fails CI.
- **Verify, run, then write.** The tarball and `.sha256` land in a temp dir,
  the digest is compared, and the binary runs `--version` before anything is
  renamed into the prefix. A missing or malformed checksum, or neither
  `sha256sum` nor `shasum`, stops the install. Only the digest is read, so
  both the old `dist/`-prefixed and the new bare checksum names work; the bare
  name makes `shasum -c` work by hand.
- **Targets.** x86_64 Linux gets the static musl build, aarch64 Linux gets
  `-gnu`, and aarch64 musl is pointed at `cargo install`. macOS reads
  `sysctl.proc_translated` so Rosetta shells still get the native build. The
  formula makes the same choices. The default prefix is `~/.local`; for zsh the
  script says how to extend `fpath`.
- **The formula is rendered, never hand-edited.**
  `scripts/render-formula.sh <version> <dir>` prints it from the release's
  `.sha256` files and refuses if one is missing. Completions and the man page
  come from `generate_completions_from_executable`, because the darwin x86_64
  tarball has none (decision 106).
- **Release commits it.** On non-pre-release `v*` tags, the `formula` job in
  `release.yml` checks the tag against the binary's version, renders the
  formula, attaches it to the release, and pushes it to master with
  `GITHUB_TOKEN`. That's no new token (decision 113's objection), but it is an
  unreviewed push; if branch protection refuses it, the job says to commit the
  attached file by hand. Switching to a PR is a change to that one step. There
  is no `Formula/eks.rb` until the first release.

### 120. Inside the dashboard a credential helper never prompts; `L` runs it in the foreground

Decided by the reviewer on 2026-09-30. Settles the case decision 114 left open.

Once `ui::run` owns the terminal, every `exec` helper the dashboard starts runs
non-interactively, whatever its `interactiveMode` says: `/dev/null` stdin,
captured stderr, `interactive: false` in `KUBERNETES_EXEC_INFO`. That covers the
first connect as well as a refresh an hour in. It is fixed when the dashboard's
fetchers are built, the same construction-time guarantee `LoginMode::Never`
gives (decision 77), so no background thread can reach the terminal. A helper
that needed input fails, and the failure is credential-shaped: it sets
`credentials_lost`, and the message says to press `L` rather than the CLI's
advice to set `interactiveMode`.

`L` suspends the screen as `Flow::Login` already does. It runs `aws sso login`
first when the context's SSO session has expired, as today. It then runs the
helper once in the foreground, with its own `interactiveMode` and the real
terminal, so it can prompt. The credential it prints seeds the auth layer's
keeper (decision 114), so the dashboard refetches with it and doesn't re-run the
helper non-interactively straight away. When that token expires, the next
background refresh fails the same way and offers `L` again.

The alternatives were forcing `Never` with no way to answer a prompt from inside
the dashboard, which left only "go to a shell" for non-SSO helpers, and
suspending automatically whenever a helper wanted to prompt. The second meant a
new channel from background tasks to the UI thread, and a prompt taking over
the screen with no key pressed, which decision 77 already refused for logging
in. The CLI commands are unchanged.

### 121. The dashboard's fetchers share one credential per context through a `Store`, and `L` seeds it

Carries out decision 120.

- **Where the guarantee lives.** `exec::Prompt` is `AsConfigured` (the CLI and
  `L`) or `Never`. `k8s::auth::Store` has no constructor that takes one; every
  helper run through it is `Never`. Each dashboard fetcher takes a `Store`
  (`credentials::Via::Dashboard` for the listing the CLI shares), so a fetcher
  built without one does not compile. A test drives all four.
- **`Never` also means its own process group** on Unix, so a helper that opens
  `/dev/tty` itself (Python's `getpass`) is refused by the terminal rather than
  reading the dashboard's keys. Observed: `EIO` at once, so it fails as muted.
  `AsConfigured` keeps decision 114's no-group rule.
- **Muted.** A helper that fails or prints a non-credential after `Never` took
  away a terminal its `interactiveMode` would have given it is
  `exec::Error::Muted` → `Failure::HelperMuted`: credential-shaped (it arms
  `L`), and worded to press `L`. A block that says `Never` itself loses nothing
  and fails in the CLI's words. A stall stays `HelperStalled`.
- **One keeper per context.** Decision 120's "seeds the keeper" needs one that
  outlives a fetch, so the store keeps it, keyed by context and checked against
  the helper's command line. Fetches reuse it instead of running the helper
  each time. A certificate is kept until its last minute.
- **`L`** runs `aws sso login` as before (always, for an Identity Center
  profile: the refusal is the evidence), then the helper whenever there is one,
  under `--timeout`, then `Store::seed`. It errors only with neither. After it,
  a failed container or log pane is refetched too, and a log stream refused
  for credentials arms `L` (`LogEvent::Refused`).

### 122. `--json` is a per-command flag over the table's own rows, in base units, with `null` for unknown

Pulled up from Ideas when Open was empty; the shape below is the reviewer's to
change.

- **Surface.** `--json` on `contexts`, `current`, `nodes`, `pods` — the read
  commands. Not global (it means nothing to the dashboard or `use`), and not
  `-o json`: one format does not need a format switch yet. Refuses `-q`.
- **Same rows.** `json.rs` is pure over `NodeRow`/`PodRow`/`ClusterView`, after
  sorting, so the two outputs cannot disagree. `--wide` and terminal narrowing
  do not apply; every field is always present.
- **Spelling.** Quantities are numbers in base units (cores, bytes, counts),
  integers when whole. Instants are RFC 3339; the human ages are left out.
  The tables' `-` becomes `null`. `ready` and `readiness_gates` are
  `{ready, total}`, parsed back from the row's `1/2` text rather than widening
  `PodRow` for one consumer.
- **Document shape.** An object, never a bare array, so fields can be added.
  Listings carry `cluster` and `notes`; pods also the `namespace` read.
- **Notes.** Only what explains a `null` or dates usage, chosen through the
  same `metrics::Outcome` as the footnotes and worded for fields. Ordering
  notes are dropped: the array order is the answer. Prose, not a contract.
- **Stability.** Field names are not versioned. Until a release says
  otherwise, the schema may change with the tables.

### 123. AWS APIs beyond login go through the AWS CLI, not the SDK

Decided by the reviewer on 2026-10-02, for the CloudWatch tasks in Milestone 7.

`eks` runs `aws eks describe-cluster`, `aws logs filter-log-events` and the
like as child processes and parses their `--output json`. It does not depend
on `aws-sdk-*`. This matches `aws::login`, keeps the binary and the dependency
tree light, and leaves the profile, the SSO cache and the credential chain to
the CLI that every EKS context already needs. Each call is a `tokio` child with
`kill_on_drop`, as in `k8s::exec`, so `--timeout` and Ctrl-C stop it rather
than abandon it. Inside the dashboard it runs non-interactively, as decision
120 requires of credential helpers.

The cost is a Python start-up on every call. Paging and `--follow` polling pay
it once per request, and that is acceptable because no call sits on the render
path. Live tailing is done by polling, because `start-live-tail` prints output
meant for a person to read, not for `eks` to parse.

### 124. `eks exec`: prefix matching, `-C` for the container, and a TTY only between two terminals

- **Dependency.** `kube`'s `ws` feature, which brings `tokio-tungstenite`,
  `tungstenite`, and their `rand`/`sha1`/`data-encoding`. `cargo deny` passes
  with no new exceptions. `tests/exec.rs` uses the same `tokio-tungstenite` as
  a dev-dependency to play the API server.
- **`-C`, not `-c`.** `-c` is the global `--context`, and changing that would
  break every script using it. A `--context` that does not resolve on `exec`
  gets a line pointing at `--container`, since that is the likely slip.
- **Pod names.** A full name or a unique prefix of one; an exact name wins over
  longer names it starts. Nothing matching in the namespace triggers one
  best-effort cluster-wide search, so the error can name the namespace with
  `-n`. A role that may `get` but not `list` pods falls back to the name as
  typed. `-l`/`--field-selector` narrow the candidates.
- **TTY when stdin *and* stdout are terminals**, not just stdin as the roadmap
  and `kubectl` have it: a TTY writes `\r\n` and merges stderr, which would
  corrupt `eks exec api -- cat f > f.local`.
- **Shell search.** `/bin/sh -c 'exec bash if present, else sh'`, then
  `/bin/bash` alone; `cmd.exe` when the pod's `spec.os` or `nodeSelector` says
  Windows, or when both Linux shells are missing and the node's label says so.
  A missing executable is read from the runtime's message, the only signal the
  client gets.
- **Not running.** Checked before connecting, for the pod's phase and the
  container's own state. The pod's last five events are printed rather than
  only pointed at.
- **Terminal.** Raw mode lives in a guard restored on `Drop`. `SIGTERM` and
  `SIGHUP` end the session through the same guard (exit 128+n). Without a TTY,
  Ctrl-C exits 130 via `block_on_interruptible`. A closed stdout pipe exits 141
  quietly.
- **Stdin close needs `v5.channel.k8s.io`** (Kubernetes 1.30+). On older
  clusters `kube` closes the whole stream at end of input, so piped stdin can
  lose the command's last output, as with `kubectl`.

### 125. `x` checks off the render thread, probes for a shell, and refuses on a status line

- **Two halves.** A background check (`exec::spawn_prepare`) runs the pod
  `get`, `pick::container`, `pick::running`, and the shell search. Only a
  `Plan` that will start reaches the foreground session. A refusal goes on a
  status line above the footer, the screen is left alone, and the next key
  clears it.
- **Probe.** `eks exec` learns that a shell is missing by trying to start it.
  The dashboard must know before it gives up the terminal, so it first runs
  each candidate shell with no stdin and no TTY. A shell that is present
  exits at once; a missing one is refused exactly as the real session would
  be. The cost is one extra short-lived process and round trip per `x`. A
  probe still running when `--timeout` expires counts as found.
- **Wording.** `exec::Surface` changes the advice, not the diagnosis: `press
  p` rather than `kubectl logs --previous`, `press r` rather than "run it
  again", and an `eks exec … -n … -C …` line that pastes as it is.
- **Targets.** A highlighted container; a highlighted pod's default container
  by `pick::container`'s rule; the container whose log is open. With the
  sidebar focused, `x` says to press `tab` rather than guessing a row.
- **Cancel.** `Esc` while the check runs, or leaving the pane or the cluster.
  The event loop drops the receiver, so a late answer opens nothing.
- **Hint.** In the containers pane `x shell` takes the slot of `s/S sort`,
  which does nothing there. On the pod and log panes it comes after `/`, so a
  narrow terminal clips it before `q quit`.

### 126. A dashboard session reads the keyboard with `poll(2)`, and installs no signal handlers

- **Keyboard.** `tokio::io::stdin` reads on a blocking thread nothing can
  cancel. After the session, its pending read takes the dashboard's next
  input. A manual run against a pseudo-terminal showed that this is the
  terminal's reply to `ratatui`'s cursor-position query, so the dashboard
  failed on return. `exec::keyboard::Keyboard` waits with `poll(2)` and a
  50 ms timeout, reads only when there is input, and its `Drop` stops and
  joins the thread.
- **Dependency.** `rustix`'s `event` feature, Unix only. `rustix` is already
  built for crossterm. Elsewhere the dashboard falls back to `tokio`'s stdin
  and loses that first key; no release target is affected.
- **Signals.** `eks exec` catches `SIGTERM`/`SIGHUP` to restore the terminal.
  A dashboard session does not, because `tokio` never removes a handler it
  installs, and the dashboard would stop answering `kill`. The dashboard as
  a whole does not handle those signals yet either.
- **Terminal.** `leave_terminal` now shows the cursor, which `ratatui` keeps
  hidden. A banner names the pod and container and says how to get back.

### 127. `eks port-forward`: one stream per connection, a three-second watch, and a pod named directly ends the command

- **Streams.** Each accepted connection opens its own `Api::portforward`
  WebSocket. `kube`'s `Portforwarder` carries one stream per port per
  socket, so this is the only way two connections are independent. The cost
  is one upgrade request per connection.
- **Following pods.** `svc/` and `deploy/` keep their label selector and
  look at its pods every 3 seconds, and at once when a connection's upgrade
  gets a `404`. The current pod is kept while it is ready, and otherwise the
  oldest ready one is chosen. With nothing ready, an unready or terminating
  current pod is kept, and connections are refused only once it is gone. A
  connection that hit the `404` waits up to 10 seconds for the new pod and
  is retried once, since nothing was sent. The service object is read once:
  a `targetPort` edited while forwarding is not picked up, but a named one is
  resolved again against each new pod.
- **A pod named directly** is looked at on the same schedule. Deleted or
  finished ends the command (exit 1), naming the `deploy/` that would have
  followed it. That is found from the owning ReplicaSet's name and the pod's
  `pod-template-hash`, with no extra request. Terminating is said once and
  kept.
- **Local port.** No LOCAL means REMOTE's number if it can be bound, and any
  port otherwise, with the reason on the line. For a service, REMOTE is the
  service port. A LOCAL that was typed is used exactly or is an error.
  `--address` defaults to `127.0.0.1`; `localhost` adds `::1`, which may be
  missing. Any non-loopback listener is called out on its line.
- **Permission check first.** A `SelfSubjectAccessReview` for `create` on
  `pods/portforward` runs before binding, so a missing grant is an error now
  rather than a message on the first click. Clusters before 1.30 authorised
  the WebSocket upgrade as `get`; a custom role with only `get` would be
  refused here though it could forward. A review that fails is not a
  refusal.
- **Prompting.** Several candidate ports with stdin and stderr both
  terminals are a numbered question. Without one, the same table is the
  error, with a command to paste.
- **Ctrl-C exits 0**, not 130: it is how a forward is meant to end.
- **Half-close.** `kube` stops delivering the pod's bytes once the client
  half-closes its side, as `kubectl` does. Clients that read their answer
  before closing (browsers, `curl`, database drivers) are unaffected.

### 128. Dashboard forwards: ports are rows, `f`/`F` act on the highlighted one, and a forward outlives its pane

- **Ports are rows.** Each declared port is a row under its container, so
  the highlight says which port `f` forwards and the row shows its URL.
  `Enter` and `x` on a port act on its container. Ordinary init containers
  show no ports, by `ports::declared`'s rule.
- **Keys.** `f` forwards the highlighted port; `F` stops it, or dismisses a
  forward that stopped by itself. `f` on anything else (a container, a UDP
  port, a port already forwarded) puts a sentence on the status line. Outside
  this pane `f` still follows a log and `F` still retypes the field selector.
- **Always to the pod, on loopback, preferring the pod's port number.**
  The dashboard has no way to type `LOCAL:REMOTE` or `--address`; the CLI
  remains the tool for those and for following a `deploy/` or `svc/`.
- **Same machinery as the CLI.** `commands::forward::spawn_dashboard` runs
  the command's permission check, listener, per-connection streams, and
  watch loop. Their stderr lines go through a `Report` that, for the
  dashboard, is a channel of `k8s::forward::Event`s. Advice in those lines
  takes a `forward::Surface`, as `exec` messages do.
- **One thread per forward**, held by the event loop and stopped by
  dropping its handle. `App` decides which forwards should exist; the loop
  drops any handle `App` no longer wants before each frame.
- **A stopped forward stays on the strip** with its reason, since it may
  have stopped while nobody was looking. `f` on its port restarts it and
  `F` dismisses it. `c` clears every stopped forward from any pane, because
  a deleted pod's port row is gone with it; the strip's title offers `c`
  while there is one to clear.
- **Credentials.** A forward on the selected cluster that ends for want of
  credentials arms `L`, and a successful `L` restarts it. A forward on
  another cluster does neither, for decision 76's reason.
- **Quit hint.** The strip's title says forwards end when `eks` quits, and
  the armed quit counts them. `q quit` in the footer is unchanged, because
  a longer hint clips at 100 columns.

### 129. `eks control-plane-logs`: the AWS CLI's own paging, streams named for two types, JSON Lines, and a thirty-second follow window

- **Defaults.** `--type audit`, `--since 1h`. Audit is the question this
  command exists for; an hour keeps a busy cluster's audit log to a few pages.
  `--since` also takes an RFC 3339 instant.
- **Paging** uses the AWS CLI's `--max-items 5000` and `--starting-token`,
  following its `NextToken`. Each page is a CLI start-up and one step on the
  progress line. Events are collected, then printed in time order.
- **Streams.** `audit`, `authenticator`, and `scheduler` are read by stream
  prefix. `api` cannot be (every audit stream also starts `kube-apiserver-`)
  and `controller-manager` has two prefixes, so for those the group's streams
  are listed alongside `describe-cluster` and named, at most 100, newest
  first, with an hour's grace on CloudWatch's lazy `lastEventTimestamp`.
  `--follow` lists them again every minute.
- **`--grep`** is sent as one quoted filter-pattern term and then checked as
  a case-sensitive substring here, so the flag means one thing whatever the
  pattern language does with punctuation.
- **`--follow`** polls every 5 s from 30 s before the newest event printed,
  dropping repeats by event ID: control-plane instances deliver late, and
  starting at the newest timestamp exactly would skip an event stamped before
  it. Ctrl-C exits 0, as for port-forward; a closed stdout ends it quietly.
  A poll that is throttled, cannot reach AWS, or outlives `--timeout` is
  reported once and retried on the next tick; any other failure ends it.
- **`--json` is JSON Lines**, not one document: a follow never ends. Each
  line is `time`, `type`, `stream`, `id`, and `message`, an audit event's as
  an object.
- **Not enabled** is an error naming the types that are on and the exact
  `update-cluster-config` command; eks never runs it.
- **The AWS CLI never prompts:** `/dev/null` stdin, its own process group,
  `AWS_PAGER=""`. A profile that needs an MFA code typed fails rather than
  hangs.
- **Version.** AWS CLI v2. Only a usage error asks `aws --version`, so a
  version 1 that works keeps working.

### 130. The toolchain is pinned in `rust-toolchain.toml`, and CI installs it from there

CI used `dtolnay/rust-toolchain@stable`, so it moved to each Rust release the
day it shipped, while a session running `make check` had whatever stable was
installed locally. When 1.99 added clippy's `assert_is_empty`, PRs #97–#100
passed `make check` on 1.97 and failed CI, and each needed a follow-up commit.
`rust-toolchain.toml` now names one release (1.99.0, with clippy and
rustfmt). rustup reads it for every `cargo` here, and CI and the release build
install it with `rustup toolchain install`, not `@stable`. Dependabot's
`rust-toolchain` ecosystem proposes the next release as its own weekly PR, so
new lints are fixed in that PR and not in an unrelated nightly PR. The MSRV job
is unaffected because `scripts/msrv.sh` runs `cargo +<rust-version>`. That job
also exports the MSRV as `RUSTUP_TOOLCHAIN`, so rust-cache keys on the
toolchain that builds and the pin is never downloaded there.

### 131. `eks logs`: the cluster while the pod runs, Container Insights once it is gone, every CloudWatch line labelled

- **Two sources, chosen by the cluster.** `exec::find` (`locate`'s search,
  with "no such pod" as an answer) decides. A running pod is read from the
  API server only, as `kubectl logs` reads it; CloudWatch is asked only when
  no running pod in the namespace starts with the name. A prefix that matches
  several running pods is still `pick::ambiguous`'s error.
- **`-C` for the container**, not the roadmap's `-c`, for decision 124's
  reason; `-c` that does not resolve as a context gets the same hint.
- **The read.** `filter-log-events` on `/aws/containerinsights/<cluster>/application`
  (`log_group` in `config.toml`, `{cluster}` filled in), with a JSON pattern
  on `kubernetes.namespace_name` and `kubernetes.pod_name = "<prefix>*"`.
  Every container is read, so a wrong `-C` can be answered with the right
  ones. Text that is not `[a-z0-9.-]` is refused before any call, so nothing
  typed is ever escaped into a pattern. Stream names cannot narrow it: Fluent
  Bit puts the node first.
- **Resolution** is `exec`'s where CloudWatch can follow it: an exact name
  wins, a prefix of several gone pods lists them with when each last spoke.
  CloudWatch keeps no pod spec, so the default-container annotation is
  unreadable; several containers and no `-C` is a question, not a guess.
- **`--previous`.** On a running pod, a container whose `restartCount` is 0
  is refused before asking. In CloudWatch it is the instance (`docker_id`)
  before the last one the window holds. With `--follow` it is noted on stderr
  and the follow dropped, on both paths: that instance has stopped.
- **`--since`.** Unset reads every line the kubelet kept (no tail, unlike the
  dashboard's 200) and the last hour of CloudWatch, which the note on stderr
  says, with the `--since` that reaches further.
- **Labels.** Each CloudWatch line is `[cloudwatch <time>] <line>`, with
  `stderr` in the label for that stream, the label in the muted ink. API lines
  are printed as they are, so a pipe gets what `kubectl logs` would give it.
- **No group** (`ResourceNotFoundException`) says Container Insights is not
  set up and prints the `aws eks create-addon … amazon-cloudwatch-observability`
  command; eks never runs it. A missing group named by `log_group` points at
  the config instead.
- **Selectors** narrow the running pods only: Container Insights' default
  Fluent Bit config drops labels. A search that found nothing says so when
  `-l` or `--field-selector` was given.
- **Shared AWS handling.** `commands::cloudwatch` holds `Aws`, the login
  retry, and paging for both CloudWatch commands.
- **No `--json` yet.** Whether an API line gets a time (it has none unless
  `timestamps=true` is asked for) is the reviewer's call.
