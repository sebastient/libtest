#!/bin/sh
# Release packaging via cargo-c, proved end to end: install liba into a
# staging prefix, then build a consumer against the INSTALLED tree using
# nothing but pkg-config — no knowledge of this source layout at all.
#
# Why cargo-c, and how much of it: it is adopted for soname/versioning, the
# install layout with symlinks, and pkg-config generation. It is NOT adopted
# for its usual convention of hosting the C API inside the library crate
# behind a `capi` feature — that would collapse `a` and `a-capi` into one
# package and put #[no_mangle] symbols back on every Rust consumer's build
# path (see "Workspace mechanics" in ARCHITECTURE.md). The `capi` feature
# here is deliberately empty; cargo-c only requires that it exist.
#
# Header generation stays with gen-header.sh, which also PROVES the header
# (C11 + C++17 pedantic compile, derived constant check, layout goldens).
# cargo-c is handed that file as an install asset.
set -e
cd "$(dirname "$0")"

command -v cargo-cinstall >/dev/null ||
    { echo "cargo-c required: cargo install cargo-c --locked"; exit 1; }

# cargo-c's prebuilt binary links Homebrew OpenSSL/zlib by @rpath; on macOS
# those are not on the default search path. Harmless where they resolve.
if [ "$(uname)" = Darwin ]; then
    DYLD_FALLBACK_LIBRARY_PATH=/opt/homebrew/opt/openssl@3/lib:/opt/homebrew/lib:/usr/lib
    export DYLD_FALLBACK_LIBRARY_PATH
fi

STAGE="$(pwd)/target/stage"
PREFIX=/usr/local
rm -rf "$STAGE"

# The header must exist and be current before cargo-c installs it.
if command -v cbindgen >/dev/null; then ./gen-header.sh >/dev/null; fi

echo "== cargo cinstall (staged) =="
for pkg in a-capi b-capi; do
    cargo cinstall --manifest-path "crates/$pkg/Cargo.toml" \
        --target-dir target/capi --prefix="$PREFIX" --destdir="$STAGE" 2>&1 |
        grep -E "Installing" || true
done

ROOT="$STAGE$PREFIX"
echo
echo "== installed layout =="
find "$ROOT" -type f -o -type l | sed "s|$ROOT/||" | sort

case "$(uname)" in
Darwin) EXT=dylib; REAL="$ROOT/lib/liba.0.1.0.dylib" ;;
*) EXT=so; REAL=$(ls "$ROOT"/lib/liba.so.0.1.0) ;;
esac

echo
echo "== versioning =="
[ -f "$REAL" ] || { echo "FAIL: versioned library missing"; exit 1; }
if [ "$EXT" = dylib ]; then
    # Pre-1.0: ld64 rejects a 0 major in -compatibility_version, so MINOR is
    # the compatibility axis until 1.0 (the convention the docs describe).
    id=$(otool -D "$REAL" | tail -1)
    echo "install_name: $id"
    case "$id" in
    */liba.0.1.dylib) echo "  ok  compatibility axis is MAJOR.MINOR (pre-1.0 rule)" ;;
    *) echo "FAIL: unexpected install_name"; exit 1 ;;
    esac
else
    soname=$(readelf -d "$REAL" | sed -n 's/.*SONAME.*\[\(.*\)\].*/\1/p')
    echo "soname: $soname"
    [ "$soname" = "liba.so.0" ] || { echo "FAIL: expected soname liba.so.0"; exit 1; }
fi
# The dev symlink is what `-la` resolves through; the runtime symlink is
# what a consumer records. Both must exist or deployment breaks.
for link in "$ROOT/lib/liba.$EXT" "$ROOT/lib/liba.0.1.$EXT"; do
    [ -L "$link" ] || [ -f "$link" ] ||
        { echo "FAIL: missing symlink $link"; exit 1; }
done
echo "  ok  dev and runtime symlinks present"

echo
echo "== export trimming survived cargo-c =="
# The version script / exported-symbols list is applied by a-capi's build.rs,
# which cargo-c runs like any other build script. Verify rather than assume:
# a Rust cdylib that exports its runtime can have its allocator interposed
# by a second Rust dylib in the same process.
if [ "$EXT" = dylib ]; then
    leaked=$(nm -gU "$REAL" | awk '{print $3}' | grep -cv '^_a_' || true)
else
    leaked=$(nm -D --defined-only "$REAL" | awk '{print $3}' | grep -cv '^a_' || true)
fi
echo "non-a_* exported symbols: $leaked (expect 0)"
[ "$leaked" -eq 0 ] || { echo "FAIL: cargo-c build leaks runtime symbols"; exit 1; }

echo
echo "== consumer builds via pkg-config alone =="
PKG_CONFIG_PATH="$ROOT/lib/pkgconfig"
export PKG_CONFIG_PATH
# --define-prefix makes pkg-config rewrite the baked-in /usr/local to where
# the tree actually lives, which is what any relocatable install needs.
CFLAGS=$(pkg-config --define-prefix --cflags liba)
LIBS=$(pkg-config --define-prefix --libs liba)
echo "cflags: $CFLAGS"
echo "libs:   $LIBS"

cat >"${TMPDIR:-/tmp}/liba_pkgconfig_consumer.c" <<'EOF'
/* Knows nothing about the source tree: only <liba.h> and -la. */
#include <liba.h>
#include <stdio.h>
int main(void) {
    AHandle *h = a_create(7);
    a_fill(h, 3);
    ABuf *f = a_frame(h);
    AFrameDescV2 d = a_buf_export2(f);
    int ok = a_id(h) == 7 && d.fourcc == A_FOURCC_RGBA8888 && d.stride == 128 &&
             a_buf_map(f).ptr[0] == 3;
    a_buf_release(f);
    a_destroy(h);
    printf("%s\n", ok ? "consumer: PASS" : "consumer: FAIL");
    return ok ? 0 : 1;
}
EOF
# Same -Werror the harness gets: a header that is pedantically clean on its
# own can still be unusable by a real consumer.
# shellcheck disable=SC2086
cc "${TMPDIR:-/tmp}/liba_pkgconfig_consumer.c" -o "${TMPDIR:-/tmp}/liba_pkgconfig_consumer" \
    -Wall -Wextra -Werror $CFLAGS $LIBS

# A DESTDIR-staged tree is not runnable in place, and that is correct rather
# than a bug: the library's install_name/soname records the FINAL prefix
# (/usr/local/lib/...), which is exactly what lets an installed consumer
# find it with no rpath at all. DESTDIR exists to build a package, not to
# run from. To exercise the staged tree we point the loader's fallback
# search at it — an rpath would not help, because the recorded path is
# absolute, not @rpath-relative.
if [ "$EXT" = dylib ]; then
    DYLD_FALLBACK_LIBRARY_PATH="$ROOT/lib" \
        "${TMPDIR:-/tmp}/liba_pkgconfig_consumer"
else
    LD_LIBRARY_PATH="$ROOT/lib" "${TMPDIR:-/tmp}/liba_pkgconfig_consumer"
fi

echo
echo "== how libb's dependency on liba is expressed =="
# This differs by platform, and the difference is not cosmetic.
#
# ELF: libb records a DT_NEEDED on liba's VERSIONED soname, so a compatible
# liba upgrade is a file swap rather than a relink.
#
# Mach-O: consumer cdylibs are linked with `-undefined dynamic_lookup` (see
# "Platform linking" in ARCHITECTURE.md), which by design records NOTHING —
# the a_* symbols are resolved from whatever is already loaded. So on macOS
# the dependency cannot live in the binary, and pkg-config's `Requires:` is
# where it is stated instead. That is why libb.pc declares it: it is not
# redundant metadata on this platform, it is the only copy.
BLIB=$(ls "$ROOT"/lib/libb.0.1.0.* 2>/dev/null | head -1)
if [ -n "$BLIB" ] && [ -f "$BLIB" ]; then
    if [ "$EXT" = dylib ]; then
        recorded=$(otool -L "$BLIB" | grep -c liba || true)
        echo "recorded liba dependencies: $recorded (expected 0 on Mach-O)"
        [ "$recorded" -eq 0 ] ||
            { echo "FAIL: unexpected recorded dependency on Mach-O"; exit 1; }
    else
        readelf -d "$BLIB" | grep liba ||
            { echo "FAIL: ELF libb must record a DT_NEEDED on liba"; exit 1; }
    fi
fi
# Either way, a consumer of libb must inherit -la, and pkg-config is what
# guarantees that.
echo "pkg-config --libs libb: $(pkg-config --define-prefix --libs libb)"
pkg-config --define-prefix --libs libb | grep -q '\-la' ||
    { echo "FAIL: libb.pc does not propagate liba"; exit 1; }
echo "  ok  consumers of libb inherit liba"

echo
echo "== the consumer records the RUNTIME soname, not a build path =="
# This is the payoff of the soname/install_name policy: what gets recorded
# is the versioned runtime name, so a compatible upgrade is a file swap.
if [ "$EXT" = dylib ]; then
    otool -L "${TMPDIR:-/tmp}/liba_pkgconfig_consumer" | grep liba
else
    readelf -d "${TMPDIR:-/tmp}/liba_pkgconfig_consumer" | grep liba
fi
