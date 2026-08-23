#!/bin/sh
# Version-drift experiment: rebuild ONLY liba.so with the "v2" implementation
# (private field added mid-struct, new data written into the shared reserved
# area), leave libb.so and the harness binary untouched, and rerun.
set -e
cd "$(dirname "$0")"

[ -x target/debug/harness ] || { echo "run ./run.sh first"; exit 1; }

sum() { md5 -q "$1" 2>/dev/null || md5sum "$1" | cut -d' ' -f1; }
LIBB=$(ls target/debug/libb.dylib 2>/dev/null || ls target/debug/libb.so)

echo "libb checksum before: $(sum "$LIBB")"
cargo build --manifest-path crates/a-capi/Cargo.toml --target-dir target --features v2
echo "libb checksum after:  $(sum "$LIBB")"
echo
./target/debug/harness
