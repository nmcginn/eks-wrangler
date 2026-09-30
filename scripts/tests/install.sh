#!/usr/bin/env bash
# Tests for scripts/install.sh, run by `make script-test`.
#
# Nothing is downloaded and no real release is needed. The installer runs
# under `sh` (dash on Debian and Ubuntu) with a PATH that holds only the tools
# it may use plus stand-ins: `uname`, `sysctl`, and `ldd` answer whatever
# machine a case describes, and `curl`/`wget` serve fixture tarballs out of
# $work/releases as if it were GitHub's releases page. Each case can leave a
# tool out of that PATH, which is how "no sha256 tool" and "no curl" are
# tested on a machine that has both.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
script="$here/../install.sh"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

base="https://releases.invalid/eks"
export EKS_DOWNLOAD_URL="$base"

# --- Stand-ins.

mkdir -p "$work/stubs"
cat > "$work/stubs/uname" <<'STUB'
#!/bin/sh
case "$1" in
  -s) echo "$STUB_OS" ;;
  -m) echo "$STUB_ARCH" ;;
esac
STUB
cat > "$work/stubs/sysctl" <<'STUB'
#!/bin/sh
[ -n "${STUB_TRANSLATED:-}" ] || exit 1
echo "$STUB_TRANSLATED"
STUB
cat > "$work/stubs/ldd" <<'STUB'
#!/bin/sh
echo "${STUB_LDD:-ldd (GNU libc) 2.39}"
STUB
# Serves $base/<path> from $work/releases/<path>, logging each URL asked for;
# a path with no file is a 404, which `curl -f` reports as exit 22.
cat > "$work/stubs/curl" <<STUB
#!/bin/sh
out=""; url=""
while [ "\$#" -gt 0 ]; do
  case "\$1" in
    -o) out="\$2"; shift 2 ;;
    -*) shift ;;
    *) url="\$1"; shift ;;
  esac
done
echo "curl \$url" >> "$work/fetched"
src="$work/releases/\${url#$base/}"
[ -f "\$src" ] || exit 22
cat "\$src" > "\$out"
STUB
cat > "$work/stubs/wget" <<STUB
#!/bin/sh
out=""; url=""
while [ "\$#" -gt 0 ]; do
  case "\$1" in
    -O) out="\$2"; shift 2 ;;
    -*) shift ;;
    *) url="\$1"; shift ;;
  esac
done
echo "wget \$url" >> "$work/fetched"
src="$work/releases/\${url#$base/}"
[ -f "\$src" ] || exit 8
cat "\$src" > "\$out"
STUB
chmod +x "$work/stubs"/*

# The real tools the installer is allowed to use.
tools=(sh tar gzip cut head tr mkdir cp chmod mv rm mktemp dirname grep cat sha256sum shasum perl)

# Builds a PATH directory holding the stand-ins and real tools, minus any
# named in $@, and prints it.
path_without() {
  local dir="$work/path-$RANDOM$RANDOM"
  mkdir -p "$dir"
  local name
  for name in "${tools[@]}" uname sysctl ldd curl wget; do
    local skip=""
    for omit in "$@"; do [ "$omit" = "$name" ] && skip=1; done
    [ -n "$skip" ] && continue
    if [ -e "$work/stubs/$name" ]; then
      ln -s "$work/stubs/$name" "$dir/$name"
    elif real="$(command -v "$name")"; then
      ln -s "$real" "$dir/$name"
    fi
  done
  echo "$dir"
}
full_path="$(path_without)"

# --- Fixture releases.

# Writes a fixture `eks` into $1: it answers --version, and `completions`
# and `man` with a recognisable line, all with shell builtins so it runs on
# the sandbox PATH. STUB_BROKEN_BINARY makes it fail the way a binary built
# for another glibc would.
fixture_binary() {
  cat > "$1/eks" <<'BIN'
#!/bin/sh
[ -z "${STUB_BROKEN_BINARY:-}" ] || { echo "version \`GLIBC_2.99' not found" >&2; exit 1; }
case "$1" in
  --version) echo "eks 0.2.0" ;;
  completions) echo "# generated $2 completions" ;;
  man) echo ".TH EKS 1 generated" ;;
  *) exit 2 ;;
esac
BIN
  chmod +x "$1/eks"
}

# Publishes a fixture tarball for target $1 under release path $2 (e.g.
# latest/download), with its checksum file. `bare` as $3 leaves out the
# completions and man page, as the x86_64-apple-darwin tarball does.
publish() {
  local target="$1" rel="$2" kind="${3:-full}"
  local stage="$work/stage/eks-$target"
  rm -rf "$work/stage"
  mkdir -p "$stage"
  fixture_binary "$stage"
  cp /dev/null "$stage/README.md"
  if [ "$kind" = full ]; then
    mkdir -p "$stage/completions"
    echo "# shipped bash completions" > "$stage/completions/eks.bash"
    echo "# shipped zsh completions" > "$stage/completions/_eks"
    echo "# shipped fish completions" > "$stage/completions/eks.fish"
    echo ".TH EKS 1 shipped" > "$stage/eks.1"
  fi
  mkdir -p "$work/releases/$rel"
  tar -czf "$work/releases/$rel/eks-$target.tar.gz" -C "$work/stage" "eks-$target"
  (cd "$work/releases/$rel" && sha256sum "eks-$target.tar.gz" > "eks-$target.tar.gz.sha256")
}

for t in x86_64-unknown-linux-musl x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu aarch64-apple-darwin; do
  publish "$t" latest/download
done
publish x86_64-apple-darwin latest/download bare
publish x86_64-unknown-linux-musl download/v0.2.0

failures=0
pass() { echo "ok   $1"; }
# Records a failure and shows the output that caused it, indented under it.
fail() {
  echo "FAIL $1" >&2
  while IFS= read -r line; do echo "     $line" >&2; done <<<"$output"
  failures=$((failures + 1))
}

# Runs the installer on a fresh prefix under `sh`, as `curl | sh` would, and
# records its exit status and combined output. The machine defaults to x86_64
# Linux; set STUB_OS/STUB_ARCH (and friends) in the environment to change it.
# RUN_PATH picks the sandbox PATH.
run() {
  prefix="$work/prefix-$RANDOM$RANDOM"
  : > "$work/fetched"
  mkdir -p "$work/tmp"
  set +e
  output="$(env -i \
    PATH="${RUN_PATH:-$full_path}" HOME="$work/home" TMPDIR="$work/tmp" \
    SHELL="${RUN_SHELL:-/bin/bash}" EKS_DOWNLOAD_URL="$EKS_DOWNLOAD_URL" \
    EKS_PREFIX="${RUN_PREFIX:-$prefix}" \
    ${EKS_VERSION:+EKS_VERSION="$EKS_VERSION"} \
    ${EKS_TARGET:+EKS_TARGET="$EKS_TARGET"} \
    STUB_OS="${STUB_OS:-Linux}" STUB_ARCH="${STUB_ARCH:-x86_64}" \
    ${STUB_TRANSLATED:+STUB_TRANSLATED="$STUB_TRANSLATED"} \
    ${STUB_LDD:+STUB_LDD="$STUB_LDD"} \
    ${STUB_BROKEN_BINARY:+STUB_BROKEN_BINARY=1} \
    sh "$script" "$@" 2>&1)"
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

expect_fetched() {
  local name="$1" needle="$2"
  if grep -qF -- "$needle" "$work/fetched"; then pass "$name"; else
    fail "$name: fetched $(tr '\n' ' ' < "$work/fetched")— not '$needle'"; fi
}

expect_file() {
  local name="$1" file="$2" content="$3"
  if [ -f "$file" ] && grep -qF -- "$content" "$file"; then pass "$name"; else
    fail "$name: $file missing or lacks '$content'"; fi
}

expect_absent() {
  local name="$1" file="$2"
  if [ -e "$file" ]; then fail "$name: $file exists"; else pass "$name"; fi
}

# --- The acceptance case: a verified install of the right build.

run
expect_status "a_verified_install_succeeds" 0
expect_fetched "x86_64_linux_gets_the_static_musl_build" "$base/latest/download/eks-x86_64-unknown-linux-musl.tar.gz"
expect_fetched "the_checksum_is_fetched_beside_the_tarball" "eks-x86_64-unknown-linux-musl.tar.gz.sha256"
expect_output "a_verified_install_says_the_checksum_matched" "checksum verified"
expect_file "the_binary_lands_in_prefix_bin" "$prefix/bin/eks" "eks 0.2.0"
if [ -x "$prefix/bin/eks" ]; then pass "the_installed_binary_is_executable"; else
  fail "the_installed_binary_is_executable"; fi
expect_file "the_man_page_lands_in_share_man_man1" "$prefix/share/man/man1/eks.1" "shipped"
expect_file "bash_completions_land_where_bash_completion_looks" "$prefix/share/bash-completion/completions/eks" "shipped bash"
expect_file "zsh_completions_land_in_site_functions" "$prefix/share/zsh/site-functions/_eks" "shipped zsh"
expect_file "fish_completions_land_in_vendor_completions" "$prefix/share/fish/vendor_completions.d/eks.fish" "shipped fish"
expect_output "the_summary_names_the_installed_version" "installed eks 0.2.0 to $prefix/bin/eks"
if [ -z "$(ls -A "$work/tmp")" ]; then pass "the_temporary_directory_is_removed"; else
  fail "the_temporary_directory_is_removed: $(ls -A "$work/tmp")"; fi

# --- Each machine maps to the build that runs on it.

STUB_ARCH=aarch64 run
expect_fetched "aarch64_linux_gets_the_gnu_build" "eks-aarch64-unknown-linux-gnu.tar.gz"
expect_status "aarch64_linux_installs" 0

STUB_ARCH=aarch64 STUB_LDD="musl libc (aarch64)" run
expect_status "aarch64_alpine_is_refused" 1
expect_output "aarch64_alpine_is_told_there_is_no_musl_build" "no release build for aarch64 Linux with musl libc"
expect_output "an_unsupported_machine_is_pointed_at_cargo_install" "cargo install --locked --git https://github.com/nmcginn/eks-wrangler"
expect_absent "an_unsupported_machine_downloads_nothing" "$prefix"

STUB_ARCH=amd64 run
expect_fetched "amd64_reads_as_x86_64" "eks-x86_64-unknown-linux-musl.tar.gz"

STUB_OS=Darwin STUB_ARCH=arm64 run
expect_fetched "apple_silicon_gets_the_aarch64_darwin_build" "eks-aarch64-apple-darwin.tar.gz"
expect_status "apple_silicon_installs" 0

STUB_OS=Darwin STUB_ARCH=x86_64 STUB_TRANSLATED=1 run
expect_fetched "a_rosetta_shell_on_apple_silicon_gets_the_native_build" "eks-aarch64-apple-darwin.tar.gz"

STUB_OS=Darwin STUB_ARCH=x86_64 STUB_TRANSLATED=0 run
expect_fetched "an_intel_mac_gets_the_x86_64_darwin_build" "eks-x86_64-apple-darwin.tar.gz"
expect_status "an_intel_mac_installs" 0
expect_file "a_tarball_without_a_man_page_gets_one_from_the_binary" "$prefix/share/man/man1/eks.1" "generated"
expect_file "a_tarball_without_completions_gets_them_from_the_binary" "$prefix/share/zsh/site-functions/_eks" "generated zsh completions"
expect_file "generated_bash_completions_are_for_bash" "$prefix/share/bash-completion/completions/eks" "generated bash completions"

STUB_OS=Darwin STUB_ARCH=x86_64 run
expect_fetched "a_mac_without_sysctl_translation_is_taken_at_its_word" "eks-x86_64-apple-darwin.tar.gz"

STUB_ARCH=riscv64 run
expect_status "an_unknown_architecture_is_refused" 1
expect_output "an_unknown_architecture_is_named" "no release build for riscv64 processors"

STUB_OS=FreeBSD run
expect_status "an_unknown_os_is_refused" 1
expect_output "an_unknown_os_is_named" "no release build for FreeBSD"

EKS_TARGET=x86_64-unknown-linux-gnu run
expect_fetched "eks_target_overrides_detection" "eks-x86_64-unknown-linux-gnu.tar.gz"

run --target aarch64-apple-darwin
expect_fetched "the_target_flag_overrides_detection" "eks-aarch64-apple-darwin.tar.gz"

run --target sparc-sun-solaris
expect_status "a_target_that_is_not_released_is_a_usage_error" 2
expect_output "a_bad_target_lists_the_real_ones" "x86_64-unknown-linux-musl"

# --- Versions.

run --version 0.2.0
expect_status "a_pinned_version_installs" 0
expect_fetched "a_pinned_version_reads_that_releases_tag" "$base/download/v0.2.0/eks-x86_64-unknown-linux-musl.tar.gz"

run --version v0.2.0
expect_fetched "a_leading_v_is_accepted" "$base/download/v0.2.0/"

run --version=0.2.0
expect_fetched "the_equals_form_of_a_flag_works" "$base/download/v0.2.0/"

EKS_VERSION=0.2.0 run
expect_fetched "eks_version_pins_the_release" "$base/download/v0.2.0/"

EKS_VERSION=0.9.9 run --version latest
expect_fetched "the_version_flag_beats_eks_version" "$base/latest/download/"

run --version banana
expect_status "a_version_that_is_not_one_is_a_usage_error" 2
expect_output "a_bad_version_says_what_a_good_one_looks_like" "pass one like 0.2.0"

run --version 0.3.0
expect_status "a_release_that_does_not_exist_fails" 1
expect_output "a_missing_release_points_at_the_releases_page" "check that release 0.3.0 exists at https://github.com/nmcginn/eks-wrangler/releases"
expect_absent "a_missing_release_installs_nothing" "$prefix"

# --- Checksums. Nothing is installed unless the digest matches.

good_sum="$(cat "$work/releases/latest/download/eks-x86_64-unknown-linux-musl.tar.gz.sha256")"
sum_file="$work/releases/latest/download/eks-x86_64-unknown-linux-musl.tar.gz.sha256"

# A previous install that a failed one must leave alone.
old_prefix="$work/prefix-old"
mkdir -p "$old_prefix/bin"
echo "old eks" > "$old_prefix/bin/eks"

echo "0000000000000000000000000000000000000000000000000000000000000000  eks-x86_64-unknown-linux-musl.tar.gz" > "$sum_file"
RUN_PREFIX="$old_prefix" run
expect_status "a_checksum_mismatch_fails" 1
expect_output "a_mismatch_names_both_digests" "expected 0000000000000000000000000000000000000000000000000000000000000000, got ${good_sum%% *}"
expect_output "a_mismatch_says_nothing_was_installed" "nothing was installed"
expect_output "a_mismatch_says_what_to_do" "Run this again"
expect_file "a_mismatch_leaves_the_existing_binary_alone" "$old_prefix/bin/eks" "old eks"
expect_absent "a_mismatch_installs_no_man_page" "$old_prefix/share"
if [ -z "$(ls -A "$work/tmp")" ]; then pass "a_failed_install_removes_its_temporary_directory"; else
  fail "a_failed_install_removes_its_temporary_directory"; fi

echo "not a digest" > "$sum_file"
run
expect_status "a_checksum_file_without_a_digest_fails" 1
expect_output "a_malformed_checksum_file_says_it_cannot_verify" "does not hold a SHA-256 digest"
expect_absent "a_malformed_checksum_installs_nothing" "$prefix"

echo "abc123  eks-x86_64-unknown-linux-musl.tar.gz" > "$sum_file"
run
expect_status "a_truncated_digest_fails" 1

: > "$sum_file"
run
expect_status "an_empty_checksum_file_fails" 1

rm "$sum_file"
run
expect_status "a_missing_checksum_file_fails" 1
expect_output "a_missing_checksum_says_it_cannot_verify" "cannot be verified — nothing was installed"
expect_absent "a_missing_checksum_installs_nothing" "$prefix"

# The layout release.yml wrote before this change: a digest, then a path
# under dist/. Only the digest is read, in either case.
digest="${good_sum%% *}"
echo "$(tr 'a-f' 'A-F' <<<"$digest")  dist/eks-x86_64-unknown-linux-musl.tar.gz" > "$sum_file"
run
expect_status "a_checksum_file_naming_another_directory_still_verifies" 0
expect_status "an_uppercase_digest_still_verifies" 0
echo "$good_sum" > "$sum_file"

RUN_PATH="$(path_without sha256sum shasum)" run
expect_status "no_sha256_tool_fails_rather_than_skipping_verification" 1
expect_output "no_sha256_tool_says_what_to_install" "neither sha256sum nor shasum is installed"
expect_absent "no_sha256_tool_installs_nothing" "$prefix"

RUN_PATH="$(path_without sha256sum)" run
expect_status "shasum_alone_verifies_as_on_macos" 0
expect_output "shasum_alone_reports_the_verified_digest" "checksum verified ($digest)"

# --- The binary must start before it replaces anything.

STUB_BROKEN_BINARY=1 RUN_PREFIX="$old_prefix" run
expect_status "a_binary_that_cannot_run_here_fails" 1
expect_output "a_binary_that_cannot_run_says_why" "GLIBC_2.99"
expect_file "a_binary_that_cannot_run_leaves_the_existing_one_alone" "$old_prefix/bin/eks" "old eks"

RUN_PREFIX="$old_prefix" run
expect_status "an_upgrade_succeeds" 0
expect_file "an_upgrade_replaces_the_existing_binary" "$old_prefix/bin/eks" "eks 0.2.0"
if ls "$old_prefix/bin" | grep -q tmp; then fail "an_upgrade_leaves_no_temporary_file_behind"; else
  pass "an_upgrade_leaves_no_temporary_file_behind"; fi

# --- Downloaders.

RUN_PATH="$(path_without curl)" run
expect_status "wget_is_used_when_curl_is_missing" 0
expect_fetched "wget_fetches_the_same_url" "wget $base/latest/download/eks-x86_64-unknown-linux-musl.tar.gz"

RUN_PATH="$(path_without curl wget)" run
expect_status "no_downloader_fails" 1
expect_output "no_downloader_says_what_to_install" "neither curl nor wget is installed"

# --- Telling the user what to do next.

run
expect_output "a_prefix_off_the_path_says_how_to_add_it" "export PATH=\"$prefix/bin:\$PATH\""

RUN_PATH="$full_path:$work/prefix-onpath/bin" RUN_PREFIX="$work/prefix-onpath" run
refute_output "a_prefix_on_the_path_says_nothing_about_path" "is not on your PATH"

RUN_SHELL=/bin/zsh run
expect_output "a_zsh_user_is_told_how_to_load_completions" "fpath=($prefix/share/zsh/site-functions \$fpath)"
run
refute_output "a_bash_user_is_not_told_about_fpath" "fpath="

unset EKS_PREFIX
set +e
output="$(env -i PATH="$full_path" TMPDIR="$work/tmp" EKS_DOWNLOAD_URL="$base" \
  STUB_OS=Linux STUB_ARCH=x86_64 sh "$script" 2>&1)"
status=$?
set -e
expect_status "no_home_and_no_prefix_fails" 1
expect_output "no_home_says_to_pass_a_prefix" "pass --prefix DIR"

set +e
output="$(env -i PATH="$full_path" HOME="$work/home" TMPDIR="$work/tmp" EKS_DOWNLOAD_URL="$base" \
  STUB_OS=Linux STUB_ARCH=x86_64 sh "$script" 2>&1)"
status=$?
set -e
expect_status "the_default_prefix_is_home_local" 0
expect_file "the_default_prefix_puts_eks_in_home_local_bin" "$work/home/.local/bin/eks" "eks 0.2.0"

mkdir -p "$work/readonly"
echo > "$work/readonly/bin"
RUN_PREFIX="$work/readonly" run
expect_status "an_unwritable_prefix_fails" 1
expect_output "an_unwritable_prefix_says_to_pick_another" "pick a writable location with --prefix DIR"

# --- Usage.

run --help
expect_status "help_succeeds" 0
expect_output "help_lists_the_flags" "--prefix DIR"

run --frobnicate
expect_status "an_unknown_flag_is_a_usage_error" 2
expect_output "an_unknown_flag_is_named" "unknown option '--frobnicate'"
expect_output "an_unknown_flag_shows_usage" "Usage: install.sh"

run --prefix
expect_status "a_flag_missing_its_value_is_a_usage_error" 2
expect_output "a_flag_missing_its_value_is_named" "--prefix needs a value"

if [ "$failures" -ne 0 ]; then
  echo "$failures install test(s) failed" >&2
  exit 1
fi
echo "install: all tests passed"
