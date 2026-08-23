"""Cross-extension-module type sharing test: module b consumes a.A objects."""
import sys

import a
import b

try:
    import numpy as np
except ImportError:
    np = None

variant = sys.argv[1] if len(sys.argv) > 1 else "unknown-variant"

x = a.A(40)
assert x.id == 40
assert x.counter == 0

# b mutates and reads the A created by module a.
assert b.process(x) == 42, "b.process should return id + counter"
assert x.counter == 2, "a sees the increments done by b"
assert b.process_fast(x) == 1.5 * 42

# Zero-copy payload access from b (bytes never enter Python).
x.fill(1)
expected = sum((1 + (i % 256)) % 256 for i in range(4096))
assert b.checksum(x) == expected

# Type safety: non-A objects are rejected by module a's unwrap, not UB.
for bogus in ("nope", 42, None, object()):
    try:
        b.process(bogus)
        raise SystemExit(f"expected TypeError for {bogus!r}")
    except TypeError:
        pass

# --- Frames via the buffer protocol: zero-copy, shapes/strides, lifetimes ---
y = a.A(7)
y.fill(1)
f = b.grab_frame(y)  # created by b, but its type lives in module a
assert isinstance(f, a.Frame)
assert f._data_ptr() == y._data_ptr(), "zero-copy snapshot across modules"
assert f.shape == (32, 30, 4)
assert f.strides == (128, 4, 1), "row stride is padded: NOT C-contiguous"

mv = memoryview(f)
assert mv.readonly and mv.ndim == 3
assert mv.shape == (32, 30, 4) and mv.strides == (128, 4, 1)
assert not mv.c_contiguous, "padding rows must surface as non-contiguous"
# fill pattern is (seed + byte_offset) % 256; strides map indices to offsets
assert mv[0, 0, 0] == 1
assert mv[0, 1, 0] == (1 + 4) % 256, "pixel stride = channels = 4 bytes"
assert mv[1, 0, 0] == (1 + 128) % 256, "row stride = 128 bytes incl. padding"
assert mv[31, 29, 3] == (1 + 31 * 128 + 29 * 4 + 3) % 256
assert len(bytes(mv)) == 32 * 30 * 4, "logical size excludes row padding"

# COW: mutating the producer detaches it; live views keep the old bytes.
y.fill(9)
assert mv[0, 0, 0] == 1 and y._data_ptr() != f._data_ptr()

# Lifetimes: the view keeps the Frame alive, the Frame keeps the bytes
# alive — drop every other reference and read again.
del y
del f
assert mv[31, 29, 3] == (1 + 31 * 128 + 29 * 4 + 3) % 256
mv.release()

# numpy interop (optional): zero-copy ndarray honouring shape and strides.
if np is not None:
    z = a.A(3)
    z.fill(1)
    fz = b.grab_frame(z)
    arr = np.asarray(fz)
    assert arr.dtype == np.uint8
    assert arr.shape == (32, 30, 4) and arr.strides == (128, 4, 1)
    assert not arr.flags["C_CONTIGUOUS"], "padded rows: not contiguous"
    assert not arr.flags["WRITEABLE"], "immutable snapshot"
    # Zero-copy proof: the ndarray's data pointer IS the frame's bytes.
    assert arr.__array_interface__["data"][0] == fz._data_ptr()
    assert int(arr[1, 0, 0]) == (1 + 128) % 256
    assert int(arr[..., 0].sum()) == sum(
        (1 + (r * 128 + c * 4) % 256) % 256 for r in range(32) for c in range(30)
    )
    del z, fz  # ndarray keeps the frame alive via its base object
    assert int(arr[0, 0, 0]) == 1

# Writable buffer requests must be refused (frames are immutable snapshots).
f2 = a.A(8).frame()
try:
    import ctypes

    ctypes.c_char.from_buffer(f2)  # requests a writable buffer
    raise SystemExit("expected BufferError/TypeError for writable request")
except (BufferError, TypeError):
    pass

# Contiguity requests must be REFUSED (padded rows): PEP 3118 conformance.
if np is not None:
    frame_for_contig = b.grab_frame(a.A(1))
    try:
        np.frombuffer(frame_for_contig, dtype=np.uint8)  # requests C-contiguous
        raise SystemExit("expected contiguity rejection")
    except (BufferError, ValueError, TypeError):
        pass

# Type safety for frame-taking entry points too.
try:
    b.grab_frame("nope")
    raise SystemExit("expected TypeError from grab_frame")
except TypeError:
    pass

# --- Callbacks and streaming ---
s = a.A(50)
got = []
sid = s.subscribe(lambda fr: got.append(int(memoryview(fr)[0, 0, 0])))
s.stream(4, 2)
s.stream_join()  # releases the GIL internally; callbacks need it
assert got == [0, 1, 2, 3], f"in-order delivery on producer thread: {got}"
s.unsubscribe(sid)  # also GIL-releasing; guarantees no further callbacks
s.stream(2, 1)
s.stream_join()
assert got == [0, 1, 2, 3], "no deliveries after unsubscribe"

# --- Async: awaitables built on completion callbacks ---
import asyncio


async def async_checks():
    # Cancellation: a timed-out capture must not raise InvalidStateError in
    # the loop when the producer completes later (guarded set_result).
    loop_errors = []
    asyncio.get_running_loop().set_exception_handler(
        lambda loop, ctx: loop_errors.append(ctx)
    )
    try:
        await asyncio.wait_for(s.capture(), timeout=0)
    except (asyncio.TimeoutError, TimeoutError):
        pass
    await asyncio.sleep(0.05)  # let the abandoned completion arrive
    assert not loop_errors, f"loop exception after cancelled capture: {loop_errors}"

    frame = await s.capture()
    assert isinstance(frame, a.Frame)
    assert memoryview(frame)[0, 0, 0] == 0xCA
    cs = await b.capture_checksum(s)
    assert cs == sum((0xCA + (i % 256)) % 256 for i in range(4096))
    # Concurrency sanity: two captures awaited together.
    f1, f2 = await asyncio.gather(s.capture(), s.capture())
    assert memoryview(f1)[0, 0, 0] == memoryview(f2)[0, 0, 0] == 0xCA


asyncio.run(async_checks())

# A raising subscriber callback must not kill the stream (reported via
# sys.unraisablehook) and deliveries continue.
import sys

unraisable = []
old_hook = sys.unraisablehook
sys.unraisablehook = lambda args: unraisable.append(args)
try:
    s2 = a.A(51)
    got2 = []

    def bad_then_good(fr):
        got2.append(int(memoryview(fr)[0, 0, 0]))
        if len(got2) == 1:
            raise ValueError("deliberate callback failure")

    sid2 = s2.subscribe(bad_then_good)
    s2.stream(3, 1)
    s2.stream_join()
    assert got2 == [0, 1, 2], f"stream continued past callback exception: {got2}"
    assert len(unraisable) == 1, "exactly one unraisable report expected"
    s2.unsubscribe(sid2)
finally:
    sys.unraisablehook = old_hook

# Two instances with subscriptions: unsubscribing one must not disturb the
# other (regression for the registry-keying fix).
sa, sb_ = a.A(60), a.A(61)
ga, gb = [], []
ia = sa.subscribe(lambda fr: ga.append(1))
ib = sb_.subscribe(lambda fr: gb.append(1))
sa.unsubscribe(ia)
sb_.stream(2, 1)
sb_.stream_join()
sa.stream(2, 1)
sa.stream_join()
assert gb == [1, 1] and ga == [], f"cross-instance unsubscribe leak: {ga} {gb}"
sb_.unsubscribe(ib)

# --- Capsule v5: the scoped exclusive borrow (`with_a`) ---
# Module a now holds its PyRefMut for the DURATION of b's callback rather
# than dropping it on return, so the handle b works through is exclusively
# borrowed for exactly its window of use. Two consequences are observable
# from Python; both used to be either impossible to detect or UB.

# 1. A conflicting borrow is reported, not aliased. `_test_call_while_borrowed`
#    takes `&mut self`, so PyO3 holds an exclusive borrow of `r` while the
#    callback runs — b's attempt to borrow it again must fail cleanly.
r = a.A(77)
seen = []


def reenter():
    try:
        b.process(r)
        seen.append("NO ERROR")
    except RuntimeError as e:
        seen.append(type(e).__name__)
    except TypeError as e:  # pre-v5 module a: unwrap_a reports null
        seen.append(type(e).__name__)


r._test_call_while_borrowed(reenter)
assert seen == ["RuntimeError"], f"re-entrant borrow should be refused, got {seen}"
assert r.counter == 0, "the refused call must not have mutated the object"
# Still usable afterwards: the refusal is transient, not a poisoned state.
assert b.process(r) == 79, "object works normally once the borrow is released"

# 2. A panic inside b's callback unwinds through TWO extern "C" frames
#    (b's trampoline, then a's with_a) and still arrives as a Python
#    exception rather than aborting the interpreter.
try:
    b._test_panic_in_with_a(r)
    raise SystemExit("expected the callback panic to surface as an exception")
except BaseException as e:  # PyO3 maps a caught panic to PanicException
    assert not isinstance(e, SystemExit), "panic did not surface"
    assert "deliberate panic" in str(e), f"unexpected panic payload: {e}"
# The interpreter is alive and the object is still coherent.
assert b.process(r) == 81, "usable after a shielded callback panic"

# --- async iteration: `async for frame in src.frames()` ---
# The pull-shaped view of the push-shaped subscription. A queued frame
# resolves the awaitable immediately; an empty queue parks it until the
# producer thread delivers.


async def async_iteration_checks():
    src3 = a.A(80)
    stream = src3.frames(4)
    assert stream.pending == 0 and stream.dropped == 0

    # Consume WHILE the producer runs: the first await parks, and the
    # producer thread resolves it via call_soon_threadsafe.
    src3.stream(3, 5)
    seen3 = []
    async for frame in stream:
        seen3.append(int(memoryview(frame)[0, 0, 0]))
        if len(seen3) == 3:
            break
    assert seen3 == [0, 1, 2], f"in-order async delivery: {seen3}"
    src3.stream_join()
    src3.unsubscribe(stream.id)

    # Bounded queue, drop-OLDEST: a consumer that never polls keeps the
    # newest frames, not a stale backlog.
    src4 = a.A(81)
    small = src4.frames(2)
    src4.stream(5, 1)
    src4.stream_join()
    assert small.pending == 2, f"queue exceeded capacity: {small.pending}"
    assert small.dropped == 3, f"expected 3 dropped, got {small.dropped}"
    kept = [int(memoryview(await small.__anext__())[0, 0, 0]) for _ in range(2)]
    assert kept == [3, 4], f"drop-oldest keeps the newest frames: {kept}"
    src4.unsubscribe(small.id)

    # Single-consumer contract: a second concurrent await is a usage error.
    src5 = a.A(82)
    st5 = src5.frames(1)
    first = st5.__anext__()
    try:
        st5.__anext__()
        raise SystemExit("expected a RuntimeError for concurrent await")
    except RuntimeError:
        pass
    first.cancel()
    src5.unsubscribe(st5.id)


asyncio.run(async_iteration_checks())

# --- GC: a callback that captures its own producer is a collectable cycle ---
# The callable is held by a Rust closure inside the subscription, which the
# collector cannot see on its own. `__traverse__` reports it and `__clear__`
# drops it, so this pair is ordinary garbage rather than a permanent leak.
import gc
import weakref


def _make_cycle():
    src = a.A(90)

    def on_frame(frame, _src=None):
        # Captures `src` itself: src -> subscription -> closure -> on_frame
        # -> src. Invisible to the GC without traversal support.
        return src.counter

    src.subscribe(on_frame)
    return weakref.ref(src)


ref = _make_cycle()
gc.collect()
assert ref() is None, "a callback capturing its producer must be collectable"

# Traversal must also be honest in the other direction: while the object is
# still reachable, the GC must NOT collect it. `id(live)` captures the
# producer (keeping the cycle) without borrowing the pyclass — see the
# borrow-conflict check below for why that distinction matters.
live = a.A(91)
live_ref = weakref.ref(live)
lid = live.subscribe(lambda frame: id(live))
gc.collect()
assert live_ref() is not None, "a reachable producer must survive collection"
live.stream(1, 1)
live.stream_join()
live.unsubscribe(lid)

# --- A callback must not touch the producer object during a &mut method ---
# `stream`/`stream_join`/`unsubscribe` take `&mut self`, so PyO3 holds an
# exclusive borrow of the pyclass for the WHOLE call — including the part
# where the GIL is released and the producer thread delivers. A callback
# that reads an attribute of that same object then needs a shared borrow of
# something already mutably borrowed, and gets a clean RuntimeError instead
# of a data race. This is Rust's aliasing rule showing through the binding,
# not a PyO3 wart: two threads really are accessing one object, one of them
# mutably. Callbacks should capture what they need, not the producer.
conflict = a.A(92)
reports = []
old_hook2 = sys.unraisablehook
sys.unraisablehook = lambda args: reports.append(args.exc_type)
try:
    cid = conflict.subscribe(lambda frame: conflict.counter)
    conflict.stream(1, 1)
    conflict.stream_join()
finally:
    sys.unraisablehook = old_hook2
assert reports and reports[0] is RuntimeError, (
    f"expected a clean RuntimeError from the conflicting borrow, got {reports}"
)
conflict.unsubscribe(cid)

print(f"PYTHON RESULT: PASS ({variant})")
