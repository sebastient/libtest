//! Pure-Rust application using the first-class Rust APIs of both libraries,
//! fully statically linked.

use a::A;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

/// Minimal executor: park the thread until the future resolves. Enough to
/// exercise the runtime-agnostic futures without pulling in a runtime.
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

// Every value compared here is exactly representable and produced by the
// same deterministic arithmetic on both sides of the boundary — an exact
// comparison is the point (a tolerance would hide real ABI drift).
#[expect(clippy::float_cmp, reason = "exact ABI values, drift must not be masked")]
#[expect(clippy::many_single_char_names, reason = "terse scenario locals, read top to bottom")]
fn main() {
    let mut x = A::new(40);
    let r = b::process(&mut x);
    let f = b::process_fast(&x);
    println!(
        "static rust app: process={r} process_fast={f} sizeof(A)={}",
        A::impl_size()
    );
    assert_eq!(r, 42);
    assert_eq!(f, 1.5 * 42.0);

    // Buffers: identical semantics to the C ABI path, in safe Rust.
    x.fill(1);
    let cs = b::checksum(&x);
    assert_eq!(b::data_ptr(&x), x.data().as_ptr(), "zero-copy view");
    let frame = b::grab_frame(&x);
    x.fill(2); // COW: the snapshot keeps the old bytes
    assert_eq!(b::frame_checksum(&frame), cs, "snapshot immutable");
    // NB: the checksum is seed-independent (4096 is a multiple of 256, so
    // every byte value occurs equally often) — compare bytes, not sums.
    assert_eq!(frame.data()[0], 1, "snapshot keeps seed-1 bytes");
    assert_eq!(x.data()[0], 2, "producer sees seed-2 bytes");
    let mut out = [0u8; 64];
    assert_eq!(x.copy_data(&mut out), 64);
    drop(x);
    assert_eq!(b::frame_checksum(&frame), cs, "frame outlives producer");
    let info = frame.info();
    assert_eq!(
        (info.rows, info.cols, info.channels, info.row_stride),
        (32, 30, 4, 128),
        "frame geometry"
    );

    // Error-code convention (backend-agnostic Result surface).
    let mut z = A::new(1);
    assert_eq!(z.try_fill(0), Err(a::AStatus::InvalidArgument));
    z.try_fill(5).unwrap();

    // Exclusive write access: refused while shared, granted when unique.
    let mut zf = b::grab_frame(&z);
    assert!(zf.data_mut().is_none(), "shared with producer: read-only");
    drop(z);
    zf.data_mut().expect("unique: writable")[0] = 0xAA;
    assert_eq!(zf.data()[0], 0xAA);

    // Send + Sync: move a handle and a frame to another thread, and share
    // a frame by reference into a scoped thread.
    let cs_frame = b::frame_checksum(&frame);
    let moved = std::thread::spawn(move || (b::frame_checksum(&frame), b::frame_checksum(&zf)));
    let (m1, m2) = moved.join().unwrap();
    assert_eq!(m1, cs_frame);
    assert!(m2 > 0);
    let mut w = A::new(9);
    w.fill(3);
    let wcs = b::checksum(&w);
    std::thread::scope(|s| {
        s.spawn(|| assert_eq!(b::checksum(&w), wcs, "shared &A across threads"));
    });

    // Callbacks and streaming: subscriber closure on the producer thread.
    let mut src = A::new(50);
    let firsts = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicU32::new(0));
    let (f2, c2) = (firsts.clone(), count.clone());
    let sid = src.subscribe(move |frame| {
        f2.lock().unwrap().push(frame.data()[0]);
        c2.fetch_add(1, Ordering::SeqCst);
    });
    src.stream(4, 2);
    src.stream_join();
    assert_eq!(*firsts.lock().unwrap(), vec![0, 1, 2, 3], "in-order delivery");
    src.unsubscribe(sid);
    src.stream(2, 1);
    src.stream_join();
    assert_eq!(count.load(Ordering::SeqCst), 4, "no deliveries after unsubscribe");

    // First-class async over the completion callback, both in a and via b.
    let cap = block_on(src.capture());
    assert_eq!(cap.data()[0], 0xCA, "async capture");
    let cs2 = block_on(b::capture_checksum(&src));
    assert_eq!(cs2, b::frame_checksum(&cap), "b's composed async checksum");

    // Storage descriptors: platform buffer objects behind the same API.
    let mut d = A::new(60);
    assert_eq!(d.set_storage(99), Err(a::AStatus::InvalidArgument));
    let kind = if cfg!(target_os = "macos") {
        a::A_STORAGE_IOSURFACE
    } else if cfg!(target_os = "linux") {
        a::A_STORAGE_DMABUF
    } else {
        a::A_STORAGE_HEAP
    };
    if kind != a::A_STORAGE_HEAP {
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

    println!("RESULT: PASS");
}
