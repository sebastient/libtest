#!/bin/sh
# Generate include/liba.h from a-capi via cbindgen, then prove the header:
#  - compiles standalone as C11 AND C++17, with pedantic warnings as errors
#  - declares the A_* contract constants
#  - matches the frozen struct layouts (size/offset golden values, LP64) —
#    the drift class no name-level check can see.
# The harness includes this header (run.sh), so every signature is also
# regression-checked against a real consumer compile.
set -e
cd "$(dirname "$0")"

mkdir -p include
cbindgen --config crates/a-capi/cbindgen.toml --crate a-capi \
    --output include/liba.h crates/a-capi

# Contract constants: cbindgen does not lift a-abi's `const`s and status
# enum into #defines, so cbindgen.toml mirrors them by hand in
# `after_includes`. A hand-written mirror is exactly the thing that drifts
# silently — an appended AStatus code that never reaches C consumers is
# invisible to every name-level check. So DERIVE the expected set from
# a-abi and compare names AND values, the same two-representations
# discipline check-abi.sh applies to symbols.
ABI=crates/a-abi/src/lib.rs
EXPECT="${TMPDIR:-/tmp}/liba_expected_consts.txt"
: >"$EXPECT"

# AStatus variants with literal discriminants. `Unknown = i32::MIN` is
# excluded by construction (non-numeric literal): it is never sent over the
# wire, so it has no C-side counterpart.
sed -n '/pub enum AStatus/,/^}/p' "$ABI" |
    sed -n 's/^ *\([A-Z][A-Za-z0-9]*\) = \(-\{0,1\}[0-9][0-9]*\),$/\1 \2/p' |
    while read -r variant value; do
        name=$(printf '%s' "$variant" |
            sed 's/\([a-z0-9]\)\([A-Z]\)/\1_\2/g' | tr '[:lower:]' '[:upper:]')
        echo "A_STATUS_$name $value" >>"$EXPECT"
    done

# Every other `pub const A_*` in a-abi. Two value forms are understood:
# an integer literal, and a `fourcc(b'X', ...)` call. Anything else is a
# HARD ERROR rather than a silent skip — a checker that quietly ignores what
# it cannot parse is how the A_STATUS_BUSY gap happened in the first place.
# A_PY_* is excluded on purpose: it versions the PyCapsule contract between
# Python extension modules, a different boundary with no C-header
# counterpart (see a-abi's APyCapiV1).
# sed -E: BSD sed has no \| alternation in basic regex. The `|| true`
# guards against grep's "no matches" exit status tripping `set -e`.
sed -nE "s/^pub const (A_[A-Z_0-9]*): u(32|64) = (.*);\$/\1|\3/p" "$ABI" |
    { grep -v '^A_PY_' || true; } >"$EXPECT.raw"

while IFS='|' read -r name expr; do
    [ -n "$name" ] || continue
    case "$expr" in
    "fourcc("*)
        # fourcc(b'R', b'A', b'2', b'4') -> a | b<<8 | c<<16 | d<<24
        chars=$(printf '%s' "$expr" | sed "s/fourcc(//; s/)//; s/b'//g; s/'//g; s/, */ /g")
        set -- $chars
        [ $# -eq 4 ] || { echo "cannot parse fourcc for $name: $expr"; exit 1; }
        v=$(( $(printf '%d' "'$1") | ($(printf '%d' "'$2") << 8) |
              ($(printf '%d' "'$3") << 16) | ($(printf '%d' "'$4") << 24) ))
        echo "$name $v" >>"$EXPECT"
        ;;
    *)
        # Integer literal ONLY if it is the WHOLE expression: strip digits,
        # underscores and a sign, and require nothing to remain. A prefix
        # match would silently evaluate `1 + 1` as 1.
        rest=$(printf '%s' "$expr" | tr -d '0-9_-')
        if [ -z "$rest" ] && [ -n "$expr" ]; then
            echo "$name $(printf '%s' "$expr" | tr -d '_')" >>"$EXPECT"
        else
            echo "UNEVALUATABLE constant $name = $expr"
            echo "  extend the extractor in gen-header.sh, or exclude it explicitly"
            exit 1
        fi
        ;;
    esac
done <"$EXPECT.raw"

fail=0
count=0
while read -r name value; do
    [ -n "$name" ] || continue
    count=$((count + 1))
    got=$(sed -n "s/^#define $name (*\(-*[0-9][0-9]*\)u*)*\$/\1/p" include/liba.h)
    if [ -z "$got" ]; then
        echo "MISSING constant: $name (declared in a-abi, absent from liba.h)"
        echo "  add it to after_includes in crates/a-capi/cbindgen.toml"
        fail=1
    elif [ "$got" != "$value" ]; then
        echo "VALUE MISMATCH: $name is $got in liba.h but $value in a-abi"
        fail=1
    fi
done <"$EXPECT"
[ "$count" -gt 0 ] || { echo "constant extraction from $ABI produced nothing"; exit 1; }
[ "$fail" -eq 0 ] || exit 1

cat >"${TMPDIR:-/tmp}/liba_layout_check.c" <<'EOF'
#include "liba.h"
#include <stddef.h>
#ifdef __cplusplus
#define LAYOUT_ASSERT(cond, msg) static_assert(cond, msg)
#else
#define LAYOUT_ASSERT(cond, msg) _Static_assert(cond, msg)
#endif
/* Frozen ABI layouts — golden values identical on all LP64 targets. */
LAYOUT_ASSERT(sizeof(ASharedV1) == 64, "ASharedV1 size");
LAYOUT_ASSERT(offsetof(ASharedV1, id) == 8, "ASharedV1.id");
LAYOUT_ASSERT(offsetof(ASharedV1, counter) == 16, "ASharedV1.counter");
LAYOUT_ASSERT(offsetof(ASharedV1, scale) == 24, "ASharedV1.scale");
LAYOUT_ASSERT(offsetof(ASharedV1, fd) == 32, "ASharedV1.fd");
LAYOUT_ASSERT(offsetof(ASharedV1, _reserved) == 40, "ASharedV1._reserved");
LAYOUT_ASSERT(sizeof(ABufView) == 16, "ABufView size");
LAYOUT_ASSERT(sizeof(ABufViewMut) == 16, "ABufViewMut size");
LAYOUT_ASSERT(sizeof(AFrameInfoV1) == 32, "AFrameInfoV1 size");
LAYOUT_ASSERT(sizeof(AFrameDescV1) == 32, "AFrameDescV1 size");
/* v2 is a SUPERSET with an identical prefix: same offsets for every v1
 * field, new fields strictly after. That is what lets a consumer cast a
 * v2 descriptor down to v1 — and what a name-level check cannot see. */
LAYOUT_ASSERT(sizeof(AFrameDescV2) == 64, "AFrameDescV2 size");
LAYOUT_ASSERT(offsetof(AFrameDescV2, kind) == offsetof(AFrameDescV1, kind), "v2 prefix: kind");
LAYOUT_ASSERT(offsetof(AFrameDescV2, fd) == offsetof(AFrameDescV1, fd), "v2 prefix: fd");
LAYOUT_ASSERT(offsetof(AFrameDescV2, id) == offsetof(AFrameDescV1, id), "v2 prefix: id");
LAYOUT_ASSERT(offsetof(AFrameDescV2, offset) == offsetof(AFrameDescV1, offset), "v2 prefix: offset");
LAYOUT_ASSERT(offsetof(AFrameDescV2, len) == offsetof(AFrameDescV1, len), "v2 prefix: len");
LAYOUT_ASSERT(offsetof(AFrameDescV2, fourcc) == 32, "AFrameDescV2.fourcc");
LAYOUT_ASSERT(offsetof(AFrameDescV2, modifier) == 40, "AFrameDescV2.modifier");
LAYOUT_ASSERT(offsetof(AFrameDescV2, stride) == 56, "AFrameDescV2.stride");
LAYOUT_ASSERT(offsetof(AFrameDescV1, fd) == 8, "AFrameDescV1.fd");
LAYOUT_ASSERT(offsetof(AFrameDescV1, id) == 12, "AFrameDescV1.id");
LAYOUT_ASSERT(offsetof(AFrameDescV1, len) == 24, "AFrameDescV1.len");
int main(void) { return 0; }
EOF
cc -std=c11 -Wall -Wextra -Wpedantic -Werror -fsyntax-only -Iinclude \
    "${TMPDIR:-/tmp}/liba_layout_check.c"
cc -x c++ -std=c++17 -Wall -Wextra -Wpedantic -Werror -fsyntax-only -Iinclude \
    "${TMPDIR:-/tmp}/liba_layout_check.c"

echo "generated include/liba.h ($(wc -l <include/liba.h | tr -d ' ') lines): constants present, C11+C++17 pedantic-clean, layouts asserted"
