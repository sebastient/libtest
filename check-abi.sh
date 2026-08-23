#!/bin/sh
# ABI conformance check: every function DECLARED in a-ffi must be EXPORTED by
# liba.so — a declaration without an export is a load-time (or worse, silent)
# failure waiting in every consumer. Exports not declared in a-ffi are
# reported as information (C-only surface, e.g. test hooks, is legitimate).
# Names only: signature mismatches need the cbindgen header + a C compile
# check (see gen-header.sh) or a full ABI diff tool.
set -e
cd "$(dirname "$0")"

LIB=$(ls target/debug/liba.dylib 2>/dev/null || ls target/debug/liba.so)
case "$(uname)" in
Darwin) EXPORTS=$(nm -gU "$LIB" | awk '{print $3}' | sed 's/^_//') ;;
*) EXPORTS=$(nm -D --defined-only "$LIB" | awk '{print $3}') ;;
esac
DECLS=$(grep -oE 'pub fn [a-z_0-9]+' crates/a-ffi/src/lib.rs | awk '{print $3}')

fail=0
for d in $DECLS; do
    echo "$EXPORTS" | grep -qx "$d" || { echo "FAIL: declared in a-ffi but not exported by liba: $d"; fail=1; }
done
for e in $EXPORTS; do
    echo "$DECLS" | grep -qx "$e" || echo "info: exported by liba, not declared in a-ffi: $e"
done
[ $fail -eq 0 ] && echo "check-abi: all $(echo "$DECLS" | wc -l | tr -d ' ') a-ffi declarations are exported"
exit $fail
