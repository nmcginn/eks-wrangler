#!/usr/bin/env bash
# Time the real `eks` binary from exec to exit with hyperfine, and print the
# result as a Markdown table on stdout.
#
# `benches/startup.rs` measures the computation `eks` controls, inside a
# criterion process that never execs the binary. Most of what a user waits
# on is the part that skips: exec, the dynamic linker, and the binary's own
# startup before it reaches any of that computation. This measures that part
# (decision 112).
#
# Two commands are timed, both needing no terminal and no cluster:
#
# - `eks --version` is clap answering before `run` reads anything, so it is
#   close to the floor any Rust binary pays to start and exit.
# - `eks contexts` reads the same synthetic 50-cluster kubeconfig the
#   criterion benches parse, and prints its table.
#
# The gap between the two rows is the work `eks` itself does. Opening a real
# terminal (`ratatui::init()`'s raw mode and alternate screen) is not in
# either row, because CI has no TTY, and the output says so.
#
# Hyperfine's own progress goes to stderr, so a caller can append stdout to
# `$GITHUB_STEP_SUMMARY` and get only the table. The exit status is
# hyperfine's: it fails when a timed command exits non-zero, never over how
# long one took.
#
# `HYPERFINE` names the hyperfine to run (default: `hyperfine`), which is
# also how `scripts/tests/bench-startup.sh` checks what it is asked to do.
set -euo pipefail

hyperfine="${HYPERFINE:-hyperfine}"
here="$(cd "$(dirname "$0")" && pwd)"
fixture="$here/../benches/fixtures/kubeconfig-50.yaml"

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <path to a release eks binary>" >&2
  exit 2
fi

bin="$1"
if [ ! -x "$bin" ]; then
  echo "bench-startup: $bin is not an executable — build it first with \`cargo build --release\`" >&2
  exit 2
fi
if ! command -v "$hyperfine" > /dev/null 2>&1; then
  echo "bench-startup: hyperfine is not installed — install it with \`cargo install --locked hyperfine\` (CI pins the version in .github/workflows/ci.yml)" >&2
  exit 2
fi
if [ ! -f "$fixture" ]; then
  echo "bench-startup: the kubeconfig fixture $fixture is missing — restore it from git" >&2
  exit 2
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# An empty HOME, so a contributor's own `~/.config/eks/config.toml` (or a
# malformed one, which costs a warning) cannot make their numbers differ
# from CI's. KUBECONFIG pins the one file `eks contexts` reads.
mkdir -p "$work/home"
export HOME="$work/home"
KUBECONFIG="$(cd "$(dirname "$fixture")" && pwd)/$(basename "$fixture")"
export KUBECONFIG

# `--shell=none` spawns the binary directly: going through a shell would add
# the shell's own startup to every row, which for a command this fast is the
# noise hyperfine's own docs warn about. Without a shell hyperfine splits the
# command line itself, so the path is quoted for it.
quoted="$(printf '%q' "$bin")"

"$hyperfine" \
  --shell=none \
  --warmup 5 \
  --style basic \
  --export-markdown "$work/table.md" \
  --command-name 'eks --version' "$quoted --version" \
  --command-name 'eks contexts (50 clusters)' "$quoted contexts" \
  >&2

echo '### Process startup (hyperfine)'
echo
echo 'Wall-clock time from exec to exit of the release binary. Excludes opening a'
echo 'terminal (raw mode, alternate screen): CI has no TTY to open.'
echo
cat "$work/table.md"
