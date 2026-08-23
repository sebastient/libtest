#!/bin/sh
# The size ladder: measure every rung below the default cdylib floor, and
# say what each one costs. Numbers, not estimates — whether a deployment
# needs a given rung is a product question, but the price should not be.
#
# Rungs:
#   1. release                 LTO, debuginfo stripped. The honest default.
#   2. release-small           opt-level=z, fat LTO, panic=abort, stripped.
#                              COST: panic=abort trades away the panic
#                              shield — a_test_panic aborts the process
#                              instead of returning A_STATUS_PANIC.
#   3. + build-std             nightly, libstd rebuilt with the
#      + immediate-abort       immediate-abort panic strategy, which drops
#                              the panic formatting/backtrace machinery
#                              that dominates a stock libstd.
#                              COST: nightly toolchain, and panic messages
#                              (including a_last_error_message's detail).
#   4. no_std                  The floor. NOT a build of liba — see
#                              crates/a-nostd-probe, which measures what a
#                              Rust cdylib costs with no libstd at all.
#                              COST: Arc, Vec, threads, Mutex — i.e. the
#                              buffer model, streaming and async. That is a
#                              different component, not a smaller one.
set -e
cd "$(dirname "$0")"

case "$(uname)" in
Darwin) EXT=dylib; SZ="stat -f%z" ;;
*) EXT=so; SZ="stat -c%s" ;;
esac

report() { printf '  %-42s %9s bytes\n' "$1" "$2"; }
human() { awk -v b="$1" 'BEGIN { printf "%.0f KB", b / 1024 }'; }

echo "== rung 1: release =="
cargo build -q --manifest-path crates/a-capi/Cargo.toml --target-dir target/size --release
R1=$($SZ "target/size/release/liba.$EXT")
report "release (LTO, debuginfo stripped)" "$R1"

echo "== rung 2: release-small =="
cargo build -q --manifest-path crates/a-capi/Cargo.toml --target-dir target/size \
    --profile release-small
R2=$($SZ "target/size/release-small/liba.$EXT")
report "release-small (opt-z, panic=abort)" "$R2"

# Rung 2's cost, measured rather than asserted: with panic = "abort",
# catch_unwind cannot catch, so the shielded entry point that returns
# A_STATUS_PANIC in a debug build takes the process down instead.
cat >"${TMPDIR:-/tmp}/panic_probe.c" <<'EOF'
#include <stdio.h>
extern int a_test_panic(void);
int main(void) {
    int rc = a_test_panic();
    printf("returned %d\n", rc); /* unreachable under panic=abort */
    return 0;
}
EOF
cc "${TMPDIR:-/tmp}/panic_probe.c" -o "${TMPDIR:-/tmp}/panic_probe" \
    -Ltarget/size/release-small -la -Wl,-rpath,"$(pwd)/target/size/release-small"
if "${TMPDIR:-/tmp}/panic_probe" >/dev/null 2>&1; then
    echo "  UNEXPECTED: the panic shield survived panic=abort"
    exit 1
else
    echo "  cost confirmed: a_test_panic aborts (shield traded away)"
fi

echo "== rung 3: build-std + immediate-abort (nightly) =="
R3=""
if cargo +nightly --version >/dev/null 2>&1 &&
    rustup +nightly component list --installed 2>/dev/null | grep -q rust-src; then
    TRIPLE=$(rustc -vV | sed -n 's/^host: //p')
    # NOTE: this flag was renamed. It used to be
    # `-Zbuild-std-features=panic_immediate_abort`; current nightlies make
    # it a real panic strategy and reject the old spelling outright.
    if RUSTFLAGS="-Zunstable-options -Cpanic=immediate-abort" \
        cargo +nightly build -q --manifest-path crates/a-capi/Cargo.toml \
        --target-dir target/size-bs --profile release-small \
        -Z build-std=std,panic_abort --target "$TRIPLE" 2>/dev/null; then
        R3=$($SZ "target/size-bs/$TRIPLE/release-small/liba.$EXT")
        report "build-std + immediate-abort" "$R3"
    else
        echo "  (build failed — flag spelling may have changed again)"
    fi
else
    echo "  (skipped: needs nightly + rust-src)"
fi

echo "== rung 4: no_std floor (probe, not liba) =="
cargo build -q --manifest-path crates/a-nostd-probe/Cargo.toml \
    --target-dir target/size-ns --profile release-small
R4=$($SZ "target/size-ns/release-small/libanostd.$EXT")
report "no_std cdylib probe" "$R4"

# The probe must actually WORK, or its size means nothing: a library that
# does not run is always smaller than one that does.
cat >"${TMPDIR:-/tmp}/anostd_check.c" <<'EOF'
#include <stdio.h>
#include <stdint.h>
typedef struct AnHandle AnHandle;
extern AnHandle *an_create(uint64_t id);
extern void an_destroy(AnHandle *p);
extern uint64_t an_id(const AnHandle *p);
extern uint64_t an_counter(const AnHandle *p);
extern void an_increment(AnHandle *p);
int main(void) {
    AnHandle *h = an_create(42);
    an_increment(h);
    an_increment(h);
    int ok = h && an_id(h) == 42 && an_counter(h) == 2;
    an_destroy(h);
    /* The pool is finite: exhaustion is a hard failure, not a slow one. */
    AnHandle *keep[64];
    for (int i = 0; i < 64; i++) keep[i] = an_create((uint64_t)i);
    ok = ok && keep[63] != NULL && an_create(99) == NULL;
    for (int i = 0; i < 64; i++) an_destroy(keep[i]);
    printf("%s\n", ok ? "  no_std probe: PASS (it really runs)" : "  no_std probe: FAIL");
    return ok ? 0 : 1;
}
EOF
cc "${TMPDIR:-/tmp}/anostd_check.c" -o "${TMPDIR:-/tmp}/anostd_check" \
    -Wall -Wextra -Werror -Ltarget/size-ns/release-small -lanostd \
    -Wl,-rpath,"$(pwd)/target/size-ns/release-small"
"${TMPDIR:-/tmp}/anostd_check"

echo
echo "== ladder =="
printf '  %-42s %9s  %s\n' "rung" "size" "vs release"
printf '  %-42s %9s  %s\n' "release" "$(human "$R1")" "—"
printf '  %-42s %9s  %s\n' "release-small" "$(human "$R2")" \
    "$(awk -v a="$R1" -v b="$R2" 'BEGIN { printf "-%.0f%%", (1 - b/a) * 100 }')"
[ -n "$R3" ] && printf '  %-42s %9s  %s\n' "+ build-std + immediate-abort" "$(human "$R3")" \
    "$(awk -v a="$R1" -v b="$R3" 'BEGIN { printf "-%.0f%%", (1 - b/a) * 100 }')"
printf '  %-42s %9s  %s\n' "no_std floor (probe, different component)" "$(human "$R4")" \
    "$(awk -v a="$R1" -v b="$R4" 'BEGIN { printf "-%.0f%%", (1 - b/a) * 100 }')"
echo
echo "  Everything between rung 3 and rung 4 is libstd: allocator, threads,"
echo "  sync primitives, and the formatting machinery they pull in. liba"
echo "  cannot cross that gap without giving up the buffer model, streaming"
echo "  and async — see crates/a-nostd-probe for the itemised list."
