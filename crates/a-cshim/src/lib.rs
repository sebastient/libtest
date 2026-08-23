//! C-ABI logic over crate `a`'s static backend — the single home for the
//! bodies behind liba.so's exports and the self-contained Python module's
//! capsule vtable.
//!
//! Everything here takes the *opaque* `a-abi` handle types (`AHandle`,
//! `ABuf`) rather than crate `a`'s concrete types, so the signatures match
//! `AVtableV1` exactly and a vtable can be built from them for free.
//!
//! What deliberately does NOT live here: the `#[no_mangle]` attributes and
//! the doc comments carrying the ownership/thread contracts. Those stay in
//! `a-capi`, because cbindgen reads that crate's *source* to generate
//! `include/liba.h` — it does not expand macros or follow calls, so the
//! exported signatures and their documentation must be written out there
//! literally. The split is therefore: contract and symbol surface in
//! `a-capi`, logic here, each written once.
//!
//! SAFETY (applies to every `unsafe` block below): each function's
//! precondition is the one `a-capi` documents for the corresponding export
//! — a valid handle from `a_create`, or a live buffer handle — and every
//! unsafe operation is exactly what that precondition licenses.

// Each function's precondition is documented once, on the corresponding
// `a-capi` export (which is also what cbindgen turns into the header's doc
// comments). Repeating all 26 here would create a second copy to drift.
#![expect(
    clippy::missing_safety_doc,
    reason = "preconditions are documented on the a-capi exports these back"
)]

use a::{Frame, A};
use a_abi::{
    ABuf, ABufView, ABufViewMut, AFrameCb, AFrameDescV1, AFrameInfoV1, AHandle, AStatus, ASharedV1,
    AVtableV1,
};
use core::ffi::c_void;
use core::mem::ManuallyDrop;

/// Wrapper making a raw context pointer movable into a callback closure.
struct SendPtr(*mut c_void);
// SAFETY: the C contract requires `user` to be usable from the producer
// thread; this wrapper only carries it there.
unsafe impl Send for SendPtr {}

/// Error detail retrieval and the quiet panic hook — see `a-rt`, which owns
/// them so that cdylibs which cannot depend on this crate (b-py's vtable
/// build embeds no implementation) still get the same hook.
pub use a_rt::err;

/// Panic shield: no unwind may cross `extern "C"`. Fallible entry points
/// wrap their bodies so an internal invariant failure surfaces as
/// `AStatus::Panic` instead of aborting the host process. (Only meaningful
/// under `panic = "unwind"`; a `panic = "abort"` build trades the shield for
/// smaller binaries — see ARCHITECTURE.md.)
///
/// Also records the detail retrievable via `a_last_error_message`. The panic
/// path deliberately does NOT overwrite it: the hook already stored a
/// message naming the panic site, which beats restating "internal panic".
fn shield(f: impl FnOnce() -> AStatus) -> AStatus {
    err::install_hook();
    match std::panic::catch_unwind(core::panic::AssertUnwindSafe(f)) {
        Ok(status) => {
            if status != AStatus::Ok {
                err::set(&status.to_string());
            }
            status
        }
        Err(_) => AStatus::Panic,
    }
}

// Handle casts: the opaque C type in, the real type out.
unsafe fn r(p: *const AHandle) -> *const A {
    p.cast::<A>()
}
unsafe fn rm(p: *mut AHandle) -> *mut A {
    p.cast::<A>()
}
/// Buffer-handle cast. This crate pins the static backend, where
/// `a::RawBuf` is `*const Vec<u8>` — hence the plain cast. (In the dynamic
/// backend the alias is `*mut ABuf`; code that must compile against both
/// spells it `b as a::RawBuf`.)
unsafe fn rb(b: *const ABuf) -> a::RawBuf {
    b.cast()
}

pub unsafe extern "C" fn a_create(id: u64) -> *mut AHandle {
    // Earliest reliable hook point: a consumer must create an A before it
    // can do anything else, and installing at load time would impose on
    // hosts that never call in.
    err::install_hook();
    Box::into_raw(Box::new(A::new(id))).cast::<AHandle>()
}

pub unsafe extern "C" fn a_destroy(p: *mut AHandle) {
    if !p.is_null() {
        drop(unsafe { Box::from_raw(rm(p)) });
    }
}

pub unsafe extern "C" fn a_id(p: *const AHandle) -> u64 {
    unsafe { (*r(p)).id() }
}

pub unsafe extern "C" fn a_counter(p: *const AHandle) -> u64 {
    unsafe { (*r(p)).counter() }
}

pub unsafe extern "C" fn a_increment(p: *mut AHandle) {
    unsafe { (*rm(p)).increment() }
}

pub unsafe extern "C" fn a_scale(p: *const AHandle) -> f64 {
    unsafe { (*r(p)).scale() }
}

pub unsafe extern "C" fn a_shared(p: *const AHandle) -> *const ASharedV1 {
    unsafe { (*r(p)).shared() }
}

pub unsafe extern "C" fn a_impl_size() -> usize {
    A::impl_size()
}

pub unsafe extern "C" fn a_data(p: *const AHandle) -> ABufView {
    let d = unsafe { (*r(p)).data() };
    ABufView {
        ptr: d.as_ptr(),
        len: d.len(),
    }
}

pub unsafe extern "C" fn a_fill(p: *mut AHandle, seed: u8) {
    // Infallible by contract, but `fill` can still panic internally; a
    // panic here is swallowed (state: at worst the old bytes remain).
    err::install_hook();
    let _ = std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| unsafe {
        (*rm(p)).fill(seed);
    }));
}

pub unsafe extern "C" fn a_try_fill(p: *mut AHandle, seed: u8) -> i32 {
    shield(|| match unsafe { (*rm(p)).try_fill(seed) } {
        Ok(()) => AStatus::Ok,
        Err(e) => e,
    })
    .code()
}

pub unsafe extern "C" fn a_copy_data(p: *const AHandle, out: *mut u8, cap: usize) -> usize {
    if out.is_null() || cap == 0 {
        return 0;
    }
    unsafe { (*r(p)).copy_data(core::slice::from_raw_parts_mut(out, cap)) }
}

pub unsafe extern "C" fn a_frame(p: *const AHandle) -> *mut ABuf {
    unsafe { (*r(p)).frame().into_raw().cast::<ABuf>().cast_mut() }
}

pub unsafe extern "C" fn a_buf_retain(b: *mut ABuf) {
    unsafe { Frame::retain_raw(rb(b)) }
}

pub unsafe extern "C" fn a_buf_release(b: *mut ABuf) {
    unsafe { Frame::release_raw(rb(b)) }
}

pub unsafe extern "C" fn a_buf_map(b: *const ABuf) -> ABufView {
    let f = ManuallyDrop::new(unsafe { Frame::from_raw(rb(b)) });
    let d = f.data();
    ABufView {
        ptr: d.as_ptr(),
        len: d.len(),
    }
}

pub unsafe extern "C" fn a_buf_map_mut(b: *mut ABuf) -> ABufViewMut {
    match unsafe { Frame::map_mut_raw(rb(b)) } {
        Some((ptr, len)) => ABufViewMut { ptr, len },
        None => ABufViewMut {
            ptr: core::ptr::null_mut(),
            len: 0,
        },
    }
}

pub unsafe extern "C" fn a_buf_info(b: *const ABuf) -> AFrameInfoV1 {
    ManuallyDrop::new(unsafe { Frame::from_raw(rb(b)) }).info()
}

pub unsafe extern "C" fn a_subscribe(p: *mut AHandle, cb: AFrameCb, user: *mut c_void) -> u64 {
    unsafe { (*rm(p)).subscribe_c(cb, user, a_abi::A_DELIVERY_BLOCKING) }
}

pub unsafe extern "C" fn a_subscribe_with(
    p: *mut AHandle,
    cb: AFrameCb,
    user: *mut c_void,
    policy: u32,
) -> u64 {
    unsafe { (*rm(p)).subscribe_c(cb, user, policy) }
}

pub unsafe extern "C" fn a_sub_dropped(p: *const AHandle, id: u64) -> u64 {
    unsafe { (*r(p)).dropped(id) }
}

pub unsafe extern "C" fn a_unsubscribe(p: *mut AHandle, id: u64) {
    unsafe { (*rm(p)).unsubscribe(id) }
}

pub unsafe extern "C" fn a_capture(p: *const AHandle, cb: AFrameCb, user: *mut c_void) {
    let user = SendPtr(user);
    unsafe {
        (*r(p)).capture_cb(move |frame| {
            // Rust 2021 disjoint capture would move the raw pointer FIELD
            // out of the Send wrapper and silently drop the justification;
            // force whole-struct capture.
            let user = user;
            cb(user.0, frame.into_raw().cast::<ABuf>().cast_mut());
        });
    }
}

pub unsafe extern "C" fn a_capture2(p: *mut AHandle, cb: AFrameCb, user: *mut c_void) -> u64 {
    let user = SendPtr(user);
    unsafe {
        (*rm(p)).capture_cb_cancellable(move |frame| {
            let user = user; // whole-struct capture; see a_capture
            // NULL frame == cancelled. Exactly-once still holds, which is
            // what lets a consumer free `user` after this returns.
            let raw = frame.map_or(core::ptr::null_mut(), |f| {
                f.into_raw().cast::<ABuf>().cast_mut()
            });
            cb(user.0, raw);
        })
    }
}

pub unsafe extern "C" fn a_capture_cancel(p: *mut AHandle, id: u64) -> i32 {
    shield(|| match unsafe { (*rm(p)).cancel_capture(id) } {
        Ok(()) => AStatus::Ok,
        Err(e) => e,
    })
    .code()
}

pub unsafe extern "C" fn a_stream(p: *mut AHandle, count: u32, period_ms: u32) {
    unsafe { (*rm(p)).stream(count, period_ms) }
}

pub unsafe extern "C" fn a_stream_join(p: *mut AHandle) {
    unsafe { (*rm(p)).stream_join() }
}

pub unsafe extern "C" fn a_set_storage(p: *mut AHandle, kind: u32) -> i32 {
    shield(|| match unsafe { (*rm(p)).set_storage(kind) } {
        Ok(()) => AStatus::Ok,
        Err(e) => e,
    })
    .code()
}

pub unsafe extern "C" fn a_buf_export(b: *const ABuf) -> AFrameDescV1 {
    ManuallyDrop::new(unsafe { Frame::from_raw(rb(b)) }).export()
}

pub unsafe extern "C" fn a_buf_export2(b: *const ABuf) -> a_abi::AFrameDescV2 {
    ManuallyDrop::new(unsafe { Frame::from_raw(rb(b)) }).export2()
}

pub unsafe extern "C" fn a_set_frame_storage(p: *mut AHandle, kind: u32) -> i32 {
    shield(|| match unsafe { (*rm(p)).set_frame_storage(kind) } {
        Ok(()) => AStatus::Ok,
        Err(e) => e,
    })
    .code()
}

pub unsafe extern "C" fn a_frame_storage(p: *const AHandle) -> u32 {
    unsafe { (*r(p)).frame_storage() }
}

/// Deliberately panicking entry point, proving the shield returns `Panic`
/// to the caller instead of unwinding across the boundary.
pub unsafe extern "C" fn a_test_panic() -> i32 {
    shield(|| panic!("deliberate internal panic for shield validation")).code()
}

/// The complete C API as a function table — the capsule payload for
/// consumers that cannot rely on the OS linker (Python extension modules
/// are dlopen'd `RTLD_LOCAL`). Because every function above already takes
/// the opaque handle types, this is a plain struct literal.
pub static VTABLE: AVtableV1 = AVtableV1 {
    a_create,
    a_destroy,
    a_id,
    a_counter,
    a_increment,
    a_scale,
    a_shared,
    a_impl_size,
    a_data,
    a_fill,
    a_try_fill,
    a_copy_data,
    a_frame,
    a_buf_retain,
    a_buf_release,
    a_buf_map,
    a_buf_map_mut,
    a_buf_info,
    a_subscribe,
    a_unsubscribe,
    a_capture,
    a_stream,
    a_stream_join,
    a_set_storage,
    a_buf_export,
    a_buf_export2,
    a_set_frame_storage,
    a_frame_storage,
    a_subscribe_with,
    a_sub_dropped,
    a_capture2,
    a_capture_cancel,
};
