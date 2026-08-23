#!/bin/sh
# Windows: rustc's windows-gnu cdylib import library (liba.dll.a) offers the
# ENTIRE export surface, including Rust runtime symbols (__rustc::rust_panic
# et al). A second Rust dylib linking it collides with its own runtime. Fix:
# generate a FILTERED import library containing only the a_* C ABI — the PE
# analogue of the ELF version-script export trim — and remove the full one.
# Usage: win-import.sh <triple> <dlltool-machine>   (run after building a-capi)
set -e
cd "$(dirname "$0")"
TRIPLE=$1
MACHINE=$2
TC=${ANDROID_NDK_HOME:-$HOME/Library/Android/sdk/ndk/29.0.14206865}/toolchains/llvm/prebuilt/darwin-x86_64/bin
DIR=target/$TRIPLE/debug

# The def derives from a-ffi's DECLARATIONS (the canonical consumer
# contract), not from whatever the DLL happens to export — an accidental
# extra `pub extern "C"` export can never sneak into consumer surfaces.
{
    echo "LIBRARY a.dll"
    echo "EXPORTS"
    grep -oE 'pub fn a_[a-z_0-9]+' crates/a-ffi/src/lib.rs | awk '{print "    " $3}'
} >"$DIR/a.def"
# The filtered lib goes in a DEDICATED link dir: lld's MinGW search prefers
# liba.dll.a and even the raw a.dll over liba.a, and both carry the full
# runtime export surface. Module build scripts point -L at winlink/ only,
# so the linker can only ever see the filtered import library.
mkdir -p "$DIR/winlink"
"$TC/llvm-dlltool" -d "$DIR/a.def" -l "$DIR/winlink/liba.a" -m "$MACHINE"
echo "$TRIPLE: filtered import lib with $(grep -c '^    a_' "$DIR/a.def") a_* exports"
