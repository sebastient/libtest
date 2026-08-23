//! BoltFFI module `amod` — A's surface as an independent module. Frames and
//! sources can be handed to OTHER modules (see b-mod) as raw u64 handles;
//! all lifetime operations execute inside liba, so allocation symmetry
//! holds across module boundaries.

use boltffi::{data, error, export};
use std::sync::Mutex;

// SAFETY-BOUNDARY: the export macro forbids `unsafe` in exported bodies,
// so the raw-handle plumbing lives here. These helpers convert UNCHECKED
// integers from the foreign side into handles — the safety obligation has
// moved to the .NET caller (pass only handles obtained from this module's
// raw_* methods, respect their borrow/own semantics). Garbage in is UB, by
// the same trust model as any C API taking a pointer.
fn frame_from_raw(handle: u64) -> a::Frame {
    unsafe { a::Frame::from_raw(handle as a::RawBuf) }
}

#[error]
#[derive(Debug, Clone)]
pub enum AmError {
    InvalidArgument,
    Panic,
    Internal,
}

fn map_status(e: a::AStatus) -> AmError {
    match e {
        a::AStatus::InvalidArgument => AmError::InvalidArgument,
        a::AStatus::Panic => AmError::Panic,
        _ => AmError::Internal,
    }
}

/// Storage descriptor (kind: 0 heap, 1 DMA-BUF, 2 IOSurface); `fd` is owned
/// by the receiver, `id` is a global IOSurfaceID. Primitives — zero-copy.
#[data]
#[derive(Debug, Clone, Copy)]
pub struct AmFrameDesc {
    pub kind: u32,
    pub fd: i32,
    pub id: u32,
    pub len: u64,
}

/// Immutable frame snapshot.
pub struct AmFrame {
    inner: a::Frame,
}

#[export]
impl AmFrame {
    /// Reconstruct a frame from a raw handle previously produced by
    /// `raw_retained` (takes ownership of that reference).
    pub fn from_raw(handle: u64) -> Self {
        Self {
            inner: frame_from_raw(handle),
        }
    }

    pub fn first_byte(&self) -> u8 {
        self.inner.data()[0]
    }

    pub fn len(&self) -> u64 {
        self.inner.data().len() as u64
    }

    pub fn export_desc(&self) -> AmFrameDesc {
        let d = self.inner.export();
        AmFrameDesc {
            kind: d.kind,
            fd: d.fd,
            id: d.id,
            len: d.len as u64,
        }
    }

    /// Hand out an OWNED (+1 refcount) raw handle for another module.
    /// The receiver must balance it (e.g. bmod's consume_* functions, or
    /// `AmFrame.from_raw` to re-wrap it here).
    pub fn raw_retained(&self) -> u64 {
        self.inner.clone().into_raw() as u64
    }
}

/// The A object, module edition.
pub struct AmSource {
    inner: Mutex<a::A>,
}

#[export]
impl AmSource {
    // NOTE id is u32, not u64: BoltFFI's C# backend gives every class an
    // internal `(ulong handle)` constructor, so a public one-u64 primary
    // constructor collides with it (CS0111) — fork finding, see docs.
    pub fn new(id: u32) -> Self {
        Self {
            inner: Mutex::new(a::A::new(id as u64)),
        }
    }

    pub fn id(&self) -> u64 {
        self.inner.lock().unwrap().id()
    }

    pub fn counter(&self) -> u64 {
        self.inner.lock().unwrap().counter()
    }

    pub fn fill(&self, seed: u8) {
        self.inner.lock().unwrap().fill(seed)
    }

    pub fn try_fill(&self, seed: u8) -> Result<(), AmError> {
        self.inner.lock().unwrap().try_fill(seed).map_err(map_status)
    }

    pub fn set_storage(&self, kind: u32) -> Result<(), AmError> {
        self.inner.lock().unwrap().set_storage(kind).map_err(map_status)
    }

    pub fn frame(&self) -> AmFrame {
        AmFrame {
            inner: self.inner.lock().unwrap().frame(),
        }
    }

    pub async fn capture(&self) -> AmFrame {
        let fut = self.inner.lock().unwrap().capture();
        AmFrame { inner: fut.await }
    }

    /// BORROWED raw handle to the underlying A, valid while this source is
    /// alive and not used concurrently with mutating calls. Another module
    /// (bmod) can operate on the same object through liba's C ABI.
    pub fn raw_handle(&self) -> u64 {
        self.inner.lock().unwrap().as_raw() as u64
    }
}
