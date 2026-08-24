//! Python extension module `a` — the one home of the `a.A` Python type.
//!
//! Publishes `a._C_API`, a `PyCapsule` in the numpy/datetime style, so other
//! extension modules can (1) unwrap an `a.A` object to its C handle with the
//! type check performed here, and (2) optionally reach the whole
//! implementation through a function vtable without OS-level linking.
//!
//! Lifecycle rules the application must follow:
//! - Join streams (`stream_join`) and let pending captures resolve BEFORE
//!   interpreter exit: a producer thread attaching to a finalizing
//!   interpreter is terminated mid-Rust-frame by `CPython` (dropping an `A`
//!   tears its subscriptions down, so scoped usage is naturally safe).
//! - Module `a` must not be reloaded while consumers hold its capsule;
//!   single interpreter only (no subinterpreters).

use a_abi::{APyCapiV1, AStatus, A_PY_CAPI_VERSION};
#[cfg(feature = "dynamic")]
use a_abi::AVtableV1;
use pyo3::exceptions::{PyBufferError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::PyCapsule;
use std::ffi::CString;
use std::os::raw::{c_int, c_void};

/// Loop-thread completion helper: sets the result only if the future was
/// not cancelled — the canonical asyncio cross-thread pattern (mirrors
/// `futures._set_result_unless_cancelled`). Scheduled via
/// `call_soon_threadsafe`, so it always runs on the loop's own thread.
#[pyfunction]
fn _resolve(fut: &Bound<'_, PyAny>, value: &Bound<'_, PyAny>) -> PyResult<()> {
    if !fut.call_method0("cancelled")?.is_truthy()? {
        fut.call_method1("set_result", (value,))?;
    }
    Ok(())
}

/// Complete an asyncio future from a foreign thread: cancellation-guarded,
/// and a closed event loop silently discards the completion (the capture
/// was abandoned); other errors go to `sys.unraisablehook`.
fn complete_future(py: Python<'_>, loop_ref: &Py<PyAny>, fut_ref: &Py<PyAny>, value: Py<PyAny>) {
    let result: PyResult<()> = (|| {
        let resolve = pyo3::wrap_pyfunction!(_resolve, py)?;
        loop_ref.call_method1(py, "call_soon_threadsafe", (resolve, fut_ref, value))?;
        Ok(())
    })();
    if let Err(e) = result {
        if !e.is_instance_of::<PyRuntimeError>(py) {
            e.write_unraisable(py, None);
        }
    }
}

/// One subscription's Python callable, shared between the pyclass (which
/// must be able to traverse it for the GC) and the Rust closure that
/// delivers to it.
///
/// The `Mutex` is only ever taken by a thread that ALREADY holds the GIL —
/// deliveries attach to the interpreter before locking. That invariant is
/// what makes `__traverse__` safe to lock blockingly: a GC pass holds the
/// GIL, so no other thread can be inside the lock at that moment. (The
/// invariant is a GIL argument, so it is one more thing to re-examine for
/// free-threaded builds — see the open items.)
type CallbackSlot = std::sync::Arc<std::sync::Mutex<Option<Py<PyAny>>>>;

// `weakref`: lets callers hold a non-owning reference, and is what makes
// the collectability of a callback cycle observable from Python at all.
#[pyclass(name = "A", weakref)]
struct PyA {
    inner: a::A,
    /// Subscription id -> its callable. Exists so the GC can see the
    /// Python references this object keeps alive through Rust closures;
    /// without it a callback that captures its own `A` is a cycle no
    /// collector can break, and the pair leaks until `unsubscribe`.
    subs: Vec<(u64, CallbackSlot)>,
}

// Read-only state is exposed as properties, not accessor methods: `x.id`
// reads as an attribute in Python where `x.id()` reads as a C API in
// disguise. Behaviour-carrying entry points (`fill`, `frame`, `stream`)
// stay methods, because they do something.
#[pymethods]
impl PyA {
    #[new]
    fn new(id: u64) -> Self {
        Self {
            inner: a::A::new(id),
            subs: Vec::new(),
        }
    }

    #[getter]
    fn id(&self) -> u64 {
        self.inner.id()
    }

    #[getter]
    fn counter(&self) -> u64 {
        self.inner.counter()
    }

    fn increment(&mut self) {
        self.inner.increment();
    }

    #[getter]
    fn scale(&self) -> f64 {
        self.inner.scale()
    }

    fn fill(&mut self, seed: u8) {
        self.inner.fill(seed);
    }

    /// Zero-copy refcounted snapshot as an `a.Frame`.
    fn frame(&self) -> PyFrame {
        PyFrame {
            inner: self.inner.frame(),
        }
    }

    /// Test hook: where this object's payload lives (pointer identity).
    fn _data_ptr(&self) -> usize {
        self.inner.data().as_ptr() as usize
    }

    /// Subscribe a Python callable, invoked as `cb(frame)` from the
    /// producer thread (which attaches to the interpreter per delivery).
    /// Exceptions are reported via `sys.unraisablehook` and the stream
    /// continues.
    ///
    /// The callable is reachable from `__traverse__`, so a callback that
    /// captures this very `A` is an ordinary collectable cycle rather than
    /// a permanent leak. Streams must still be joined before interpreter
    /// exit (see the module docs).
    fn subscribe(&mut self, cb: Py<PyAny>) -> u64 {
        let slot: CallbackSlot = std::sync::Arc::new(std::sync::Mutex::new(Some(cb)));
        let held = slot.clone();
        let id = self.inner.subscribe(move |frame| {
            // Attach FIRST, then lock — that ordering is the invariant that
            // keeps `__traverse__`'s lock contention-free (see CallbackSlot).
            Python::attach(|py| {
                let cb = {
                    let guard = held.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    // Cleared by the GC: the subscription outlives the
                    // callable, and has nothing left to call.
                    let Some(cb) = guard.as_ref() else { return };
                    cb.clone_ref(py)
                };
                let ctx = cb.bind(py).clone().into_any();
                match Py::new(py, PyFrame { inner: frame }) {
                    Ok(f) => {
                        if let Err(e) = cb.call1(py, (f,)) {
                            e.write_unraisable(py, Some(&ctx));
                        }
                    }
                    Err(e) => e.write_unraisable(py, Some(&ctx)),
                }
            });
        });
        self.subs.push((id, slot));
        id
    }

    /// Report every Python reference this object keeps alive — including
    /// the ones held indirectly through Rust closures, which the collector
    /// cannot discover on its own. Each callable is stored ONCE (a shared
    /// slot, not a copy per holder) and so is visited once, which is what
    /// the GC's reference accounting requires: visiting fewer times than
    /// you hold makes a cycle uncollectable, visiting more is a crash.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "PyVisit by value is the signature PyO3's tp_traverse slot requires"
    )]
    fn __traverse__(&self, visit: pyo3::PyVisit<'_>) -> Result<(), pyo3::PyTraverseError> {
        for (_, slot) in &self.subs {
            let guard = slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(cb) = guard.as_ref() {
                visit.call(cb)?;
            }
        }
        Ok(())
    }

    /// Break the cycle by dropping the callables. Deliberately does NOT
    /// unsubscribe: `unsubscribe` blocks until any in-flight callback
    /// returns, that callback needs the GIL, and `__clear__` runs with the
    /// GIL held — the exact deadlock this module documents elsewhere.
    /// Dropping the reference is enough to make the cycle collectable; the
    /// subscription itself is torn down when the object is deallocated.
    fn __clear__(&mut self) {
        for (_, slot) in &self.subs {
            *slot.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        }
    }

    /// Remove a subscription. Releases the GIL while it waits for any
    /// in-flight callback — that callback needs the GIL to finish, so
    /// holding it here would deadlock.
    fn unsubscribe(&mut self, py: Python<'_>, id: u64) {
        let inner = &mut self.inner;
        py.detach(|| inner.unsubscribe(id));
        // Drop our reference only AFTER the teardown guarantee has been
        // met, so nothing can still be mid-delivery through this slot.
        self.subs.retain(|(sid, _)| *sid != id);
    }

    /// Produce `count` frames ~`period_ms` apart on an internal thread.
    /// Releases the GIL: starting a stream implicitly JOINS any previous
    /// one, and that join must not hold the GIL the producer's callbacks
    /// need (the same rule as `stream_join`).
    fn stream(&mut self, py: Python<'_>, count: u32, period_ms: u32) {
        let inner = &mut self.inner;
        py.detach(|| inner.stream(count, period_ms));
    }

    /// Wait for the stream to finish. Releases the GIL (deliveries into
    /// Python callbacks need it).
    fn stream_join(&mut self, py: Python<'_>) {
        let inner = &mut self.inner;
        py.detach(|| inner.stream_join());
    }

    /// Subscribe and expose the deliveries as an async iterator:
    /// `async for frame in src.frames()`.
    ///
    /// `capacity` bounds the queue; at capacity the OLDEST frame is
    /// dropped, so a consumer that falls behind sees the newest frames
    /// rather than a stale backlog (count via `.dropped`).
    ///
    /// Teardown is the caller's, as with any subscription:
    /// `src.unsubscribe(stream.id)`.
    #[pyo3(signature = (capacity = 4))]
    fn frames(&mut self, py: Python<'_>, capacity: usize) -> PyResult<Py<PyFrameStream>> {
        let state = std::sync::Arc::new(std::sync::Mutex::new(StreamState {
            queue: std::collections::VecDeque::new(),
            pending: None,
            dropped: 0,
            capacity: capacity.max(1),
        }));
        let sink = state.clone();
        let id = self.inner.subscribe(move |frame| {
            // Attach first, then lock — the same ordering rule as
            // CallbackSlot, for the same reason.
            Python::attach(|py| {
                let mut st = sink
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some((event_loop, fut)) = st.pending.take() {
                    // A consumer is waiting: hand the frame straight over.
                    match Py::new(py, PyFrame { inner: frame }) {
                        Ok(obj) => complete_future(py, &event_loop, &fut, obj.into_any()),
                        Err(e) => e.write_unraisable(py, None),
                    }
                    return;
                }
                while st.queue.len() >= st.capacity {
                    st.queue.pop_front();
                    st.dropped += 1;
                }
                st.queue.push_back(frame);
            });
        });
        Py::new(py, PyFrameStream { state, id })
    }

    /// Test hook for the v5 `with_a` borrow contract (sibling of liba's
    /// `a_test_panic`): `&mut self` makes `PyO3` hold an exclusive `PyRefMut`
    /// for this whole method, so anything `cb` does that tries to borrow
    /// this object again must observe the conflict. Proves that a
    /// re-entrant unwrap yields a clean `Busy` rather than a second
    /// aliasing handle.
    #[expect(
        clippy::unused_self,
        clippy::needless_pass_by_value,
        reason = "&mut self IS the point (it holds the exclusive borrow); PyO3 passes                   Python and Py<PyAny> by value"
    )]
    fn _test_call_while_borrowed(&mut self, py: Python<'_>, cb: Py<PyAny>) -> PyResult<()> {
        cb.call0(py)?;
        Ok(())
    }

    /// Async capture: returns an asyncio future resolved with an `a.Frame`
    /// from the producer thread via `loop.call_soon_threadsafe`.
    #[expect(
        clippy::needless_pass_by_value,
        reason = "PyRef by value is PyO3's receiver form for a borrowed pyclass"
    )]
    fn capture(slf: PyRef<'_, Self>) -> PyResult<Bound<'_, PyAny>> {
        let py = slf.py();
        let event_loop = py.import("asyncio")?.call_method0("get_running_loop")?;
        let fut = event_loop.call_method0("create_future")?;
        let fut_ref: Py<PyAny> = fut.clone().unbind();
        let loop_ref: Py<PyAny> = event_loop.unbind();
        slf.inner.capture_cb(move |frame| {
            Python::attach(|py| match Py::new(py, PyFrame { inner: frame }) {
                Ok(f) => complete_future(py, &loop_ref, &fut_ref, f.into_any()),
                Err(e) => e.write_unraisable(py, None),
            });
        });
        Ok(fut)
    }
}

/// Shared state of a Python async iterator over a subscription.
///
/// Same locking invariant as `CallbackSlot`: only touched by a thread that
/// already holds the GIL, so no deadlock with the interpreter is possible.
struct StreamState {
    queue: std::collections::VecDeque<a::Frame>,
    /// `(event_loop, future)` for an `__anext__` awaiting a frame. At most
    /// one — an async iterator is polled by a single consumer by contract.
    pending: Option<(Py<PyAny>, Py<PyAny>)>,
    dropped: u64,
    capacity: usize,
}

/// `async for frame in src.frames()` — the pull-shaped view of the
/// push-shaped subscription, resolved from the producer thread.
#[pyclass(name = "FrameStream")]
struct PyFrameStream {
    state: std::sync::Arc<std::sync::Mutex<StreamState>>,
    #[pyo3(get)]
    id: u64,
}

#[pymethods]
impl PyFrameStream {
    /// Frames discarded because the queue was full when a new one arrived.
    #[getter]
    fn dropped(&self) -> u64 {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .dropped
    }

    /// Frames waiting to be consumed right now.
    #[getter]
    fn pending(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .queue
            .len()
    }

    fn __aiter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Return an awaitable for the next frame. A queued frame resolves the
    /// future immediately; otherwise the future is parked and the producer
    /// thread resolves it on the next delivery.
    fn __anext__<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let event_loop = py.import("asyncio")?.call_method0("get_running_loop")?;
        let fut = event_loop.call_method0("create_future")?;
        let mut st = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(frame) = st.queue.pop_front() {
            let obj = Py::new(py, PyFrame { inner: frame })?;
            fut.call_method1("set_result", (obj,))?;
        } else {
            // Replacing an existing pending future would abandon a waiting
            // consumer silently; an async iterator polled concurrently is a
            // usage error, so say so.
            if st.pending.is_some() {
                return Err(PyRuntimeError::new_err(
                    "FrameStream is already being awaited; async iterators are single-consumer",
                ));
            }
            st.pending = Some((event_loop.unbind(), fut.clone().unbind()));
        }
        Ok(fut)
    }
}

/// Immutable frame snapshot exposing the `CPython` buffer protocol: zero-copy,
/// 3-D (rows × cols × channels of u8), with a padded row stride, so it is
/// NOT C-contiguous — consumers must request strided buffers (memoryview
/// and numpy do). Every exported buffer holds a reference to this object
/// (`Py_buffer.obj`), which holds the implementation's refcount, so views
/// outlive both the producing `A` and any other Python references.
#[pyclass(name = "Frame")]
struct PyFrame {
    inner: a::Frame,
}

/// Per-view shape/stride storage; must live as long as the `Py_buffer`.
struct ViewMeta {
    shape: [isize; 3],
    strides: [isize; 3],
}

#[pymethods]
impl PyFrame {
    #[getter]
    fn shape(&self) -> (usize, usize, usize) {
        let i = self.inner.info();
        (i.rows, i.cols, i.channels)
    }

    /// Strides in bytes per dimension (row, pixel, channel).
    #[getter]
    fn strides(&self) -> (usize, usize, usize) {
        let i = self.inner.info();
        (i.row_stride, i.channels, 1)
    }

    /// Test hook: where the frame's bytes live (pointer identity).
    fn _data_ptr(&self) -> usize {
        self.inner.data().as_ptr() as usize
    }

    /// # Safety
    /// Called by `CPython` with a valid `view`; the filled buffer follows the
    /// buffer-protocol contract (kept alive via `view.obj`).
    unsafe fn __getbuffer__(
        slf: Bound<'_, Self>,
        view: *mut pyo3::ffi::Py_buffer,
        flags: c_int,
    ) -> PyResult<()> {
        if view.is_null() {
            return Err(PyBufferError::new_err("Py_buffer is null"));
        }
        // SAFETY: CPython guarantees `view` points at a valid Py_buffer.
        // Null `obj` first so an early error leaves a well-formed view.
        unsafe { (*view).obj = std::ptr::null_mut() };
        if flags & pyo3::ffi::PyBUF_WRITABLE == pyo3::ffi::PyBUF_WRITABLE {
            return Err(PyBufferError::new_err("frame buffers are read-only"));
        }
        if flags & pyo3::ffi::PyBUF_STRIDES != pyo3::ffi::PyBUF_STRIDES {
            return Err(PyBufferError::new_err(
                "frame is not contiguous: a strided buffer request is required",
            ));
        }
        // PEP 3118: a contiguity REQUEST (its bits include STRIDES, so it
        // passes the check above) must be refused — rows are padded, and a
        // consumer entitled to contiguity would misread the padding.
        for contig in [
            pyo3::ffi::PyBUF_C_CONTIGUOUS,
            pyo3::ffi::PyBUF_F_CONTIGUOUS,
            pyo3::ffi::PyBUF_ANY_CONTIGUOUS,
        ] {
            if flags & contig == contig {
                return Err(PyBufferError::new_err(
                    "frame has padded rows and is not contiguous",
                ));
            }
        }
        let (info, buf_ptr) = {
            let frame = &slf.borrow().inner;
            (frame.info(), frame.data().as_ptr())
        };
        // CPython types Py_buffer's shape/strides/len as Py_ssize_t
        // (isize); our dimensions are frame-scale and cannot approach it.
        #[expect(clippy::cast_possible_wrap, reason = "Py_ssize_t is isize by CPython's ABI")]
        let meta = Box::into_raw(Box::new(ViewMeta {
            shape: [info.rows as isize, info.cols as isize, info.channels as isize],
            strides: [info.row_stride as isize, info.channels as isize, 1],
        }));
        // SAFETY: `view` is CPython's valid Py_buffer, and `meta` is the
        // box just leaked above — it stays alive until __releasebuffer__
        // reclaims it via `view.internal`, which is exactly as long as the
        // shape/stride pointers below are read.
        unsafe {
            (*view).buf = buf_ptr as *mut c_void;
            // len is the logical byte count (product of shape × itemsize),
            // which excludes the row padding.
            #[expect(clippy::cast_possible_wrap, reason = "Py_ssize_t is isize by CPython's ABI")]
            let logical_len = (info.rows * info.cols * info.channels) as isize;
            (*view).len = logical_len;
            (*view).readonly = 1;
            (*view).itemsize = 1;
            (*view).format = if flags & pyo3::ffi::PyBUF_FORMAT == pyo3::ffi::PyBUF_FORMAT {
                c"B".as_ptr().cast_mut()
            } else {
                std::ptr::null_mut()
            };
            (*view).ndim = 3;
            (*view).shape = (*meta).shape.as_mut_ptr();
            (*view).strides = (*meta).strides.as_mut_ptr();
            (*view).suboffsets = std::ptr::null_mut();
            (*view).internal = meta.cast::<c_void>();
            // The exported buffer owns a reference to this PyFrame: the
            // chain memoryview -> PyFrame -> Frame refcount keeps the bytes
            // alive after every other reference is dropped.
            (*view).obj = slf.into_any().into_ptr();
        }
        Ok(())
    }

    /// # Safety
    /// Called by `CPython` exactly once per successful `__getbuffer__`.
    #[expect(
        clippy::unused_self,
        reason = "signature is fixed by PyO3's buffer-protocol slot"
    )]
    unsafe fn __releasebuffer__(&self, view: *mut pyo3::ffi::Py_buffer) {
        // SAFETY: paired one-to-one with a successful __getbuffer__, so
        // `internal` is the ViewMeta box that call leaked, still unfreed.
        unsafe {
            if !view.is_null() && !(*view).internal.is_null() {
                drop(Box::from_raw((*view).internal.cast::<ViewMeta>()));
            }
        }
    }
}

/// Capsule entry: wrap an owned buffer reference into a new `a.Frame`.
/// Consumes the reference UNCONDITIONALLY: on failure, `PyFrame`'s drop has
/// already released it (per the v2+ capsule contract) — the caller must not
/// release it again.
#[expect(
    clippy::ptr_as_ptr,
    reason = "a::RawBuf differs in constness per backend; the alias cast is the portable form"
)]
unsafe extern "C" fn wrap_frame(buf: *mut a_abi::ABuf) -> *mut core::ffi::c_void {
    Python::attach(|py| {
        // SAFETY: the v2+ capsule contract transfers the caller's buffer
        // reference to us unconditionally.
        // `a::RawBuf` differs in constness between backends (static:
        // *const Vec<u8>, dynamic: *mut ABuf), so cast through the alias —
        // cast_const()/cast() would only compile for one of them.
        let inner = unsafe { a::Frame::from_raw(buf as a::RawBuf) };
        match Py::new(py, PyFrame { inner }) {
            Ok(obj) => obj.into_ptr() as *mut core::ffi::c_void,
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// Capsule entry: type-check a `PyObject` as `a.A` and return its C handle
/// (null if it is any other type). GIL must be held; the handle is borrowed
/// for the duration of the caller's current operation.
unsafe extern "C" fn unwrap_a(obj: *mut core::ffi::c_void) -> *mut a_abi::AHandle {
    Python::attach(|py| {
        // SAFETY: capsule contract — `obj` is a live PyObject* and the GIL
        // is held, so borrowing it for this call is sound.
        let any = unsafe { Bound::from_borrowed_ptr(py, obj.cast()) };
        match any.downcast::<PyA>() {
            // try_borrow_mut: (a) the handle may be mutated through, so its
            // provenance must derive from an exclusive borrow (static
            // backend); (b) a conflicting live PyRef yields null (a clean
            // TypeError on the consumer side) instead of a panic->abort.
            // The exclusivity the consumer then relies on is the GIL — see
            // the free-threading open item in ARCHITECTURE.md.
            Ok(cell) => match cell.try_borrow_mut() {
                Ok(mut a) => a.inner.as_raw_mut().cast::<a_abi::AHandle>(),
                Err(_) => core::ptr::null_mut(),
            },
            Err(_) => core::ptr::null_mut(),
        }
    })
}

/// Capsule entry (v5): scoped exclusive borrow — the sound successor to
/// `unwrap_a`.
///
/// The whole point is the placement of `_guard`: the `PyRefMut` is bound to
/// a local that lives until this function returns, so module a's exclusive
/// borrow of the pyclass is still held while `cb` runs. `unwrap_a` cannot do
/// this — it must drop its guard to return, leaving the handle exclusive
/// only for as long as something else (the GIL) serializes access.
///
/// Miri agrees this is the difference that matters: with the guard held, the
/// handle's provenance is live for exactly its window of use, so no foreign
/// write can disable its tag mid-flight (see `crates/app/tests/scenarios.rs`,
/// `capsule_shape_*`).
unsafe extern "C" fn with_a(
    obj: *mut core::ffi::c_void,
    cb: a_abi::AWithACb,
    user: *mut core::ffi::c_void,
) -> i32 {
    Python::attach(|py| {
        // SAFETY: capsule contract — `obj` is a live PyObject* and the GIL
        // is held, so borrowing it for this call is sound.
        let any = unsafe { Bound::from_borrowed_ptr(py, obj.cast()) };
        let Ok(cell) = any.downcast::<PyA>() else {
            return AStatus::InvalidArgument.code();
        };
        // Busy, not InvalidArgument: a live conflicting borrow is transient
        // (a re-entrant call, or another thread under free-threading), and
        // the consumer can distinguish "wrong type" from "try again".
        let Ok(mut guard) = cell.try_borrow_mut() else {
            return AStatus::Busy.code();
        };
        let raw = guard.inner.as_raw_mut().cast::<a_abi::AHandle>();
        // `cb` is foreign code; an unwind escaping through this extern "C"
        // frame would abort. Shield it exactly as the capi entry points do.
        // `_guard` is NOT dropped until this whole expression completes.
        // SAFETY: `cb` is the consumer's trampoline; the capsule contract
        // requires it to be callable with the GIL held. `_guard` above is
        // still live, so `raw` is exclusively borrowed for this whole call.
        std::panic::catch_unwind(core::panic::AssertUnwindSafe(|| unsafe { cb(user, raw) }))
            .unwrap_or(AStatus::Panic.code())
    })
}

// The capsule vtable. dynamic build: entries are liba.so's linked symbols.
// static build: entries are local wrappers over the embedded implementation,
// making this module fully self-contained.
#[cfg(feature = "dynamic")]
static VTABLE: AVtableV1 = AVtableV1 {
    a_create: a_ffi::a_create,
    a_destroy: a_ffi::a_destroy,
    a_id: a_ffi::a_id,
    a_counter: a_ffi::a_counter,
    a_increment: a_ffi::a_increment,
    a_scale: a_ffi::a_scale,
    a_shared: a_ffi::a_shared,
    a_impl_size: a_ffi::a_impl_size,
    a_data: a_ffi::a_data,
    a_fill: a_ffi::a_fill,
    a_try_fill: a_ffi::a_try_fill,
    a_copy_data: a_ffi::a_copy_data,
    a_frame: a_ffi::a_frame,
    a_buf_retain: a_ffi::a_buf_retain,
    a_buf_release: a_ffi::a_buf_release,
    a_buf_map: a_ffi::a_buf_map,
    a_buf_map_mut: a_ffi::a_buf_map_mut,
    a_buf_info: a_ffi::a_buf_info,
    a_subscribe: a_ffi::a_subscribe,
    a_unsubscribe: a_ffi::a_unsubscribe,
    a_capture: a_ffi::a_capture,
    a_stream: a_ffi::a_stream,
    a_stream_join: a_ffi::a_stream_join,
    a_set_storage: a_ffi::a_set_storage,
    a_buf_export: a_ffi::a_buf_export,
    a_buf_export2: a_ffi::a_buf_export2,
    a_set_frame_storage: a_ffi::a_set_frame_storage,
    a_frame_storage: a_ffi::a_frame_storage,
    a_subscribe_with: a_ffi::a_subscribe_with,
    a_sub_dropped: a_ffi::a_sub_dropped,
    a_capture2: a_ffi::a_capture2,
    a_capture_cancel: a_ffi::a_capture_cancel,
};

#[cfg(feature = "static")]
// The self-contained build's vtable is `a-cshim`'s, verbatim: the same
// bodies liba.so exports, minus the `#[no_mangle]` symbols (nothing here is
// resolved by the OS linker). Previously this module re-implemented all 25
// of them — see a-cshim's crate docs for why the logic can only live in an
// rlib between the two consumers.
use a_cshim::VTABLE;

// gil_used = false declares Py_mod_gil = Py_MOD_GIL_NOT_USED, so a
// free-threaded interpreter keeps the GIL disabled when it imports this
// module instead of silently re-enabling it. PyO3 gates the whole
// mechanism behind #[cfg(all(not(Py_LIMITED_API), Py_GIL_DISABLED))], so
// on the abi3/GIL build this attribute compiles to nothing -- one source
// tree, no cargo feature, and no way to assert it against an interpreter
// that would not honour it.
//
// This is an UNCONDITIONAL claim of thread-safety: get it wrong and the
// failure mode is a silent data race, not a warning. The evidence is
// py/test_ft.py, which must pass on a free-threaded build before this
// line is allowed to stay.
#[pymodule(gil_used = false)]
#[pyo3(name = "a")]
fn a_module(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Same quiet-hook policy as liba: a shielded panic is an ordinary error
    // return, and PyO3 turns it into a PanicException carrying the message,
    // so Rust's default hook would only add a duplicate backtrace on
    // stderr. `A_PANIC_VERBOSE=1` restores it. In the `dynamic` build liba
    // installs the same hook on its first `a_create`.
    #[cfg(feature = "static")]
    a_cshim::err::install_hook();
    m.add_class::<PyA>()?;
    m.add_class::<PyFrame>()?;
    m.add_class::<PyFrameStream>()?;
    m.add_function(pyo3::wrap_pyfunction!(_resolve, m)?)?;
    let name = CString::new(format!("a.{}", a_abi::A_PY_CAPI_ATTR)).unwrap();
    // The capsule stores the APyCapiV1 record by value, so its pointer() IS
    // the API record's address (C capsule convention); the capsule's
    // lifetime is the module's, which is what consumers assume.
    let capi = APyCapiV1 {
        abi_version: A_PY_CAPI_VERSION,
        unwrap_a,
        vtable: &raw const VTABLE,
        wrap_frame,
        with_a,
    };
    let capsule = PyCapsule::new(m.py(), capi, Some(name))?;
    m.add(a_abi::A_PY_CAPI_ATTR, capsule)?;
    Ok(())
}
