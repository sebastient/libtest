//! First-class Rust API of library B. Written once against crate `a`'s
//! public API — completely agnostic to whether `a` is the compiled-in
//! implementation (static) or a wrapper over liba.so (dynamic).

use a::{A, A_ABI_VERSION};

/// Opaque-access path: only `a`'s public methods. In dynamic mode each call
/// crosses into liba.so; in static mode it inlines.
pub fn process(x: &mut A) -> u64 {
    x.increment();
    x.increment();
    x.id() + x.counter()
}

/// Fast path via the frozen repr(C) window: one `shared()` fetch, then
/// direct field reads — no per-field crossing even in dynamic mode.
pub fn process_fast(x: &A) -> f64 {
    let s = x.shared();
    // Min-version semantics (`<`, never `==`): a newer liba bumping the
    // window version is still append-only-compatible with this reader.
    // NaN-as-error is a C-ism kept so this signature matches the C surface;
    // a pure-Rust API would return Option<f64>.
    if s.abi_version < A_ABI_VERSION {
        return f64::NAN;
    }
    // The shared window's arithmetic is defined in f64 by the ABI; this is
    // the contract's own precision, not an accident of this consumer.
    #[expect(clippy::cast_precision_loss, reason = "f64 result is the ABI contract")]
    let sum = (s.id + s.counter) as f64;
    s.scale * sum
}

/// The implementation size B observes through `a` — used by the drift test.
pub fn observed_impl_size() -> usize {
    A::impl_size()
}

/// Zero-copy consumer of A's payload via the borrowed-view model.
pub fn checksum(x: &A) -> u64 {
    x.data().iter().map(|&v| v as u64).sum()
}

/// Where B sees A's payload in memory — used to prove zero-copy by pointer
/// identity from the harness.
pub fn data_ptr(x: &A) -> *const u8 {
    x.data().as_ptr()
}

/// Shared-ownership model: grab a refcounted snapshot that stays valid
/// independently of `x`'s later mutations or destruction.
pub fn grab_frame(x: &A) -> a::Frame {
    x.frame()
}

pub fn frame_checksum(f: &a::Frame) -> u64 {
    f.data().iter().map(|&v| v as u64).sum()
}

/// B's own derived async operation, callback form: capture a frame from A
/// and complete with its checksum. Backend-agnostic — composes over
/// whatever transport `a` was built with.
pub fn capture_checksum_cb(x: &A, f: impl FnOnce(u64) + Send + 'static) {
    x.capture_cb(move |frame| f(frame_checksum(&frame)));
}

/// The same operation as a first-class Rust future.
pub async fn capture_checksum(x: &A) -> u64 {
    let frame = x.capture().await;
    frame_checksum(&frame)
}
