//! Dynamic backend: `A` wraps the opaque liba.so handle. Identical public
//! surface to the static backend; zero implementation code is linked in —
//! every method resolves to liba.so at load time.

use a_ffi::{
    a_buf_export, a_buf_export2, a_buf_info, a_buf_map, a_buf_map_mut, a_buf_release, a_buf_retain,
    a_capture, a_capture2, a_capture_cancel, a_copy_data, a_counter, a_create, a_data, a_destroy,
    a_fill, a_frame, a_frame_storage, a_id, a_impl_size, a_increment, a_scale, a_set_frame_storage,
    a_set_storage, a_shared, a_stream, a_stream_join, a_sub_dropped, a_subscribe_with, a_try_fill,
    a_unsubscribe, ABuf, AFrameDescV1, AFrameInfoV1, AHandle, ASharedV1, AStatus,
};
use core::ffi::c_void;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use std::future::Future;
use std::sync::Mutex;

type BoxedStreamCb = Box<dyn FnMut(Frame) + Send>;
type BoxedOnceCb = Box<dyn FnOnce(Frame) + Send>;

/// Registry entry keyed by (owning handle, subscription id): liba allocates
/// ids PER OBJECT, so id alone is ambiguous across instances — matching on
/// the handle too is what makes unsubscribe free the right closure box.
struct SubPtr {
    handle: usize,
    id: u64,
    cb: *mut BoxedStreamCb,
}
unsafe impl Send for SubPtr {}

/// Rust-closure subscriptions: the box must outlive the C subscription; the
/// C contract (`a_unsubscribe` returns only after any in-flight invocation)
/// makes dropping it right after unsubscribe sound.
static RUST_SUBS: Mutex<Vec<SubPtr>> = Mutex::new(Vec::new());

unsafe extern "C" fn stream_trampoline(user: *mut c_void, frame: *mut ABuf) {
    // SAFETY: `user` is the box installed by `subscribe`, kept alive in
    // RUST_SUBS until after `a_unsubscribe` returns; `frame` is a live
    // handle liba owns for the duration of this call.
    let f = unsafe { &mut *(user as *mut BoxedStreamCb) };
    // Borrowed-frame contract: retain (clone) before handing ownership on.
    let borrowed = ManuallyDrop::new(Frame(unsafe { NonNull::new_unchecked(frame) }));
    f((*borrowed).clone());
}

/// Completion trampoline for CANCELLABLE captures: a NULL frame means the
/// capture was cancelled. Still invoked exactly once, so the box is freed
/// here either way.
type BoxedOnceCancellableCb = Box<dyn FnOnce(Option<Frame>) + Send>;

unsafe extern "C" fn once_cancellable_trampoline(user: *mut c_void, frame: *mut ABuf) {
    // SAFETY: exactly-once is the C contract, so reclaiming the box here
    // (and only here) is correct.
    let f = unsafe { Box::from_raw(user as *mut BoxedOnceCancellableCb) };
    let frame = NonNull::new(frame).map(Frame);
    f(frame);
}

unsafe extern "C" fn once_trampoline(user: *mut c_void, frame: *mut ABuf) {
    // Completion contract: invoked exactly once, frame ownership transfers.
    // SAFETY: single invocation is the C contract, so reclaiming the box
    // here (and only here) is correct, as is consuming the frame reference.
    let f = unsafe { Box::from_raw(user as *mut BoxedOnceCb) };
    f(unsafe { Frame::from_raw(frame) });
}

/// repr(transparent) over the handle pointer: an `A` value IS the pointer,
/// which is what lets `with_raw` view a C-provided handle as `&mut A`.
#[repr(transparent)]
pub struct A(NonNull<AHandle>);

/// The C-facing handle representation for this backend.
pub type Raw = *mut AHandle;

impl A {
    pub fn new(id: u64) -> Self {
        Self(NonNull::new(unsafe { a_create(id) }).expect("a_create returned null"))
    }

    pub fn id(&self) -> u64 {
        unsafe { a_id(self.0.as_ptr()) }
    }

    pub fn counter(&self) -> u64 {
        unsafe { a_counter(self.0.as_ptr()) }
    }

    pub fn increment(&mut self) {
        unsafe { a_increment(self.0.as_ptr()) }
    }

    pub fn scale(&self) -> f64 {
        unsafe { a_scale(self.0.as_ptr()) }
    }

    pub fn shared(&self) -> &ASharedV1 {
        unsafe { &*a_shared(self.0.as_ptr()) }
    }

    /// Diagnostic: liba.so's report of its private struct size.
    pub fn impl_size() -> usize {
        unsafe { a_impl_size() }
    }

    /// Borrow an `A` from a raw C handle for the duration of a callback.
    /// Ownership is NOT taken: `ManuallyDrop` prevents `a_destroy`.
    ///
    /// # Safety
    /// `p` must be a valid, exclusively borrowed handle for the call.
    pub unsafe fn with_raw<R>(p: Raw, f: impl FnOnce(&mut A) -> R) -> R {
        // SAFETY: caller guarantees `p` is valid and exclusively borrowed.
        // ManuallyDrop stops this temporary A from running a_destroy.
        let mut a = ManuallyDrop::new(A(unsafe { NonNull::new_unchecked(p) }));
        f(&mut a)
    }

    /// Borrowed view of the payload, zero-copy across the boundary. The
    /// lifetime tie to `&self` upholds the C contract (view invalidated by
    /// mutation or destroy) in safe Rust.
    pub fn data(&self) -> &[u8] {
        unsafe {
            let v = a_data(self.0.as_ptr());
            core::slice::from_raw_parts(v.ptr, v.len)
        }
    }

    /// Refill the payload with a deterministic pattern (mutation).
    pub fn fill(&mut self, seed: u8) {
        unsafe { a_fill(self.0.as_ptr(), seed) }
    }

    /// Fallible fill. Seed 0 is reserved (demonstrates the error-code
    /// convention); otherwise identical to `fill`.
    pub fn try_fill(&mut self, seed: u8) -> Result<(), AStatus> {
        match AStatus::from_code(unsafe { a_try_fill(self.0.as_ptr(), seed) }) {
            AStatus::Ok => Ok(()),
            e => Err(e),
        }
    }

    /// Caller-provided buffer model: copies up to `out.len()` bytes.
    pub fn copy_data(&self, out: &mut [u8]) -> usize {
        unsafe { a_copy_data(self.0.as_ptr(), out.as_mut_ptr(), out.len()) }
    }

    /// Storage kind for frames this producer EMITS (capture/stream pools).
    pub fn set_frame_storage(&mut self, kind: u32) -> Result<(), AStatus> {
        match AStatus::from_code(unsafe { a_set_frame_storage(self.0.as_ptr(), kind) }) {
            AStatus::Ok => Ok(()),
            e => Err(e),
        }
    }

    /// The storage kind emitted frames are allocated in.
    pub fn frame_storage(&self) -> u32 {
        unsafe { a_frame_storage(self.0.as_ptr()) }
    }

    /// Zero-copy refcounted snapshot of the current payload.
    pub fn frame(&self) -> Frame {
        Frame(NonNull::new(unsafe { a_frame(self.0.as_ptr()) }).expect("a_frame returned null"))
    }

    /// Reallocate the payload in the given storage kind.
    pub fn set_storage(&mut self, kind: u32) -> Result<(), AStatus> {
        match AStatus::from_code(unsafe { a_set_storage(self.0.as_ptr(), kind) }) {
            AStatus::Ok => Ok(()),
            e => Err(e),
        }
    }

    /// This object's C-facing handle (borrowed; no ownership transfer).
    pub fn as_raw(&self) -> Raw {
        self.0.as_ptr()
    }

    /// Same handle; by-convention exclusive (`&mut self`) — the handle is
    /// opaque in this backend, so provenance is liba's concern, but keeping
    /// the read/write split mirrors the static backend's contract.
    pub fn as_raw_mut(&mut self) -> Raw {
        self.0.as_ptr()
    }

    /// Losing the returned id makes the subscription (and its closure box)
    /// permanent until this object is dropped.
    #[must_use]
    /// Subscribe a Rust callback; invoked from the producer thread with an
    /// owned (retained) frame per delivery.
    pub fn subscribe(&mut self, f: impl FnMut(Frame) + Send + 'static) -> u64 {
        self.subscribe_with(f, a_abi::A_DELIVERY_BLOCKING)
    }

    /// Subscribe with an explicit delivery policy (`A_DELIVERY_*`).
    pub fn subscribe_with(&mut self, f: impl FnMut(Frame) + Send + 'static, policy: u32) -> u64 {
        let boxed: *mut BoxedStreamCb = Box::into_raw(Box::new(Box::new(f) as BoxedStreamCb));
        let id =
            unsafe { a_subscribe_with(self.0.as_ptr(), stream_trampoline, boxed.cast(), policy) };
        RUST_SUBS.lock().unwrap().push(SubPtr {
            handle: self.0.as_ptr() as usize,
            id,
            cb: boxed,
        });
        id
    }

    /// Cancellable capture; see the static backend for the contract.
    #[must_use]
    pub fn capture_cb_cancellable(
        &mut self,
        f: impl FnOnce(Option<Frame>) + Send + 'static,
    ) -> u64 {
        let boxed: *mut BoxedOnceCancellableCb =
            Box::into_raw(Box::new(Box::new(f) as BoxedOnceCancellableCb));
        unsafe { a_capture2(self.0.as_ptr(), once_cancellable_trampoline, boxed.cast()) }
    }

    /// Cancel an in-flight capture, blocking until its callback has run.
    pub fn cancel_capture(&mut self, id: u64) -> Result<(), AStatus> {
        match AStatus::from_code(unsafe { a_capture_cancel(self.0.as_ptr(), id) }) {
            AStatus::Ok => Ok(()),
            e => Err(e),
        }
    }

    /// Frames this subscription missed (always 0 under blocking delivery).
    pub fn dropped(&self, id: u64) -> u64 {
        unsafe { a_sub_dropped(self.0.as_ptr(), id) }
    }

    /// Remove a subscription; on return, no invocation is in flight and
    /// none will follow. Must not be called from within the callback.
    pub fn unsubscribe(&mut self, id: u64) {
        unsafe { a_unsubscribe(self.0.as_ptr(), id) };
        Self::forget_registry_entry(self.0.as_ptr() as usize, id);
    }

    /// Free the closure box for one (handle, id) registry entry. Sound only
    /// AFTER liba's unsubscribe/teardown guarantee: no invocation in flight
    /// and none to come.
    fn forget_registry_entry(handle: usize, id: u64) {
        let mut subs = RUST_SUBS.lock().unwrap();
        if let Some(i) = subs.iter().position(|s| s.handle == handle && s.id == id) {
            let entry = subs.remove(i);
            drop(unsafe { Box::from_raw(entry.cb) });
        }
    }

    /// One-shot async capture via completion callback (owned frame).
    pub fn capture_cb(&self, f: impl FnOnce(Frame) + Send + 'static) {
        let boxed: *mut BoxedOnceCb = Box::into_raw(Box::new(Box::new(f) as BoxedOnceCb));
        unsafe { a_capture(self.0.as_ptr(), once_trampoline, boxed.cast()) }
    }

    /// First-class async capture, built on the completion callback.
    pub fn capture(&self) -> impl Future<Output = Frame> + Send + 'static {
        crate::oneshot::via(|complete| self.capture_cb(complete))
    }

    /// Produce `count` frames ~`period_ms` apart on liba's internal thread.
    pub fn stream(&mut self, count: u32, period_ms: u32) {
        unsafe { a_stream(self.0.as_ptr(), count, period_ms) }
    }

    /// Block until the active stream completes. Must not be called from
    /// within a callback.
    pub fn stream_join(&mut self) {
        unsafe { a_stream_join(self.0.as_ptr()) }
    }
}

impl Drop for A {
    fn drop(&mut self) {
        // Free this handle's closure boxes first: a_destroy tears down the
        // subscriptions inside liba (its Drop guarantees no callback in
        // flight afterwards), so the boxes are unreachable once it returns.
        let handle = self.0.as_ptr() as usize;
        unsafe { a_destroy(self.0.as_ptr()) }
        let mut subs = RUST_SUBS.lock().unwrap();
        subs.retain(|s| {
            if s.handle == handle {
                drop(unsafe { Box::from_raw(s.cb) });
                false
            } else {
                true
            }
        });
    }
}

impl core::fmt::Debug for A {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("A").field(&self.0).finish()
    }
}

// Send/Sync policy: the dynamic wrapper declares exactly the auto traits the
// static type derives (Arc payload + plain data => Send + Sync). Soundness
// relies on the C contract: liba's `&self` entry points do only reads with
// no interior mutability, and all mutation requires `&mut A`, so Rust's
// aliasing rules provide the synchronization in both backends alike.
unsafe impl Send for A {}
unsafe impl Sync for A {}

/// Refcounted, immutable buffer snapshot: a handle whose strong count lives
/// inside liba.so. Clone/Drop map onto retain/release across the C ABI.
#[repr(transparent)]
pub struct Frame(NonNull<ABuf>);

/// C-facing handle for a refcounted buffer in this backend.
pub type RawBuf = *mut ABuf;

impl Frame {
    pub fn data(&self) -> &[u8] {
        unsafe {
            let v = a_buf_map(self.0.as_ptr());
            core::slice::from_raw_parts(v.ptr, v.len)
        }
    }

    /// The frame's geometry (shape and strides).
    pub fn info(&self) -> AFrameInfoV1 {
        unsafe { a_buf_info(self.0.as_ptr()) }
    }

    /// Cross-process storage descriptor (see `AFrameDescV1` for ownership).
    /// Descriptor v2 — see the static backend for what it adds.
    pub fn export2(&self) -> a_abi::AFrameDescV2 {
        unsafe { a_buf_export2(self.0.as_ptr()) }
    }

    pub fn export(&self) -> AFrameDescV1 {
        unsafe { a_buf_export(self.0.as_ptr()) }
    }

    /// Exclusive write access: `Some` only while this is the sole reference.
    pub fn data_mut(&mut self) -> Option<&mut [u8]> {
        unsafe {
            let v = a_buf_map_mut(self.0.as_ptr());
            if v.ptr.is_null() {
                None
            } else {
                Some(core::slice::from_raw_parts_mut(v.ptr, v.len))
            }
        }
    }

    /// Transfer this reference to a raw C handle (no refcount change).
    pub fn into_raw(self) -> RawBuf {
        let p = self.0.as_ptr();
        core::mem::forget(self);
        p
    }

    /// Take ownership of one reference from a raw C handle.
    ///
    /// # Safety
    /// `p` must come from `into_raw`/`a_frame` and carry a reference the
    /// caller is entitled to consume.
    pub unsafe fn from_raw(p: RawBuf) -> Frame {
        // SAFETY: caller transfers a reference it owns; refcount unchanged.
        Frame(unsafe { NonNull::new_unchecked(p) })
    }
}

impl Clone for Frame {
    fn clone(&self) -> Self {
        unsafe { a_buf_retain(self.0.as_ptr()) };
        Frame(self.0)
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { a_buf_release(self.0.as_ptr()) }
    }
}

// Sound because the buffer is immutable while shared and liba's refcount is
// atomic (Arc). Mirrors the static backend, where Arc<Vec<u8>> is Send+Sync.
unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}
