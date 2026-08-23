#!/bin/sh
# Build and test the Python extension modules in both deployment variants:
#   v1 shared-liba:     a-py/b-py link liba.so; capsule used for type unwrap
#   v2 capsule-vtable:  a-py embeds the implementation; b-py reaches it only
#                       through the capsule's function table (no liba link)
set -e
cd "$(dirname "$0")"

case "$(uname)" in
Darwin) EXT=dylib ;;
*) EXT=so ;;
esac
[ -f "target/debug/liba.$EXT" ] || { echo "run ./run.sh first (builds liba)"; exit 1; }

# a-py is built without abi3 (the buffer protocol is outside the limited API
# before Python 3.11), so pin the interpreter pyo3 builds against to the
# venv's python.
[ -d venv ] || python3 -m venv venv
PY="$(pwd)/venv/bin/python"
export PYO3_PYTHON="$PY"
# numpy powers the zero-copy ndarray interop tests; without it those
# sections self-skip, so install it into the venv (never globally).
"$PY" -m pip install --quiet numpy || echo "WARNING: numpy install failed — interop tests will be skipped"

echo "== variant 1: shared liba.$EXT =="
cargo build --manifest-path crates/a-py/Cargo.toml
cargo build --manifest-path crates/b-py/Cargo.toml
mkdir -p py/v1 py/v2
cp "crates/a-py/target/debug/liba.$EXT" py/v1/a.so
cp "crates/b-py/target/debug/libb.$EXT" py/v1/b.so

echo "== variant 2: self-contained capsule vtable =="
cargo build --manifest-path crates/a-py/Cargo.toml \
    --no-default-features --features static --target-dir crates/a-py/target-static
cargo build --manifest-path crates/b-py/Cargo.toml \
    --no-default-features --features vtable --target-dir crates/b-py/target-vtable
cp "crates/a-py/target-static/debug/liba.$EXT" py/v2/a.so
cp "crates/b-py/target-vtable/debug/libb.$EXT" py/v2/b.so

echo
echo "-- variant 1 test"
PYTHONPATH=py/v1 "$PY" py/test_ab.py "v1 shared-liba"
echo "-- variant 2 test"
PYTHONPATH=py/v2 "$PY" py/test_ab.py "v2 capsule-vtable"

echo
echo "-- linkage evidence"
if [ "$EXT" = dylib ]; then
    # otool -L line 1 is the file, line 2 its own install name; deps follow.
    echo "v1 a.so liba dependency: $(otool -L py/v1/a.so | tail -n +3 | grep -c liba | tr -d ' ')"
    echo "v2 a.so liba dependency: $(otool -L py/v2/a.so | tail -n +3 | grep -c liba | tr -d ' ') (self-contained)"
else
    echo "v1 a.so liba dependency: $(ldd py/v1/a.so | grep -c liba)"
    echo "v2 a.so liba dependency: $(ldd py/v2/a.so | grep -c liba) (self-contained)"
fi
echo "v2 b.so undefined a_* symbols: $(nm -u py/v2/b.so | grep -c ' _a_\|^_a_' | tr -d ' ') (capsule only)"
