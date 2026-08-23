//! Python extension module `b` — consumes `a.A` objects across the
//! extension-module boundary via module a's `_C_API` `PyCapsule`, then runs
//! crate `b`'s ordinary Rust logic on the unwrapped handle. The payload
//! bytes never enter Python.

use a_abi::{APyCapiV1, AStatus};
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyCapsule;

/// Holds an OWNED reference to module a's capsule: pinning the capsule pins
/// its boxed payload, so the derived `&APyCapiV1` cannot dangle even if
/// module `a` is torn out of sys.modules (numpy's `import_array` does the
/// same by keeping the capsule reachable).
static CAPI: PyOnceLock<Py<PyCapsule>> = PyOnceLock::new();

/// Hard floor: the capsule version that introduced the oldest field this
/// module cannot work without. v2 added `wrap_frame` (used by `grab_frame`)
/// and v3 the vtable's capture entries (used by `capture_checksum` in the
/// vtable backend) — so v3.
const CAPI_REQUIRED: u32 = 3;

/// Soft gate: the version that introduced the scoped-borrow `with_a` entry.
/// Below this, `with_a` falls back to `unwrap_a`.
const CAPI_WITH_A: u32 = 5;

/// Fetch (once) module a's capsule. Importing module `a` here also
/// guarantees the implementation is loaded before any unwrap.
fn capi(py: Python<'_>) -> PyResult<&'static APyCapiV1> {
    let capsule = CAPI.get_or_try_init(py, || {
        let module = py.import("a")?;
        let attr = module.getattr(a_abi::A_PY_CAPI_ATTR)?;
        let capsule = attr
            .downcast_into::<PyCapsule>()
            .map_err(|_| PyRuntimeError::new_err("a._C_API is not a capsule"))?;
        // Capsule-name check: the C convention's type guard against a
        // spoofed or mismatched attribute.
        let name = capsule.name()?;
        if name.map(std::ffi::CStr::to_bytes) != Some(b"a._C_API".as_slice()) {
            return Err(PyRuntimeError::new_err("a._C_API capsule name mismatch"));
        }
        let capi = unsafe { &*(capsule.pointer() as *const APyCapiV1) };
        // Min-version semantics: the floor is the version that introduced
        // the oldest fields this consumer CANNOT do without — never the
        // version it happened to be compiled against. Anything newer is
        // accepted (append-only ⇒ every field we use is still there), and
        // optional newer fields are gated individually at their call sites
        // (see `CAPI_WITH_A` in `with_a`). Compiling against a newer a-abi
        // than the loaded module a is therefore fine.
        if capi.abi_version < CAPI_REQUIRED {
            return Err(PyRuntimeError::new_err(format!(
                "a._C_API version {} < required {}",
                capi.abi_version, CAPI_REQUIRED
            )));
        }
        // vtable backend: install the function table before first use.
        #[cfg(feature = "vtable")]
        unsafe {
            a::init_vtable(capi.vtable);
        };
        Ok::<Py<PyCapsule>, PyErr>(capsule.unbind())
    })?;
    Ok(unsafe { &*(capsule.bind(py).pointer() as *const APyCapiV1) })
}

/// Context handed to the `with_a` trampoline. Carries the closure in, the
/// result out, and any panic payload out — the closure runs inside two
/// `extern "C"` frames, so an unwind must be caught and replayed on this
/// side rather than allowed to cross them.
struct WithCtx<F, R> {
    f: Option<F>,
    out: Option<R>,
    panic: Option<Box<dyn core::any::Any + Send>>,
}

/// `extern "C"` trampoline invoked by module a with the borrow held.
///
/// The shield here is not redundant with module a's: an unwind escaping
/// THIS frame aborts before a's `catch_unwind` could ever see it, because
/// this is the innermost `extern "C"` boundary. Catch, stash, report
/// `Panic`, and let the Rust side resume the unwind once both C frames have
/// been left.
unsafe extern "C" fn with_trampoline<F, R>(
    user: *mut core::ffi::c_void,
    h: *mut a_abi::AHandle,
) -> i32
where
    F: FnOnce(&mut a::A) -> R,
{
    // SAFETY: `user` is the `&raw mut ctx` this module passed to `with_a`
    // one frame up; it is live for the whole call and unaliased.
    let ctx = unsafe { &mut *user.cast::<WithCtx<F, R>>() };
    let Some(f) = ctx.f.take() else {
        // `with_a` invokes the callback at most once; a second entry means
        // the contract was violated on the provider side.
        return AStatus::Panic.code();
    };
    // SAFETY: module a invoked us with the pyclass's exclusive borrow HELD,
    // so `h` is valid and unaliased for exactly this call — the guarantee
    // `unwrap_a` could not make (see the capsule v5 discussion).
    let call = core::panic::AssertUnwindSafe(|| unsafe { a::A::with_raw(h.cast(), f) });
    match std::panic::catch_unwind(call) {
        Ok(v) => {
            ctx.out = Some(v);
            AStatus::Ok.code()
        }
        Err(p) => {
            ctx.panic = Some(p);
            AStatus::Panic.code()
        }
    }
}

/// Run `f` on the `a.A` behind `obj`, with the handle type-checked inside
/// module a.
///
/// Prefers the v5 `with_a` entry, where module a holds its exclusive borrow
/// for the whole call — the handle's provenance is then live for exactly
/// its window of use, which is what makes this sound without relying on the
/// GIL to keep other accesses out (both Miri models reject the alternative;
/// see `crates/app/tests/scenarios.rs`).
///
/// Falls back to `unwrap_a` against a pre-v5 module a. That path is only as
/// sound as the GIL — correct on a stock `CPython`, unsound on a
/// free-threaded one — which is exactly why v5 exists.
fn with_a<F, R>(obj: &Bound<'_, PyAny>, f: F) -> PyResult<R>
where
    F: FnOnce(&mut a::A) -> R,
{
    let capi = capi(obj.py())?;

    if capi.abi_version < CAPI_WITH_A {
        let raw = unsafe { (capi.unwrap_a)(obj.as_ptr().cast()) };
        if raw.is_null() {
            return Err(PyTypeError::new_err("expected an a.A object"));
        }
        return Ok(unsafe { a::A::with_raw(raw.cast(), f) });
    }

    let mut ctx = WithCtx {
        f: Some(f),
        out: None,
        panic: None,
    };
    let status = unsafe {
        (capi.with_a)(
            obj.as_ptr().cast(),
            with_trampoline::<F, R>,
            (&raw mut ctx).cast(),
        )
    };

    // Replay a callback panic now that both extern "C" frames are behind us;
    // PyO3's pymethod wrapper turns it back into a Python exception.
    if let Some(p) = ctx.panic.take() {
        std::panic::resume_unwind(p);
    }

    match AStatus::from_code(status) {
        AStatus::Ok => ctx
            .out
            .ok_or_else(|| PyRuntimeError::new_err("a.with_a reported Ok without invoking the callback")),
        AStatus::InvalidArgument => Err(PyTypeError::new_err("expected an a.A object")),
        // Transient by contract — surface it as such so callers can retry
        // rather than treating it as a type error.
        AStatus::Busy => Err(PyRuntimeError::new_err(
            "the a.A object is already borrowed (re-entrant or concurrent access)",
        )),
        other => Err(PyRuntimeError::new_err(format!("a.with_a failed: {other}"))),
    }
}

/// Opaque-access path through liba (or the vtable): increments twice,
/// returns id + counter.
#[pyfunction]
fn process(obj: &Bound<'_, PyAny>) -> PyResult<u64> {
    with_a(obj, b::process)
}

/// Zero-copy checksum of A's payload — the bytes never enter Python.
#[pyfunction]
fn checksum(obj: &Bound<'_, PyAny>) -> PyResult<u64> {
    with_a(obj, |x| b::checksum(x))
}

/// repr(C) fast-path read via the shared window.
#[pyfunction]
fn process_fast(obj: &Bound<'_, PyAny>) -> PyResult<f64> {
    with_a(obj, |x| b::process_fast(x))
}

/// Grab a zero-copy frame snapshot and hand it back as an `a.Frame` —
/// constructed inside module a via the capsule, so there is exactly one
/// Frame type in the process.
#[pyfunction]
fn grab_frame(obj: &Bound<'_, PyAny>) -> PyResult<Py<PyAny>> {
    let py = obj.py();
    let capi = capi(py)?;
    let raw = with_a(obj, |x| b::grab_frame(x).into_raw())?;
    let wrapped = unsafe { (capi.wrap_frame)(raw.cast::<a_abi::ABuf>()) };
    if wrapped.is_null() {
        // wrap_frame consumes the reference unconditionally (v2+ contract):
        // on failure it has already been released — do NOT release again.
        return Err(PyRuntimeError::new_err("a.wrap_frame failed"));
    }
    Ok(unsafe { Py::from_owned_ptr(py, wrapped.cast()) })
}

/// B's derived async operation as a Python awaitable: capture a frame from
/// the `a.A` object and resolve with its checksum. Composed over crate b's
/// callback API; completion crosses threads via `call_soon_threadsafe`.
#[pyfunction]
fn capture_checksum<'py>(obj: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    let py = obj.py();
    let event_loop = py.import("asyncio")?.call_method0("get_running_loop")?;
    let fut = event_loop.call_method0("create_future")?;
    let fut_ref: Py<PyAny> = fut.clone().unbind();
    let loop_ref: Py<PyAny> = event_loop.unbind();
    with_a(obj, |x| {
        b::capture_checksum_cb(x, move |value| {
            Python::attach(|py| {
                // Cancellation-guarded completion; a closed loop discards
                // the result silently (capture abandoned).
                let result: PyResult<()> = (|| {
                    let resolve = pyo3::wrap_pyfunction!(_resolve, py)?;
                    loop_ref.call_method1(py, "call_soon_threadsafe", (resolve, &fut_ref, value))?;
                    Ok(())
                })();
                if let Err(e) = result {
                    if !e.is_instance_of::<PyRuntimeError>(py) {
                        e.write_unraisable(py, None);
                    }
                }
            });
        });
    })?;
    Ok(fut)
}

/// Test hook: panic inside the `with_a` callback. The unwind starts two
/// `extern "C"` frames deep, so it must be caught by the trampoline, ferried
/// out as a payload, and resumed on this side — where `PyO3` turns it into a
/// Python exception. Without that relay it would abort the interpreter.
#[pyfunction]
fn _test_panic_in_with_a(obj: &Bound<'_, PyAny>) -> PyResult<u64> {
    with_a(obj, |_x| panic!("deliberate panic inside a with_a callback"))
}

/// Loop-thread completion helper (see a-py's `_resolve`).
#[pyfunction]
fn _resolve(fut: &Bound<'_, PyAny>, value: &Bound<'_, PyAny>) -> PyResult<()> {
    if !fut.call_method0("cancelled")?.is_truthy()? {
        fut.call_method1("set_result", (value,))?;
    }
    Ok(())
}

#[pymodule]
#[pyo3(name = "b")]
fn b_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Each Rust cdylib links its own libstd, so panic hooks do NOT carry
    // across extension modules: module a installing one leaves this
    // module's panics printing a backtrace to stderr. Install our own.
    // (Verified — see a-rt's crate docs.)
    a_rt::err::install_hook();
    m.add_function(wrap_pyfunction!(process, m)?)?;
    m.add_function(wrap_pyfunction!(checksum, m)?)?;
    m.add_function(wrap_pyfunction!(process_fast, m)?)?;
    m.add_function(wrap_pyfunction!(grab_frame, m)?)?;
    m.add_function(wrap_pyfunction!(capture_checksum, m)?)?;
    m.add_function(wrap_pyfunction!(_resolve, m)?)?;
    m.add_function(wrap_pyfunction!(_test_panic_in_with_a, m)?)?;
    Ok(())
}
