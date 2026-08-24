"""Free-threaded (PEP 703) concurrency battery for modules a and b.

This is the evidence behind `Py_mod_gil = Py_MOD_GIL_NOT_USED`. Declaring
that slot is an unconditional assertion that the extension is safe without
the GIL; if the assertion is wrong the failure mode is a silent data race,
not a warning. So the flag is only allowed to be set while this file passes.

Two traps make a naive free-threaded check worthless, and both are guarded
here:

  1. A module that has NOT declared the slot causes CPython to re-enable the
     GIL at import, with a RuntimeWarning. Every test below then runs under
     the GIL and passes while proving nothing. `require_no_gil()` refuses to
     continue in that state.
  2. The functional battery (test_ab.py) is single-threaded, so even with
     the GIL genuinely off it exercises no parallelism at all. Everything
     here is deliberately multi-threaded and contended.

Run under a real free-threaded interpreter:

    PYTHONPATH=py/ft venv-ft/bin/python py/test_ft.py

Before the slot is declared, force the GIL off to make the tests meaningful:

    PYTHON_GIL=0 PYTHONPATH=py/ft venv-ft/bin/python py/test_ft.py
"""

import gc
import os
import sys
import threading

import a
import b

variant = sys.argv[1] if len(sys.argv) > 1 else "free-threaded"

# Scaled so contention is likely on a normal machine without making the
# suite slow. THREADS above core count is deliberate: preemption between a
# borrow and its use is exactly the interleaving the GIL used to prevent.
THREADS = 8
ITERS = 500
# A deadlock must fail the run, not hang it. Every contended section is
# wrapped in a watchdog: a gate that hangs forever teaches people to skip it.
TIMEOUT = 60.0

failures = []
checks = 0


def check(cond, msg):
    global checks
    checks += 1
    if cond:
        print(f"  ok   {msg}")
    else:
        print(f"  FAIL {msg}")
        failures.append(msg)


def require_no_gil():
    """Refuse to report success if the GIL is actually on.

    Without this the suite is a rubber stamp: importing a module that has
    not declared `Py_mod_gil` silently re-enables the GIL, every test below
    serialises, and the run goes green having tested nothing.
    """
    if not hasattr(sys, "_is_gil_enabled"):
        print("SKIP: interpreter has no sys._is_gil_enabled (not a 3.13+ build)")
        raise SystemExit(0)
    if sys._is_gil_enabled():
        print(
            "FAIL: the GIL is ENABLED, so this suite would prove nothing.\n"
            "  Either the extension has not declared Py_mod_gil = Py_MOD_GIL_NOT_USED\n"
            "  (check for a RuntimeWarning at import), or this is not a free-threaded\n"
            "  build. To audit a module before the slot is declared, re-run with\n"
            "  PYTHON_GIL=0."
        )
        raise SystemExit(1)
    print(f"-- free-threaded battery ({variant}): GIL is off, {THREADS} threads")


def run_threads(fn, n=THREADS, timeout=TIMEOUT):
    """Run fn(i) on n threads; return (errors, timed_out).

    Exceptions are collected rather than raised: under contention the
    interesting result is usually the *distribution* of outcomes, not the
    first traceback.
    """
    errors = []
    lock = threading.Lock()
    barrier = threading.Barrier(n)

    def wrapped(i):
        # Start together — staggered starts hide the races we are hunting.
        barrier.wait()
        try:
            fn(i)
        except BaseException as exc:  # noqa: BLE001 - recording, not handling
            with lock:
                errors.append(exc)

    ts = [threading.Thread(target=wrapped, args=(i,), daemon=True) for i in range(n)]
    for t in ts:
        t.start()
    for t in ts:
        t.join(timeout)
    return errors, any(t.is_alive() for t in ts)


require_no_gil()

# --- 1. The with_a exclusivity contract, under real parallelism ------------
#
# THE headline test. b.process performs exactly two increments through the
# capsule's with_a, which holds module a's borrow for the callback's whole
# duration. Under the GIL that exclusivity was free. Without it, the counter
# is an exact ledger: every call that returned successfully contributed
# exactly 2, so `counter == 2 * successes` unless an update was lost to a
# race. A torn read or a lost increment shows up as an inequality, which is
# the whole point -- this is the Python-level form of the borrow shape that
# Miri rejects in crates/app/tests/scenarios.rs.
print("\n-- with_a exclusivity")
shared = a.A(1)
ok_count = [0]
ok_lock = threading.Lock()


def hammer_process(_i):
    local = 0
    for _ in range(ITERS):
        try:
            b.process(shared)
            local += 1
        except RuntimeError:
            # A conflicting borrow is a legitimate, documented outcome
            # (AStatus::Busy). It must be reported as an error, never
            # silently produce a second aliasing handle.
            pass
    with ok_lock:
        ok_count[0] += local


errs, hung = run_threads(hammer_process)
check(not hung, "concurrent b.process did not deadlock")
check(not errs, f"concurrent b.process raised nothing unexpected ({errs[:1]})")
check(
    shared.counter == 2 * ok_count[0],
    f"no lost updates: counter={shared.counter} == 2*successes={2 * ok_count[0]}",
)

# --- 2. pyclass mutation under contention ---------------------------------
#
# The same ledger one level down: a.increment() goes through PyO3's
# non-frozen pyclass borrow rather than the capsule. Under free-threading a
# conflicting borrow becomes reachable where the GIL made it impossible.
print("\n-- pyclass borrow contention")
counted = a.A(2)
inc_ok = [0]
inc_lock = threading.Lock()


def hammer_increment(_i):
    local = 0
    for _ in range(ITERS):
        try:
            counted.increment()
            local += 1
        except RuntimeError:
            pass
    with inc_lock:
        inc_ok[0] += local


errs, hung = run_threads(hammer_increment)
check(not hung, "concurrent increment did not deadlock")
check(not errs, f"concurrent increment raised nothing unexpected ({errs[:1]})")
check(
    counted.counter == inc_ok[0],
    f"no lost increments: counter={counted.counter} == successes={inc_ok[0]}",
)

# --- 3. Subscription churn while frames are being delivered ----------------
#
# subscribe/unsubscribe mutate a Vec on the pyclass while the producer
# thread is invoking callbacks that lock CallbackSlot. This is where the
# attach-then-lock ordering rule has to hold without the GIL serialising the
# two sides.
print("\n-- subscription churn against a live producer")
churn = a.A(3)
delivered = [0]
deliver_lock = threading.Lock()


def on_frame(_f):
    with deliver_lock:
        delivered[0] += 1


# subscribe/unsubscribe take &mut self, so PyO3 hands out an exclusive
# PyRefMut. Concurrent callers therefore MEET, and the loser gets
# RuntimeError("Already borrowed") -- an outcome the GIL made unreachable.
# That is correct refusal, not corruption, so it is counted rather than
# treated as a defect; what must never happen is a deadlock, a wrong
# subscription id, or a delivery to an unsubscribed callback.
churn_conflicts = [0]
conflict_lock = threading.Lock()


def churn_subs(_i):
    local = 0
    for _ in range(50):
        try:
            sid = churn.subscribe(on_frame)
        except RuntimeError:
            local += 1
            continue
        try:
            churn.unsubscribe(sid)
        except RuntimeError:
            local += 1
    with conflict_lock:
        churn_conflicts[0] += local


churn.stream(200, 1)
errs, hung = run_threads(churn_subs)
churn.stream_join()
check(not hung, "subscribe/unsubscribe churn did not deadlock")
check(
    not errs,
    f"subscribe/unsubscribe churn raised only borrow conflicts ({errs[:1]})",
)
print(f"  note {churn_conflicts[0]} borrow conflicts refused cleanly (GIL would have hidden these)")

# --- 4. GC pressure while callbacks are running ----------------------------
#
# The specific deadlock this hunts: a callback holds CallbackSlot (or
# StreamState) and then re-enters CPython -- allocating a Frame, resolving a
# future. If a collection on another thread reaches __traverse__/__clear__
# on the same object and takes that lock, the two orderings meet. Under the
# GIL this interleaving could not occur.
print("\n-- GC pressure during delivery")
gcx = a.A(4)
gc_seen = [0]
gc_lock = threading.Lock()


def gc_cb(_f):
    with gc_lock:
        gc_seen[0] += 1


gcx.subscribe(gc_cb)


def gc_hammer(i):
    if i % 2 == 0:
        for _ in range(200):
            gc.collect()
    else:
        for _ in range(200):
            gcx.frame()


gcx.stream(200, 1)
errs, hung = run_threads(gc_hammer)
gcx.stream_join()
check(not hung, "gc.collect() during delivery did not deadlock")
check(not errs, f"gc.collect() during delivery raised nothing ({errs[:1]})")

# --- 5. Concurrent readers of one Frame's buffer ---------------------------
#
# Frames are immutable refcounted snapshots, so concurrent readers SHOULD be
# the easy case -- but the buffer protocol hands out raw pointers and the
# refcount now moves without the GIL. Getting a wrong answer here would mean
# the zero-copy contract is unsound under parallelism.
print("\n-- concurrent Frame buffer readers")
src = a.A(5)
src.fill(3)
frame = src.frame()
expected_first = 3


def read_frame(_i):
    for _ in range(200):
        mv = memoryview(frame)
        # Tuple index, not mv[0]: the frame carries a padded row stride
        # (128 bytes for 120 of data), so the view is non-contiguous and a
        # sub-view slice is not representable.
        if mv[0, 0, 0] != expected_first:
            raise AssertionError(f"torn read: {mv[0, 0, 0]} != {expected_first}")
        mv.release()


errs, hung = run_threads(read_frame)
check(not hung, "concurrent Frame readers did not deadlock")
check(not errs, f"concurrent Frame readers saw consistent bytes ({errs[:1]})")

# --- 6. Frame lifetime under concurrent create/drop ------------------------
#
# Exercises the refcount that crosses the .so boundary (retain/release) from
# several threads at once. A lost decrement leaks; a lost increment is a
# use-after-free. leaks.sh covers the single-threaded case natively.
print("\n-- concurrent frame create/drop")
churn2 = a.A(6)
churn2.fill(9)


def frame_churn(_i):
    for _ in range(300):
        f = churn2.frame()
        _ = bytes(memoryview(f)[:8])
        del f


errs, hung = run_threads(frame_churn)
check(not hung, "concurrent frame create/drop did not deadlock")
check(not errs, f"concurrent frame create/drop raised nothing ({errs[:1]})")

# --- 7. Concurrent __anext__ on one FrameStream ----------------------------
#
# StreamState.pending is documented "at most one -- an async iterator is
# polled by a single consumer by contract". Under the GIL that contract
# enforced itself. Without it, the check-and-set must be atomic or a parked
# consumer gets silently replaced and waits forever. It IS atomic (both
# happen under one mutex guard in __anext__), and this proves it: the
# symptom of a lost race would be a timeout, not an exception.
print("\n-- concurrent __anext__ (single-consumer contract)")
import asyncio  # noqa: E402 - only needed by this section

sx = a.A(7)
stream = sx.frames(8)
outcomes = {"frame": 0, "refused": 0, "timeout": 0}
outcome_lock = threading.Lock()


async def _one(local):
    # __anext__ must be called from INSIDE the running loop: it resolves the
    # future against asyncio.get_running_loop(), so invoking it before
    # run_until_complete starts the loop raises "no running event loop" --
    # a RuntimeError indistinguishable from the single-consumer refusal this
    # section exists to observe. Getting that wrong made every attempt look
    # "refused" and the section prove nothing.
    try:
        await asyncio.wait_for(stream.__anext__(), 5.0)
        local["frame"] += 1
    except (asyncio.TimeoutError, TimeoutError):
        # A future that parked and was then silently abandoned.
        local["timeout"] += 1
    except RuntimeError as exc:
        if "single-consumer" not in str(exc):
            raise
        local["refused"] += 1


def await_next(_i):
    local = {"frame": 0, "refused": 0, "timeout": 0}
    loop = asyncio.new_event_loop()
    try:
        for _ in range(20):
            loop.run_until_complete(_one(local))
    finally:
        loop.close()
    with outcome_lock:
        for k, v in local.items():
            outcomes[k] += v


sx.stream(400, 1)
errs, hung = run_threads(await_next, n=4)
sx.stream_join()
check(not hung, "concurrent __anext__ did not deadlock")
check(not errs, f"concurrent __anext__ raised only documented errors ({errs[:1]})")
check(
    outcomes["timeout"] == 0,
    f"no consumer was silently abandoned (timeouts={outcomes['timeout']}, "
    f"frames={outcomes['frame']}, refused={outcomes['refused']})",
)
# Without this the section can pass while delivering nothing at all, which
# is exactly how the first version of it lied.
check(
    outcomes["frame"] > 0,
    f"the contended stream actually delivered frames ({outcomes['frame']})",
)

# --- Result ----------------------------------------------------------------
print()
if failures:
    print(f"RESULT: FAIL ({len(failures)} of {checks} checks) [{variant}]")
    for f in failures:
        print(f"  - {f}")
    raise SystemExit(1)
print(f"RESULT: PASS ({checks} checks, GIL off) [{variant}]")
