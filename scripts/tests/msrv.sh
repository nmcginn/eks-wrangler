#!/usr/bin/env bash
# Tests for scripts/msrv.sh, run by `make script-test`.
#
# Nothing is compiled and no toolchain is installed: `CARGO` and `RUSTUP`
# point the script at stand-ins that record what they were asked to do and
# answer from environment variables, so the checks run anywhere and say
# exactly which toolchain the script builds on and with which flags.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../msrv.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# The stand-in cargo. `metadata` prints $STUB_METADATA (or fails when
# $STUB_METADATA_STATUS is non-zero); anything else is appended to
# $work/cargo-calls, one call per line, and exits with $STUB_BUILD_STATUS
# for `build` and $STUB_TEST_STATUS for `test`.
cat > "$work/cargo" <<'STUB'
#!/usr/bin/env bash
dir="$(dirname "$0")"
if [ "$1" = "metadata" ]; then
  if [ "${STUB_METADATA_STATUS:-0}" -ne 0 ]; then
    echo "error: failed to parse manifest (stub)" >&2
    exit "$STUB_METADATA_STATUS"
  fi
  printf '%s\n' "$STUB_METADATA"
  exit 0
fi
echo "$* (in $PWD)" >> "$dir/cargo-calls"
case "$2" in
  build) exit "${STUB_BUILD_STATUS:-0}" ;;
  test) exit "${STUB_TEST_STATUS:-0}" ;;
esac
STUB
chmod +x "$work/cargo"
export CARGO="$work/cargo"

# The stand-in rustup: records its arguments and the auto-install setting it
# ran under, and reports the toolchain as installed unless
# $STUB_MISSING_TOOLCHAIN is set, in which case it fails the way rustup does.
cat > "$work/rustup" <<'STUB'
#!/usr/bin/env bash
dir="$(dirname "$0")"
echo "$* RUSTUP_AUTO_INSTALL=${RUSTUP_AUTO_INSTALL:-unset}" >> "$dir/rustup-calls"
if [ -n "${STUB_MISSING_TOOLCHAIN:-}" ]; then
  echo "error: toolchain '$2' is not installed (stub)" >&2
  exit 1
fi
echo "rustc $2.0 (stub)"
STUB
chmod +x "$work/rustup"
export RUSTUP="$work/rustup"

# What `cargo metadata --no-deps` prints, cut down to the fields around the
# one the script reads.
metadata_with() {
  printf '{"packages":[{"name":"eks","version":"0.1.0","edition":"2024","rust_version":%s}],"workspace_members":[]}' "$1"
}
export STUB_METADATA
STUB_METADATA="$(metadata_with '"1.90"')"

root="$(cd "$here/../.." && pwd)"

failures=0
pass() { echo "ok   $1"; }
# Records a failure and shows the output that caused it, indented under it.
fail() {
  echo "FAIL $1" >&2
  while IFS= read -r line; do echo "     $line" >&2; done <<<"$output"
  failures=$((failures + 1))
}

# Runs the script, recording its exit status, its stdout, its stderr, and
# which tools it called, each separately.
run() {
  rm -f "$work/cargo-calls" "$work/rustup-calls"
  set +e
  "$@" > "$work/stdout" 2> "$work/stderr"
  status=$?
  set -e
  stdout="$(cat "$work/stdout")"
  stderr="$(cat "$work/stderr")"
  calls="$(cat "$work/cargo-calls" 2> /dev/null || true)"
  rustup_calls="$(cat "$work/rustup-calls" 2> /dev/null || true)"
  output="$stdout"$'\n'"$stderr"$'\n'"cargo: $calls"$'\n'"rustup: $rustup_calls"
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

expect_equal() {
  local name="$1" got="$2" want="$3"
  if [ "$got" = "$want" ]; then pass "$name"; else
    fail "$name: got '$got', wanted '$want'"; fi
}

# --- --print: what CI's toolchain step installs.

run "$script" --print
expect_status "print_exits_zero" 0
expect_equal "print_writes_exactly_the_declared_version" "$stdout" "1.90"
expect_equal "print_builds_nothing" "$calls" ""

STUB_METADATA="$(metadata_with '"1.85.1"')" run "$script" --print
expect_equal "a_three_part_version_is_printed_as_declared" "$stdout" "1.85.1"

# The real manifest, read by the real cargo: the stand-in above proves what
# the script does with cargo's answer, and this proves cargo still answers
# in the shape the script reads.
if command -v cargo > /dev/null 2>&1; then
  real="$(CARGO=cargo "$script" --print 2>&1 || true)"
  declared="$(sed -n 's/^rust-version *= *"\([^"]*\)".*/\1/p' "$root/Cargo.toml")"
  if [ -n "$declared" ] && [ "$real" = "$declared" ]; then
    pass "the_real_manifests_rust_version_is_what_print_reports"
  else
    output="--print said '$real', Cargo.toml says '$declared'"
    fail "the_real_manifests_rust_version_is_what_print_reports"
  fi
fi

# README.md names the version for people installing from source, which is a
# second copy of it; this keeps a raised rust-version from leaving the
# README promising an older Rust than CI proves.
declared="$(sed -n 's/^rust-version *= *"\([^"]*\)".*/\1/p' "$root/Cargo.toml")"
readme="$(grep -o 'Requires Rust [0-9.]* or newer' "$root/README.md" || true)"
expect_equal "the_readme_names_the_declared_msrv" "$readme" "Requires Rust $declared or newer"

# --- The acceptance case: every target builds, then the tests run, on the
# declared toolchain, against the committed Cargo.lock.

run "$script"
expect_status "a_toolchain_that_builds_and_passes_exits_zero" 0
build_call="$(sed -n 1p <<<"$calls")"
test_call="$(sed -n 2p <<<"$calls")"
expect_in "the_build_runs_on_the_declared_toolchain" "$build_call" "+1.90 build"
expect_in "the_build_covers_every_target" "$build_call" "--all-targets"
expect_in "the_build_uses_the_committed_lockfile" "$build_call" "--locked"
expect_in "the_tests_run_on_the_declared_toolchain_after_the_build" "$test_call" "+1.90 test"
expect_in "the_tests_use_the_committed_lockfile" "$test_call" "--locked"
expect_in "cargo_runs_from_the_repository_root" "$build_call" "(in $root)"
expect_in "a_pass_says_which_version_it_proved" "$stdout" "on Rust 1.90."
expect_in "the_toolchain_is_asked_about_without_installing_it" "$rustup_calls" "run 1.90 rustc --version RUSTUP_AUTO_INSTALL=0"

# --- A compiler too old for the tree fails the job, and says what to do.

STUB_BUILD_STATUS=101 run "$script"
expect_status "a_build_failure_fails_the_script" 1
expect_in "a_build_failure_names_the_msrv_it_broke" "$stderr" "no longer builds on Rust 1.90"
expect_in "a_build_failure_says_how_to_hold_a_dependency_back" "$stderr" "cargo update <crate> --precise <version>"
expect_in "a_build_failure_says_raising_the_msrv_is_recorded" "$stderr" "record why in docs/DECISIONS.md"
refute_in "a_build_failure_runs_no_tests" "$calls" "+1.90 test"
refute_in "a_build_failure_claims_no_pass" "$stdout" "passes its tests on"

STUB_TEST_STATUS=101 run "$script"
expect_status "a_test_failure_fails_the_script" 1
expect_in "a_test_failure_is_worded_as_one" "$stderr" "no longer passes its tests on Rust 1.90"

# --- Awkward inputs: each is a usage error that says what to do, and none
# of them starts a build.

STUB_MISSING_TOOLCHAIN=1 run "$script"
expect_status "a_missing_toolchain_is_a_usage_error" 2
expect_in "a_missing_toolchain_says_how_to_install_it" "$stderr" "rustup toolchain install 1.90 --profile minimal"
refute_in "a_missing_toolchain_hides_rustups_own_noise" "$stderr" "(stub)"
expect_equal "a_missing_toolchain_builds_nothing" "$calls" ""

RUSTUP="$work/no-such-rustup" run "$script"
expect_status "no_rustup_is_a_usage_error" 2
expect_in "no_rustup_says_where_to_get_it" "$stderr" "https://rustup.rs"
expect_equal "no_rustup_builds_nothing" "$calls" ""

# A missing rust-version reads as null in cargo's JSON; `--print` must not
# hand CI's toolchain step an empty string to install.
STUB_METADATA="$(metadata_with null)" run "$script" --print
expect_status "no_declared_msrv_is_a_usage_error" 2
expect_in "no_declared_msrv_says_where_to_declare_one" "$stderr" "Add one under [package]"
expect_equal "no_declared_msrv_prints_no_version" "$stdout" ""

STUB_METADATA="$(metadata_with null)" run "$script"
expect_status "no_declared_msrv_verifies_nothing" 2
expect_equal "no_declared_msrv_builds_nothing" "$calls" ""

STUB_METADATA_STATUS=101 run "$script" --print
expect_status "an_unreadable_manifest_is_a_usage_error" 2
expect_in "an_unreadable_manifest_keeps_cargos_reason" "$stderr" "failed to parse manifest (stub)"
expect_in "an_unreadable_manifest_says_to_fix_it_first" "$stderr" "fix the manifest error above first"

run "$script" --verbose
expect_status "an_unknown_flag_is_a_usage_error" 2
expect_in "an_unknown_flag_prints_usage" "$stderr" "usage:"

run "$script" --print extra
expect_status "an_extra_argument_is_a_usage_error" 2

if [ "$failures" -ne 0 ]; then
  echo "$failures msrv test(s) failed" >&2
  exit 1
fi
echo "msrv: all tests passed"
