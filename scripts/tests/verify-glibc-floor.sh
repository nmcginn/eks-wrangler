#!/usr/bin/env bash
# Tests for scripts/verify-glibc-floor.sh, run by `make script-test`.
#
# No binary is built and no real readelf is run: `READELF` points the script at
# a stand-in that prints fixture output, so the checks run the same on a
# contributor's macOS laptop (which has no readelf) as in CI, and each case
# names exactly the version needs it is about.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../verify-glibc-floor.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The stand-in readelf: `--version-info` prints $work/version-info and
# `--dyn-syms` prints $work/dyn-syms, whatever binary it is pointed at.
cat > "$work/readelf" <<'STUB'
#!/usr/bin/env bash
dir="$(dirname "$0")"
case "$1" in
  --version-info) cat "$dir/version-info" ;;
  --dyn-syms) cat "$dir/dyn-syms" ;;
  *) echo "stub readelf: unexpected $1" >&2; exit 2 ;;
esac
STUB
chmod +x "$work/readelf"
export READELF="$work/readelf"
touch "$work/eks"

# Writes fixture readelf output for a binary importing each `symbol@GLIBC_x.y`
# given, in the layout real readelf uses for both sections.
fixture() {
  : > "$work/version-info"
  : > "$work/dyn-syms"
  {
    echo "Version needs section '.gnu.version_r' contains 1 entry:"
    echo " Addr: 0x0000000000000f10  Offset: 0x00000f10  Link: 7 (.dynstr)"
    echo "  000000: Version: 1  File: libc.so.6  Cnt: $#"
  } >> "$work/version-info"
  local i=0
  for sym in "$@"; do
    i=$((i + 1))
    printf '  0x%04x:   Name: %s  Flags: none  Version: %d\n' \
      "$((i * 16))" "${sym##*@}" "$((i + 1))" >> "$work/version-info"
    printf '    %2d: 0000000000000000     0 FUNC    GLOBAL DEFAULT  UND %s (%d)\n' \
      "$i" "$sym" "$((i + 1))" >> "$work/dyn-syms"
  done
}

failures=0
pass() { echo "ok   $1"; }
# Records a failure and shows the output that caused it, indented under it.
fail() {
  echo "FAIL $1" >&2
  while IFS= read -r line; do echo "     $line" >&2; done <<<"$output"
  failures=$((failures + 1))
}

# Runs the script and records its exit status and combined output.
run() {
  set +e
  output="$("$script" "$@" 2>&1)"
  status=$?
  set -e
}

expect_status() {
  local name="$1" want="$2"
  if [ "$status" -eq "$want" ]; then pass "$name"; else
    fail "$name: exit $status, wanted $want"; fi
}

expect_output() {
  local name="$1" needle="$2"
  if grep -qF -- "$needle" <<<"$output"; then pass "$name"; else
    fail "$name: output lacks '$needle'"; fi
}

refute_output() {
  local name="$1" needle="$2"
  if grep -qF -- "$needle" <<<"$output"; then
    fail "$name: output unexpectedly has '$needle'"
  else pass "$name"; fi
}

# --- The acceptance case: a binary built against the floor passes it.

fixture memcpy@GLIBC_2.14 getauxval@GLIBC_2.16 clock_gettime@GLIBC_2.17 malloc@GLIBC_2.2.5
run "$work/eks" 2.17
expect_status "a_binary_needing_exactly_its_floor_passes" 0
expect_output "a_passing_binary_reports_the_newest_version_it_needs" "needs at most glibc 2.17"

run "$work/eks" 2.34
expect_status "a_binary_older_than_a_higher_floor_passes" 0

# --- The failure the check exists for: a runner-glibc build.

fixture memcpy@GLIBC_2.14 pidfd_spawnp@GLIBC_2.39 pidfd_getpid@GLIBC_2.39 gnu_get_libc_version@GLIBC_2.34
run "$work/eks" 2.17
expect_status "a_binary_needing_more_than_its_floor_fails" 1
expect_output "a_failure_names_the_version_needed_and_the_floor" "needs glibc 2.39, newer than its declared floor of 2.17"
expect_output "a_failure_names_each_symbol_above_the_floor" "pidfd_spawnp@GLIBC_2.39"
expect_output "a_failure_names_every_offending_symbol_not_just_the_newest" "gnu_get_libc_version@GLIBC_2.34"
refute_output "a_failure_leaves_out_symbols_within_the_floor" "memcpy@GLIBC_2.14"
expect_output "a_failure_says_how_to_build_against_the_floor" "cargo zigbuild --target <triple>.2.17"

run "$work/eks" 2.34
expect_status "amazon_linux_2023s_floor_rejects_a_2_39_binary" 1
refute_output "a_symbol_exactly_at_the_floor_is_not_listed_as_an_offender" "gnu_get_libc_version@GLIBC_2.34"

# --- Versions compare as numbers, component by component.

fixture a@GLIBC_2.9 b@GLIBC_2.17
run "$work/eks" 2.17
expect_status "2_9_is_older_than_2_17_despite_sorting_after_it_as_text" 0

fixture a@GLIBC_2.3.4 b@GLIBC_2.2.5
run "$work/eks" 2.3
expect_status "a_three_part_version_above_a_two_part_floor_fails" 1
expect_output "a_three_part_version_is_reported_whole" "needs glibc 2.3.4"

fixture a@GLIBC_2.17
run "$work/eks" 2.17.0
expect_status "a_trailing_zero_component_changes_nothing" 0

fixture a@GLIBC_2.18
run "$work/eks" 2.17
expect_status "one_minor_version_over_the_floor_fails" 1

# --- Version tags that are not a glibc release say nothing about the floor.

fixture a@GLIBC_2.17 b@GLIBC_PRIVATE c@GCC_7.0.0
run "$work/eks" 2.17
expect_status "glibc_private_and_other_libraries_tags_are_ignored" 0

# --- Awkward inputs.

fixture
run "$work/eks" 2.17
expect_status "a_binary_with_no_glibc_needs_fails" 1
expect_output "a_binary_with_no_glibc_needs_is_asked_whether_it_is_a_gnu_build" "is it a -gnu build?"

run "$work/eks" latest
expect_status "a_floor_that_is_not_a_version_is_a_usage_error" 2
expect_output "a_bad_floor_says_what_a_good_one_looks_like" "pass one like 2.17"

run "$work/missing" 2.17
expect_status "a_missing_binary_is_a_usage_error" 2
expect_output "a_missing_binary_says_to_build_it" "build it first"

run "$work/eks"
expect_status "a_missing_floor_is_a_usage_error" 2

if [ "$failures" -ne 0 ]; then
  echo "$failures verify-glibc-floor test(s) failed" >&2
  exit 1
fi
echo "verify-glibc-floor: all tests passed"
