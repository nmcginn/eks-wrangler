#!/usr/bin/env bash
# Fail unless the ELF binary given as $1 is statically linked.
#
# The musl release build exists to run on any Linux, however old its glibc or
# however bare its container image, and that promise only holds if nothing is
# loaded at run time. `file` would say "static-pie linked", but its wording
# varies across versions; the ELF headers do not. A binary is static when it
# asks for no dynamic loader (no PT_INTERP program header) and names no shared
# library (no DT_NEEDED entry). A static-pie binary still has a dynamic section
# — for its own relocations — so the check is for what that section *needs*,
# not whether it exists.
set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <binary>" >&2
  exit 2
fi

bin="$1"
if [ ! -f "$bin" ]; then
  echo "verify-static: $bin does not exist — build it first" >&2
  exit 2
fi

headers="$(readelf --program-headers --wide "$bin")"
dynamic="$(readelf --dynamic --wide "$bin")"

status=0
if grep -q 'INTERP' <<<"$headers"; then
  interp="$(grep -o 'Requesting program interpreter: [^]]*' <<<"$headers" || true)"
  echo "verify-static: $bin is dynamically linked (${interp:-has a PT_INTERP header})" >&2
  status=1
fi
if grep -q '(NEEDED)' <<<"$dynamic"; then
  echo "verify-static: $bin needs shared libraries at run time:" >&2
  grep '(NEEDED)' <<<"$dynamic" | sed 's/.*Shared library: /  /' >&2
  status=1
fi

if [ "$status" -eq 0 ]; then
  echo "verify-static: $bin is statically linked"
fi
exit "$status"
