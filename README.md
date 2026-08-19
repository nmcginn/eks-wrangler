# eks

[![Release build](https://github.com/nmcginn/eks-wrangler/actions/workflows/release.yml/badge.svg)](https://github.com/nmcginn/eks-wrangler/actions/workflows/release.yml)
[![CI](https://github.com/nmcginn/eks-wrangler/actions/workflows/ci.yml/badge.svg)](https://github.com/nmcginn/eks-wrangler/actions/workflows/ci.yml)

A fast, keyboard-driven explorer for AWS EKS clusters.

Browsing a cluster should feel like browsing a filesystem — immediate, obvious,
and pleasant to look at. `eks` aims to be the tool you reach for instead of
assembling `kubectl` incantations.

> **Status: early.** Cluster switching works today; live cluster data and the
> dashboard are being built out one pull request at a time. See
> [`docs/ROADMAP.md`](docs/ROADMAP.md).

## Install

From source, until there are published binaries:

```sh
git clone https://github.com/nmcginn/eks-wrangler
cd eks-wrangler
make install        # puts `eks` in ~/.cargo/bin
```

Requires a stable Rust toolchain (see `rust-version` in `Cargo.toml`) and a
kubeconfig — `aws eks update-kubeconfig --name <cluster>` if you do not have one.

## Usage

```sh
eks                     # open the dashboard
eks contexts            # list available clusters
eks nodes               # list the nodes of the active cluster
eks pods -A             # list pods across every namespace
eks use staging         # switch cluster
eks current             # show the active cluster
```

Clusters are listed by short name rather than ARN:

```
$ eks contexts
  NAME       REGION     NAMESPACE
  prod-use1  us-east-1  default
* staging    eu-west-1  payments
```

`eks use` takes either that short name or the full context name, and tells you
when a short name is ambiguous rather than picking one for you.

`eks nodes` is the first command that talks to a cluster. It uses whichever
context is active, or the one named by `--context`, which also accepts a short
cluster name:

```
$ eks nodes --context staging
NAME                         STATUS                       VERSION              CPU      CPU REQ      CPU USE      MEMORY         MEM REQ     MEM USE      AGE
ip-10-0-1-9.ec2.internal     Ready                        v1.33.1-eks-1a2b3c4  3920m/4  1500m (38%)  392m (10%)   14.8Gi/15.6Gi  6Gi (41%)   3.7Gi (25%)  12d
ip-10-0-11-200.ec2.internal  NotReady,SchedulingDisabled  v1.32.9-eks-9f8e7d6  3920m/4  3800m (97%)  1200m (31%)  14.8Gi/15.6Gi  15Gi (96%)  4Gi (27%)    10h
```

`CPU` and `MEMORY` are what the node has: allocatable — what pods may actually
ask for — over total capacity, the gap between them being what the kubelet
reserves for itself. `CPU REQ` and `MEM REQ` are what the pods already on the
node have booked, and what share of allocatable that is. The percentage, not the
capacity, is what decides whether the next pod schedules.

`CPU USE` and `MEM USE` are what the node is actually doing, sampled from
metrics-server. They answer a different question from the request columns, and
the gap between the pair is the interesting part: the second node above has
booked 97% of its CPU and is using a third of it, which is a node full of
over-generous requests rather than a node that is busy.

Those two columns need the `metrics.k8s.io` API, which comes from
[metrics-server](https://github.com/kubernetes-sigs/metrics-server) — an add-on
EKS does not install for you. Without it the columns are simply absent and a note
under the table says so; the rest of the listing is unaffected.

`--sort` reorders the node listing too, by `name` (the default, unchanged),
`status`, `cpu`, `memory`, `cpu-requested`, `memory-requested`, or `age`, and
`--sort-reverse` flips any of them:

```sh
eks nodes --sort status              # the NotReady node, first
eks nodes --sort cpu                 # the node closest to being full
eks nodes --sort cpu-requested       # the node the scheduler will refuse next
eks nodes --sort age --sort-reverse  # the node that has been up longest
```

The node orders rank by *share*, not by the raw figure: a two-core node at 95%
is closer to trouble than a sixty-four-core node burning twenty times as much and
sitting at 30%, and the node table already shows every figure as a percentage of
what the node can give out. `eks pods --sort cpu` ranks by the figure instead:
a pod's percentage is a share of what it asked for, which is whatever somebody
put in a manifest, so a pod at 400% of a 10m request is burning 40m and is not
the row you are looking for.

Nodes there is nothing to rank stay at the end under either direction, exactly as
they do for pods: a node metrics-server has not sampled is not the idlest node in
the cluster.

A reordered listing says which order it is in, on a line under the table beside
the metrics note — `Sorted by cpu, reversed.` A listing nobody reordered says
nothing, so the default output is exactly what it always was.

When an ordering ranks *nothing* — `--sort cpu` on a cluster with no
metrics-server, where there is no `CPU USE` column to sort by — a second line
says so: `Nothing here has cpu to sort by.` Without it the line above names an
ordering that did nothing, over rows the alphabet put in that order.

`--wide` adds the columns `kubectl get nodes -o wide` adds, on the end of the
table rather than in the middle of it, so the default listing is the same one
with its tail cut off:

```
$ eks nodes --wide
NAME                         STATUS    VERSION              CPU      CPU REQ      MEMORY         MEM REQ     AGE  INTERNAL-IP  EXTERNAL-IP  OS-IMAGE                      KERNEL-VERSION                   CONTAINER-RUNTIME
ip-10-0-1-9.ec2.internal     Ready     v1.33.1-eks-1a2b3c4  3920m/4  1500m (38%)  14.8Gi/15.6Gi  6Gi (41%)   12d  10.0.1.9     -            Amazon Linux 2023.9.20260714  6.1.148-172.265.amzn2023.x86_64  containerd://1.7.28
ip-10-0-11-200.ec2.internal  NotReady  v1.32.9-eks-9f8e7d6  3920m/4  3800m (97%)  14.8Gi/15.6Gi  15Gi (96%)  10h  10.0.11.200  -            Amazon Linux 2023.6.20251201  6.1.134-152.225.amzn2023.x86_64  containerd://1.7.25
```

`INTERNAL-IP` is the address in a target group and in a security-group rule, and
the one that finds the instance in the EC2 console — none of which the node name
will do. `OS-IMAGE` is the column that says a node group is a release behind the
rest of the cluster. A node in a private subnet has no `EXTERNAL-IP`, and a `-`
there is the healthy answer.

Nothing extra is fetched for any of it: every one of those fields came back with
the nodes, so `--wide` costs no request.

`eks pods` lists one namespace — the context's own, unless `-n` names another —
or every namespace with `-A`:

```
$ eks pods -A
NAMESPACE    NAME                    READY  STATUS            RESTARTS    CPU/REQ          MEMORY/REQ         AGE  NODE
kube-system  aws-node-4kd9p          2/2    Running           0           14m/25m (56%)    142Mi/256Mi (55%)  12d  ip-10-0-1-9.ec2.internal
payments     api-7c9f6d4b8-x2vnq     1/1    Running           0           262m/500m (52%)  576Mi/1Gi (56%)    3h   ip-10-0-1-9.ec2.internal
payments     ledger-migrate-2hq4t    0/1    Init:1/2          0           -                -                  42s  ip-10-0-11-200.ec2.internal
payments     reconcile-5d4b9-nzk8p   0/1    CrashLoopBackOff  9 (5m ago)  3m/250m (1%)     18Mi/512Mi (4%)    26m  ip-10-0-11-200.ec2.internal
storefront   checkout-6f7c8d9-pl4mn  0/1    Completed         0           -                -                  2d   ip-10-0-1-9.ec2.internal
```

`STATUS` is the same derived word `kubectl get pods` shows, not the raw
`status.phase` — none of `CrashLoopBackOff`, `Init:1/2`, `Terminating`, or
`Completed` exists in the API, and they are the ones worth reading.

`RESTARTS` says when, not just how many. `9 (5m ago)` is a pod crashing now;
`9` on its own is a pod that crashed nine times last Tuesday and has been fine
since, and those are not the same problem. The time is the newest restart among
the containers the count covers, and a pod that has never restarted keeps a
plain `0`.

`CPU/REQ` and `MEMORY/REQ` are what the pod is actually doing, against what it
asked for: `262m/500m (52%)` is a pod using about half its CPU request. The
figure is summed across the pod's containers from the same metrics-server the
node table uses, and the request is the same number `eks nodes` totals into that
node's `CPU REQ` — the scheduler's arithmetic, not a second sum, so the two
commands cannot disagree about one pod.

The request is the only denominator a pod has. `262m` on its own cannot be read:
a quarter of a core is fine, throttled, or a mistake depending entirely on what
the pod asked for, and it is the request a reader would go on to change. A
figure above 100% is shown as it is — that is the pod being throttled, or the one
about to be OOM-killed, which is the moment anybody reads the column for.

A pod that asked for nothing keeps a bare figure, and the heading drops to `CPU`
with it: there is no denominator, and `262m/0` is not a percentage of anything.
The columns appear only when metrics-server answers — no metrics-server means no
empty columns, just a note under the table — and a pod it has not sampled yet
reads `-` rather than a zero that would look like an idle pod.

`--sort` reorders the listing by a column. Alphabetical order is the right one
for reading a namespace and the wrong one during an incident — the pod that
restarted eight seconds ago, or the one burning a core, sits wherever its name
puts it among ninety-nine healthy ones. The orders are `name` (the default,
unchanged), `restarts`, `age`, `cpu`, and `memory`, and every one but `name` puts
the row you went looking for first: the newest restart, the youngest pod, the
largest figure.

`cpu` and `memory` rank what a pod is *using*, not its share of what it asked
for. A pod at 400% of a 10m request is burning 40m and is nobody's problem; one
at 60% of four cores is eating the node. The percentage in the cell is about that
pod's own sizing; the figure beside it is what the listing is usually opened for.

```sh
eks pods -A --sort restarts     # what is crashing right now, across the cluster
eks pods --sort memory          # what is closest to being OOM-killed
eks pods -A --sort age          # what has just rolled out
```

`--sort-reverse` flips that, for the other reading of the same column — the pod
using the *least* CPU, or the one that has been up longest:

```sh
eks pods --sort age --sort-reverse   # what has been running since before all this
```

Pods there is nothing to rank stay at the end under either direction. A pod that
has never restarted does not belong at the top of a restart ordering, and a pod
metrics-server has not sampled is not the idlest pod in the namespace — it is a
pod nobody has measured, which is a fact about the scraper rather than about the
pod.

A reordered listing says so under the table — `Sorted by restarts.`, or
`Sorted by age, reversed.` — because a sorted table and an unsorted one look
alike to anyone who did not type the command, and the unrankable tail makes a
reversed listing look like the ordering running the other way. A plain
`eks pods` says nothing, and prints what it always did.

If the ordering ranked no row at all — `--sort restarts` in a namespace where
nothing has ever crashed, `--sort cpu` with no metrics-server — a second line
says `Nothing here has restarts to sort by.` One ranked row is enough to silence
it: the row you went looking for is then at an end of the table, which is the job.

Note that `--sort age` prints the *youngest* first, which is the opposite way
round from `kubectl --sort-by=.metadata.creationTimestamp`. One rule across every
order here beat matching a different tool on one of them; `--sort-reverse` gives
you `kubectl`'s reading.

Narrow the listing with `-l` (labels) and `--field-selector` (fields), the same
selectors `kubectl` takes. The filtering happens on the API server, and a
selector that will not parse is rejected before anything connects, with the part
that is wrong quoted back:

```sh
eks pods -l app=api,tier notin (canary)     # by label
eks pods --field-selector status.phase!=Running   # only the ones that are not Running
```

`--wide` adds the three columns `kubectl get pods -o wide` has that this table
does not — `NODE` is here by default — in `kubectl`'s own order:

```
$ eks pods --wide
NAME                   READY  STATUS            RESTARTS    AGE  IP          NODE                         NOMINATED NODE               READINESS GATES
api-7c9f6d4b8-x2vnq    1/1    Running           0           3h   10.0.1.42   ip-10-0-1-9.ec2.internal     -                            1/1
ledger-migrate-2hq4t   0/1    Pending           0           42s  -           -                            ip-10-0-11-200.ec2.internal  -
reconcile-5d4b9-nzk8p  0/1    CrashLoopBackOff  9 (5m ago)  26m  10.0.11.87  ip-10-0-11-200.ec2.internal  -                            -
```

`IP` is the pod's VPC address on EKS, so it is what a target group holds and what
a security-group rule has to allow. `NOMINATED NODE` is the one case where a
`Pending` pod is not stuck — the scheduler is evicting something to make room,
and that is where the pod will land. `READINESS GATES` is the only way `READY`
can read `1/1` on a pod the cluster still calls unready: every container up, and
an external controller withholding its condition. A pod with no gates reads `-`
rather than `0/0`, which would suggest something unsatisfied where there is
nothing to satisfy.

Unlike the usage columns, the wide ones appear whatever is in them. You asked for
them; a column of `-` under `NOMINATED NODE` is the answer "nothing here is being
preempted", and dropping it would leave you unable to tell that from a flag that
did nothing.

Credentials come from the kubeconfig context itself, so whatever works for
`kubectl` works here. When they have expired, `eks` says so and tells you how to
refresh them instead of printing an HTTP status code.

### Options

| Flag | Description |
| --- | --- |
| `-c, --context <NAME>` | Use a specific context for this invocation |
| `-n, --namespace <NS>` | Scope resources to a namespace |
| `-A, --all-namespaces` | List pods across every namespace (`eks pods`) |
| `-l, --selector <SEL>` | Filter pods by label selector (`eks pods`) |
| `--field-selector <SEL>` | Filter pods by field selector (`eks pods`) |
| `--sort <ORDER>` | Order the listing. Pods: `name` (default), `restarts`, `age`, `cpu`, `memory`. Nodes: `name` (default), `status`, `cpu`, `memory`, `cpu-requested`, `memory-requested`, `age` |
| `--sort-reverse` | Reverse `--sort`; unrankable rows stay at the end. Either flag adds a line under the table naming the order |
| `--wide` | Add the extra columns `kubectl -o wide` shows. Pods: `IP`, `NOMINATED NODE`, `READINESS GATES`. Nodes: `INTERNAL-IP`, `EXTERNAL-IP`, `OS-IMAGE`, `KERNEL-VERSION`, `CONTAINER-RUNTIME` |
| `--kubeconfig <PATH>` | Override the kubeconfig search path |
| `-v, --verbose` | Increase log verbosity (repeatable) |

`KUBECONFIG` is honoured, including multi-path values, with the same precedence
`kubectl` uses.

### Keys

| Key | Action |
| --- | --- |
| `j` / `k`, `↓` / `↑` | Move |
| `Home` / `End` | Jump to first / last |
| `q`, `Esc`, `Ctrl-C` | Quit |

## Development

```sh
make            # list available targets
make test       # run the suite — no cluster or credentials needed
make check      # everything CI runs; run this before pushing
```

The test suite never touches AWS. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
for the module map and testing approach, and [`CLAUDE.md`](CLAUDE.md) for the
priorities this project is built around.

Most changes here are written by Claude and land as one reviewed pull request per
night. [`docs/ROADMAP.md`](docs/ROADMAP.md) is the backlog that drives it.

## Licence

MIT — see [LICENSE](LICENSE).
