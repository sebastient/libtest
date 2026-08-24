#!/bin/sh
# The pure-Rust workspace builds normally (`cargo build --workspace`).
# The capi packages live OUTSIDE the workspace and are built explicitly here,
# each in its own invocation with its own backend features. --target-dir
# funnels their artifacts into the shared target/ directory.
# Pass extra args (e.g. --features v2) through to a-capi's build.
set -e
cd "$(dirname "$0")"

echo "== 1. Pure-Rust workspace (app + libs, static backend) =="
cargo build --workspace

echo "== 2. liba.so — C API over the static implementation =="
cargo build --manifest-path crates/a-capi/Cargo.toml --target-dir target "$@"

echo "== 3. libb.so — C API consuming A through liba's C ABI (dynamic) =="
cargo build --manifest-path crates/b-capi/Cargo.toml --target-dir target

TARGET=target/debug
case "$(uname)" in
Darwin) EXT=dylib ;;
*) EXT=so ;;
esac

# Make the dylibs findable at run time via rpath regardless of cwd.
if [ "$EXT" = dylib ]; then
    install_name_tool -id @rpath/liba.dylib "$TARGET/liba.dylib"
    install_name_tool -id @rpath/libb.dylib "$TARGET/libb.dylib"
fi

# Regenerate the shipped header first when cbindgen is available (the
# committed include/liba.h is used otherwise); the harness compiles against
# it, so header/ABI drift fails right here.
if command -v cbindgen >/dev/null; then
    ./gen-header.sh
fi

EXTRA=""
[ "$EXT" = dylib ] && EXTRA="-framework IOSurface -framework CoreFoundation"
# -Wall -Wextra -Werror on the CONSUMER compile, not just on the header's
# standalone check in gen-header.sh: a header can be pedantically clean on
# its own and still be unusable, e.g. by leaking a const-qualified alias
# into a handle the caller owns and must release. Only a real consumer
# compile catches that class.
cc harness/main.c -o "$TARGET/harness" -Iinclude \
    -Wall -Wextra -Werror \
    -L"$TARGET" -la -lb $EXTRA -Wl,-rpath,"$(pwd)/$TARGET"

echo
echo "== C harness (liba.$EXT + libb.$EXT) =="
"$TARGET/harness"

echo
echo "== Pure-Rust static app =="
"$TARGET/app"
echo "-- shared-library dependencies of app (expect NO liba/libb):"
if [ "$EXT" = dylib ]; then
    otool -L "$TARGET/app" | tail -n +2
else
    ldd "$TARGET/app"
fi

echo
echo "== ABI conformance (a-ffi declarations vs liba exports) =="
./check-abi.sh

echo
echo "== Full ABI diff: the gate, self-tested (libabigail) =="
# The third conformance layer: check-abi.sh compares names and gen-header.sh
# asserts struct layouts, but only a type-level diff catches an existing
# export whose parameter type changed underneath its unchanged name.
#
# --self-test rather than a comparison, because a real gate diffs against
# the PREVIOUS RELEASE and this reference implementation has none. What runs
# here proves the gate works; supplying the artifact is the one piece a real
# project adds (./check-abidiff.sh <previous-liba.so>).
./check-abidiff.sh --self-test

echo
echo "== Refcount balance across the .so boundary (leak checker) =="
./leaks.sh
