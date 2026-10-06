# Roadmap

The backlog for the nightly loop. Open tasks are ordered by priority: take the
highest unchecked one that fits in a single pull request.

Each task lists acceptance criteria. A task is done when they hold, tests cover
them, and `make check` passes. In the same PR, move it to **Done** as one line
naming what landed and its decision numbers. The PR and `docs/DECISIONS.md` hold
the detail, so this file stays short.

If a task is larger than one PR, land the smallest **complete** slice (one a
user meets as a finished change), tick nothing, and split the rest into new
tasks here. Split only at a seam: a surface this PR does not touch, or a
decision the reviewer should make first. An entry that exists only because the
last change stopped short of the bar in `CLAUDE.md` is unfinished work, not a
follow-up, and it belongs in the PR that raised it.

---

## Open

The people this is for meet EKS rarely, and usually because something is wrong
or because they are building something new. Each task below is judged by
whether it gets that person from "which pod is it?" to an answer without
reaching for `kubectl` or the AWS console, and without already knowing the
names of things.

### Milestone 7 — CloudWatch

- [ ] **Control-plane logs: `eks control-plane-logs`.** EKS writes the API
  server, audit, authenticator, controller-manager, and scheduler logs to
  `/aws/eks/<cluster>/cluster`, but only for the types someone has switched
  on. For the audience this tool is for, audit ("who deleted my pod?") and
  authenticator ("why am I unauthorized?") answer the questions `kubectl`
  cannot.
  *Decided (2026-10-02):* `eks` calls AWS by running the AWS CLI
  (`aws eks describe-cluster`, `aws logs filter-log-events`) and reading its
  JSON output, not through the SDK. This keeps the binary light and matches
  `aws sso login`. See decision 123.
  *Acceptance:* `--type audit|authenticator|api|controller-manager|scheduler`,
  plus `--since`, `--grep`, and `--follow`. Region, cluster name, and AWS
  profile come from the context, the profile through `aws::profile`, so no
  extra flags are needed. A type that is not enabled is reported with the
  exact `aws eks update-cluster-config` command that would enable it and a
  note that CloudWatch charges for ingestion. `eks` never turns logging on
  itself. Audit events print as one line each (time, user, verb, resource,
  response code), not raw JSON, and `--json` prints them whole. Missing
  `logs:FilterLogEvents` or `eks:DescribeCluster` permission names the
  action. A missing or too-old `aws` binary says which version is needed.
  Each call is a child process `--timeout` can kill, like the credential
  helper. Paging follows `nextToken` until `--since` is covered, with the
  progress line showing how far it has got. `--follow` polls
  `filter-log-events` from the last event's timestamp and drops repeats
  rather than using `start-live-tail`, whose output is meant for a person to
  read, not for `eks` to parse. Expired SSO sessions go through the same offer as every other
  command. Log-type selection, the not-enabled advice, and audit
  summarising are pure functions over recorded fixtures.

- [ ] **`eks logs`, with a CloudWatch fallback for pods that are gone.**
  `kubectl logs` cannot show a pod that has been deleted or rescheduled,
  which is exactly the pod being troubleshot. `eks logs <pod> [-c]
  [--previous] [--since] [--follow]` reads from the API while the pod exists
  and falls back to the Container Insights / Fluent Bit group
  (`/aws/containerinsights/<cluster>/application`, overridable in
  `config.toml`) when it does not.
  *Acceptance:* pod and container resolution reuse `eks exec`'s rules. A
  prefix that matches no live pod is looked up in CloudWatch by
  `kubernetes.pod_name`. Every line from CloudWatch is labelled as such, so a
  reader always knows which source they are looking at. No log group at all
  gets a message saying Container Insights is not set up, with a pointer to
  how to install it. CloudWatch is reached through the AWS CLI, as in the
  previous task (decision 123). Record parsing and the source decision are
  pure functions over fixtures.

- [ ] **Control-plane logs in the dashboard.** A cluster-level pane, opened
  from the sidebar, that shows the CLI's control-plane log view with the
  same type switch, `/` search, follow, and not-enabled advice, streamed off
  the render thread with cancellation, like the container-logs pane. The
  `aws` children it starts run non-interactively, under the same rule as the
  dashboard's credential helpers (decision 120). An expired session sets
  `credentials_lost` and offers `L`.

---

## Done

### Milestone 1 — Live cluster data

- [x] Async runtime and Kubernetes client bootstrap: `eks nodes`, clear credential errors. (9–12)
- [x] Quantity parsing, and node capacity and allocatable. (13, 14)
- [x] Pod requests per node, and utilisation percentages. (15, 16)
- [x] `eks pods` listing, with kubectl's STATUS derivation. (17, 18)
- [x] metrics-server usage for nodes. (20–22)
- [x] metrics-server usage for pods. (23–25)
- [x] Label and field selectors for `eks pods`. (19)
- [x] Carry the pod selectors into the dashboard. (57)
- [x] How long ago the last restart was: `9 (5m ago)`. (26)
- [x] Sort pods by how recently they restarted. (27, 28)
- [x] More `--sort` orderings, and `--sort-reverse`. (29–32)
- [x] Sort the node table too. (33–35)
- [x] Say which ordering a listing is in. (36)
- [x] Carry the sort note into the dashboard's panes. (58)
- [x] An ordering that ranked nothing says so. (37)
- [x] Point the "nothing ranked" note at what would fix it. (38)
- [x] Suggest only orderings that tell the rows apart; diagnose a vacuous one. (70, 71)
- [x] Carry the "nothing ranked" note into the dashboard's panes. (63)
- [x] Carry `--sort` into the dashboard: `s` cycles, `S` reverses. (58)
- [x] `--wide` for `eks pods` and `eks nodes`. (39)
- [x] What `--wide` means in a pane: pod facts in the pod-containers pane. (72)
- [x] A node's `--wide` facts in the pod-drilldown pane. (78)
- [x] Pod usage against its request. (40)
- [x] Pod requests as their own columns, including devices. (79)
- [x] Sort pods by share of request: `cpu-share`, `memory-share`. (81)
- [x] Dashboard bars divide by capacity. (52, 53)
- [x] Sampling window and age beside usage; an empty sample set says so. (41)
- [x] A stale sample gets ` (stale)` on its own cell. (82)
- [x] The same stale marker on the node pane's bars. (83)
- [x] Freshness and unsampled notes in the dashboard's panes. (54)
- [x] Extended resources (devices) in the node table. (42)
- [x] `--sort-resource` for the node table. (84)
- [x] `R` resource sort in the node pane. (85)
- [x] `--sort-resource` for `eks pods`. (86)
- [x] `R` resource sort in the pod-drilldown pane. (87)
- [x] `ephemeral-storage` and nonzero `hugepages-*` columns. (59)
- [x] Narrow mode for the node table. (46)
- [x] Narrow mode for the pod table; one width measurement for both. (47)
- [x] A sort note that names a column the terminal dropped. (88)
- [x] The same hidden-column note for `--sort-resource`. (89)
- [x] Pod count per node, `12/58 (21%)`, and `--sort pods`. (48)
- [x] Paginate the pod listing. (43)
- [x] Severity colour in the CLI tables, and `--color`. (49)
- [x] `eks contexts` honours `--color`. (73)
- [x] What "hot" means for a pod against its own request. (90)
- [x] Grade a pod's usage against its limit once over its request. (91)
- [x] Paginate node listings. (43)
- [x] Say that a listing is still arriving: the progress line. (92, 93)
- [x] `--timeout` covers the credential helper. (50)
- [x] Offer an AWS SSO login when the session has expired. (74–77)
- [x] Refresh a token that expires partway through a paged listing. (110, 114)
- [x] Kill the credential helper on timeout, rather than abandon it. (110, 114)
- [x] Leave the terminal tidy when a command is interrupted. (94)
- [x] A listing's footnotes are a pure function. (95)
- [x] `eks contexts` on the shared table renderer. (45)
- [x] Global flags before a subcommand.
- [x] A `--timeout` for cluster requests. (44)

### Milestone 2 — The dashboard

- [x] Node pane with live data, off the render thread. (51)
- [x] Background refresh: interval, `r`, and cluster switch. (55)
- [x] Drill from a node into its pods; focus model. (56, 62)
- [x] Drill from a pod into its containers. (60)
- [x] Pod detail: container requests and limits. (61)
- [x] Log viewing: streaming, cancellation, scrollback. (64–66)
- [x] Fuzzy search with `/`. (67, 68)
- [x] A container's previous log with `p`. (69)
- [x] Recent events in the pod-detail view. (80)
- [x] `/` filters the pod-detail view's events too. (96)
- [x] A node's pressure conditions in the pod-drilldown pane. (97)
- [x] Explain a failed pod listing in the node pane. (98)
- [x] Usage figures in the pod-drilldown pane. (99)
- [x] Refresh the pod-drilldown pane on the node pane's triggers. (100)
- [x] Edit the dashboard's selectors live with `l`/`F`. (101)
- [x] Search the container-logs pane. (102)

### Milestone 3 — Polish

- [x] Config file at `~/.config/eks/config.toml`. (103)
- [x] Light theme, `--theme`, and `COLORFGBG` detection. (104)
- [x] Detect the background with an OSC 11 query after first paint. (111, 115)
- [x] Startup budget benchmarks with criterion, reported in CI. (105)
- [x] Wall-clock startup benchmarks with hyperfine. (112, 116)
- [x] Shell completions and a man page. (106)
- [x] Golden-file rendering tests. (107)
- [x] Dashboard credential helpers never prompt; `L` runs the helper in the foreground and seeds a shared store. (120, 121)

### Milestone 4 — Distribution and hardening

- [x] Release targets: aarch64-linux-gnu, x86_64-linux-musl. (108)
- [x] glibc 2.17 floor for the `-gnu` binaries. (109)
- [x] Supply-chain checks with cargo-deny. (117)
- [x] MSRV verification job. (118)
- [x] Install story: checksum-verifying `install.sh` and a release-rendered `Formula/eks.rb`. (113, 119)

### Milestone 5 — Scripting

- [x] `--json` on every read command: `contexts`, `current`, `nodes`, `pods`. (122)

### Milestone 6 — Into the workload

- [x] `eks exec`: a shell or a command in a container, by pod prefix. (124)
- [x] Exec from the dashboard: `x` checks, then hands the terminal to the shell. (125, 126)
- [x] `eks port-forward`: pods, `svc/`, and `deploy/`, following a service's pods through a rollout. (127)
- [x] Container ports and forwards in the dashboard: `f`/`F` on a port, and a forwards strip. (128)

---

## Ideas, not yet scheduled

Pull these up into a milestone when they become the most valuable next thing.

- Multi-account support (the current design assumes one AWS account).
- Resource editing: scale a deployment, delete a pod, cordon/drain a node.
  Needs a confirmation-and-undo design first; destructive actions deserve care.
- Cost attribution per namespace or workload.
- CloudWatch metrics (Container Insights) beside metrics-server's figures.
- Watch-based incremental updates instead of polling.
- A `?` overlay listing every key in the current pane. The footer no longer
  fits at 100 columns: on the pod list `/ filter` is clipped there, and
  `x shell` appears only from about 112.
