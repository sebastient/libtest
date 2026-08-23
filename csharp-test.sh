#!/bin/sh
# Build and run the two-module C# validation on the host. Requires the
# module dylibs built via the expansion env (see bolt_build in the docs) and
# liba built by run.sh.
set -e
cd "$(dirname "$0")"
DOTNET=${DOTNET:-$HOME/.dotnet/dotnet}

# Workaround for a fork codegen gap: the FfiBuf support struct always
# references NativeMethods.BufFromBytes, but its DllImport is only emitted
# for modules with ENCODED ARGUMENTS; amod has encoded returns only.
# (Candidate fork fix alongside the existing helper-gating commits.)
python3 - <<'PYEOF'
p = 'crates/a-mod/dist/csharp/Amod.cs'
s = open(p).read()
decl = """        internal const string LibName = "amod";

        [DllImport(LibName, EntryPoint = "boltffi_buf_from_bytes")]
        internal static extern FfiBuf BufFromBytes(byte[] bytes, nuint len);
"""
if 'BufFromBytes(byte[]' not in s:
    s = s.replace('        internal const string LibName = "amod";\n', decl)
    open(p, 'w').write(s)
    print("patched BufFromBytes DllImport into Amod.cs")
PYEOF

"$DOTNET" build cstest -c Debug -v quiet --nologo
OUT=cstest/bin/Debug/net8.0
cp crates/a-mod/target/debug/libamod.dylib "$OUT/" 2>/dev/null ||
    cp crates/a-mod/target/debug/libamod.so "$OUT/"
cp crates/b-mod/target/debug/libbmod.dylib "$OUT/" 2>/dev/null ||
    cp crates/b-mod/target/debug/libbmod.so "$OUT/"
"$DOTNET" "$OUT/cstest.dll"
