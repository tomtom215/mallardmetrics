#!/usr/bin/env bash
#
# Verify that a Linux binary can run on the runtime image's C library.
#
# Usage:  scripts/check-binary-abi.sh <binary> [max-glibc [max-glibcxx]]
#         scripts/check-binary-abi.sh target/release/mallard-metrics 2.36 3.4.30
#
# Checks, using readelf/objdump only (no execution, so it also works on a
# cross-compiled binary):
#   1. the binary is a dynamic ELF whose DT_NEEDED entries are limited to the
#      libraries a minimal glibc runtime provides (libc, libm, libgcc_s,
#      libstdc++ and the loader). A musl or otherwise unexpected dependency fails.
#   2. no versioned glibc symbol is newer than max-glibc (default 2.36, the
#      glibc of Debian 12 / gcr.io/distroless/cc-debian12).
#   3. no GLIBCXX symbol is newer than max-glibcxx (default 3.4.30, the
#      libstdc++ of Debian 12).
#
# Why: a binary built on a newer distribution than the runtime image starts with
# "version `GLIBC_2.xx' not found", and one built against musl cannot install the
# DuckDB `behavioral` extension at all (the community repository publishes no
# linux_*_musl build).
#
# Exit: 0 when compatible, 1 when not, 2 on a usage error.

set -euo pipefail

BIN="${1:-}"
MAX_GLIBC="${2:-2.36}"
MAX_GLIBCXX="${3:-3.4.30}"

if [ -z "$BIN" ] || [ ! -f "$BIN" ]; then
  echo "usage: $0 <binary> [max-glibc [max-glibcxx]]" >&2
  exit 2
fi
for tool in readelf objdump sort grep; do
  command -v "$tool" >/dev/null 2>&1 || { echo "error: $tool is required" >&2; exit 2; }
done

fail=0

# 1. Shared-library dependencies.
needed="$(readelf -d "$BIN" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')"
if [ -z "$needed" ]; then
  echo "FAIL $BIN has no DT_NEEDED entries (a static binary, or not an ELF file)."
  echo "     A static musl binary cannot install the behavioral extension."
  exit 1
fi
allowed='^(libc\.so\.6|libm\.so\.6|libdl\.so\.2|libpthread\.so\.0|librt\.so\.1|libgcc_s\.so\.1|libstdc\+\+\.so\.6|ld-linux-x86-64\.so\.2|ld-linux-aarch64\.so\.1)$'
while IFS= read -r lib; do
  if ! printf '%s' "$lib" | grep -Eq "$allowed"; then
    echo "FAIL unexpected shared library dependency: $lib"
    fail=1
  fi
done <<< "$needed"

# 2 and 3. Highest symbol version required of each library.
newest() { # prefix -> highest version number, or empty
  objdump -T "$BIN" 2>/dev/null \
    | grep -o "$1_[0-9][0-9.]*" | sed "s/^$1_//" | sort -uV | tail -n 1
}
at_most() { # version limit -> success when version <= limit
  [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -n 1)" = "$2" ]
}

glibc="$(newest GLIBC)"
glibcxx="$(newest GLIBCXX)"
echo "$BIN: needs glibc ${glibc:-none} (max ${MAX_GLIBC}), libstdc++ GLIBCXX ${glibcxx:-none} (max ${MAX_GLIBCXX})"

if [ -n "$glibc" ] && ! at_most "$glibc" "$MAX_GLIBC"; then
  echo "FAIL requires glibc $glibc but the runtime provides $MAX_GLIBC."
  echo "     Build on an older base image (rust:<ver>-bookworm, ubuntu-22.04)."
  fail=1
fi
if [ -n "$glibcxx" ] && ! at_most "$glibcxx" "$MAX_GLIBCXX"; then
  echo "FAIL requires GLIBCXX_$glibcxx but the runtime provides $MAX_GLIBCXX."
  fail=1
fi

[ "$fail" -eq 0 ] && echo "ok"
exit "$fail"
