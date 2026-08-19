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

`eks pods` lists one namespace — the context's own, unless `-n` names another —
or every namespace with `-A`:

```
$ eks pods -A
NAMESPACE    NAME                     READY  STATUS             RESTARTS    CPU   MEMORY  AGE  NODE
kube-system  aws-node-4kd9p           2/2    Running            0           14m   142Mi   12d  ip-10-0-1-9.ec2.internal
payments     api-7c9f6d4b8-x2vnq      1/1    Running            0           262m  576Mi   3h   ip-10-0-1-9.ec2.internal
payments     ledger-migrate-2hq4t     0/1    Init:1/2           0           -     -       42s  ip-10-0-11-200.ec2.internal
payments     reconcile-5d4b9-nzk8p    0/1    CrashLoopBackOff   9 (5m ago)  3m    18Mi    26m  ip-10-0-11-200.ec2.internal
storefront   checkout-6f7c8d9-pl4mn   0/1    Completed          0           -     -       2d   ip-10-0-1-9.ec2.internal
```

`STATUS` is the same derived word `kubectl get pods` shows, not the raw
`status.phase` — none of `CrashLoopBackOff`, `Init:1/2`, `Terminating`, or
`Completed` exists in the API, and they are the ones worth reading.

`RESTARTS` says when, not just how many. `9 (5m ago)` is a pod crashing now;
`9` on its own is a pod that crashed nine times last Tuesday and has been fine
since, and those are not the same problem. The time is the newest restart among
the containers the count covers, and a pod that has never restarted keeps a
plain `0`.

`CPU` and `MEMORY` are what the pod is actually doing, summed across its
containers from the same metrics-server the node table uses. They appear only
when that API answers — no metrics-server means no empty columns, just a note
under the table — and a pod it has not sampled yet reads `-` rather than a zero
that would look like an idle pod.

`--sort restarts` reorders the listing by that column, most recently restarted
first. Alphabetical order is the right one for reading a namespace and the wrong
one during an incident — the pod that restarted eight seconds ago sits wherever
its name puts it, among ninety-nine healthy ones. Pods that have never restarted
go to the end, and the count only breaks ties between restarts that happened at
the same moment. The default order is unchanged.

```sh
eks pods -A --sort restarts     # what is crashing right now, across the cluster
```

Narrow the listing with `-l` (labels) and `--field-selector` (fields), the same
selectors `kubectl` takes. The filtering happens on the API server, and a
selector that will not parse is rejected before anything connects, with the part
that is wrong quoted back:

```sh
eks pods -l app=api,tier notin (canary)     # by label
eks pods --field-selector status.phase!=Running   # only the ones that are not Running
```

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
| `--sort <ORDER>` | Order the pod listing: `name` (default) or `restarts` |
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
