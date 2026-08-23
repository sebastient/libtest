//! Rust-level scenario battery for the static backend.
//!
//! These mirror the checks in `src/main.rs` (and, where they overlap, the C
//! harness) but as discrete `#[test]`s so they can run under Miri:
//!
//! ```sh
//! MIRIFLAGS="-Zmiri-disable-isolation" cargo +nightly miri test -p app
//! MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-tree-borrows" cargo +nightly miri test -p app
//! ```
//!
//! Miri cannot execute FFI, so anything reaching a real platform allocator
//! (`dma_heap` ioctls, `IOSurface`) is gated behind `cfg(not(miri))`. Everything
//! else — refcounts, COW, the borrow/aliasing model, threads, the producer
//! thread and its callbacks — runs unmodified, which is the part that
//! carries the soundness argument.

use a::A;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

/// Minimal park/unpark executor — same one `src/main.rs` uses, duplicated
/// here because a binary crate's internals aren't importable from its
/// integration tests.
fn block_on<F: Future>(fut: F) -> F::Output {
    struct ThreadWaker(std::thread::Thread);
    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => std::thread::park(),
        }
    }
}

#[test]
#[expect(clippy::float_cmp, reason = "exact ABI values, drift must not be masked")]
fn rust_api_basics() {
    let mut x = A::new(40);
    assert_eq!(b::process(&mut x), 42);
    assert_eq!(b::process_fast(&x), 1.5 * 42.0);
    assert_eq!(A::impl_size(), b::observed_impl_size());
}

#[test]
fn zero_copy_view_and_cow() {
    let mut x = A::new(7);
    x.fill(1);
    let cs = b::checksum(&x);
    assert_eq!(b::data_ptr(&x), x.data().as_ptr(), "zero-copy: pointer identity");

    let frame = b::grab_frame(&x);
    x.fill(2); // COW detach — the snapshot must keep the old bytes
    assert_eq!(b::frame_checksum(&frame), cs, "snapshot immutable across COW");
    assert_eq!(frame.data()[0], 1, "snapshot keeps seed-1 bytes");
    assert_eq!(x.data()[0], 2, "producer sees seed-2 bytes");
    assert_ne!(
        frame.data().as_ptr(),
        x.data().as_ptr(),
        "COW: addresses diverge after post-snapshot mutation"
    );
}

#[test]
fn frame_outlives_producer() {
    let mut x = A::new(8);
    x.fill(1);
    let cs = b::checksum(&x);
    let frame = b::grab_frame(&x);
    drop(x);
    assert_eq!(b::frame_checksum(&frame), cs, "frame outlives its producer");

    let info = frame.info();
    assert_eq!(
        (info.rows, info.cols, info.channels, info.row_stride),
        (32, 30, 4, 128),
        "geometry travels with the payload; stride > cols*channels"
    );
    assert_eq!(frame.data().len(), 4096);
}

#[test]
fn copy_data_is_bounded_by_capacity() {
    let mut x = A::new(9);
    x.fill(3);
    let mut out = [0u8; 64];
    assert_eq!(x.copy_data(&mut out), 64, "writes at most the capacity given");
    assert_eq!(out[0], x.data()[0]);
}

#[test]
fn error_code_convention() {
    let mut z = A::new(1);
    assert_eq!(z.try_fill(0), Err(a::AStatus::InvalidArgument), "seed 0 reserved");
    z.try_fill(5).unwrap();
    assert_eq!(z.data()[0], 5);
}

#[test]
fn exclusive_write_requires_uniqueness() {
    let mut z = A::new(2);
    z.fill(4);
    let mut zf = b::grab_frame(&z);
    assert!(zf.data_mut().is_none(), "refused while shared with the producer");
    drop(z);
    zf.data_mut().expect("unique: writable")[0] = 0xAA;
    assert_eq!(zf.data()[0], 0xAA, "in-place write once unique");
}

#[test]
fn handles_and_frames_cross_threads() {
    let mut w = A::new(9);
    w.fill(3);
    let wcs = b::checksum(&w);
    let frame = b::grab_frame(&w);

    // Move a frame to another thread (Send).
    let moved = std::thread::spawn(move || b::frame_checksum(&frame));
    assert_eq!(moved.join().unwrap(), wcs);

    // Share &A across a scoped thread (Sync).
    std::thread::scope(|s| {
        s.spawn(|| assert_eq!(b::checksum(&w), wcs, "shared &A across threads"));
    });
}

#[test]
fn subscribe_stream_and_teardown_guarantee() {
    let mut src = A::new(50);
    let firsts = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicU32::new(0));
    let (f2, c2) = (firsts.clone(), count.clone());

    let sid = src.subscribe(move |frame| {
        f2.lock().unwrap().push(frame.data()[0]);
        c2.fetch_add(1, Ordering::SeqCst);
    });
    src.stream(4, 1);
    src.stream_join();
    assert_eq!(*firsts.lock().unwrap(), vec![0, 1, 2, 3], "in-order delivery");

    // The teardown guarantee: after unsubscribe returns, the callback can
    // never run again — so a later stream must not increment the counter.
    src.unsubscribe(sid);
    src.stream(2, 1);
    src.stream_join();
    assert_eq!(count.load(Ordering::SeqCst), 4, "no deliveries after unsubscribe");
}

#[test]
fn destroy_is_full_teardown() {
    // Dropping an A with an active stream and a live subscription must join
    // the stream and run the unsubscribe protocol — nothing in flight after.
    let count = Arc::new(AtomicU32::new(0));
    let c2 = count.clone();
    let mut src = A::new(51);
    let _ = src.subscribe(move |_| {
        c2.fetch_add(1, Ordering::SeqCst);
    });
    src.stream(3, 1);
    drop(src); // must not leak the trampoline box, must not fire afterwards
    let seen = count.load(Ordering::SeqCst);
    assert!(seen <= 3, "at most the streamed count was delivered, got {seen}");
}

#[test]
fn async_capture_over_completion_callback() {
    let src = A::new(52);
    let cap = block_on(src.capture());
    assert_eq!(cap.data()[0], 0xCA, "async capture resolved from producer thread");
}

#[test]
fn composed_async_across_crates() {
    let src = A::new(53);
    let cap = block_on(src.capture());
    let cs = block_on(b::capture_checksum(&src));
    assert_eq!(cs, b::frame_checksum(&cap), "b's composed async checksum");
}

#[test]
fn unsupported_storage_kind_is_rejected() {
    let mut d = A::new(60);
    assert_eq!(
        d.set_storage(99),
        Err(a::AStatus::InvalidArgument),
        "unknown kinds are refused at runtime, not compile time"
    );
}

#[test]
fn frame_pool_storage_is_independent_of_payload_storage() {
    let mut d = A::new(61);
    assert_eq!(d.frame_storage(), a::A_STORAGE_HEAP, "emitted frames default to heap");
    assert_eq!(
        d.set_frame_storage(99),
        Err(a::AStatus::InvalidArgument),
        "unsupported pool kinds fail eagerly rather than downgrading per frame"
    );
    // Heap is always supported, so this round-trips on every platform.
    d.set_frame_storage(a::A_STORAGE_HEAP).unwrap();
    assert_eq!(d.frame_storage(), a::A_STORAGE_HEAP);
}

#[test]
fn descriptor_v2_supersets_v1() {
    let mut x = A::new(62);
    x.fill(1);
    let f = x.frame();
    let (v1, v2) = (f.export(), f.export2());
    assert_eq!(
        (v1.kind, v1.fd, v1.id, v1.offset, v1.len),
        (v2.kind, v2.fd, v2.id, v2.offset, v2.len),
        "v2 reproduces every v1 field"
    );
    assert_eq!(v2.fourcc, a_abi::A_FOURCC_RGBA8888);
    assert_eq!(v2.modifier, a_abi::A_MODIFIER_LINEAR);
    assert_eq!((v2.width, v2.height, v2.stride), (30, 32, 128));
    assert_eq!(v2.plane_count, 1, "single-plane; NV12 would be a v3 shape");
}

/// Wall-clock assertion: the whole claim is "the producer is not held up",
/// which is a statement about elapsed time. Miri runs orders of magnitude
/// slower and cannot model that, so this one is native-only — the
/// *soundness* of the pump (its locking and teardown) is covered by
/// `blocking_delivery_is_lossless` and the teardown tests, which do run
/// under Miri.
#[test]
#[cfg(not(miri))]
fn latest_delivery_drops_rather_than_blocking_the_producer() {
    use std::time::Instant;
    let seen = Arc::new(AtomicU32::new(0));
    let s2 = seen.clone();
    let mut src = A::new(70);
    // 20ms per frame against a 1ms stream period: the subscriber cannot
    // keep up, so under LATEST it must miss frames instead of holding the
    // producer up.
    let sid = src.subscribe_with(
        move |_| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            s2.fetch_add(1, Ordering::SeqCst);
        },
        a::A_DELIVERY_LATEST,
    );
    let started = Instant::now();
    src.stream(8, 1);
    src.stream_join();
    let producer = started.elapsed();
    assert!(
        producer < std::time::Duration::from_millis(100),
        "producer was held up by a slow subscriber: {producer:?}"
    );
    src.unsubscribe(sid);
    let delivered = seen.load(Ordering::SeqCst) as u64;
    assert!(delivered + src.dropped(sid) <= 8, "seen + dropped exceeds produced");
}

#[test]
fn blocking_delivery_is_lossless() {
    let seen = Arc::new(AtomicU32::new(0));
    let s2 = seen.clone();
    let mut src = A::new(71);
    let sid = src.subscribe_with(move |_| { s2.fetch_add(1, Ordering::SeqCst); }, a::A_DELIVERY_BLOCKING);
    src.stream(4, 1);
    src.stream_join();
    assert_eq!(seen.load(Ordering::SeqCst), 4, "every frame delivered");
    assert_eq!(src.dropped(sid), 0, "blocking delivery drops nothing");
    src.unsubscribe(sid);
}

#[test]
fn capture_cancellation_still_invokes_exactly_once() {
    let calls = Arc::new(AtomicU32::new(0));
    let got_none = Arc::new(AtomicU32::new(0));
    let (c2, n2) = (calls.clone(), got_none.clone());
    let mut src = A::new(72);
    let id = src.capture_cb_cancellable(move |frame| {
        c2.fetch_add(1, Ordering::SeqCst);
        if frame.is_none() {
            n2.fetch_add(1, Ordering::SeqCst);
        }
    });
    // cancel BLOCKS until the callback has run, so no sleep is needed —
    // which is the point of making it block.
    src.cancel_capture(id).unwrap();

    // The INVARIANT, true regardless of who won the race: exactly one
    // invocation. That is what lets every trampoline free its context box
    // unconditionally, and it is the part worth checking under Miri.
    assert_eq!(calls.load(Ordering::SeqCst), 1, "invoked exactly once");
    let none = got_none.load(Ordering::SeqCst);
    assert!(none <= 1, "at most one outcome can be reported");

    // WHICH side won is timing-dependent: the capture sleeps 2ms and the
    // cancel is issued immediately, so natively the cancel wins ~always.
    // Miri's scheduler makes the capture win instead, which is a legitimate
    // outcome of a genuine race rather than a failure.
    #[cfg(not(miri))]
    assert_eq!(none, 1, "cancel issued immediately wins the 2ms race natively");
    assert_eq!(
        src.cancel_capture(id),
        Err(a::AStatus::InvalidArgument),
        "cancelling a finished capture is a benign race"
    );
}

#[test]
fn frames_as_an_async_iterator() {
    let mut src = A::new(73);
    let mut stream = src.frames(4);
    src.stream(3, 1);
    src.stream_join();
    // The queue is bounded and the producer has finished, so three frames
    // are waiting to be pulled.
    assert_eq!(stream.pending(), 3);
    let first = block_on(stream.next()).expect("a frame");
    assert_eq!(first.data()[0], 0, "frames arrive in order");
    assert_eq!(block_on(stream.next()).unwrap().data()[0], 1);
    assert_eq!(block_on(stream.next()).unwrap().data()[0], 2);
    assert_eq!(stream.dropped(), 0, "capacity 4 held all 3 frames");
    src.unsubscribe(stream.id());
}

#[test]
fn async_iterator_queue_is_bounded() {
    let mut src = A::new(74);
    let mut stream = src.frames(2);
    src.stream(5, 1);
    src.stream_join();
    assert_eq!(stream.pending(), 2, "queue never exceeds its capacity");
    assert_eq!(stream.dropped(), 3, "the three oldest frames were dropped");
    // Drop-OLDEST means what survives is the newest pair.
    assert_eq!(block_on(stream.next()).unwrap().data()[0], 3);
    assert_eq!(block_on(stream.next()).unwrap().data()[0], 4);
    src.unsubscribe(stream.id());
}

/// Platform buffer objects need real ioctls / `IOSurface` calls, which Miri
/// cannot execute — this is the one scenario that stays native-only.
#[test]
#[cfg(not(miri))]
fn platform_storage_descriptors() {
    let kind = if cfg!(target_os = "macos") {
        a::A_STORAGE_IOSURFACE
    } else if cfg!(target_os = "linux") {
        a::A_STORAGE_DMABUF
    } else {
        a::A_STORAGE_HEAP
    };
    if kind == a::A_STORAGE_HEAP {
        return;
    }

    let mut d = A::new(60);
    d.set_storage(kind).unwrap();
    d.fill(5);
    let fr = d.frame();
    let desc = fr.export();
    assert_eq!(desc.kind, kind, "descriptor kind");
    assert_eq!(desc.len, 4096);
    assert_eq!(fr.data()[0], 5, "storage-backed frame readable");

    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{FromRawFd, OwnedFd};
        assert!(desc.fd >= 0, "dma-buf fd exported");
        drop(unsafe { OwnedFd::from_raw_fd(desc.fd) });
    }
    #[cfg(target_os = "macos")]
    assert_ne!(desc.id, 0, "IOSurface ID exported");
}

// ---------------------------------------------------------------------------
// Capsule aliasing models
// ---------------------------------------------------------------------------
//
// Miri cannot run CPython, so the PyCapsule path cannot be executed directly.
// What CAN be modelled exactly is the *borrow shape* `a-py`'s `unwrap_a`
// creates, which is where the soundness question actually lives:
//
//   1. module a derives a `*mut AHandle` from an exclusive borrow
//      (`try_borrow_mut()` -> `as_raw_mut()`), and that guard is DROPPED
//      when `unwrap_a` returns;
//   2. module b then mutates through the raw pointer via `A::with_raw`,
//      i.e. AFTER the borrow it derived from has expired.
//
// Under the GIL, nothing else can touch the object in between, so the shape
// survives in practice. These two tests isolate what happens when something
// *does* touch it in between — which is precisely what free-threaded CPython
// makes reachable, and what Tree Borrows models.

/// The contract as written today: derive a handle from an exclusive borrow,
/// let that borrow expire, then mutate through the handle. With no
/// intervening access this is the well-behaved case — it is the baseline the
/// current design relies on the GIL to preserve.
#[test]
fn capsule_shape_expired_borrow_no_interleaving() {
    let mut x = A::new(1);
    let before = x.counter();

    // step 1 — inside module a; the PyRefMut equivalent ends at this scope.
    let raw = { x.as_raw_mut() };
    // step 2 — inside module b, borrow already expired.
    unsafe { A::with_raw(raw, A::increment) };

    assert_eq!(x.counter(), before + 1);
}

/// The same shape, but with an access through the owner interleaved between
/// deriving the handle and using it — the free-threading hazard, made
/// deterministic. Under Stacked/Tree Borrows the intervening `&mut` pops the
/// raw pointer's tag, so step 3 is a use of an invalidated pointer.
///
/// Ignored by default: it is a *diagnostic* for the aliasing model, not a
/// behavioural assertion. Run it explicitly under Miri:
///   cargo +nightly miri test -p app -- --ignored `capsule_shape_interleaved`
#[test]
#[ignore = "aliasing diagnostic: run under Miri to evaluate the unwrap_a contract"]
fn capsule_shape_interleaved_access_invalidates_handle() {
    let mut x = A::new(1);

    // step 1 — module a hands out a handle derived from an exclusive borrow.
    let raw = x.as_raw_mut();
    // step 2 — something else touches the object first. Under the GIL this
    // cannot interleave; under free-threading (or a re-entrant call) it can.
    x.increment();
    // step 3 — module b uses the handle it was given earlier.
    unsafe { A::with_raw(raw, A::increment) };

    assert_eq!(x.counter(), 2);
}

/// The proposed v4 contract (`with_a(obj, cb, user)`): module a holds the
/// exclusive borrow for the WHOLE duration of the consumer's callback, so
/// the handle's provenance is live for exactly its window of use and no
/// intervening access is representable.
#[test]
fn capsule_shape_guard_held_across_callback() {
    let mut x = A::new(1);
    let before = x.counter();

    // The borrow spans the consumer's entire operation.
    let out = {
        let guard: &mut A = &mut x;
        let raw = guard.as_raw_mut();
        unsafe {
            A::with_raw(raw, |a| {
                a.increment();
                a.counter()
            })
        }
    };

    assert_eq!(out, before + 1);
    assert_eq!(x.counter(), before + 1);
}
