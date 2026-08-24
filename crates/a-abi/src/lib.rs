//! Layout-only ABI crate: the *contract* between liba and libb.
//!
//! This crate contains no code, only a `#[repr(C)]` layout. Both sides may
//! statically link it — duplicating a type definition costs nothing at
//! runtime. The rules that make version drift safe:
//!
//! 1. Fields are append-only: existing offsets never move, never change type.
//! 2. New fields are carved out of `_reserved` (or appended, with
//!    `struct_size` telling readers how much is valid).
//! 3. `abi_version` / `struct_size` header lets a reader detect what it got.
//!
//! This is the same discipline as `struct sockaddr_storage` or Vulkan's
//! `sType`/`pNext` structs.

#![no_std]

pub const A_ABI_VERSION: u32 = 1;

/// Status codes for fallible C API entry points. Convention: `Ok` is zero,
/// errors are negative, and new codes are append-only. `Panic` means an
/// internal invariant failed and was caught at the boundary — the object's
/// state is unspecified and it should only be destroyed.
///
/// ABI rule: status values cross the boundary as plain `i32`, NEVER as this
/// enum — a newer library may return a code this build does not know, and
/// materializing an unknown discriminant in a Rust enum is instant UB.
/// Convert with [`AStatus::from_code`], which maps unrecognized codes to
/// [`AStatus::Unknown`].
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AStatus {
    Ok = 0,
    Panic = -1,
    InvalidArgument = -2,
    /// The target is exclusively borrowed elsewhere and cannot be borrowed
    /// again right now. Unlike the other errors this one is *transient*:
    /// the same call may succeed later. Appended after the initial code set
    /// as the worked example of append-only status evolution: a new code
    /// needs no `A_ABI_VERSION` bump because it changes no layout, and an
    /// older consumer built before it existed maps it to `Unknown` and
    /// treats it as fatal — safe, it merely loses the retry hint.
    Busy = -3,
    /// The requested resource kind is known and valid on this platform, but
    /// the runtime cannot provide one right now: the backing device node is
    /// absent or unreadable (`/dev/dma_heap`, `EACCES`), or the allocator is
    /// exhausted. Distinct from `InvalidArgument`, which means the caller
    /// named a kind this build does not know at all — a caller bug. The
    /// split exists because consumers must be able to tell "skip this on
    /// this box" from "fix your code"; collapsing both into
    /// `InvalidArgument` made every platform-storage consumer treat an
    /// unprivileged CI container as a test failure. Appended under the same
    /// append-only rule as `Busy`: no layout change, so no `A_ABI_VERSION`
    /// bump, and an older consumer maps it to `Unknown` and treats it as
    /// fatal — safe, it merely loses the skip hint.
    Unavailable = -4,
    /// Never sent over the wire: represents a code appended by a newer
    /// library that this build does not recognize. Treat like `Panic`.
    Unknown = i32::MIN,
}

impl AStatus {
    /// Convert a wire status code, tolerating codes appended by newer
    /// libraries (append-only evolution).
    pub const fn from_code(code: i32) -> AStatus {
        match code {
            0 => AStatus::Ok,
            -1 => AStatus::Panic,
            -2 => AStatus::InvalidArgument,
            -3 => AStatus::Busy,
            -4 => AStatus::Unavailable,
            _ => AStatus::Unknown,
        }
    }

    /// The wire representation of this status.
    pub const fn code(self) -> i32 {
        self as i32
    }
}

impl core::fmt::Display for AStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            AStatus::Ok => "ok",
            AStatus::Panic => "internal panic caught at the library boundary",
            AStatus::InvalidArgument => "invalid argument",
            AStatus::Busy => "target is exclusively borrowed elsewhere",
            AStatus::Unavailable => "resource kind unavailable in this environment",
            AStatus::Unknown => "unrecognized status code from a newer library",
        };
        f.write_str(text)
    }
}

impl core::error::Error for AStatus {}

/// Borrowed view into a buffer owned by the other side of the boundary.
/// Purely descriptive — carries no ownership. Validity rules (per the API
/// that returned it): invalidated by any mutation of, or the destruction
/// of, the object it was obtained from.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ABufView {
    pub ptr: *const u8,
    pub len: usize,
}

/// Exclusive writable view. Only obtainable while the buffer reference is
/// unique; `ptr` is null when the buffer is shared.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ABufViewMut {
    pub ptr: *mut u8,
    pub len: usize,
}

/// Opaque handle to struct A. Zero-sized-with-marker pattern: cannot be
/// constructed, dereferenced, or moved by value outside liba — only
/// pointed at.
#[repr(C)]
pub struct AHandle {
    _data: [u8; 0],
    _marker: core::marker::PhantomData<(*mut u8, core::marker::PhantomPinned)>,
}

/// Opaque handle to a refcounted buffer owned by A's implementation.
#[repr(C)]
pub struct ABuf {
    _data: [u8; 0],
    _marker: core::marker::PhantomData<(*mut u8, core::marker::PhantomPinned)>,
}

/// Geometry of a frame buffer: rows × cols × channels of u8, with a row
/// stride in BYTES that may exceed cols × channels (padded/non-contiguous
/// rows). Element stride is `channels`, channel stride is 1.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AFrameInfoV1 {
    pub rows: usize,
    pub cols: usize,
    pub channels: usize,
    pub row_stride: usize,
}

/// Frame storage kinds for `a_set_storage` / `AFrameDescV1.kind`.
pub const A_STORAGE_HEAP: u32 = 0;
/// Linux DMA-BUF (allocated from /`dev/dma_heap`).
pub const A_STORAGE_DMABUF: u32 = 1;
/// macOS `IOSurface` (shareable via its global `IOSurfaceID`).
pub const A_STORAGE_IOSURFACE: u32 = 2;

/// Exported descriptor for a frame's underlying storage — the cross-process
/// zero-copy handle.
///
/// Evolution note: this struct is returned BY VALUE, which bakes its size
/// into every consumer call site — the reserved-tail/`struct_size` growth
/// model cannot apply. By-value ABI types are immutable forever and evolve
/// by suffixed successors (`AFrameDescV2` + `a_buf_export2`). The same rule
/// covers `ABufView`, `ABufViewMut`, and `AFrameInfoV1`. Kind decides which field is meaningful:
/// - `A_STORAGE_HEAP`: neither (`fd` = -1, `id` = 0); process-local only.
/// - `A_STORAGE_DMABUF`: `fd` is a dup'd DMA-BUF fd OWNED by the caller
///   (close it when done; mmap it for CPU access, or import into DRM/V4L2/
///   EGL; use `DMA_BUF_IOCTL_SYNC` around CPU access of device-written data).
/// - `A_STORAGE_IOSURFACE`: `id` is the global `IOSurfaceID`
///   (`IOSurfaceLookup(id)` in any process; lock before CPU access).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AFrameDescV1 {
    pub kind: u32,
    pub _pad: u32,
    pub fd: i32,
    pub id: u32,
    pub offset: usize,
    pub len: usize,
}

/// Build a DRM-style fourcc from four ASCII characters — the format
/// vocabulary shared by DRM/KMS, V4L2, EGL, GBM and Wayland, which is why a
/// descriptor carrying one can be imported by any of them.
pub const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// `DRM_FORMAT_RGBA8888` — what this prototype's 4-channel u8 frames are.
pub const A_FOURCC_RGBA8888: u32 = fourcc(b'R', b'A', b'2', b'4');
/// `DRM_FORMAT_MOD_LINEAR`: no tiling or compression, rows laid out in
/// order at `stride` bytes apart. Anything else (vendor tiling, framebuffer
/// compression) is what makes a buffer unreadable to an importer that does
/// not know the modifier — which is precisely why it must travel WITH the
/// fd rather than be assumed.
pub const A_MODIFIER_LINEAR: u64 = 0;

/// Descriptor v2: everything `AFrameDescV1` carries, plus the pixel-format
/// vocabulary a real device pipeline needs to import the buffer.
///
/// This struct is the worked example of the by-value evolution rule stated
/// on `AFrameDescV1`. `AFrameDescV1` is returned BY VALUE, so its size is
/// baked into every compiled call site and it can never grow — not even
/// into a reserved tail. The successor is therefore a NEW type reached by a
/// NEW entry point (`a_buf_export2`), and both live on forever: an old
/// consumer keeps calling `a_buf_export` against a new library and gets
/// exactly the 32 bytes it was compiled for.
///
/// Keeping V1's fields as an identical prefix is a convention, not a
/// requirement — it costs nothing and makes the successor a superset.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct AFrameDescV2 {
    // --- identical prefix to AFrameDescV1 ---
    pub kind: u32,
    pub _pad: u32,
    pub fd: i32,
    pub id: u32,
    pub offset: usize,
    pub len: usize,
    // --- new in v2 ---
    /// DRM fourcc (`A_FOURCC_*`); 0 if the producer does not describe a
    /// pixel format.
    pub fourcc: u32,
    pub _pad2: u32,
    /// DRM format modifier (`A_MODIFIER_LINEAR` here). Meaningless without
    /// `fourcc`, and an importer that does not recognise it must refuse the
    /// buffer rather than guess.
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    /// Row stride in BYTES; may exceed `width × bytes-per-pixel`.
    pub stride: u32,
    /// Planes in this descriptor. Always 1 here — multi-planar formats
    /// (NV12 and friends) would need per-plane fd/offset/stride arrays,
    /// which is a v3 shape, not a v2 one.
    pub plane_count: u32,
}

/// Delivery policy for a subscription.
///
/// The default is lossless and synchronous, which makes a slow subscriber
/// slow the producer — correct for a recorder, wrong for a live preview.
/// `LATEST` inverts that trade: the producer never blocks, and a subscriber
/// that cannot keep up simply misses frames (the count is retrievable, so
/// the loss is measurable rather than invisible).
///
/// Blocking delivery, the producer thread calls the callback directly, no
/// frames dropped. A slow callback slows the stream for EVERY subscriber.
pub const A_DELIVERY_BLOCKING: u32 = 0;
/// Bounded queue of depth 1 with drop-oldest: the producer hands the frame
/// to a per-subscription pump thread and moves on. A subscriber always sees
/// the newest frame available when it becomes ready, never a stale backlog.
pub const A_DELIVERY_LATEST: u32 = 1;

/// Frame callback. Invoked from an internal producer thread, never the
/// registering thread. For subscriptions the frame is BORROWED for the call
/// (retain to keep it); for captures it is OWNED by the callee (release it).
/// Callbacks must not call `a_unsubscribe`, `a_stream_join` or
/// `a_capture_cancel` (deadlock by contract) and should return promptly.
///
/// For a CANCELLABLE completion callback (`a_capture2`), `frame` may be
/// NULL, meaning the capture was cancelled before it produced anything.
/// The callback is still invoked exactly once — that invariant is what lets
/// every trampoline free its context box unconditionally, so cancellation
/// signals through the argument rather than by not calling.
pub type AFrameCb = unsafe extern "C" fn(user: *mut core::ffi::c_void, frame: *mut ABuf);

/// Callback shape for the capsule's `with_a` entry. Invoked with the
/// caller's context and a handle that is valid, and EXCLUSIVELY borrowed,
/// for exactly the duration of the call. Returns a wire status code, which
/// `with_a` passes back to its caller unchanged.
pub type AWithACb =
    unsafe extern "C" fn(user: *mut core::ffi::c_void, h: *mut AHandle) -> i32;

/// Function table over A's complete C API — the same entry points liba.so
/// exports, as callable pointers. Lets a consumer reach the implementation
/// without OS-level linking (e.g. across Python extension modules, which are
/// dlopen'd `RTLD_LOCAL`). Append-only, like every layout in this crate:
/// fields added at the END, gated by the capsule's `abi_version`.
#[repr(C)]
pub struct AVtableV1 {
    pub a_create: unsafe extern "C" fn(id: u64) -> *mut AHandle,
    pub a_destroy: unsafe extern "C" fn(p: *mut AHandle),
    pub a_id: unsafe extern "C" fn(p: *const AHandle) -> u64,
    pub a_counter: unsafe extern "C" fn(p: *const AHandle) -> u64,
    pub a_increment: unsafe extern "C" fn(p: *mut AHandle),
    pub a_scale: unsafe extern "C" fn(p: *const AHandle) -> f64,
    pub a_shared: unsafe extern "C" fn(p: *const AHandle) -> *const ASharedV1,
    pub a_impl_size: unsafe extern "C" fn() -> usize,
    pub a_data: unsafe extern "C" fn(p: *const AHandle) -> ABufView,
    pub a_fill: unsafe extern "C" fn(p: *mut AHandle, seed: u8),
    /// Returns a wire status code (see `AStatus::from_code`).
    pub a_try_fill: unsafe extern "C" fn(p: *mut AHandle, seed: u8) -> i32,
    pub a_copy_data: unsafe extern "C" fn(p: *const AHandle, out: *mut u8, cap: usize) -> usize,
    pub a_frame: unsafe extern "C" fn(p: *const AHandle) -> *mut ABuf,
    pub a_buf_retain: unsafe extern "C" fn(b: *mut ABuf),
    pub a_buf_release: unsafe extern "C" fn(b: *mut ABuf),
    pub a_buf_map: unsafe extern "C" fn(b: *const ABuf) -> ABufView,
    pub a_buf_map_mut: unsafe extern "C" fn(b: *mut ABuf) -> ABufViewMut,
    // -- appended in capsule abi_version 2 --
    pub a_buf_info: unsafe extern "C" fn(b: *const ABuf) -> AFrameInfoV1,
    // -- appended in capsule abi_version 3 --
    /// Register a streaming callback; returns a subscription id.
    pub a_subscribe:
        unsafe extern "C" fn(p: *mut AHandle, cb: AFrameCb, user: *mut core::ffi::c_void) -> u64,
    /// Remove a subscription. Blocks until any in-flight invocation returns;
    /// guarantees no further invocation after it returns.
    pub a_unsubscribe: unsafe extern "C" fn(p: *mut AHandle, id: u64),
    /// One-shot async capture; `cb` is invoked exactly once with an OWNED
    /// frame reference.
    pub a_capture:
        unsafe extern "C" fn(p: *const AHandle, cb: AFrameCb, user: *mut core::ffi::c_void),
    /// Produce `count` frames ~`period_ms` apart on an internal thread.
    pub a_stream: unsafe extern "C" fn(p: *mut AHandle, count: u32, period_ms: u32),
    /// Block until the active stream (if any) completes. Not callable from
    /// a callback.
    pub a_stream_join: unsafe extern "C" fn(p: *mut AHandle),
    // -- appended in capsule abi_version 4 --
    /// Reallocate A's payload in the given storage kind (contents carried
    /// over). `InvalidArgument` if the kind is unsupported on this platform.
    pub a_set_storage: unsafe extern "C" fn(p: *mut AHandle, kind: u32) -> i32,
    /// Export a buffer's storage descriptor (see `AFrameDescV1`).
    pub a_buf_export: unsafe extern "C" fn(b: *const ABuf) -> AFrameDescV1,
    // -- appended in capsule abi_version 6 --
    /// Successor to `a_buf_export` carrying fourcc/modifier/geometry.
    pub a_buf_export2: unsafe extern "C" fn(b: *const ABuf) -> AFrameDescV2,
    /// Storage kind for frames the producer EMITS (capture/stream pools),
    /// independent of its own payload's kind.
    pub a_set_frame_storage: unsafe extern "C" fn(p: *mut AHandle, kind: u32) -> i32,
    pub a_frame_storage: unsafe extern "C" fn(p: *const AHandle) -> u32,
    /// Subscribe with an explicit delivery policy (`A_DELIVERY_*`).
    pub a_subscribe_with: unsafe extern "C" fn(
        p: *mut AHandle,
        cb: AFrameCb,
        user: *mut core::ffi::c_void,
        policy: u32,
    ) -> u64,
    /// Frames this subscription missed (always 0 under blocking delivery).
    pub a_sub_dropped: unsafe extern "C" fn(p: *const AHandle, id: u64) -> u64,
    /// Cancellable capture; returns an id for `a_capture_cancel`.
    pub a_capture2: unsafe extern "C" fn(
        p: *mut AHandle,
        cb: AFrameCb,
        user: *mut core::ffi::c_void,
    ) -> u64,
    /// Cancel an in-flight capture, blocking until its callback has run.
    pub a_capture_cancel: unsafe extern "C" fn(p: *mut AHandle, id: u64) -> i32,
}

// Function pointers are Send + Sync; the struct is immutable published data.
unsafe impl Sync for AVtableV1 {}

/// Version of the Python capsule contract below. Consumers check
/// `abi_version >= <version that introduced the fields they use>` — never
/// exact equality — so append-only evolution keeps older consumers working.
/// v1: `unwrap_a`, vtable (through `a_buf_map_mut`). v2: + `wrap_frame`,
/// `vtable.a_buf_info`. v3: + vtable streaming/capture entries. v4: + vtable
/// storage entries (`a_set_storage`, `a_buf_export`). v5: + `with_a` (scoped
/// exclusive borrow; supersedes `unwrap_a`, which remains for older
/// consumers).
pub const A_PY_CAPI_VERSION: u32 = 5;

/// Attribute name under which module `a` publishes its capsule.
pub const A_PY_CAPI_ATTR: &str = "_C_API";

/// The `PyCapsule` payload published by Python module `a` (numpy/datetime
/// `_C_API` pattern). `unwrap_a` type-checks a `PyObject*` inside module a —
/// where the one true `A` pyclass lives — and returns its handle (null if
/// the object is not an `a.A`); call only with the GIL held, and treat the
/// handle as borrowed for the duration of the current call. `vtable` gives
/// linker-free access to the full implementation.
#[repr(C)]
pub struct APyCapiV1 {
    pub abi_version: u32,
    pub unwrap_a: unsafe extern "C" fn(obj: *mut core::ffi::c_void) -> *mut AHandle,
    pub vtable: *const AVtableV1,
    // -- appended in abi_version 2 --
    /// Wrap a buffer reference into a new `a.Frame` Python object; returns
    /// an owned `PyObject`*, null on failure. Ownership of the reference
    /// transfers UNCONDITIONALLY — on failure it has already been released
    /// (do not release it again). GIL held.
    pub wrap_frame: unsafe extern "C" fn(buf: *mut ABuf) -> *mut core::ffi::c_void,
    // -- appended in abi_version 5 --
    /// Scoped exclusive borrow — the sound successor to `unwrap_a`.
    ///
    /// Type-checks `obj` as an `a.A` inside module a, then invokes `cb` with
    /// its handle **while module a continues to hold the pyclass's exclusive
    /// borrow**. Because the borrow outlives the handle's window of use, the
    /// handle's provenance stays live for exactly as long as the consumer
    /// uses it, and no access through any other path can interleave.
    ///
    /// This is what `unwrap_a` cannot offer: it drops its borrow guard on
    /// return, so its handle is only exclusive for as long as something else
    /// serializes access — under `CPython` that something is the GIL. Both
    /// Miri aliasing models reject the interleaved shape (see
    /// `crates/app/tests/scenarios.rs`), so free-threaded interpreters must
    /// use `with_a`.
    ///
    /// Returns `cb`'s status, or `InvalidArgument` if `obj` is not an `a.A`,
    /// `Busy` if it is already borrowed, `Panic` if `cb` unwound. GIL held.
    /// `cb` must not re-enter the same object (self-deadlock/`Busy` by
    /// construction) — the same restriction the C API documents for calling
    /// `a_unsubscribe` from inside a callback.
    pub with_a: unsafe extern "C" fn(
        obj: *mut core::ffi::c_void,
        cb: AWithACb,
        user: *mut core::ffi::c_void,
    ) -> i32,
}

// Published as immutable data whose vtable pointer targets a static with
// process lifetime; safe to reference from any thread (calls themselves are
// governed by the GIL rule on unwrap_a).
unsafe impl Sync for APyCapiV1 {}
unsafe impl Send for APyCapiV1 {}

/// Publicly frozen prefix of struct A. liba hands out a pointer to this;
/// consumers read fields directly with zero function-call overhead.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ASharedV1 {
    pub abi_version: u32,
    pub struct_size: u32,
    pub id: u64,
    pub counter: u64,
    pub scale: f64,
    pub fd: i32,
    pub _pad: u32,
    pub _reserved: [u8; 24],
}
