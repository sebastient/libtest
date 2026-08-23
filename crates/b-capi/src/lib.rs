//! libb.so — the ergonomic C API over crate `b`. The same source builds two
//! ways: `dynamic` (default) consumes A through liba.so's C ABI, `static`
//! embeds A's implementation (the naive contrast case).
//!
//! `a::Raw` is the backend's C-facing handle type — an opaque pointer either
//! way, so the exported C signatures are identical in both builds.
//!
//! SAFETY (applies to every `unsafe` block below): each entry point states
//! its precondition in a `# Safety` section, and the single unsafe call in
//! its body — `A::with_raw` or `Frame::from_raw` — is exactly what that
//! precondition licenses.

/// # Safety
/// `p` must be a valid handle from `a_create` (dynamic) with exclusive
/// access for the duration of the call.
#[no_mangle]
pub unsafe extern "C" fn b_process(p: a::Raw) -> u64 {
    unsafe { a::A::with_raw(p, b::process) }
}

/// # Safety
/// `p` must be a valid handle from `a_create`.
#[no_mangle]
pub unsafe extern "C" fn b_process_fast(p: a::Raw) -> f64 {
    unsafe { a::A::with_raw(p, |x| b::process_fast(x)) }
}

/// Reports what this library believes A's private size is — used by the
/// drift test to show decoupling (dynamic) vs. stale divergence (static).
#[no_mangle]
pub extern "C" fn b_observed_impl_size() -> usize {
    b::observed_impl_size()
}

/// Completion callback for B's derived async operations.
pub type BU64Cb = unsafe extern "C" fn(user: *mut core::ffi::c_void, value: u64);

struct SendPtr(*mut core::ffi::c_void);
unsafe impl Send for SendPtr {}

/// B's own async C API composed over A's: capture a frame, complete with
/// its checksum. `cb` is invoked exactly once from an internal thread.
///
/// # Safety
/// `p` must be a valid handle; `cb`/`user` callable from another thread.
#[no_mangle]
pub unsafe extern "C" fn b_capture_checksum(
    p: a::Raw,
    cb: BU64Cb,
    user: *mut core::ffi::c_void,
) {
    let user = SendPtr(user);
    unsafe {
        a::A::with_raw(p, |x| {
            b::capture_checksum_cb(x, move |v| {
                let user = user; // capture the Send wrapper, not its raw field
                cb(user.0, v);
            });
        });
    }
}

/// # Safety
/// `p` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn b_checksum(p: a::Raw) -> u64 {
    unsafe { a::A::with_raw(p, |x| b::checksum(x)) }
}

/// Where B sees A's payload — pointer-identity proof of zero-copy.
///
/// # Safety
/// `p` must be a valid handle; same validity rules as `a_data`.
#[no_mangle]
pub unsafe extern "C" fn b_data_ptr(p: a::Raw) -> *const u8 {
    unsafe { a::A::with_raw(p, |x| b::data_ptr(x)) }
}

/// Returns a retained buffer handle; release with liba's `a_buf_release`.
///
/// # Safety
/// `p` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn b_grab_frame(p: a::Raw) -> *mut a_abi::ABuf {
    unsafe { a::A::with_raw(p, |x| b::grab_frame(x)) }.into_raw()
}

/// # Safety
/// `f` must be a live buffer handle; the reference is borrowed, not consumed.
#[no_mangle]
pub unsafe extern "C" fn b_frame_checksum(f: *mut a_abi::ABuf) -> u64 {
    let frame = core::mem::ManuallyDrop::new(unsafe { a::Frame::from_raw(f as a::RawBuf) });
    b::frame_checksum(&frame)
}
