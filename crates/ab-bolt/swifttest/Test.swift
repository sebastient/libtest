// Swift validation of the BoltFFI `ab` module: handles, B-over-A logic,
// errors, COW, IOSurface-descriptor zero-copy, async, and streams.
import Foundation
import IOSurface

func check(_ cond: Bool, _ msg: String) {
    if cond { print("  ok   \(msg)") } else { print("  FAIL \(msg)"); exit(1) }
}

@main
struct Test {
    static func main() async {
        let src = AbSource(id: 40)
        check(src.id() == 40 && src.counter() == 0, "constructor and accessors")
        check(src.process() == 42, "B's process over A (id + counter)")
        check(src.counter() == 2, "A sees B's increments")
        check(src.processFast() == 1.5 * 42, "B's repr(C) fast path")

        src.fill(seed: 1)
        let expected = (0..<4096).reduce(UInt64(0)) { $0 + UInt64((1 + ($1 % 256)) % 256) }
        check(src.checksum() == expected, "B's zero-copy checksum via Rust")

        do {
            try src.tryFill(seed: 0)
            check(false, "tryFill(0) should throw")
        } catch let e as AbError {
            check(e == .invalidArgument, "error enum maps to Swift throw")
        } catch { check(false, "unexpected error type") }

        // Snapshot + COW (data() is the copying fallback).
        src.fill(seed: 1)
        let snap = src.frame()
        src.fill(seed: 2)
        check(snap.data()[0] == 1, "snapshot immutable after producer COW")
        let shape = snap.shape()
        check(shape.rows == 32 && shape.cols == 30 && shape.rowStride == 128,
              "shape/strides cross as a record")

        // Zero-copy: IOSurface descriptor, resolved independently in Swift.
        try! src.setStorage(kind: 2)
        src.fill(seed: 5)
        let sf = src.frame()
        let desc = sf.exportDesc()
        check(desc.kind == 2 && desc.id != 0, "IOSurface descriptor exported")
        guard let surf = IOSurfaceLookup(IOSurfaceID(desc.id)) else {
            check(false, "IOSurfaceLookup resolves the ID"); return
        }
        check(true, "IOSurfaceLookup resolves the ID")
        IOSurfaceLock(surf, .readOnly, nil)
        let base = IOSurfaceGetBaseAddress(surf).assumingMemoryBound(to: UInt8.self)
        check(base[0] == 5 && base[127] == UInt8((5 + 127) % 256),
              "Swift reads Rust's bytes zero-copy through the surface")
        src.fill(seed: 6)
        check(base[0] == 5 && sf.data()[0] == 5, "COW isolates the surface snapshot")
        IOSurfaceUnlock(surf, .readOnly, nil)

        // Async: Swift async/await over Rust completion callbacks.
        let cap = try! await src.capture()
        check(cap.data()[0] == 0xCA, "async capture resolves with a frame")
        let cs = try! await src.captureChecksum()
        check(cs == cap.checksum(), "B's composed async checksum")

        // Streaming: AsyncStream over the producer thread.
        let stream = src.events()
        src.start(count: 4, periodMs: 2)
        var got: [UInt8] = []
        for await ev in stream {
            got.append(ev.firstByte)
            if got.count == 4 { break }
        }
        check(got == [0, 1, 2, 3], "in-order AsyncStream delivery")
        src.join()

        print("SWIFT RESULT: PASS")
    }
}
