#!/bin/sh
# Miri run of the static backend under the Rust-level scenario battery
# (crates/app/tests/scenarios.rs — the #[test] form of what src/main.rs and
# the C harness check natively).
#
# Both aliasing models are exercised: Stacked Borrows (the current default)
# and Tree Borrows (the likely future model, stricter about the reborrow
# patterns this architecture relies on).
#
# -Zmiri-disable-isolation is required because the streaming scenarios use
# thread::sleep. Platform-storage scenarios (dma_heap ioctls, IOSurface) are
# cfg(not(miri)) — Miri cannot execute FFI.
set -e
cd "$(dirname "$0")"

command -v rustup >/dev/null || { echo "rustup required"; exit 1; }
cargo +nightly miri --version >/dev/null 2>&1 ||
    { echo "install miri first: rustup +nightly component add miri"; exit 1; }

BASE="-Zmiri-disable-isolation"

echo "== Stacked Borrows =="
MIRIFLAGS="$BASE" cargo +nightly miri test -p app

echo
echo "== Tree Borrows =="
MIRIFLAGS="$BASE -Zmiri-tree-borrows" cargo +nightly miri test -p app

echo
echo "== Aliasing diagnostic (#[ignore]d: EXPECTED TO FAIL) =="
# capsule_shape_interleaved_access_invalidates_handle models what the
# PyCapsule `unwrap_a` contract permits once the GIL no longer serializes
# access: a handle derived from an exclusive borrow, an intervening access
# through the owner, then use of the handle. Both models reject it. This is
# a diagnostic, not a regression test — it documents WHY the GIL is
# load-bearing for soundness (see ARCHITECTURE.md, Open items).
for model in "" "-Zmiri-tree-borrows"; do
    name=$([ -z "$model" ] && echo "Stacked" || echo "Tree")
    if MIRIFLAGS="$BASE $model" cargo +nightly miri test -p app -- --ignored \
        >/dev/null 2>&1; then
        echo "UNEXPECTED: $name Borrows accepted the interleaved capsule shape."
        echo "  The aliasing model changed — revisit the unwrap_a contract."
        exit 1
    else
        echo "$name Borrows rejects the interleaved capsule shape (as expected)"
    fi
done
