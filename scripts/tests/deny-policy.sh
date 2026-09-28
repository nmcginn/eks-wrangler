#!/usr/bin/env bash
# Tests for deny.toml, run by `make deny` and CI's `supply-chain` job.
#
# `cargo deny check` passing on the real tree only proves the policy lets in
# what we already have — an empty allow-everything file would pass too. These
# cases point the real deny.toml at small fixture workspaces, each depending on
# a local crate built to break exactly one rule, and check that the rule
# fires. Every dependency is a path or a local git repository, so nothing here
# touches crates.io or the advisory database, and nothing is compiled: `cargo
# deny` reads the graph from `cargo metadata`.
#
# Needs cargo-deny on PATH. It is not part of `make check` for that reason
# (see decision 117).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
policy="$here/../../deny.toml"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

if ! command -v cargo-deny >/dev/null 2>&1; then
  echo "deny-policy: cargo-deny is not installed." >&2
  echo "Install the version CI uses: cargo install --locked cargo-deny@0.20.2" >&2
  exit 2
fi

# Writes a crate at $work/<dir> with the given package name, version, and
# licence expression, and nothing in it but an empty lib.rs.
crate() {
  local dir="$1" name="$2" version="$3" license="$4"
  mkdir -p "$work/$dir/src"
  : > "$work/$dir/src/lib.rs"
  cat > "$work/$dir/Cargo.toml" <<EOF
[package]
name = "$name"
version = "$version"
edition = "2021"
license = "$license"
publish = false
EOF
}

# Writes the fixture root at $work/root, MIT like `eks` itself, with each
# argument as one line of its [dependencies] table.
root() {
  rm -rf "$work/root"
  crate root fixture 0.1.0 MIT
  {
    echo
    echo "[dependencies]"
    for dep in "$@"; do echo "$dep"; done
  } >> "$work/root/Cargo.toml"
}

failures=0
pass() { echo "ok   $1"; }
# Records a failure and shows the output that caused it, indented under it.
fail() {
  echo "FAIL $1" >&2
  while IFS= read -r line; do echo "     $line" >&2; done <<<"$output"
  failures=$((failures + 1))
}

# Runs one `cargo deny check` section against the fixture root under the real
# policy, recording its exit status and combined output.
check() {
  section="$1"
  set +e
  output="$(cargo deny --color never --manifest-path "$work/root/Cargo.toml" \
    --config "$policy" check "$1" 2>&1)"
  status=$?
  set -e
}

expect_pass() {
  if [ "$status" -eq 0 ]; then pass "$1"; else fail "$1: exit $status, wanted 0"; fi
}

expect_reject() {
  local name="$1" needle="$2"
  # The exit status is a bitmask of failed checks, so a bans failure (2) and
  # a usage error (2) look the same; cargo-deny's own "<check> FAILED" line is
  # what says the policy, not the invocation, did the rejecting.
  if [ "$status" -eq 0 ]; then
    fail "$name: passed, wanted a rejection"
  elif ! grep -qF -- "$section FAILED" <<<"$output"; then
    fail "$name: exit $status, but $section did not report FAILED"
  elif ! grep -qF -- "$needle" <<<"$output"; then
    fail "$name: rejected, but output lacks '$needle'"
  else
    pass "$name"
  fi
}

# --- Licences.

crate permissive permissive 1.0.0 "MIT OR Apache-2.0"
root 'permissive = { path = "../permissive", version = "1.0.0" }'
check licenses
expect_pass "a_permissively_licensed_dependency_is_accepted"

crate copyleft copyleft 1.0.0 "GPL-3.0-only"
root 'copyleft = { path = "../copyleft", version = "1.0.0" }'
check licenses
expect_reject "a_gpl_dependency_is_rejected" "copyleft"

crate agpl agpl 1.0.0 "AGPL-3.0-or-later"
root 'agpl = { path = "../agpl", version = "1.0.0" }'
check licenses
expect_reject "an_agpl_dependency_is_rejected" "agpl"

# option-ext's MPL-2.0 exception is for option-ext alone.
crate option-ext option-ext 0.2.0 "MPL-2.0"
root 'option-ext = { path = "../option-ext", version = "0.2.0" }'
check licenses
expect_pass "the_mpl_exception_admits_option_ext"

crate mpl other-mpl 1.0.0 "MPL-2.0"
root 'other-mpl = { path = "../mpl", version = "1.0.0" }'
check licenses
expect_reject "the_mpl_exception_admits_no_other_crate" "other-mpl"

# A choice of licences passes if any one of them is allowed — r-efi's
# "MIT OR Apache-2.0 OR LGPL-2.1-or-later" is exactly this shape.
crate either either-way 1.0.0 "MIT OR LGPL-2.1-or-later"
root 'either-way = { path = "../either", version = "1.0.0" }'
check licenses
expect_pass "an_or_expression_passes_on_its_allowed_alternative"

# Both halves of an AND have to be allowed — ring's "Apache-2.0 AND ISC" is
# fine, but a permissive licence cannot carry a copyleft one past the check.
crate both both-at-once 1.0.0 "MIT AND GPL-3.0-only"
root 'both-at-once = { path = "../both", version = "1.0.0" }'
check licenses
expect_reject "an_and_expression_fails_on_either_disallowed_half" "both-at-once"

# --- Bans.

crate openssl-sys openssl-sys 0.9.0 "MIT"
root 'openssl-sys = { path = "../openssl-sys", version = "0.9.0" }'
check bans
expect_reject "openssl_sys_is_banned_outright" "openssl-sys"

crate native-tls native-tls 0.2.0 "MIT OR Apache-2.0"
root 'native-tls = { path = "../native-tls", version = "0.2.0" }'
check bans
expect_reject "native_tls_is_banned_outright" "native-tls"

crate dup-1 dup 1.0.0 "MIT"
crate dup-2 dup 2.0.0 "MIT"
root 'dup1 = { package = "dup", path = "../dup-1", version = "1.0.0" }' \
     'dup2 = { package = "dup", path = "../dup-2", version = "2.0.0" }'
check bans
expect_reject "two_versions_of_one_crate_are_rejected" "duplicate entries for crate 'dup'"

# The skip list names the older version of each accepted duplicate, so the
# pair it describes passes...
crate syn-2 syn 2.0.0 "MIT OR Apache-2.0"
crate syn-3 syn 3.0.0 "MIT OR Apache-2.0"
root 'syn2 = { package = "syn", path = "../syn-2", version = "2.0.0" }' \
     'syn3 = { package = "syn", path = "../syn-3", version = "3.0.0" }'
check bans
expect_pass "an_accepted_duplicate_pair_passes"

# ...and a third version of the same crate still fails.
crate syn-4 syn 4.0.0 "MIT OR Apache-2.0"
root 'syn2 = { package = "syn", path = "../syn-2", version = "2.0.0" }' \
     'syn3 = { package = "syn", path = "../syn-3", version = "3.0.0" }' \
     'syn4 = { package = "syn", path = "../syn-4", version = "4.0.0" }'
check bans
expect_reject "a_third_version_of_an_accepted_duplicate_is_rejected" "duplicate entries for crate 'syn'"

root 'permissive = { path = "../permissive", version = "*" }'
check bans
expect_reject "a_wildcard_version_requirement_is_rejected" "wildcard"

# --- Sources.

# A git dependency, served from a local repository so the test needs no
# network. It is the source that is rejected, not anything in the crate.
crate from-git from-git 1.0.0 "MIT"
git -C "$work/from-git" init -q
git -C "$work/from-git" add -A
git -C "$work/from-git" -c user.name=test -c user.email=test@example.invalid \
  commit -q -m fixture
root "from-git = { git = \"file://$work/from-git\" }"
check sources
expect_reject "a_git_dependency_is_rejected" "from-git"

root 'permissive = { path = "../permissive", version = "1.0.0" }'
check sources
expect_pass "a_path_dependency_is_not_a_foreign_source"

if [ "$failures" -ne 0 ]; then
  echo "$failures deny-policy test(s) failed" >&2
  exit 1
fi
echo "deny-policy: all tests passed"
