#!/bin/sh
# Wheel packaging for the SELF-CONTAINED Python variant, proved end to end:
# build both wheels with maturin, install them into a FRESH venv that has
# never seen this source tree, and run the same battery build-py.sh runs.
#
# Why only the self-contained variant is packaged this way: it embeds A's
# implementation (a-py/static) and reaches it from module b through the
# capsule vtable (b-py/vtable), so neither wheel needs liba.so to exist.
# The shared-library variant deliberately depends on a system-installed,
# soname-versioned liba.so — that is an OS packaging story (see the
# cargo-c notes in ARCHITECTURE.md), not something to vendor into a wheel.
set -e
cd "$(dirname "$0")"

[ -d venv ] || python3 -m venv venv
PY="$(pwd)/venv/bin/python"
"$PY" -m maturin --version >/dev/null 2>&1 ||
    { echo "installing maturin into ./venv"; "$PY" -m pip install --quiet maturin; }

OUT="$(pwd)/target/wheels"
rm -rf "$OUT"
mkdir -p "$OUT"

echo "== building wheels (self-contained variant) =="
# Backend selection lives in each pyproject.toml's [tool.maturin]; passing
# it here too would just be a second place to drift.
"$PY" -m maturin build --manifest-path crates/a-py/Cargo.toml --out "$OUT" -q
"$PY" -m maturin build --manifest-path crates/b-py/Cargo.toml --out "$OUT" -q
ls -1 "$OUT"

echo
echo "== installing into a fresh venv (no source tree on the path) =="
VENV="$(pwd)/target/wheel-venv"
rm -rf "$VENV"
python3 -m venv "$VENV"
"$VENV/bin/pip" install --quiet --no-index --find-links "$OUT" a b
"$VENV/bin/pip" install --quiet numpy ||
    echo "WARNING: numpy install failed — interop sections will self-skip"

echo
echo "== test battery, running against the INSTALLED wheels =="
# cd elsewhere so `import a` cannot possibly resolve to the build tree.
(cd "$VENV" && "$VENV/bin/python" "$(pwd)/../../py/test_ab.py" "wheels (self-contained)") ||
    (cd / && "$VENV/bin/python" "$OLDPWD/py/test_ab.py" "wheels (self-contained)")

echo
echo "== linkage evidence: neither wheel may need liba =="
for m in a b; do
    # Ask the interpreter where the EXTENSION lives, rather than guessing a
    # filename — the package wrapper means `<pkg>.__file__` is __init__.py.
    lib=$("$VENV/bin/python" -c "import $m; print($m._ext.__file__)")
    [ -f "$lib" ] || { echo "FAIL: cannot locate $m's extension module"; exit 1; }
    case "$(uname)" in
    Darwin) n=$(otool -L "$lib" | tail -n +3 | grep -c liba || true) ;;
    *) n=$(ldd "$lib" | grep -c liba || true) ;;
    esac
    echo "$m: $n liba dependencies (expect 0)"
    [ "$n" -eq 0 ] || { echo "FAIL: $m links liba — not self-contained"; exit 1; }
done
BEXT=$("$VENV/bin/python" -c "import b; print(b._ext.__file__)")
echo "b undefined a_* symbols: $(nm -u "$BEXT" | grep -c '_a_' || true) (capsule only)"
