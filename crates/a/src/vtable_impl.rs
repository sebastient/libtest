//! Vtable backend: identical public surface to the other backends, but every
//! call dispatches through an `AVtableV1` function table installed at runtime
//! (typically imported from Python module `a`'s PyCapsule). No OS-level
//! linking to liba is required — the implementation lives wherever the
//! vtable's function pointers point (e.g. inside the `a` extension module).

use a_abi::{ABuf, AFrameDescV1, AFrameInfoV1, AHandle, ASharedV1, AStatus, AVtableV1};
use core::ffi::c_void;
use core::mem::ManuallyDrop;
use core::ptr::NonNull;
use std::future::Future;
use std::sync::{Mutex, OnceLock};

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

static RUST_SUBS: Mutex<Vec<SubPtr>> = Mutex::new(Vec::new());

unsafe extern "C" fn stream_trampoline(user: *mut c_void, frame: *mut ABuf) {
    // SAFETY: `user` is the box installed by `subscribe` and kept alive in
    // RUST_SUBS until after unsubscribe returns; `frame` is borrowed for
    // this call, so it is cloned (retained) before ownership is handed on.
    let f = unsafe { &mut *(user as *mut BoxedStreamCb) };
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
    // SAFETY: the completion contract is exactly one invocation, so the box
    // is reclaimed here and the frame reference consumed here.
    let f = unsafe { Box::from_raw(user as *mut BoxedOnceCb) };
    f(unsafe { Frame::from_raw(frame) });
}

static VTABLE: OnceLock<&'static AVtableV1> = OnceLock::new();

/// Install the function table. Idempotent; first caller wins.
///
/// # Safety
/// `vt` must point to a valid `AVtableV1` that outlives the process's use of
/// this crate (a capsule payload owned by a loaded module qualifies).
pub unsafe fn init_vtable(vt: *const AVtableV1) {
    // SAFETY: caller guarantees `vt` outlives this crate's use of it (a
    // capsule payload owned by a loaded module does).
    let _ = VTABLE.set(unsafe { &*vt });
}

fn vt() -> &'static AVtableV1 {
    VTABLE
        .get()
        .expect("a::init_vtable must be called before using the vtable backend")
}

/// repr(transparent) over the handle pointer — same as the dynamic backend.
#[repr(transparent)]
pub struct A(NonNull<AHandle>);

pub type Raw = *mut AHandle;

impl A {
    pub fn new(id: u64) -> Self {
        Self(NonNull::new(unsafe { (vt().a_create)(id) }).expect("a_create returned null"))
    }

    pub fn id(&self) -> u64 {
        unsafe { (vt().a_id)(self.0.as_ptr()) }
    }

    pub fn counter(&self) -> u64 {
        unsafe { (vt().a_counter)(self.0.as_ptr()) }
    }

    pub fn increment(&mut self) {
        unsafe { (vt().a_increment)(self.0.as_ptr()) }
    }

    pub fn scale(&self) -> f64 {
        unsafe { (vt().a_scale)(self.0.as_ptr()) }
    }

    pub fn shared(&self) -> &ASharedV1 {
        unsafe { &*(vt().a_shared)(self.0.as_ptr()) }
    }

    pub fn impl_size() -> usize {
        unsafe { (vt().a_impl_size)() }
    }

    pub fn data(&self) -> &[u8] {
        unsafe {
            let v = (vt().a_data)(self.0.as_ptr());
            core::slice::from_raw_parts(v.ptr, v.len)
        }
    }

    pub fn fill(&mut self, seed: u8) {
        unsafe { (vt().a_fill)(self.0.as_ptr(), seed) }
    }

    pub fn try_fill(&mut self, seed: u8) -> Result<(), AStatus> {
        match AStatus::from_code(unsafe { (vt().a_try_fill)(self.0.as_ptr(), seed) }) {
            AStatus::Ok => Ok(()),
            e => Err(e),
        }
    }

    pub fn copy_data(&self, out: &mut [u8]) -> usize {
        unsafe { (vt().a_copy_data)(self.0.as_ptr(), out.as_mut_ptr(), out.len()) }
    }

    /// Storage kind for frames this producer EMITS (capture/stream pools).
    pub fn set_frame_storage(&mut self, kind: u32) -> Result<(), AStatus> {
        match AStatus::from_code(unsafe { (vt().a_set_frame_storage)(self.0.as_ptr(), kind) }) {
            AStatus::Ok => Ok(()),
            e => Err(e),
        }
    }

    /// The storage kind emitted frames are allocated in.
    pub fn frame_storage(&self) -> u32 {
        unsafe { (vt().a_frame_storage)(self.0.as_ptr()) }
    }

    pub fn frame(&self) -> Frame {
        Frame(NonNull::new(unsafe { (vt().a_frame)(self.0.as_ptr()) }).expect("a_frame null"))
    }

    pub fn as_raw(&self) -> Raw {
        self.0.as_ptr()
    }

    /// Same handle; by-convention exclusive (`&mut self`).
    pub fn as_raw_mut(&mut self) -> Raw {
        self.0.as_ptr()
    }

    /// # Safety
    /// `p` must be a valid, exclusively borrowed handle for the call.
    pub unsafe fn with_raw<R>(p: Raw, f: impl FnOnce(&mut A) -> R) -> R {
        // SAFETY: caller guarantees `p` is valid and exclusively borrowed.
        // ManuallyDrop stops this temporary from destroying the handle.
        let mut a = ManuallyDrop::new(A(unsafe { NonNull::new_unchecked(p) }));
        f(&mut a)
    }

    /// Losing the returned id makes the subscription (and its closure box)
    /// permanent until this object is dropped.
    #[must_use]
    /// Subscribe a Rust callback (owned frame per delivery).
    pub fn subscribe(&mut self, f: impl FnMut(Frame) + Send + 'static) -> u64 {
        self.subscribe_with(f, a_abi::A_DELIVERY_BLOCKING)
    }

    /// Subscribe with an explicit delivery policy (`A_DELIVERY_*`).
    pub fn subscribe_with(&mut self, f: impl FnMut(Frame) + Send + 'static, policy: u32) -> u64 {
        let boxed: *mut BoxedStreamCb = Box::into_raw(Box::new(Box::new(f) as BoxedStreamCb));
        let id = unsafe {
            (vt().a_subscribe_with)(self.0.as_ptr(), stream_trampoline, boxed.cast(), policy)
        };
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
        unsafe { (vt().a_capture2)(self.0.as_ptr(), once_cancellable_trampoline, boxed.cast()) }
    }

    /// Cancel an in-flight capture, blocking until its callback has run.
    pub fn cancel_capture(&mut self, id: u64) -> Result<(), AStatus> {
        match AStatus::from_code(unsafe { (vt().a_capture_cancel)(self.0.as_ptr(), id) }) {
            AStatus::Ok => Ok(()),
            e => Err(e),
        }
    }

    /// Frames this subscription missed (always 0 under blocking delivery).
    pub fn dropped(&self, id: u64) -> u64 {
        unsafe { (vt().a_sub_dropped)(self.0.as_ptr(), id) }
    }

    /// Remove a subscription (see the dynamic backend for the contract).
    pub fn unsubscribe(&mut self, id: u64) {
        unsafe { (vt().a_unsubscribe)(self.0.as_ptr(), id) };
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
        unsafe { (vt().a_capture)(self.0.as_ptr(), once_trampoline, boxed.cast()) }
    }

    /// First-class async capture, built on the completion callback.
    pub fn capture(&self) -> impl Future<Output = Frame> + Send + 'static {
        crate::oneshot::via(|complete| self.capture_cb(complete))
    }

    /// Reallocate the payload in the given storage kind.
    pub fn set_storage(&mut self, kind: u32) -> Result<(), AStatus> {
        match AStatus::from_code(unsafe { (vt().a_set_storage)(self.0.as_ptr(), kind) }) {
            AStatus::Ok => Ok(()),
            e => Err(e),
        }
    }

    pub fn stream(&mut self, count: u32, period_ms: u32) {
        unsafe { (vt().a_stream)(self.0.as_ptr(), count, period_ms) }
    }

    pub fn stream_join(&mut self) {
        unsafe { (vt().a_stream_join)(self.0.as_ptr()) }
    }
}

impl Drop for A {
    fn drop(&mut self) {
        let handle = self.0.as_ptr() as usize;
        unsafe { (vt().a_destroy)(self.0.as_ptr()) }
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

// Same justification as the dynamic backend (see dynamic_impl.rs).
unsafe impl Send for A {}
unsafe impl Sync for A {}

#[repr(transparent)]
pub struct Frame(NonNull<ABuf>);

pub type RawBuf = *mut ABuf;

impl Frame {
    pub fn data(&self) -> &[u8] {
        unsafe {
            let v = (vt().a_buf_map)(self.0.as_ptr());
            core::slice::from_raw_parts(v.ptr, v.len)
        }
    }

    pub fn info(&self) -> AFrameInfoV1 {
        unsafe { (vt().a_buf_info)(self.0.as_ptr()) }
    }

    /// Cross-process storage descriptor (see `AFrameDescV1` for ownership).
    /// Descriptor v2 — see the static backend for what it adds.
    pub fn export2(&self) -> a_abi::AFrameDescV2 {
        unsafe { (vt().a_buf_export2)(self.0.as_ptr()) }
    }

    pub fn export(&self) -> AFrameDescV1 {
        unsafe { (vt().a_buf_export)(self.0.as_ptr()) }
    }

    pub fn data_mut(&mut self) -> Option<&mut [u8]> {
        unsafe {
            let v = (vt().a_buf_map_mut)(self.0.as_ptr());
            if v.ptr.is_null() {
                None
            } else {
                Some(core::slice::from_raw_parts_mut(v.ptr, v.len))
            }
        }
    }

    pub fn into_raw(self) -> RawBuf {
        let p = self.0.as_ptr();
        core::mem::forget(self);
        p
    }

    /// # Safety
    /// `p` must carry a reference the caller is entitled to consume.
    pub unsafe fn from_raw(p: RawBuf) -> Frame {
        // SAFETY: caller transfers a reference it owns; refcount unchanged.
        Frame(unsafe { NonNull::new_unchecked(p) })
    }
}

impl Clone for Frame {
    fn clone(&self) -> Self {
        unsafe { (vt().a_buf_retain)(self.0.as_ptr()) };
        Frame(self.0)
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { (vt().a_buf_release)(self.0.as_ptr()) }
    }
}

unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}
