#!/bin/sh
# Full ABI diff (libabigail, Linux-only) — the type-level conformance check.
#
# This is the third and strictest of the three checks, and it exists because
# the other two have a hole between them:
#
#   check-abi.sh     compares NAMES    — an export can keep its name and
#                                        change its signature underneath.
#   gen-header.sh    asserts LAYOUTS   — of public structs only, via
#                                        _Static_assert on size and offsets.
#   check-abidiff.sh compares TYPES    — the parameter and return types of
#                                        every exported function.
#
# Only the third catches `void a_fill(AHandle *, uint8_t)` quietly becoming
# `void a_fill(AHandle *, uint32_t)`: same name, same struct layouts, and
# every already-compiled consumer now passes garbage.
#
# WHY THERE IS NO BASELINE IN THIS REPOSITORY
# A real release gate diffs the candidate against THE PREVIOUS RELEASE — the
# artifact consumers actually linked against. Fetch it from wherever releases
# live (a GitHub release asset, a package repository, the installed distro
# package) and pass it to this script:
#
#     gh release download v1.2.0 --pattern 'liba.so' --dir /tmp/prev
#     ./check-abidiff.sh /tmp/prev/liba.so
#
# Wiring that fetch up is deliberately OUT OF SCOPE here: libtest is a
# reference implementation of the dual Rust/C API pattern, not a released
# library, so it has no previous release to diff against and no release
# infrastructure to borrow one from. What it can do — and does, in
# --self-test — is prove the gate itself works, so the fetch is the only
# piece a real project has to add.
#
# Committing a generated baseline instead was tried and is a trap worth
# recording. libabigail's XML embeds absolute build paths and per-codegen-
# unit hashes, so the file is machine-specific and churns by ~300 lines on
# changes abidiff itself reports as no ABI change. Stripping that metadata
# to stop the churn makes abidiff SILENTLY stop detecting real breaks —
# it still parses, still compares, and answers "no change" forever.
#
# Usage:
#   ./check-abidiff.sh <reference>   diff the built libraries against a
#                                    reference .so (or a .abi dump)
#   ./check-abidiff.sh --emit <dir>  write .abi dumps of the current build,
#                                    to compare against after a change
#   ./check-abidiff.sh --self-test   prove the gate detects a real break and
#                                    tolerates private layout drift
set -e
cd "$(dirname "$0")"

[ "$(uname)" = Linux ] || { echo "check-abidiff: libabigail is Linux-only, skipping on $(uname)"; exit 0; }
command -v abidw >/dev/null && command -v abidiff >/dev/null || {
    echo "check-abidiff: libabigail not installed, skipping"
    echo "  install with: sudo apt install libabigail-tools"
    exit 0
}

TARGET=target/debug
# --no-show-locs and a hashed type-id style drop the noisiest of the
# unstable metadata. They do NOT make a dump reproducible across machines
# (see the note above); they only make a local before/after comparison
# readable.
ABIDW="abidw --no-corpus-path --no-show-locs --type-id-style hash --drop-private-types"

# abidiff's exit status is a BIT MASK, not a level: 1 = tool error,
# 2 = usage error, 4 = ABI changed, 8 = the change is incompatible.
# The gate fails on 8 and on tool errors, and merely reports 4 — an ADDED
# export sets 4 alone, and this contract is append-only, so an addition is a
# pass, not a failure.
report() {
    lib=$1 code=$2
    if [ $((code & 3)) -ne 0 ]; then
        echo "FAIL lib$lib: abidiff could not run (exit $code)"
        return 1
    fi
    if [ $((code & 8)) -ne 0 ]; then
        echo "FAIL lib$lib: INCOMPATIBLE ABI change against the reference"
        echo "  every already-compiled consumer of lib$lib is now wrong."
        echo "  intentional? bump the soname MAJOR before releasing."
        return 1
    fi
    if [ $((code & 4)) -ne 0 ]; then
        echo "info lib$lib: compatible ABI change (an addition) — append-only, allowed"
        return 0
    fi
    echo "check-abidiff: lib$lib unchanged against the reference"
    return 0
}

suppressions_for() {
    [ -f "abi/lib$1.abignore" ] && printf -- '--suppressions abi/lib%s.abignore' "$1"
}

case "$1" in
--emit)
    OUT=${2:?usage: ./check-abidiff.sh --emit <dir>}
    mkdir -p "$OUT"
    for l in a b; do
        $ABIDW "$TARGET/lib$l.so" > "$OUT/lib$l.abi"
        echo "wrote $OUT/lib$l.abi"
    done
    echo "note: these dumps are machine-specific — useful for a local"
    echo "  before/after comparison, not as a committed baseline."
    exit 0
    ;;
--self-test)
    # Three controls, because a gate nobody has watched fail is not a gate.
    # The reference is generated here rather than committed, which is the
    # whole point: everything below is self-contained.
    #
    #   1 NEGATIVE  the `v2` feature changes a PRIVATE struct's layout. The
    #               architecture rests on that being invisible across the C
    #               ABI (see drift.sh), so the gate must stay silent — one
    #               that fires on every internal refactor gets switched off
    #               within a week. Passing this proves abi/liba.abignore is
    #               doing its job.
    #   2 POSITIVE  a_fill's parameter type is widened and liba.so REBUILT.
    #               The gate MUST fail. This is the defect class neither
    #               other check can see.
    #   3 RESTORE   the tree and the default build are clean again.
    #
    # Control 2 patches tracked source in place rather than hiding behind a
    # cargo feature, because a `#[cfg]`-selected parameter type leaks into
    # the cbindgen-generated header: cbindgen does not evaluate features, so
    # it emits BOTH variants and liba.h stops compiling. Test scaffolding
    # does not belong in a public contract header. The patch is reverted by
    # an EXIT trap on every path, including interrupt, and the tree is
    # checksummed afterwards to prove it.
    CAPI=crates/a-capi/src/lib.rs
    REF=$(mktemp -d)
    BACKUP=$(mktemp)
    cp "$CAPI" "$BACKUP"
    before=$(cksum < "$CAPI")
    restore() {
        cp "$BACKUP" "$CAPI"
        rm -f "$BACKUP"
        rm -rf "$REF"
        cargo build --manifest-path crates/a-capi/Cargo.toml --target-dir target >/dev/null 2>&1 || true
    }
    trap restore EXIT INT TERM

    SUPP=$(suppressions_for a)
    fail=0

    echo "-- reference: the current build (generated, not committed)"
    cargo build --manifest-path crates/a-capi/Cargo.toml --target-dir target >/dev/null 2>&1
    $ABIDW "$TARGET/liba.so" > "$REF/liba.abi"
    echo "   ok   captured $(grep -c "function-decl name='a_" "$REF/liba.abi") exported a_* declarations"

    echo "-- control 1/3 (negative): private layout drift must NOT trip the gate"
    cargo build --manifest-path crates/a-capi/Cargo.toml --target-dir target --features v2 >/dev/null 2>&1
    code=0
    # shellcheck disable=SC2086
    abidiff $SUPP "$REF/liba.abi" "$TARGET/liba.so" >/dev/null 2>&1 || code=$?
    if [ $((code & 12)) -eq 0 ]; then
        echo "   ok   v2 private layout change reported no ABI change"
    else
        echo "   FAIL v2 tripped the gate (exit $code) — abi/liba.abignore is too narrow"
        fail=1
    fi

    echo "-- control 2/3 (positive): a changed parameter type MUST trip the gate"
    sed -i 's/pub unsafe extern "C" fn a_fill(p: \*mut A, seed: u8)/pub unsafe extern "C" fn a_fill(p: *mut A, seed: u32)/' "$CAPI"
    sed -i 's/a_cshim::a_fill(p.cast(), seed)/a_cshim::a_fill(p.cast(), seed as u8)/' "$CAPI"
    if cargo build --manifest-path crates/a-capi/Cargo.toml --target-dir target >/dev/null 2>&1; then
        code=0
        # shellcheck disable=SC2086
        abidiff $SUPP "$REF/liba.abi" "$TARGET/liba.so" >/dev/null 2>&1 || code=$?
        if [ $((code & 8)) -ne 0 ]; then
            echo "   ok   a_fill(u8 -> u32) caught as an incompatible change"
        else
            echo "   FAIL a real ABI break went UNDETECTED (exit $code) — the gate is not working"
            fail=1
        fi
    else
        echo "   FAIL could not build the deliberately-broken library; control did not run"
        fail=1
    fi

    echo "-- control 3/3: restoring the tree and the default build"
    restore
    trap - EXIT INT TERM
    if [ "$(cksum < "$CAPI")" != "$before" ]; then
        echo "   FAIL $CAPI was not restored to its original contents"
        exit 1
    fi
    echo "   ok   source restored byte-for-byte"
    [ $fail -eq 0 ] && echo "check-abidiff --self-test: the gate detects breaks and tolerates private drift"
    exit $fail
    ;;
"")
    cat <<'USAGE'
check-abidiff: no reference given, so there is nothing to diff against.

This repository deliberately commits no ABI baseline: libabigail's dumps are
machine-specific and a real gate should diff against the PREVIOUS RELEASE,
which a reference implementation does not have. See the header of this
script.

  ./check-abidiff.sh <previous-liba.so>   diff against a released artifact
  ./check-abidiff.sh --emit <dir>         snapshot the current build
  ./check-abidiff.sh --self-test          prove the gate itself works
USAGE
    exit 0
    ;;
esac

# --- Compare against a caller-supplied reference ---------------------------
REFERENCE=$1
[ -e "$REFERENCE" ] || { echo "check-abidiff: no such reference: $REFERENCE"; exit 1; }

fail=0
if [ -d "$REFERENCE" ]; then
    # A directory of previous artifacts: match them up by name.
    for l in a b; do
        for ext in so abi; do
            cand="$REFERENCE/lib$l.$ext"
            [ -f "$cand" ] || continue
            code=0
            # shellcheck disable=SC2086
            abidiff $(suppressions_for "$l") "$cand" "$TARGET/lib$l.so" || code=$?
            report "$l" "$code" || fail=1
            break
        done
    done
else
    # A single artifact: infer which library it is from its name.
    base=$(basename "$REFERENCE")
    case "$base" in
    *libb*) l=b ;;
    *) l=a ;;
    esac
    code=0
    # shellcheck disable=SC2086
    abidiff $(suppressions_for "$l") "$REFERENCE" "$TARGET/lib$l.so" || code=$?
    report "$l" "$code" || fail=1
fi
exit $fail
