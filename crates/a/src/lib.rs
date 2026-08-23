//! First-class Rust API for `A`, with a backend switch.
//!
//! The public surface is identical in both modes — consumers like crate `b`
//! are completely backend-agnostic:
//!
//! - `static` (default): the real implementation is compiled in. Pure-Rust
//!   applications get one statically linked copy, full inlining, and no
//!   shared-library dependency.
//! - `dynamic`: `A` is a `#[repr(transparent)]` newtype over the opaque FFI
//!   handle; every method calls into liba.so through the frozen C ABI, and
//!   `Drop` calls `a_destroy`. Used to build libb.so decoupled from liba.so.

#[cfg(any(
    all(feature = "static", feature = "dynamic"),
    all(feature = "static", feature = "vtable"),
    all(feature = "dynamic", feature = "vtable")
))]
compile_error!(
    "features `static`, `dynamic`, and `vtable` are mutually exclusive \
     backends. If this fired from a workspace-wide build, build the \
     capi/py packages in separate cargo invocations (see run.sh)."
);
#[cfg(not(any(feature = "static", feature = "dynamic", feature = "vtable")))]
compile_error!("select a backend: feature `static`, `dynamic`, or `vtable`");

pub use a_abi::{
    ABufView, ABufViewMut, AFrameDescV1, AFrameInfoV1, APyCapiV1, ASharedV1, AStatus, AVtableV1,
    A_ABI_VERSION, A_DELIVERY_BLOCKING, A_DELIVERY_LATEST, A_FOURCC_RGBA8888, A_MODIFIER_LINEAR,
    A_STORAGE_DMABUF, A_STORAGE_HEAP, A_STORAGE_IOSURFACE,
};

mod oneshot;

// Backend-agnostic: built purely on `A::subscribe_with`, whose signature is
// identical in all three backends, so one file serves them all.
mod stream;
pub use stream::FrameStream;

#[cfg(feature = "static")]
mod storage;

#[cfg(feature = "static")]
mod static_impl;
#[cfg(feature = "static")]
pub use static_impl::{Frame, Raw, RawBuf, A};

#[cfg(feature = "dynamic")]
mod dynamic_impl;
#[cfg(feature = "dynamic")]
pub use dynamic_impl::{Frame, Raw, RawBuf, A};

#[cfg(feature = "vtable")]
mod vtable_impl;
#[cfg(feature = "vtable")]
pub use vtable_impl::{init_vtable, Frame, Raw, RawBuf, A};
