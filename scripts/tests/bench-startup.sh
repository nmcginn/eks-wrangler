#!/usr/bin/env bash
# Tests for scripts/bench-startup.sh, run by `make script-test`.
#
# No hyperfine is installed and nothing is timed: `HYPERFINE` points the
# script at a stand-in that records the arguments and environment it was
# given and writes a fixture table where `--export-markdown` asks, so the
# checks run anywhere and say exactly what the script asks hyperfine to do.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../bench-startup.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The stand-in hyperfine: one argument per line into $work/args, the
# environment the timed commands would inherit into $work/env, and a table
# into whatever file follows `--export-markdown`. It exits with
# $STUB_STATUS (default 0), after printing progress the way hyperfine does.
cat > "$work/hyperfine" <<'STUB'
#!/usr/bin/env bash
dir="$(dirname "$0")"
printf '%s\n' "$@" > "$dir/args"
{ echo "HOME=$HOME"; echo "KUBECONFIG=$KUBECONFIG"; } > "$dir/env"
while [ "$#" -gt 0 ]; do
  if [ "$1" = "--export-markdown" ]; then
    printf '| Command | Mean [ms] |\n|:---|---:|\n| `eks --version` | 1.2 ± 0.1 |\n' > "$2"
  fi
  shift
done
echo "Benchmark 1: eks --version (stub progress)"
exit "${STUB_STATUS:-0}"
STUB
chmod +x "$work/hyperfine"
export HYPERFINE="$work/hyperfine"

# A stand-in eks, in a directory with a space in its name, so the quoting
# `--shell=none` needs is exercised rather than assumed.
mkdir -p "$work/release dir"
printf '#!/bin/sh\nexit 0\n' > "$work/release dir/eks"
chmod +x "$work/release dir/eks"
bin="$work/release dir/eks"

failures=0
pass() { echo "ok   $1"; }
# Records a failure and shows the output that caused it, indented under it.
fail() {
  echo "FAIL $1" >&2
  while IFS= read -r line; do echo "     $line" >&2; done <<<"$output"
  failures=$((failures + 1))
}

# Runs the script, recording its exit status, its stdout, and its stderr
# separately: which stream a line lands on is part of what is under test.
run() {
  rm -f "$work/args" "$work/env"
  set +e
  "$@" > "$work/stdout" 2> "$work/stderr"
  status=$?
  set -e
  stdout="$(cat "$work/stdout")"
  stderr="$(cat "$work/stderr")"
  output="$stdout"$'\n'"$stderr"
}

expect_status() {
  local name="$1" want="$2"
  if [ "$status" -eq "$want" ]; then pass "$name"; else
    fail "$name: exit $status, wanted $want"; fi
}

# expect_in <name> <text> <needle>: passes when <needle> appears in <text>.
expect_in() {
  local name="$1" haystack="$2" needle="$3"
  if grep -qF -- "$needle" <<<"$haystack"; then pass "$name"; else
    fail "$name: lacks '$needle'"; fi
}

refute_in() {
  local name="$1" haystack="$2" needle="$3"
  if grep -qF -- "$needle" <<<"$haystack"; then
    fail "$name: unexpectedly has '$needle'"
  else pass "$name"; fi
}

# expect_arg_pair <name> <flag> <value>: passes when hyperfine was given
# <value> as the argument immediately after <flag>, at least once.
expect_arg_pair() {
  local name="$1" flag="$2" value="$3"
  if awk -v f="$flag" -v v="$value" 'prev == f && $0 == v { found = 1 } { prev = $0 } END { exit !found }' "$work/args"; then
    pass "$name"
  else
    output="$(cat "$work/args")"
    fail "$name: no '$flag' followed by '$value'"
  fi
}

# --- The acceptance case: the real binary is timed, and the table reported.

run "$script" "$bin"
expect_status "a_successful_run_exits_zero" 0
args="$(cat "$work/args")"
quoted="$(printf '%q' "$bin")"
expect_arg_pair "eks_version_is_timed_on_the_binary_given" "eks --version" "$quoted --version"
expect_arg_pair "eks_contexts_is_timed_on_the_binary_given" "eks contexts (50 clusters)" "$quoted contexts"
expect_in "commands_are_spawned_without_a_shell_in_between" "$args" "--shell=none"
expect_in "runs_are_warmed_up_before_timing" "$args" "--warmup"

# The quoting must round-trip: whatever hyperfine's own shell-words split
# does to the command line, bash's does the same for `%q` output, and the
# first word has to come back as the path exactly.
eval "set -- $quoted --version"
if [ "$1" = "$bin" ] && [ "$2" = "--version" ]; then
  pass "a_binary_path_with_a_space_splits_back_into_one_word"
else
  output="$quoted"
  fail "a_binary_path_with_a_space_splits_back_into_one_word"
fi

env="$(cat "$work/env")"
fixture="$(cd "$here/../../benches/fixtures" && pwd)/kubeconfig-50.yaml"
expect_in "contexts_reads_the_same_fixture_the_criterion_benches_parse" "$env" "KUBECONFIG=$fixture"
refute_in "a_contributors_own_home_cannot_colour_the_numbers" "$env" "HOME=$HOME"

expect_in "the_table_lands_on_stdout" "$stdout" '| `eks --version` | 1.2 ± 0.1 |'
expect_in "stdout_is_headed_for_the_job_summary" "$stdout" "### Process startup (hyperfine)"
expect_in "the_report_says_it_excludes_opening_a_terminal" "$stdout" "Excludes opening a"
refute_in "hyperfines_progress_stays_off_stdout" "$stdout" "stub progress"
expect_in "hyperfines_progress_still_reaches_the_log_on_stderr" "$stderr" "stub progress"

# The row is labelled "50 clusters"; the fixture has to keep that promise.
contexts="$(grep -c '^      cluster: arn:' "$fixture" || true)"
if [ "$contexts" -eq 50 ]; then pass "the_fixture_has_the_fifty_contexts_its_row_is_labelled_with"; else
  output="$contexts contexts"
  fail "the_fixture_has_the_fifty_contexts_its_row_is_labelled_with"; fi

# --- A timed command that fails is a failed job; a slow one never is.

STUB_STATUS=1 run "$script" "$bin"
expect_status "a_command_hyperfine_saw_fail_fails_the_script" 1
refute_in "a_failed_run_reports_no_table" "$stdout" "### Process startup"

# --- Awkward inputs.

run "$script" "$work/missing/eks"
expect_status "a_missing_binary_is_a_usage_error" 2
expect_in "a_missing_binary_says_to_build_it" "$stderr" "build it first with \`cargo build --release\`"

HYPERFINE="$work/no-such-hyperfine" run "$script" "$bin"
expect_status "a_missing_hyperfine_is_a_usage_error" 2
expect_in "a_missing_hyperfine_says_how_to_install_it" "$stderr" "cargo install --locked hyperfine"

run "$script"
expect_status "no_binary_argument_is_a_usage_error" 2
expect_in "no_binary_argument_prints_usage" "$stderr" "usage:"

# A copy of the script with no benches/ beside it stands in for a checkout
# that lost the fixture: timing `eks contexts` against no kubeconfig would
# measure an error path and report it as startup.
mkdir -p "$work/bare/scripts"
cp "$script" "$work/bare/scripts/"
run "$work/bare/scripts/bench-startup.sh" "$bin"
expect_status "a_missing_fixture_is_a_usage_error" 2
expect_in "a_missing_fixture_says_to_restore_it" "$stderr" "restore it from git"
if [ -e "$work/args" ]; then
  output="hyperfine ran"
  fail "a_missing_fixture_times_nothing"
else
  pass "a_missing_fixture_times_nothing"
fi

if [ "$failures" -ne 0 ]; then
  echo "$failures bench-startup test(s) failed" >&2
  exit 1
fi
echo "bench-startup: all tests passed"
