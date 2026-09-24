#!/usr/bin/env bash
# Fail unless the ELF binary given as $1 runs on glibc $2 or newer.
#
# A `-gnu` binary records, per symbol it imports from glibc, the oldest glibc
# version that symbol exists in (`memcpy@GLIBC_2.14`), and the dynamic loader
# refuses to start the binary on a glibc older than the newest of them. The
# release build declares the floor it links against (`x86_64-unknown-linux-gnu.2.17`
# to `cargo zigbuild`); this reads what the binary actually needs back out of
# its version-needs section, so a toolchain change, a dependency that starts
# calling a newer function, or a leg that quietly stops honouring the suffix
# fails the build instead of shipping a binary that will not start on the
# systems the release notes promise.
#
# Versions are compared component by component as numbers, not as text:
# `2.9` is older than `2.17`, and `2.2.5` is older than `2.3`.
#
# `READELF` names the readelf to run (default: `readelf`), which is also how
# `scripts/tests/verify-glibc-floor.sh` feeds it fixture output.
set -euo pipefail

readelf="${READELF:-readelf}"

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <binary> <glibc-floor, e.g. 2.17>" >&2
  exit 2
fi

bin="$1"
floor="$2"
if ! [[ "$floor" =~ ^[0-9]+(\.[0-9]+)*$ ]]; then
  echo "verify-glibc-floor: '$floor' is not a glibc version — pass one like 2.17" >&2
  exit 2
fi
if [ ! -f "$bin" ]; then
  echo "verify-glibc-floor: $bin does not exist — build it first" >&2
  exit 2
fi

# Prints 1 if version $1 is strictly newer than version $2, else 0. Missing
# trailing components count as zero, so `2.17` and `2.17.0` are the same.
newer_than() {
  awk -v a="$1" -v b="$2" 'BEGIN {
    na = split(a, x, "."); nb = split(b, y, ".")
    n = na > nb ? na : nb
    for (i = 1; i <= n; i++) {
      p = (i <= na) ? x[i] + 0 : 0
      q = (i <= nb) ? y[i] + 0 : 0
      if (p > q) { print 1; exit }
      if (p < q) { print 0; exit }
    }
    print 0
  }'
}

# Only numbered GLIBC_ versions are a floor; GLIBC_PRIVATE and other
# libraries' version tags (GCC_3.0, …) say nothing about which glibc runs it.
needs="$("$readelf" --version-info --wide "$bin" \
  | grep -o 'Name: GLIBC_[0-9][0-9.]*' \
  | sed 's/^Name: GLIBC_//' \
  | sort -u || true)"

if [ -z "$needs" ]; then
  echo "verify-glibc-floor: $bin needs no versioned glibc symbols — is it a -gnu build? (a static musl binary has no floor to check)" >&2
  exit 1
fi

newest=""
for v in $needs; do
  if [ -z "$newest" ] || [ "$(newer_than "$v" "$newest")" -eq 1 ]; then
    newest="$v"
  fi
done

if [ "$(newer_than "$newest" "$floor")" -eq 1 ]; then
  echo "verify-glibc-floor: $bin needs glibc $newest, newer than its declared floor of $floor" >&2
  echo "verify-glibc-floor: symbols that need more than $floor:" >&2
  # The symbol table is where a reader finds *which* call raised the floor;
  # the version-needs section above is only the verdict.
  "$readelf" --dyn-syms --wide "$bin" \
    | grep -o '[A-Za-z0-9_.]*@GLIBC_[0-9][0-9.]*' \
    | sort -u \
    | while IFS= read -r sym; do
        if [ "$(newer_than "${sym##*@GLIBC_}" "$floor")" -eq 1 ]; then
          echo "  $sym" >&2
        fi
      done
  echo "verify-glibc-floor: build with 'cargo zigbuild --target <triple>.$floor' so the linker binds the older symbol versions" >&2
  exit 1
fi

echo "verify-glibc-floor: $bin needs at most glibc $newest (floor $floor)"
