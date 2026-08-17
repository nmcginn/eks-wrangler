# eks

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

### Options

| Flag | Description |
| --- | --- |
| `-c, --context <NAME>` | Use a specific context for this invocation |
| `-n, --namespace <NS>` | Scope resources to a namespace |
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
