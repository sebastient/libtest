#!/bin/sh
# Windows deliverables for the C#/.NET multi-module shape, cross-compiled
# from this host with zig (stock `boltffi pack csharp` supports win-x64 only,
# built ON Windows; this fills the win-x64 + win-arm64 cross gap):
#   liba (a.dll)      — the one shared implementation
#   amod.dll/bmod.dll — the two BoltFFI modules, PE-importing a.dll
# Module builds use BoltFFI's expansion env so the exported symbols match the
# generated C# bindings (plain cargo builds emit legacy symbol names).
set -e
cd "$(dirname "$0")"

TC=${ANDROID_NDK_HOME:-$HOME/Library/Android/sdk/ndk/29.0.14206865}/toolchains/llvm/prebuilt/darwin-x86_64/bin

for SPEC in "x86_64-pc-windows-gnu i386:x86-64" "aarch64-pc-windows-gnullvm arm64"; do
    T=${SPEC% *}
    M=${SPEC#* }
    echo "== $T =="
    cargo zigbuild --manifest-path crates/a-capi/Cargo.toml --target "$T" --target-dir target
    ./win-import.sh "$T" "$M"
    for MOD in a-mod b-mod; do
        env BOLTFFI_BINDING_EXPANSION=1 \
            BOLTFFI_BINDING_EXPANSION_ROOT="$PWD/crates/$MOD" \
            BOLTFFI_BINDING_EXPANSION_SOURCE="$PWD/crates/$MOD/src/lib.rs" \
            BOLTFFI_BINDING_EXPANSION_SURFACE=native \
            BOLTFFI_BINDING_METADATA_FEATURES= \
            RUSTFLAGS="--cfg boltffi_binding_expansion" \
            cargo zigbuild --manifest-path "crates/$MOD/Cargo.toml" --target "$T"
    done
    for DLL in crates/a-mod/target/"$T"/debug/amod.dll crates/b-mod/target/"$T"/debug/bmod.dll; do
        IMPORTS=$("$TC/llvm-readobj" --coff-imports "$DLL" | grep 'Name:' | grep -c 'a\.dll')
        SYMS=$("$TC/llvm-readobj" --coff-exports "$DLL" | grep -c 'boltffi_')
        echo "$(basename "$DLL"): imports a.dll=$IMPORTS, boltffi exports=$SYMS"
    done
done
