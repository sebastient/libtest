#!/bin/sh
# Free-threaded (PEP 703) build and concurrency validation.
#
# This is the evidence behind `gil_used = false` on modules a and b. It is a
# SEPARATE entry point from build-py.sh because it needs an interpreter most
# machines do not have, and because it proves a different property: build-py
# proves the modules work, this proves they work with nothing serialising
# them.
#
# Free-threaded builds are NOT abi3 -- CPython's free-threaded ABI is a
# different one (it advertises `abi3t` tags, which PyO3 does not yet emit),
# so this produces version-specific modules while the GIL build keeps its
# single abi3 artifact. Nothing here changes the abi3 build.
#
#   ./build-ft.sh          build both variants and run both batteries
set -e
cd "$(dirname "$0")"

# A free-threaded interpreter, in order of preference. uv is the route that
# needs no root: `uv python install 3.14t`.
PYFT=""
for cand in "$(pwd)/venv-ft/bin/python" python3.14t python3.13t; do
    if command -v "$cand" >/dev/null 2>&1 || [ -x "$cand" ]; then
        if "$cand" -c "import sys; sys.exit(0 if not sys._is_gil_enabled() else 1)" 2>/dev/null; then
            PYFT="$cand"
            break
        fi
    fi
done

if [ -z "$PYFT" ]; then
    echo "build-ft: no free-threaded interpreter found, skipping"
    echo "  install one with:  uv python install 3.14t   (no root required)"
    echo "               or:   sudo apt install python3.13-nogil"
    exit 0
fi

# Isolate in a venv so numpy and friends never touch the interpreter itself.
if [ ! -x venv-ft/bin/python ]; then
    if command -v uv >/dev/null; then
        uv venv --python "$PYFT" venv-ft >/dev/null
    else
        "$PYFT" -m venv venv-ft
    fi
fi
PY="$(pwd)/venv-ft/bin/python"
export PYO3_PYTHON="$PY"
echo "build-ft: $("$PY" -c 'import sys;print(sys.version.split()[0], "free-threaded" if not sys._is_gil_enabled() else "GIL")')"

if command -v uv >/dev/null; then
    uv pip install --python "$PY" --quiet numpy >/dev/null 2>&1 ||
        echo "WARNING: numpy unavailable for this interpreter — interop tests self-skip"
else
    "$PY" -m pip install --quiet numpy >/dev/null 2>&1 ||
        echo "WARNING: numpy unavailable for this interpreter — interop tests self-skip"
fi

[ -f target/debug/liba.so ] || { echo "run ./run.sh first (builds liba)"; exit 1; }

echo "== variant 1: shared liba.so =="
cargo build --manifest-path crates/a-py/Cargo.toml --target-dir crates/a-py/target-ft
cargo build --manifest-path crates/b-py/Cargo.toml --target-dir crates/b-py/target-ft
mkdir -p py/ft py/ft2
cp crates/a-py/target-ft/debug/liba.so py/ft/a.so
cp crates/b-py/target-ft/debug/libb.so py/ft/b.so

echo "== variant 2: self-contained capsule vtable =="
cargo build --manifest-path crates/a-py/Cargo.toml \
    --no-default-features --features static --target-dir crates/a-py/target-ft-static
cargo build --manifest-path crates/b-py/Cargo.toml \
    --no-default-features --features vtable --target-dir crates/b-py/target-ft-vtable
cp crates/a-py/target-ft-static/debug/liba.so py/ft2/a.so
cp crates/b-py/target-ft-vtable/debug/libb.so py/ft2/b.so

# The declaration is only real if importing does NOT re-enable the GIL. This
# check is the whole point: without it every battery below would run under a
# silently restored GIL and pass having proved nothing.
echo
echo "-- Py_mod_gil declaration"
LD_LIBRARY_PATH="$(pwd)/target/debug" PYTHONPATH=py/ft "$PY" -W error::RuntimeWarning -c "
import sys, a, b
assert not sys._is_gil_enabled(), 'importing a/b re-enabled the GIL'
print('  ok   importing a and b left the GIL disabled (no RuntimeWarning)')
"

echo
echo "-- variant 1: functional battery"
LD_LIBRARY_PATH="$(pwd)/target/debug" PYTHONPATH=py/ft "$PY" py/test_ab.py "v1 shared-liba, free-threaded"
echo "-- variant 1: concurrency battery"
LD_LIBRARY_PATH="$(pwd)/target/debug" PYTHONPATH=py/ft "$PY" py/test_ft.py "v1 shared-liba, free-threaded"

echo
echo "-- variant 2: functional battery"
PYTHONPATH=py/ft2 "$PY" py/test_ab.py "v2 capsule-vtable, free-threaded"
echo "-- variant 2: concurrency battery"
PYTHONPATH=py/ft2 "$PY" py/test_ft.py "v2 capsule-vtable, free-threaded"

echo
echo "-- linkage evidence"
echo "v1 a.so liba dependency: $(ldd py/ft/a.so | grep -c liba)"
echo "v2 a.so liba dependency: $(ldd py/ft2/a.so | grep -c liba) (self-contained)"
# Direct evidence that gil_used = false is a compile-time no-op on the abi3
# build: PyO3 gates the slot behind #[cfg(all(not(Py_LIMITED_API),
# Py_GIL_DISABLED))], so the symbol is simply absent there.
# grep -c exits 1 on zero matches, hence the `|| true` on each.
if [ -f py/v1/a.so ]; then
    echo "abi3 build references PyUnstable_Module_SetGIL: $(nm -Du py/v1/a.so 2>/dev/null | grep -c PyUnstable_Module_SetGIL || true) (expected 0 — cfg'd out)"
else
    echo "abi3 build references PyUnstable_Module_SetGIL: (not built; run ./build-py.sh)"
fi
echo "ft   build references PyUnstable_Module_SetGIL: $(nm -Du py/ft/a.so | grep -c PyUnstable_Module_SetGIL || true) (expected 1)"
