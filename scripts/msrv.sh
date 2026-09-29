#!/usr/bin/env bash
# Builds and tests eks on the toolchain `rust-version` in Cargo.toml names,
# so the MSRV is something CI proves rather than something the manifest
# claims (decision 118).
#
#   scripts/msrv.sh           build every target, then run the tests, on it
#   scripts/msrv.sh --print   print the declared MSRV and exit
#
# The version is read from Cargo.toml every time rather than written down a
# second time here or in ci.yml, so raising it is a one-line change that the
# job follows on its own. CARGO and RUSTUP name the tools to run, for tests.
set -euo pipefail

cargo="${CARGO:-cargo}"
rustup="${RUSTUP:-rustup}"
root="$(cd "$(dirname "$0")/.." && pwd)"

usage() {
  echo "usage: $0 [--print]" >&2
  exit 2
}

mode=verify
case "$#:${1:-}" in
  0:) ;;
  1:--print) mode=print ;;
  *) usage ;;
esac

# cargo's own reading of the manifest, not a grep over TOML: `--no-deps`
# keeps it offline and limited to this package, and the JSON it prints is
# one line with `rust_version` as either a string or null.
if ! metadata="$("$cargo" metadata --no-deps --format-version 1 \
  --manifest-path "$root/Cargo.toml")"; then
  echo "error: cargo could not read $root/Cargo.toml; fix the manifest error above first." >&2
  exit 2
fi
version="$(grep -o '"rust_version":"[^"]*"' <<<"$metadata" | head -n 1 | cut -d '"' -f 4 || true)"
if [ -z "$version" ]; then
  echo "error: Cargo.toml declares no rust-version, so there is no MSRV to verify." >&2
  echo "Add one under [package], e.g. rust-version = \"1.90\"." >&2
  exit 2
fi

if [ "$mode" = print ]; then
  echo "$version"
  exit 0
fi

if ! command -v "$rustup" > /dev/null 2>&1; then
  echo "error: checking the MSRV needs rustup, to run Rust $version beside your usual toolchain." >&2
  echo "Install it from https://rustup.rs, or run this in CI's msrv job instead." >&2
  exit 2
fi

# Asked with auto-install off, so a missing toolchain is a message saying
# what to run rather than a download nobody asked for.
if ! RUSTUP_AUTO_INSTALL=0 "$rustup" run "$version" rustc --version > /dev/null 2>&1; then
  echo "error: Rust $version (the rust-version in Cargo.toml) is not installed." >&2
  echo "Install it with: rustup toolchain install $version --profile minimal" >&2
  exit 2
fi

hint() {
  echo >&2
  echo "eks no longer $1 on Rust $version, the MSRV Cargo.toml declares." >&2
  echo "Either stop needing the newer Rust (an older API, or hold a dependency back with" >&2
  echo "\`cargo update <crate> --precise <version>\`), or raise rust-version in Cargo.toml" >&2
  echo "and record why in docs/DECISIONS.md." >&2
}

# Every target first, so a bench or test that needs a newer compiler fails
# here as a build error, not as a test run that never started. --locked
# because the MSRV claim is about the Cargo.lock that ships, not whatever a
# fresh resolution would pick.
cd "$root"
if ! "$cargo" "+$version" build --locked --all-targets --all-features; then
  hint builds
  exit 1
fi
if ! "$cargo" "+$version" test --locked --all-features; then
  hint "passes its tests"
  exit 1
fi
echo "eks builds and passes its tests on Rust $version."
