//! liba.so — the ergonomic C API over crate `a` (static backend).
//! This exported surface (signatures + `ASharedV1` layout) is the entire ABI
//! contract; everything behind it may drift between versions.
//!
//! Kept as a separate leaf crate (not merged into crate `a`) so the
//! `#[no_mangle]` symbols never leak into Rust consumers' binaries, where
//! they could collide with liba.so in the same process.

// SAFETY (applies to every `unsafe` block in this file): each entry point
// documents its own precondition in a `# Safety` section, and every unsafe
// operation below is exactly the one that precondition licenses — a handle
// deref, or a refcount/ownership op on a live buffer handle. `unsafe_op_in_
// unsafe_fn` is denied here deliberately: it keeps those operations visible
// as blocks rather than blanket-covered by the function signature, which in
// a file that is ~nothing but FFI shims is the only way they stay legible.
use a::{ABufView, ABufViewMut, AFrameDescV1, AFrameInfoV1, ASharedV1, A};
use a_abi::ABuf;
use a_abi::AFrameCb;
use core::ffi::c_void;

/// Human-readable detail about the most recent failure **on the calling
/// thread**, or NULL if it has not failed yet.
///
/// Contract: advisory text for logs and debuggers — never parse it, never
/// branch on it; the `AStatus` code is the contract. The pointer is owned by
/// liba and stays valid until this thread's next failing call; copy it if
/// you need it longer. Never free it.
#[no_mangle]
pub extern "C" fn a_last_error_message() -> *const core::ffi::c_char {
    a_cshim::err::last()
}

#[no_mangle]
pub extern "C" fn a_create(id: u64) -> *mut A {
    unsafe { a_cshim::a_create(id) }.cast::<A>()
}

/// Destroy is a full teardown: joins any active stream and removes every
/// remaining subscription (same guarantee as `a_unsubscribe`) before
/// freeing — after it returns, no callback is in flight and none will fire.
/// May block for the remainder of the active stream.
///
/// # Safety
/// `p` must be a pointer returned by `a_create` that has not been destroyed.
#[no_mangle]
pub unsafe extern "C" fn a_destroy(p: *mut A) {
    unsafe { a_cshim::a_destroy(p.cast()) }
}

/// # Safety
/// `p` must be a valid pointer from `a_create`.
#[no_mangle]
pub unsafe extern "C" fn a_id(p: *const A) -> u64 {
    unsafe { a_cshim::a_id(p.cast()) }
}

/// # Safety
/// `p` must be a valid pointer from `a_create`.
#[no_mangle]
pub unsafe extern "C" fn a_counter(p: *const A) -> u64 {
    unsafe { a_cshim::a_counter(p.cast()) }
}

/// # Safety
/// `p` must be a valid pointer from `a_create`.
#[no_mangle]
pub unsafe extern "C" fn a_increment(p: *mut A) {
    unsafe { a_cshim::a_increment(p.cast()) }
}

/// # Safety
/// `p` must be a valid pointer from `a_create`.
#[no_mangle]
pub unsafe extern "C" fn a_scale(p: *const A) -> f64 {
    unsafe { a_cshim::a_scale(p.cast()) }
}

/// The repr(C) fast path: expose the frozen shared window by pointer.
///
/// # Safety
/// `p` must be valid; the returned pointer is borrowed and must not
/// outlive it.
#[no_mangle]
pub unsafe extern "C" fn a_shared(p: *const A) -> *const ASharedV1 {
    unsafe { a_cshim::a_shared(p.cast()) }
}

/// Diagnostic: private implementation size, to demonstrate that internal
/// layout drift is invisible across the boundary.
#[no_mangle]
pub extern "C" fn a_impl_size() -> usize {
    unsafe { a_cshim::a_impl_size() }
}

/// Borrowed view of A's payload (zero-copy).
///
/// # Safety
/// `p` must be valid. The view is invalidated by any mutation of `p`
/// (e.g. `a_fill`) and by `a_destroy`.
#[no_mangle]
pub unsafe extern "C" fn a_data(p: *const A) -> ABufView {
    unsafe { a_cshim::a_data(p.cast()) }
}

/// Refill the payload with a deterministic pattern (mutation: invalidates
/// outstanding `a_data` views; outstanding frames keep the old bytes).
///
/// # Safety
/// `p` must be valid with exclusive access.
#[no_mangle]
pub unsafe extern "C" fn a_fill(p: *mut A, seed: u8) {
    // Shielded because a COW detach allocates; the fallible twin is
    // `a_try_fill` — a panic here is swallowed (state: at worst the old
    // payload is intact).
    unsafe { a_cshim::a_fill(p.cast(), seed) }
}

/// Caller-provided buffer: copies up to `cap` bytes into `out`, returns the
/// number of bytes written.
///
/// # Safety
/// `p` must be valid; `out` must point to at least `cap` writable bytes,
/// or be NULL (with any `cap`) — the NULL/0 probe returns 0, C-style.
#[no_mangle]
pub unsafe extern "C" fn a_copy_data(p: *const A, out: *mut u8, cap: usize) -> usize {
    unsafe { a_cshim::a_copy_data(p.cast(), out, cap) }
}

/// Refcounted zero-copy snapshot of the payload. The caller owns one
/// reference and must balance it with `a_buf_release`.
///
/// # Safety
/// `p` must be valid.
#[no_mangle]
pub unsafe extern "C" fn a_frame(p: *const A) -> *mut ABuf {
    unsafe { a_cshim::a_frame(p.cast()) }
}

/// Atomic; may be called from any thread (callback threads included).
///
/// # Safety
/// `b` must be a live buffer handle.
#[no_mangle]
pub unsafe extern "C" fn a_buf_retain(b: *mut ABuf) {
    unsafe { a_cshim::a_buf_retain(b) }
}

/// Atomic; may be called from any thread. `b` must not be NULL.
///
/// # Safety
/// `b` must carry a reference the caller is entitled to consume.
#[no_mangle]
pub unsafe extern "C" fn a_buf_release(b: *mut ABuf) {
    unsafe { a_cshim::a_buf_release(b) }
}

/// Borrowed view of a buffer's contents; valid while the caller holds a
/// reference to `b`.
///
/// # Safety
/// `b` must be a live buffer handle.
#[no_mangle]
pub unsafe extern "C" fn a_buf_map(b: *mut ABuf) -> ABufView {
    unsafe { a_cshim::a_buf_map(b) }
}

/// Exclusive writable view: non-null only while the caller's reference is
/// the sole one (unique-ownership write access, `Arc::get_mut` semantics).
///
/// # Safety
/// `b` must be a live buffer handle; the view is valid only while that
/// reference remains unique.
#[no_mangle]
pub unsafe extern "C" fn a_buf_map_mut(b: *mut ABuf) -> ABufViewMut {
    unsafe { a_cshim::a_buf_map_mut(b) }
}

/// Geometry (shape and strides) of a frame buffer.
///
/// # Safety
/// `b` must be a live buffer handle.
#[no_mangle]
pub unsafe extern "C" fn a_buf_info(b: *mut ABuf) -> AFrameInfoV1 {
    unsafe { a_cshim::a_buf_info(b) }
}

/// Fallible fill demonstrating the error-code convention: validates its
/// arguments (seed 0 is reserved) and shields internal panics.
///
/// # Safety
/// `p` must be valid with exclusive access.
#[no_mangle]
pub unsafe extern "C" fn a_try_fill(p: *mut A, seed: u8) -> i32 {
    unsafe { a_cshim::a_try_fill(p.cast(), seed) }
}

/// Test hook proving the panic shield: always panics internally and must
/// return `AStatus::Panic` instead of aborting the process.
#[no_mangle]
pub extern "C" fn a_test_panic() -> i32 {
    unsafe { a_cshim::a_test_panic() }
}

/// Reallocate A's payload in the given storage kind (contents carried
/// over). `InvalidArgument` if the kind is unsupported on this platform.
///
/// # Safety
/// `p` must be valid with exclusive access.
#[no_mangle]
pub unsafe extern "C" fn a_set_storage(p: *mut A, kind: u32) -> i32 {
    unsafe { a_cshim::a_set_storage(p.cast(), kind) }
}

/// Export a buffer's storage descriptor (see `AFrameDescV1` for field
/// ownership: a dma-buf `fd` is dup'd and owned by the caller).
///
/// # Safety
/// `b` must be a live buffer handle.
#[no_mangle]
pub unsafe extern "C" fn a_buf_export(b: *mut ABuf) -> AFrameDescV1 {
    unsafe { a_cshim::a_buf_export(b) }
}

/// Descriptor v2: everything `a_buf_export` returns, plus the pixel-format
/// vocabulary an importer needs — DRM fourcc, format modifier, and the
/// buffer's width/height/stride.
///
/// Why a second entry point rather than a bigger struct: `AFrameDescV1` is
/// returned BY VALUE, so its size is compiled into every existing call
/// site and can never change. Both functions are permanent; a consumer
/// built against v1 keeps working verbatim.
///
/// # Safety
/// `b` must be a live buffer handle; the reference is borrowed, not
/// consumed. Same fd-ownership rule as `a_buf_export`: for
/// `A_STORAGE_DMABUF` the returned `fd` is a dup the caller must close.
#[no_mangle]
pub unsafe extern "C" fn a_buf_export2(b: *mut ABuf) -> a_abi::AFrameDescV2 {
    unsafe { a_cshim::a_buf_export2(b) }
}

/// Select the storage kind for frames this producer subsequently EMITS
/// (`a_capture`, `a_stream`) — orthogonal to `a_set_storage`, which
/// reallocates the producer's own working payload. A capture pool feeding a
/// GPU importer wants DMA-BUF frames whether or not the producer's own
/// buffer is one.
///
/// Returns `A_STATUS_INVALID_ARGUMENT` if the kind is unsupported here; the
/// check is eager so an unsupported kind fails once, loudly, instead of
/// silently downgrading every later frame to heap.
///
/// # Safety
/// `p` must be valid with exclusive access.
#[no_mangle]
pub unsafe extern "C" fn a_set_frame_storage(p: *mut A, kind: u32) -> i32 {
    unsafe { a_cshim::a_set_frame_storage(p.cast(), kind) }
}

/// The storage kind emitted frames are allocated in (default
/// `A_STORAGE_HEAP`).
///
/// # Safety
/// `p` must be valid.
#[no_mangle]
pub unsafe extern "C" fn a_frame_storage(p: *const A) -> u32 {
    unsafe { a_cshim::a_frame_storage(p.cast()) }
}

/// Register a streaming callback (see `AFrameCb` for the full contract:
/// producer thread, borrowed frame, no unsubscribe/join from the callback).
///
/// # Safety
/// `p` must be valid with exclusive access; `cb`/`user` callable from
/// another thread for the life of the subscription.
#[no_mangle]
pub unsafe extern "C" fn a_subscribe(p: *mut A, cb: AFrameCb, user: *mut c_void) -> u64 {
    unsafe { a_cshim::a_subscribe(p.cast(), cb, user) }
}

/// Subscribe with an explicit delivery policy (`A_DELIVERY_BLOCKING` or
/// `A_DELIVERY_LATEST`).
///
/// `A_DELIVERY_BLOCKING` is what `a_subscribe` does: the producer thread
/// invokes the callback directly and nothing is dropped, so a slow
/// subscriber slows the stream for everyone. `A_DELIVERY_LATEST` gives this
/// subscription a one-slot mailbox and its own pump thread: the producer
/// never blocks on it, and if a newer frame arrives before the callback
/// finishes, the older one is dropped (count via `a_sub_dropped`).
///
/// The teardown guarantee is identical in both modes: after
/// `a_unsubscribe` returns, no callback is running and none will run.
///
/// Unrecognised policies fall back to blocking rather than refusing — a
/// newer library must never silently stop delivering to an older consumer.
///
/// # Safety
/// `p` must be valid with exclusive access; `cb`/`user` must be callable
/// from another thread until `a_unsubscribe` returns.
#[no_mangle]
pub unsafe extern "C" fn a_subscribe_with(
    p: *mut A,
    cb: AFrameCb,
    user: *mut c_void,
    policy: u32,
) -> u64 {
    unsafe { a_cshim::a_subscribe_with(p.cast(), cb, user, policy) }
}

/// Frames this subscription missed because it was still busy. Always 0
/// under `A_DELIVERY_BLOCKING`, which drops nothing.
///
/// # Safety
/// `p` must be valid.
#[no_mangle]
pub unsafe extern "C" fn a_sub_dropped(p: *const A, id: u64) -> u64 {
    unsafe { a_cshim::a_sub_dropped(p.cast(), id) }
}

/// Remove a subscription. Blocks until any in-flight invocation returns;
/// after it returns the callback will never be invoked again — in either
/// delivery mode (`A_DELIVERY_LATEST` additionally joins the pump thread).
///
/// # Safety
/// `p` must be valid with exclusive access; not callable from a callback.
#[no_mangle]
pub unsafe extern "C" fn a_unsubscribe(p: *mut A, id: u64) {
    unsafe { a_cshim::a_unsubscribe(p.cast(), id) }
}

/// One-shot async capture: `cb` is invoked exactly once, from an internal
/// thread, with an OWNED frame reference (release it when done).
///
/// # Safety
/// `p` must be valid; `cb`/`user` callable from another thread.
#[no_mangle]
pub unsafe extern "C" fn a_capture(p: *const A, cb: AFrameCb, user: *mut c_void) {
    unsafe { a_cshim::a_capture(p.cast(), cb, user) }
}

/// Cancellable one-shot capture. Returns an id for `a_capture_cancel`.
///
/// `cb` is invoked **exactly once**, as with `a_capture` — but `frame` may
/// be NULL, meaning the capture was cancelled before producing anything.
/// Signalling cancellation through the argument rather than by not calling
/// is what keeps the exactly-once invariant every consumer's context
/// cleanup depends on. A non-NULL frame is OWNED, as always.
///
/// # Safety
/// `p` must be valid with exclusive access; `cb`/`user` must be callable
/// from another thread until the invocation has happened.
#[no_mangle]
pub unsafe extern "C" fn a_capture2(p: *mut A, cb: AFrameCb, user: *mut c_void) -> u64 {
    unsafe { a_cshim::a_capture2(p.cast(), cb, user) }
}

/// Cancel an in-flight capture and BLOCK until its callback has run —
/// with NULL if cancellation won the race, or with the frame if the capture
/// had already completed.
///
/// Blocking is deliberate, and the same choice `a_unsubscribe` makes: after
/// this returns nothing is in flight, so the caller may free whatever
/// `user` points at. A non-blocking cancel would leave a consumer with no
/// moment at which its context is provably dead.
///
/// Returns `A_STATUS_INVALID_ARGUMENT` for an unknown id — a capture
/// completing just before you cancel it is an ordinary race, not a fault.
/// Not callable from inside the callback.
///
/// # Safety
/// `p` must be valid with exclusive access.
#[no_mangle]
pub unsafe extern "C" fn a_capture_cancel(p: *mut A, id: u64) -> i32 {
    unsafe { a_cshim::a_capture_cancel(p.cast(), id) }
}

/// Produce `count` frames ~`period_ms` apart on an internal thread.
///
/// # Safety
/// `p` must be valid with exclusive access.
#[no_mangle]
pub unsafe extern "C" fn a_stream(p: *mut A, count: u32, period_ms: u32) {
    unsafe { a_cshim::a_stream(p.cast(), count, period_ms) }
}

/// Block until the active stream completes; not callable from a callback.
///
/// # Safety
/// `p` must be valid with exclusive access.
#[no_mangle]
pub unsafe extern "C" fn a_stream_join(p: *mut A) {
    unsafe { a_cshim::a_stream_join(p.cast()) }
}
