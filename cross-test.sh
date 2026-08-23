#!/bin/sh
# Cross-compile the C-world deliverables for aarch64 Linux with zig, deploy
# to the imx95-pro board (root ssh, dma_heap available), and run the full C
# harness there — including the DMA-BUF storage tests that cannot run on the
# macOS dev host.
set -e
cd "$(dirname "$0")"

RUST_TARGET=aarch64-unknown-linux-gnu
ZIG_TARGET=aarch64-linux-gnu
BOARD=root@imx95-pro
DIR=target/$RUST_TARGET/debug

echo "== cross-compile liba.so / libb.so ($RUST_TARGET via zigbuild) =="
cargo zigbuild --manifest-path crates/a-capi/Cargo.toml --target $RUST_TARGET --target-dir target
cargo zigbuild --manifest-path crates/b-capi/Cargo.toml --target $RUST_TARGET --target-dir target

echo "== cross-compile C harness (zig cc) =="
zig cc -target $ZIG_TARGET -fno-sanitize=undefined harness/main.c -Iinclude \
    -o "$DIR/harness" -L"$DIR" -la -lb -Wl,-rpath,'$ORIGIN'

echo "== deploy to $BOARD and run =="
ssh "$BOARD" 'mkdir -p /tmp/libtest'
scp -q "$DIR/liba.so" "$DIR/libb.so" "$DIR/harness" "$BOARD:/tmp/libtest/"
ssh "$BOARD" '/tmp/libtest/harness'
