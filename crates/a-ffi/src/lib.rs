//! Declarations-only binding to liba's C ABI — the Rust equivalent of a
//! C header. Contains ZERO implementation: linking this crate adds no code
//! to the consumer, only undefined symbols that the dynamic linker resolves
//! to liba.so at load time.

#![no_std]

pub use a_abi::{
    ABuf, ABufView, ABufViewMut, AFrameCb, AFrameDescV1, AFrameInfoV1, AHandle, AStatus,
    ASharedV1, A_ABI_VERSION, A_STORAGE_DMABUF, A_STORAGE_HEAP, A_STORAGE_IOSURFACE,
};

extern "C" {
    pub fn a_create(id: u64) -> *mut AHandle;
    pub fn a_destroy(p: *mut AHandle);
    pub fn a_id(p: *const AHandle) -> u64;
    pub fn a_counter(p: *const AHandle) -> u64;
    pub fn a_increment(p: *mut AHandle);
    pub fn a_scale(p: *const AHandle) -> f64;
    pub fn a_shared(p: *const AHandle) -> *const ASharedV1;
    pub fn a_impl_size() -> usize;

    // Buffer access — three models.
    /// Borrowed view of A's payload; invalidated by mutation or destroy.
    pub fn a_data(p: *const AHandle) -> ABufView;
    /// Refill the payload with a deterministic pattern (mutation).
    pub fn a_fill(p: *mut AHandle, seed: u8);
    /// Caller-provided buffer: copies up to `cap` bytes, returns bytes written.
    pub fn a_copy_data(p: *const AHandle, out: *mut u8, cap: usize) -> usize;
    /// Refcounted snapshot of the payload; caller owns one reference.
    pub fn a_frame(p: *const AHandle) -> *mut ABuf;
    pub fn a_buf_retain(b: *mut ABuf);
    pub fn a_buf_release(b: *mut ABuf);
    /// Borrowed view of a buffer; valid while the caller holds a reference.
    pub fn a_buf_map(b: *const ABuf) -> ABufView;
    /// Exclusive writable view; `ptr` is null unless the caller's reference
    /// is the only one.
    pub fn a_buf_map_mut(b: *mut ABuf) -> ABufViewMut;
    /// Geometry (shape/strides) of a frame buffer.
    pub fn a_buf_info(b: *const ABuf) -> AFrameInfoV1;
    /// Fallible fill: validates arguments, shields panics at the boundary.
    /// Returns a wire status code — convert with `AStatus::from_code` (a
    /// newer library may return codes this build does not know).
    pub fn a_try_fill(p: *mut AHandle, seed: u8) -> i32;

    // Streaming and async capture (see AFrameCb's contract in a-abi).
    pub fn a_subscribe(p: *mut AHandle, cb: AFrameCb, user: *mut core::ffi::c_void) -> u64;
    pub fn a_unsubscribe(p: *mut AHandle, id: u64);
    pub fn a_capture(p: *const AHandle, cb: AFrameCb, user: *mut core::ffi::c_void);
    pub fn a_stream(p: *mut AHandle, count: u32, period_ms: u32);
    pub fn a_stream_join(p: *mut AHandle);

    // Storage descriptors (see AFrameDescV1 in a-abi).
    pub fn a_set_storage(p: *mut AHandle, kind: u32) -> i32;
    pub fn a_buf_export(b: *const ABuf) -> AFrameDescV1;
    /// Successor to `a_buf_export`: adds fourcc/modifier/geometry. Both
    /// entry points live forever — a by-value struct cannot grow.
    pub fn a_buf_export2(b: *const ABuf) -> a_abi::AFrameDescV2;

    // Storage kind for EMITTED frames (capture/stream pools) — orthogonal
    // to a_set_storage, which reallocates the producer's own payload.
    pub fn a_set_frame_storage(p: *mut AHandle, kind: u32) -> i32;
    pub fn a_frame_storage(p: *const AHandle) -> u32;

    // Delivery policy (see A_DELIVERY_* in a-abi). a_subscribe keeps its
    // original signature forever; the policy-taking form is a new entry
    // point, because changing an exported signature is not evolution.
    pub fn a_subscribe_with(
        p: *mut AHandle,
        cb: AFrameCb,
        user: *mut core::ffi::c_void,
        policy: u32,
    ) -> u64;
    pub fn a_sub_dropped(p: *const AHandle, id: u64) -> u64;

    // Cancellable capture. a_capture keeps its original void signature;
    // the id-returning form is a new entry point.
    pub fn a_capture2(p: *mut AHandle, cb: AFrameCb, user: *mut core::ffi::c_void) -> u64;
    pub fn a_capture_cancel(p: *mut AHandle, id: u64) -> i32;

    // Advisory detail for the most recent failure ON THE CALLING THREAD;
    // null if it has not failed. Owned by liba, valid until this thread's
    // next failing call. The AStatus code remains the contract — never
    // parse or branch on this string.
    pub fn a_last_error_message() -> *const core::ffi::c_char;
}
