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

### Install script (macOS and Linux)

```sh
curl -fsSL https://raw.githubusercontent.com/nmcginn/eks-wrangler/master/scripts/install.sh | sh
```

It downloads the release build for your machine, checks it against the
release's published SHA-256 checksum, and only then installs `eks` into
`~/.local/bin`, with its man page and bash, zsh, and fish completions under
`~/.local/share`. If the checksum does not match, nothing is installed. Options
go after `sh -s --`:

```sh
curl -fsSL https://raw.githubusercontent.com/nmcginn/eks-wrangler/master/scripts/install.sh \
  | sh -s -- --version 0.2.0 --prefix /usr/local
```

`--version` pins a release (default: the latest), `--prefix` changes where it
goes, and `--target` overrides the detected build. It needs `curl` or `wget`,
`tar`, and `sha256sum` or `shasum`. Run it again to upgrade.

To verify a download by hand instead, fetch the tarball and its `.sha256` from
the [releases page](https://github.com/nmcginn/eks-wrangler/releases) and run
`sha256sum -c eks-<target>.tar.gz.sha256` beside them (`shasum -a 256 -c` on
macOS).

### Homebrew (macOS and Linux)

```sh
brew tap nmcginn/eks-wrangler https://github.com/nmcginn/eks-wrangler
brew install nmcginn/eks-wrangler/eks
```

The formula lives in this repository and is updated by the release workflow on
every tagged release; completions and the man page come with it.

Both need a published release: until the first one is tagged, build from
source.

### From source

```sh
git clone https://github.com/nmcginn/eks-wrangler
cd eks-wrangler
make install        # puts `eks` in ~/.cargo/bin
```

Requires Rust 1.90 or newer (the `rust-version` in `Cargo.toml`, which CI builds
and tests on).

Whichever way you install it, `eks` needs a kubeconfig —
`aws eks update-kubeconfig --name <cluster>` if you do not have one.

## Usage

```sh
eks                     # open the dashboard
eks contexts            # list available clusters
eks nodes               # list the nodes of the active cluster
eks pods -A             # list pods across every namespace
eks exec api            # a shell in the pod whose name starts with api
eks logs api            # its log, from CloudWatch once the pod is gone
eks port-forward svc/api  # the api service on localhost, following its pods
eks control-plane-logs    # who did what in the last hour, from the audit log
eks use staging         # switch cluster
eks current             # show the active cluster
eks nodes --json        # any read command, as JSON for scripts
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
NAME                         STATUS                       VERSION              CPU      CPU REQ      CPU USE      MEMORY         MEM REQ     MEM USE      PODS         AGE
ip-10-0-1-9.ec2.internal     Ready                        v1.33.1-eks-1a2b3c4  3920m/4  1500m (38%)  392m (10%)   14.8Gi/15.6Gi  6Gi (41%)   3.7Gi (25%)  21/58 (36%)  12d
ip-10-0-11-200.ec2.internal  NotReady,SchedulingDisabled  v1.32.9-eks-9f8e7d6  3920m/4  3800m (97%)  1200m (31%)  14.8Gi/15.6Gi  15Gi (96%)  4Gi (27%)    57/58 (98%)  10h

Usage is up to 12s old, averaged over 20s.
```

`CPU` and `MEMORY` are what the node has: allocatable — what pods may actually
ask for — over total capacity, the gap between them being what the kubelet
reserves for itself. `CPU REQ` and `MEM REQ` are what the pods already on the
node have booked, and what share of allocatable that is. The percentage, not the
capacity, is what decides whether the next pod schedules.

`PODS` is how many pods are on the node, out of how many it will accept. That
limit is the third reason a pod will not schedule, and the one no amount of
spare CPU fixes: on EKS it is usually the number of addresses the VPC CNI can
hand out on that instance type, so the second node above is one pod from full
whatever its cores are doing. The limit is spelled out rather than left to the
percentage, because it varies by instance type and by CNI configuration: `36%`
means something quite different on a node that takes 17 pods and one that takes
234.

The count is of the pods still occupying the node, which is the same set the
request columns are totalled from: a `Completed` Job holds no slot and no
memory, and is left out of both. So `PODS` and `CPU REQ` are always about the
same pods. When the pod listing fails, the count goes with it and the cell
reads `-/58` — the limit came back with the node and is still worth having.

`CPU USE` and `MEM USE` are what the node is actually doing, sampled from
metrics-server. They answer a different question from the request columns, and
the gap between the pair is the interesting part: the second node above has
booked 97% of its CPU and is using a third of it, which is a node full of
over-generous requests rather than a node that is busy.

Those two columns need the `metrics.k8s.io` API, which comes from
[metrics-server](https://github.com/kubernetes-sigs/metrics-server) — an add-on
EKS does not install for you. Without it the columns are simply absent and a note
under the table says so; the rest of the listing is unaffected.

There is a third case between those two, and it used to be silent: metrics-server
installed, answering, and with nothing to say yet — a fresh install, or a node
that joined a moment ago. The columns vanish exactly as they do when it is
missing, so the note says which of the two it is, because the advice is opposite:

```
CPU USE and MEM USE are not shown because nothing here has been sampled yet.
metrics-server answered for staging (eu-west-1), so it is installed — it has simply not got to anything in this listing.
A fresh install, or a node that has only just joined, takes a scrape interval or two to appear; if it stays empty, check the metrics-server pod in kube-system.
```

Where the columns *are* there, the line under the table says how old they are:
`Usage is up to 12s old, averaged over 20s.` A usage figure with nothing beside
it cannot be told from an instantaneous reading, and metrics-server going quiet
does not fail the request that asks it for a sample — the same table keeps
rendering, with figures that are minutes old and look exactly like fresh ones.
The age is the oldest sample in the listing, so it covers every row. Past a
couple of sampling windows the line says the figures are stale and where to
look:

```
Usage is up to 6m10s old, averaged over 20s — more than two sampling windows, so these figures are stale.
metrics-server can stop scraping without failing this request; check its pod in kube-system.
```

A node with a GPU — or anything else a device plugin advertises — gets a column
for it, and only a node group that has one puts it there:

```
$ eks nodes --context training
NAME                         STATUS  VERSION              CPU        CPU REQ      MEMORY         MEM REQ     PODS         NVIDIA.COM/GPU  AGE
ip-10-0-4-31.ec2.internal    Ready   v1.33.1-eks-1a2b3c4  15890m/16  12 (76%)     58.5Gi/62Gi    40Gi (68%)  9/234 (4%)   3/4 (75%)       6d
ip-10-0-4-77.ec2.internal    Ready   v1.33.1-eks-1a2b3c4  15890m/16  2 (13%)      58.5Gi/62Gi    8Gi (14%)   6/234 (3%)   0/4 (0%)        6d
ip-10-0-11-200.ec2.internal  Ready   v1.33.1-eks-1a2b3c4  3920m/4    1500m (38%)  14.8Gi/15.6Gi  6Gi (41%)   21/58 (36%)  -               12d
```

The cell is what the pods there have booked, out of what the node will hand out:
`3/4 (75%)` is one card free, and the arithmetic that decides whether the next
training job schedules. The last node is not a node with no cards free — it is a
node with no cards, which is a different answer to whoever is looking for
somewhere to put that job, so it reads `-` rather than `0/4`.

Only resources the cluster added get a *device* column. Kubernetes' own —
`cpu`, `memory`, `pods`, `hugepages-2Mi`, and the `attachable-volumes-*` limits
sitting in the same list — are left out of that rule, so a cluster with no
devices grows no columns from it. The first three have a heading of their own
instead, in the units a reader recognises; the rest have none yet.

The column shows what the node will *hand out*, which leaves one thing invisible
that the table exists to show: a card the kubelet has and is not offering,
usually one its plugin has marked unhealthy. That earns a line of its own:

```
ip-10-0-4-31.ec2.internal offers 3 of the 4 nvidia.com/gpu it reports.
A device a node has but will not offer is usually one its plugin marked unhealthy; check the device-plugin pods there, because a pod asking for the missing one will stay Pending.
```

`--sort` reorders the node listing too, by `name` (the default, unchanged),
`status`, `cpu`, `memory`, `cpu-requested`, `memory-requested`, `pods`, or
`age`, and `--sort-reverse` flips any of them:

```sh
eks nodes --sort status              # the NotReady node, first
eks nodes --sort cpu                 # the node closest to being full
eks nodes --sort cpu-requested       # the node the scheduler will refuse next
eks nodes --sort pods                # the node closest to its pod limit
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

The same three notes the node table carries appear here, worded the same way,
because they are facts about metrics-server rather than about either table:
`Usage is up to 12s old, averaged over 20s.` under a listing that has figures,
the staleness warning past a couple of sampling windows, and — where
metrics-server answered with nothing for these pods, which happens to a namespace
whose pods have only just started — a note saying it is installed and has not got
here yet, rather than the one telling you to install it.

`--sort` reorders the listing by a column. Alphabetical order is the right one
for reading a namespace and the wrong one during an incident — the pod that
restarted eight seconds ago, or the one burning a core, sits wherever its name
puts it among ninety-nine healthy ones. The orders are `name` (the default,
unchanged), `restarts`, `age`, `cpu`, `memory`, `cpu-share`, and
`memory-share`, and every one but `name` puts the row you went looking for
first: the newest restart, the youngest pod, the largest figure, or the pod
furthest over its own request.

`cpu` and `memory` rank what a pod is *using*, not its share of what it asked
for. A pod at 400% of a 10m request is burning 40m and is nobody's problem; one
at 60% of four cores is eating the node. The percentage in the cell is about that
pod's own sizing; the figure beside it is what the listing is usually opened for.

`cpu-share` and `memory-share` rank that percentage instead — the pod furthest
over its own request first, whatever the raw figure is. That is the ordering
that finds the pod at 400% of a 10m request; `cpu` finds the one eating the
node. Pods with no request, and pods nobody has sampled, stay at the tail of
either.

```sh
eks pods -A --sort restarts     # what is crashing right now, across the cluster
eks pods --sort memory          # what is closest to being OOM-killed
eks pods --sort cpu-share       # what is furthest over what it asked for
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
selectors `kubectl` takes — and the same ones the dashboard's pod-drilldown
pane reads, so a selector means one thing across the tool. The filtering
happens on the API server, and a selector that will not parse is rejected
before anything connects, with the part that is wrong quoted back:

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

### A shell in a container

`eks exec` takes the start of a pod's name rather than the whole generated one,
picks the container, and finds the shell:

```sh
eks exec api                    # bash if the image has it, else sh
eks exec api -C sidecar         # a container other than the default
eks exec api -- env             # one command instead of a shell
echo hi | eks exec api -- cat   # piped input runs without a terminal
```

When `api` starts more than one pod's name, `eks` lists them with their
namespace and status and asks for more of the name; when none in the namespace
match, it says which namespace does have one. The container is the one the
pod's `kubectl.kubernetes.io/default-container` annotation names, else its only
one. The container flag is `-C`, not `kubectl`'s `-c`, which is `--context`
throughout `eks`.

A pod that is not running gets its phase and its recent events instead of a
session; an image with no shell at all gets the `kubectl debug` command that
attaches one. With a terminal at both ends the session is interactive — Ctrl-C
and window resizes go to the container, and your terminal is put back however
the session ends. `eks` exits with the remote command's own exit code, so
`eks exec api -- test -f /ready` works in a script.

The dashboard opens the same shell with `x`, on a highlighted container, a
highlighted pod (its default container), or the container whose log you are
reading. The dashboard stays on screen while `eks` checks that the pod is
running and that the image has a shell. If either check fails, the reason
appears above the footer and nothing else changes. Otherwise the shell takes
the terminal, and exiting it brings the dashboard back as you left it.

### A container's log, even after its pod is gone

`eks logs` finds the pod by the same rules as `eks exec` (the start of its
name, `-C` for a container other than the default) and prints its log:

```sh
eks logs api                  # every line the kubelet kept
eks logs api -f               # and keep printing, until Ctrl-C
eks logs api -p               # the instance before the last restart
eks logs api --since 15m      # only the last 15 minutes
```

While the pod is running, this is `kubectl logs`. When no running pod's name
starts with what you typed, because the pod was deleted, evicted, or
rescheduled under a new name, `eks` looks for its lines in the CloudWatch group
Container Insights writes to, `/aws/containerinsights/<cluster>/application`,
through the AWS CLI:

```
$ eks logs api-7d9f
No pod starting "api-7d9f" is running in namespace shop; api-7d9f8c6b5-xk2pq was. Reading app's lines in the last 1h from CloudWatch, /aws/containerinsights/prod/application. Older lines need `--since`, e.g. `--since 1d`.
[cloudwatch 2026-10-07T06:21:02Z] listening on :8080
[cloudwatch 2026-10-07T06:21:04Z stderr] panic: out of memory
```

Every line from CloudWatch carries the `[cloudwatch …]` label, so it is never
mistaken for the cluster's own; the note above them goes to stderr, so a pipe
gets the lines alone. CloudWatch is read for the last hour unless `--since`
says otherwise. A prefix that several gone pods started with lists them, with
when each was last heard from. CloudWatch keeps no pod spec, so a pod with
several containers needs `-C`. `-p` reads the instance before the last one
CloudWatch holds. `-f` keeps polling for lines still on their way.

A cluster without Container Insights gets the `aws eks create-addon` command
that sets it up; lines are only kept from then on. A cluster that ships
container logs to another group can name it in the config file, as
`log_group`.

### A pod, a service, or a deployment on localhost

`eks port-forward` listens on this machine and carries each connection to a
pod, printing a URL to click:

```sh
eks port-forward svc/api          # the service's one port, on the same number here
eks port-forward deploy/api 8080  # a deployment's pods, on their port 8080
eks port-forward api 9000:http    # the pod's port named http, on localhost:9000
eks port-forward svc/db :5432     # any free local port
```

```
$ eks port-forward svc/api
http://127.0.0.1:80 → svc/api port 80 (pod api-7d9f8c6b5-xk2pq port 8080)
Forwarding until Ctrl-C.
```

A service is forwarded by its own port numbers, which `eks` maps to each
pod's `targetPort` — a named one included — the way the service itself does.
With no port, the one the pod or service declares is used; when there are
several, `eks` lists them (name, number, protocol, and container or where each
goes) and asks which, or says how to name one when there is no terminal to ask
at. The local port is the remote's own number when that is free, any free port
when it is not — the line says why — and exactly the one you typed when you
typed one. Listeners are on 127.0.0.1 only unless `--address` says otherwise
(`--address localhost` adds `::1`; `0.0.0.0` is called out on the line).

Every connection gets its own stream to the pod, so a browser's parallel
requests do not queue behind one another. When the pod behind a `svc/` or
`deploy/` forward is deleted, replaced in a rollout, or stops being ready,
`eks` says so and moves to another ready pod; a connection that arrives in the
middle of that waits for the new pod rather than being dropped. A pod named
directly has no successor to move to, so when it goes, `eks` exits naming what
happened and the `deploy/` forward that would have followed it. Ctrl-C closes
every listener and exits 0.

Credentials come from the kubeconfig context itself, so whatever works for
`kubectl` works here. When they have expired, `eks` says so and tells you how to
refresh them instead of printing an HTTP status code — and, if the session it
needs is an IAM Identity Center one, offers to refresh it for you.

The dashboard forwards too. A pod's containers pane lists each container's
declared ports as rows of their own; `f` on one forwards it to localhost, on
the pod's own port number when that is free, and `F` stops it. Forwards run
in the background while you move around, listed in a strip above the footer
with the URL to click, how many connections each has open, and its last
error. A forward whose pod goes away, or that cannot start, says why in the
strip rather than interrupting you, and stays there until `c` clears it. They
all end when the dashboard quits.
For anything the dashboard does not offer — a chosen local port, another
address, or following a deployment or service through a rollout — use
`eks port-forward`.

### The control plane's own logs

`eks control-plane-logs` reads the logs EKS writes to CloudWatch for the
control plane: who did what (`audit`), why someone was refused
(`authenticator`), and the API server, controller manager, and scheduler.
The cluster, region, and AWS profile come from the context, and the reading
is done by the AWS CLI (version 2), which every EKS context already needs.

```sh
eks control-plane-logs                                  # the audit log, last hour
eks control-plane-logs --grep api-7f9c --since 1d       # everything done to one pod
eks control-plane-logs -t authenticator --grep denied   # why was I unauthorized?
eks control-plane-logs -t api -f                        # follow the API server's log
```

```
$ eks control-plane-logs --grep api-7f9c
2026-10-07T06:21:02Z  ci-deployer/github-actions  patch  deployments.apps shop/api  200
2026-10-07T06:21:04Z  Admin/alice  delete  pods shop/api-7f9c  200
```

Each audit event is one line: when, who (an IAM role and session rather than
its ARN), the verb, the object, and the response code — refusals and server
errors in colour. Other types print as the component wrote them, behind the
time. `--since` takes `30s`, `15m`, `2h`, `3d`, or an RFC 3339 time (default
`1h`); `--grep` keeps events containing the text exactly as written; `-f`
keeps printing new events until Ctrl-C; `--json` prints each event whole, one
JSON object per line.

EKS only writes the types someone has switched on. Asking for one that is off
says which are on and prints the `aws eks update-cluster-config` command that
would switch it on; `eks` never runs it, because CloudWatch charges for what it
ingests and stores. A missing `logs:FilterLogEvents` or `eks:DescribeCluster`
permission is named, and an expired Identity Center session gets the same
login offer as every other command.

### Long listings and long sessions

The token `aws eks get-token` prints is good for fifteen minutes. `eks` runs the
helper once when it connects, then runs it again when that token has a minute
left, so a slow listing on a large cluster, or a dashboard left open all
afternoon, keeps working without you seeing it happen. If the cluster refuses a
token partway through a listing, `eks` fetches a fresh one and asks for that
page again, so you keep the pages already read.

### Logging in

An EKS context authenticates by running `aws eks get-token`, and that command
fails the moment your IAM Identity Center session runs out — which for most
people is once a morning. `eks` checks before it asks the cluster for anything:

```
$ eks nodes
prod (us-east-1) needs a fresh login: profile "corp" signed out of IAM Identity Center 9h ago.
Log in now with `aws sso login --profile corp`? [Y/n]
```

Say yes and it runs that command, waits for the browser, and carries on with the
listing you asked for. Say no and it tells you the command to run yourself.

The check costs nothing: it reads `~/.aws/config` to find which profile the
context uses and which Identity Center session that profile authenticates
through, then reads the AWS CLI's own token cache under `~/.aws/sso/cache/` to
see when it expires. No network call, no subprocess, and nothing that runs
before the dashboard's first frame. A token with under a minute left counts as
expired — it would die partway through a paged listing otherwise.

Some rules worth knowing, because they are what stops this being annoying:

- **A browser never opens without a yes.** The question is only asked when both
  stdin and stderr are terminals. `eks nodes > nodes.txt` in a cron job gets the
  message it always got, never a prompt nobody is there to answer.
- **The question and the answer go to stderr.** `eks nodes | column -t` prints
  the same bytes on stdout it printed before.
- **Only Identity Center profiles are offered anything.** Static keys, a
  `credential_process`, an instance role: there is nothing to log in to, and you
  get the old message.
- **You are asked at most once per command.** If the cluster then refuses
  credentials the cache thought were live — a token revoked centrally still
  reads as valid locally — `eks` offers a login once more, but never after you
  have already said no.
- **`--login never` is exactly the old behaviour**, down to the error text. It
  does not even read `~/.aws`.

In the dashboard, the question is put once before the terminal is taken over. A
session that dies while it is open shows up as a failed refresh over the rows
you already had, with `L` on the footer: pressing it hands the terminal back,
logs in, takes it again, and refetches.

### Narrow terminals

The other end of `--wide`. Both tables are wider than an 80-column terminal on a
cluster with metrics-server, and a wrapped table is harder to read than a shorter
one, so when `eks` is printing to a terminal it drops columns until the row fits
it. Each table has its own order, and each keeps what it exists for until last:

| Table | Dropped, in order | Never dropped |
| --- | --- | --- |
| `eks nodes` | `VERSION`, `AGE`, `PODS`, the `REQ` pair, the `USE` pair, `CPU` and `MEMORY`, the device columns, `STATUS` | `NAME` |
| `eks pods` | `AGE`, `NODE`, the usage pair, `RESTARTS`, `READY`, `STATUS` | `NAME`, and `NAMESPACE` under `-A` |

`PODS` goes early, ahead of the `REQ` pair it belongs with, for two reasons: a
node runs out of CPU or memory long before it runs out of pod slots unless the
CNI's address budget is what is short, and a column added later should not be
what evicts `CPU REQ` and `MEM REQ` from every 80-column listing that has been
keeping them.

Columns that are read together leave together: `CPU/REQ` without `MEMORY/REQ`
beside it is half an answer, and an eye reading a row of pairs pairs the wrong
ones. On a GPU cluster the node table keeps `NVIDIA.COM/GPU` after `CPU` has
gone, because the card is what you came for and the cores were always going to
be there. `NAMESPACE` stays on a `-A` listing for the reason `NAME` does: under
`-A`, the pair is the pod's identity, and `coredns-abc` on its own names two
pods on a cluster running a copy of it somewhere else.

Nothing is dropped when the output is not a terminal. `eks pods | grep api` and
`eks nodes > nodes.txt` print the default table, byte for byte, whatever the
window that ran them looks like — a script's columns must not depend on a
terminal size it never sees. `--wide` also wins outright: it is a request for
more columns, not for a table that gets out of the way.

### JSON output

`--json` prints any read command — `eks contexts`, `eks current`, `eks nodes`,
`eks pods` — as one JSON document instead of a table, for `jq` and scripts:

```sh
eks nodes --json | jq -r '.nodes[] | select(.severity != "ok") | .name'
eks pods -A --json | jq '[.pods[] | select(.restarts > 5)] | length'
eks pods --json | jq '.pods[] | {name, cpu: .cpu.used, asked: .cpu.requested}'
```

The document is the same rows the table shows, spelled for a program:

- **Numbers in base units.** CPU is cores (`0.25`, not `250m`), memory and
  storage are bytes, pods and devices are counts. Whole numbers print as
  integers.
- **Instants, not ages.** `created_at` and `last_restart_at` are RFC 3339;
  `3d` was only ever true at the moment it was printed.
- **`null` where the table prints `-`.** A figure that could not be read is
  `null`, never `0`: a node running nothing has `"requested": 0`, a node whose
  pods could not be listed has `"requested": null`.
- **Every field, every time.** `--wide`'s columns are always there, and nothing
  is dropped for the terminal's width. `--sort`, `-l`, `-A` and the rest still
  choose and order the rows.

A listing wraps its rows with the `cluster` it read, and `notes`: the sentences
that explain a `null` across the board (metrics-server missing, pods not
listable) and how old the usage figures are. Notes are for a person reading a
script's log, not for matching on. Errors still go to stderr with a non-zero
exit, and stdout stays empty. `eks contexts --json` cannot be combined with
`-q`, which answers the same question a different way.

### Options

| Flag | Description |
| --- | --- |
| `-c, --context <NAME>` | Use a specific context for this invocation |
| `-n, --namespace <NS>` | Scope resources to a namespace. Falls back to the config file's `namespace`, then to the context's own |
| `-A, --all-namespaces` | List pods across every namespace (`eks pods`) |
| `-l, --selector <SEL>` | Filter pods by label selector (`eks pods`, and the dashboard's pod-drilldown pane), or narrow the pods a name is matched against (`eks exec`, `eks logs`, `eks port-forward`) |
| `--field-selector <SEL>` | Filter pods by field selector, with the same reach as `-l` |
| `--address <ADDR>` | Where `eks port-forward` listens: IPs, comma-separated, or `localhost` for both loopbacks. Default `127.0.0.1` |
| `--sort <ORDER>` | Order the listing. Pods: `name` (default), `restarts`, `age`, `cpu`, `memory`, `cpu-share`, `memory-share`. Nodes: `name` (default), `status`, `cpu`, `memory`, `cpu-requested`, `memory-requested`, `pods`, `age` |
| `--sort-reverse` | Reverse `--sort`; unrankable rows stay at the end. Either flag adds a line under the table naming the order |
| `--wide` | Add the extra columns `kubectl -o wide` shows. Pods: `IP`, `NOMINATED NODE`, `READINESS GATES`. Nodes: `INTERNAL-IP`, `EXTERNAL-IP`, `OS-IMAGE`, `KERNEL-VERSION`, `CONTAINER-RUNTIME` |
| `--json` | Print the listing as JSON instead of a table (`eks contexts`, `eks current`, `eks nodes`, `eks pods`). See [JSON output](#json-output). `eks control-plane-logs --json` prints one object per event, per line |
| `--kubeconfig <PATH>` | Override the kubeconfig search path |
| `--timeout <DURATION>` | How long to wait for any one request to the cluster, or any one AWS CLI run. Default `30s`; `0` waits for as long as it takes |
| `--refresh <DURATION>` | How often the dashboard refreshes its panes in the background. Falls back to the config file's `refresh`, then to `15s`; `0` turns automatic refresh off (`r` still refreshes on demand) |
| `--color <WHEN>` | `auto`, `always`, or `never`. Spelled `--colour` too. Falls back to the config file's `color`, then to `auto` |
| `--theme <THEME>` | `auto`, `dark`, or `light`. Falls back to the config file's `theme`, then to `auto` |
| `--login <WHEN>` | Whether to log in to AWS IAM Identity Center for you when the session has run out. `auto` (default) offers, `always` does it without asking, `never` just tells you what to run |
| `-v, --verbose` | Increase log verbosity (repeatable) |

`KUBECONFIG` is honoured, including multi-path values, with the same precedence
`kubectl` uses.

All of these are global, and they parse on either side of the subcommand:
`eks --context prod nodes` and `eks nodes --context prod` are the same command.

### Config file

`~/.config/eks/config.toml` sets defaults for four of the flags above, for
whoever is tired of typing `--color always` or `--refresh 5s` every time, and
names the CloudWatch group `eks logs` reads a gone pod's lines from:

```toml
color = "always"      # or "colour" — same as --color/--colour
theme = "light"        # same as --theme
refresh = "5s"         # same grammar as --refresh and --timeout
namespace = "payments" # same as --namespace/-n
log_group = "/aws/containerinsights/{cluster}/application" # the default; {cluster} is the cluster's name
```

Every key is optional, and so is the file itself — nothing changes if it does
not exist. Precedence is the flag, then the file, then the built-in default:
`eks --color never` wins over the file's `color = "always"`, which wins over
`auto`. A file that fails to parse — bad TOML, an unknown key, a `color`/
`theme` that is not one of its own accepted values — is not fatal: `eks`
warns and runs with the built-in defaults for whatever the flags did not set,
exactly as if the file were not there.

### Colour

The listings put colour on the cells worth looking at, and on nothing else. A
`NotReady` node, a `CrashLoopBackOff` pod, and a node at 92% of its allocatable
are written in red; a cordoned node and one at 80% are amber; a cell reading `-`
because a figure could not be read is greyed out, because that is an absence
rather than an alarm.

Everything that is fine is left alone. `Ready`, `Running`, and a node at 20% are
printed in whatever colour your terminal was already using — so on a healthy
cluster `eks nodes` emits no escape sequences at all, and every scrap of colour
on screen is a row somebody should look at.

By default colour appears only when stdout is a terminal. Pipe a listing
anywhere — `eks nodes | grep NotReady`, `eks pods > pods.txt` — and it is the
same bytes it was before colour existed, so nothing downstream has to strip
escapes it did not ask for. `NO_COLOR` and `TERM=dumb` turn it off as well.

`--color always` overrides all of that, which is what a pager wants:

```
$ eks nodes --color always | less -R
```

`--color never` overrides it the other way. Both are global flags, so they work
on either listing and on either side of the subcommand.

`eks contexts` is unaffected: none of its cells is a reading off a cluster, so
there is nothing there to colour.

These switches also govern the progress line described below, on the principle
that movement is ink: a `--color never`, a `NO_COLOR`, or a `TERM=dumb` that
asks for plain output gets plain output, not a plain table with a spinner over
it.

### Theme

`--color` decides *whether* a listing paints; `--theme` decides *which*
colours it — and the dashboard, which always paints — use. `auto`, the
default, first reads the `COLORFGBG` environment variable, which some
terminals and multiplexers set and most do not. When that says nothing, the
dashboard draws in dark, then asks the terminal itself for its background
colour (an OSC 11 query) and switches to light if the answer is a light
colour — so on a light terminal you may see one dark frame before it flips.
Terminals that do not answer simply stay dark. The one-shot listings
(`eks nodes`, `eks pods`, …) never ask, since they are done before an
answer could arrive, so they read `COLORFGBG` alone and otherwise assume
dark. `--theme light`/`--theme dark`, or the config file's own `theme`,
override all of this outright — reach for one of these if `auto` picked the
wrong theme, or to colour a listing for a light terminal that does not set
`COLORFGBG`:

```
$ eks --theme light
$ eks nodes --theme light --color always | less -R
```

Both themes are tuned to meet WCAG AA contrast for body text against the
background they assume: near-black on white for `light`, light grey on a
dark terminal default for `dark`. `eks` never paints a background of its
own — it trusts whatever your terminal already shows — so `--theme light`
on a terminal that is not actually light in colour will still look wrong;
the flag fixes a wrong guess, not a mismatched terminal.

### Big clusters, and slow ones

Listings are read in pages of 500 — the same chunk size `kubectl` uses — so a
cluster with ten thousand pods does not arrive as one enormous response. Nothing
about a smaller cluster changes: a first page that comes back short ends the
listing, so most clusters are still the single request they always were.

`--timeout` is the other half of that, and it is spent per request rather than
per command, so a cluster large enough to need several pages is not cut off for
its size:

```
$ eks nodes --timeout 5s
eks: prod (us-east-1) did not answer within 5s.
A private EKS endpoint only answers from inside its VPC or over a VPN. If the cluster is merely busy, allow it longer: `--timeout 10s`.
```

That is the failure the flag exists for: a private endpoint reached from outside
its VPC does not refuse the connection, it simply never answers. `--timeout 0`
restores the old behaviour of waiting indefinitely. It covers the kubeconfig's
credential helper as well as the requests after it, spent per step rather than
per command. A helper that outlives it is stopped, not left running in the
background. What it deliberately does *not* cover is `aws sso login`: that one
is waiting for a human at a browser, and cutting it off after thirty seconds
would be cutting off the thing you asked for.

While all that is happening, one line on stderr says what it is waiting for:

```
$ eks nodes
running aws eks get-token… 3s
reading 1,500 nodes, 12,000 pods, node metrics… 6s
```

Each line replaces the one before it, and the last of them is erased before the
table is printed. The credential helper is named because a command that sits
there for thirty seconds is usually sitting in `aws eks get-token` — a laptop
that has lost its route to its SSO endpoint waits there rather than failing —
and knowing that is the difference between waiting and going to look.

It is written **only when both stdout and stderr are terminals**. Pipe or
redirect a listing and there is no progress line anywhere: `eks nodes | grep
NotReady` and `eks nodes > nodes.txt` are the same bytes on stdout as before,
and stderr stays empty, so nothing has to be filtered out of a log. That holds
under `--color always` too — that flag is about how bytes are written, not
about who is reading them.

`-v` and `RUST_LOG` turn it off too. Those put a stream of log lines on stderr,
and a line that rewrites itself cannot share a row with them — so a run you are
debugging prints the logs you asked for and nothing over the top of them.

One thing it does not survive is Ctrl-C: interrupting a listing leaves its last
row on screen above your prompt. Nothing is left wedged — no colour, no mode
change, and the next command prints normally underneath it.

### Keys

| Key | Action |
| --- | --- |
| `Tab` | Switch focus between the cluster list and the detail pane |
| `j` / `k`, `↓` / `↑` | Move the highlight — or scroll a container's log, once you are drilled in that far |
| `Home` / `End` | Jump to first / last — or to the oldest / newest line of a log |
| `PageUp` / `PageDown` | Scroll a container's log a page at a time |
| `Enter` | Drill in — a node's pods, a pod's containers, a container's log |
| `Esc` | Back out one level; quits once there is nowhere left to back out to |
| `r` | Refresh the node pane now |
| `L` | Log in to AWS again — only offered when the pane is showing a credential failure |
| `x` | Open a shell in the highlighted pod or container, or the one whose log is open; `Esc` cancels while it is being checked. See [A shell in a container](#a-shell-in-a-container) |
| `f` | Toggle following a container's log — or, on a port in a pod's containers pane, forward it to localhost |
| `F` | Stop the highlighted port's forward, or dismiss one that stopped by itself |
| `c` | Clear forwards that stopped by themselves from the strip |
| `w` | Toggle line wrap in a container's log |
| `p` | Switch a container's log between its current instance and its previous one |
| `/` | Search a container's log — jumps to the nearest match, `Esc` clears it |
| `n` / `N` | Jump to the next / previous match, wrapping past either end |
| `q`, `Ctrl-C` | Quit |

Focus starts on the cluster list; the focused pane's border is highlighted.
`Enter` drills one level further into the detail pane each time: a node's
pods, a pod's containers, and a container's own log, followed live — with a
breadcrumb in the pane's own title all the way down (` Overview › <node> ›
<pod> › <container> `). `Esc` backs out one level at a time.

`-l`/`--field-selector` narrow the pods shown for every node you drill into,
the same selectors `eks pods` takes and validated the same way — a malformed
one is rejected before the dashboard opens, naming the part that is wrong:

```sh
eks --field-selector status.phase!=Running   # only the ones that are not Running, everywhere you drill in
```

A node whose pods are all filtered out reads as "no pods match", not as "this
node has none" — the selector is why the list is empty, and the pane says so.

## Shell completions

```sh
eks completions bash > /etc/bash_completion.d/eks
eks completions zsh > "${fpath[1]}/_eks"
eks completions fish > ~/.config/fish/completions/eks.fish
```

Also takes `elvish` and `powershell`. Generated straight from the same
definition `clap` parses your arguments with, so a flag it does not know about
here cannot appear in a completion either. `make dist` writes all three
alongside a man page into `dist/`.

## Development

```sh
make            # list available targets
make test       # run the suite — no cluster or credentials needed
make check      # format, lint, tests, docs; run this before pushing
make deny       # audit dependencies against deny.toml (needs cargo-deny)
make msrv       # build and test on the oldest supported Rust (needs rustup)
```

The toolchain is pinned in `rust-toolchain.toml`; rustup installs it the first
time you build here, so `make check` lints with exactly the clippy CI does.

`make check` is every CI job but two. The first, `make deny`, runs
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) over the dependency
tree — known vulnerabilities, licences, duplicate versions, and where crates come
from — and needs `cargo install --locked cargo-deny@0.20.2` plus network access
for the advisory database. Run it whenever you add or bump a dependency.

The second, `make msrv`, builds every target and runs the tests on the
`rust-version` declared in `Cargo.toml`, the oldest Rust `eks` promises to build
on. It needs that toolchain beside your usual one — the command tells you the
`rustup toolchain install` line if it is missing. Run it when you raise a
dependency or reach for a newer standard-library API.

The test suite never touches AWS. See [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md)
for the module map and testing approach, and [`CLAUDE.md`](CLAUDE.md) for the
priorities this project is built around.

Most changes here are written by Claude and land as one reviewed pull request per
night. [`docs/ROADMAP.md`](docs/ROADMAP.md) is the backlog that drives it.

## Licence

MIT — see [LICENSE](LICENSE).
