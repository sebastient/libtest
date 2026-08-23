//! BoltFFI façade module `ab` — Swift/Kotlin surface over crates `a` and
//! `b`, following the mobile-sdk conventions: one flat module, prefix as
//! namespace, records not builders, no logic of its own. The cross-library
//! contract lives below this layer (liba.so <-> libb.so C ABI); this crate
//! only lowers shapes across the language boundary.

use boltffi::{data, error, export, ffi_stream, EventSubscription, StreamProducer};
use std::sync::{Arc, Mutex};

/// Errors surfaced to Swift/Kotlin (`throws` / `@Throws`).
#[error]
#[derive(Debug, Clone)]
pub enum AbError {
    InvalidArgument,
    /// Internal invariant failure caught at a boundary; the object should
    /// only be dropped.
    Panic,
    Internal,
}

fn map_status(e: a::AStatus) -> AbError {
    match e {
        a::AStatus::InvalidArgument => AbError::InvalidArgument,
        a::AStatus::Panic => AbError::Panic,
        _ => AbError::Internal,
    }
}

/// Frame geometry (rows × cols × channels of u8, row stride in bytes; the
/// stride may exceed cols × channels — buffers are NOT contiguous).
#[data]
#[derive(Debug, Clone, Copy)]
pub struct AbShape {
    pub rows: u32,
    pub cols: u32,
    pub channels: u32,
    pub row_stride: u32,
}

/// Cross-process storage descriptor. kind: 0 heap, 1 DMA-BUF, 2 IOSurface.
/// For DMA-BUF, `fd` is a dup'd descriptor OWNED by the receiver (import via
/// AHardwareBuffer/mmap, close when done). For IOSurface, `id` is the global
/// IOSurfaceID (`IOSurfaceLookup`). Primitives only — crossing is zero-copy.
#[data]
#[derive(Debug, Clone, Copy)]
pub struct AbFrameDesc {
    pub kind: u32,
    pub fd: i32,
    pub id: u32,
    pub len: u64,
}

/// One delivered stream frame, reduced to values (the frame's bytes stay in
/// Rust; use capture()/frame() for a handle).
#[data]
#[derive(Debug, Clone, Copy)]
pub struct AbFrameEvent {
    pub index: u32,
    pub first_byte: u8,
    pub checksum: u64,
}

/// Immutable zero-copy frame snapshot (refcounted in the implementation).
pub struct AbFrame {
    inner: a::Frame,
}

#[export]
impl AbFrame {
    /// Checksum computed by library B over the shared bytes (zero-copy on
    /// the Rust side; nothing crosses the language boundary but the result).
    pub fn checksum(&self) -> u64 {
        b::frame_checksum(&self.inner)
    }

    pub fn shape(&self) -> AbShape {
        let i = self.inner.info();
        AbShape {
            rows: i.rows as u32,
            cols: i.cols as u32,
            channels: i.channels as u32,
            row_stride: i.row_stride as u32,
        }
    }

    /// Copying fallback: the payload as an owned byte array (Swift `Data`,
    /// Kotlin `ByteArray`). For zero-copy use `export_desc`.
    pub fn data(&self) -> Vec<u8> {
        self.inner.data().to_vec()
    }

    /// Zero-copy path: export the platform storage descriptor.
    pub fn export_desc(&self) -> AbFrameDesc {
        let d = self.inner.export();
        AbFrameDesc {
            kind: d.kind,
            fd: d.fd,
            id: d.id,
            len: d.len as u64,
        }
    }
}

/// The A object: created here, consumed by library B's logic, streamed to
/// the app. BoltFFI classes must be Send + Sync with `&self` receivers, so
/// the inner A sits behind a Mutex (same pattern as mobile-sdk's HAL
/// wrappers).
pub struct AbSource {
    inner: Mutex<a::A>,
    events: Arc<StreamProducer<AbFrameEvent>>,
    subscribed: Mutex<bool>,
}

#[export]
impl AbSource {
    pub fn new(id: u64) -> Self {
        Self {
            inner: Mutex::new(a::A::new(id)),
            events: Arc::new(StreamProducer::new(256)),
            subscribed: Mutex::new(false),
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

    pub fn try_fill(&self, seed: u8) -> Result<(), AbError> {
        self.inner.lock().unwrap().try_fill(seed).map_err(map_status)
    }

    /// Switch payload storage: 0 heap, 1 DMA-BUF (Linux/Android),
    /// 2 IOSurface (Apple). Unsupported kinds raise.
    pub fn set_storage(&self, kind: u32) -> Result<(), AbError> {
        self.inner.lock().unwrap().set_storage(kind).map_err(map_status)
    }

    /// Zero-copy refcounted snapshot.
    pub fn frame(&self) -> AbFrame {
        AbFrame {
            inner: self.inner.lock().unwrap().frame(),
        }
    }

    /// Library B's opaque-access operation (increments twice, id+counter).
    pub fn process(&self) -> u64 {
        b::process(&mut self.inner.lock().unwrap())
    }

    /// Library B's repr(C) fast-path read.
    pub fn process_fast(&self) -> f64 {
        b::process_fast(&self.inner.lock().unwrap())
    }

    /// Library B's zero-copy checksum of the payload.
    pub fn checksum(&self) -> u64 {
        b::checksum(&self.inner.lock().unwrap())
    }

    /// Async capture (Swift `async`, Kotlin `suspend`): built on the same
    /// completion-callback machinery as the C API. The future does not
    /// borrow the source, so the lock is held only to start the capture.
    pub async fn capture(&self) -> AbFrame {
        let fut = self.inner.lock().unwrap().capture();
        AbFrame { inner: fut.await }
    }

    /// Library B's derived async operation.
    pub async fn capture_checksum(&self) -> u64 {
        let fut = self.inner.lock().unwrap().capture();
        let frame = fut.await;
        b::frame_checksum(&frame)
    }

    /// Frame events as Swift `AsyncStream` / Kotlin `Flow`. NOTE the
    /// backpressure contract differs from the C API: this ring buffer
    /// (capacity 256) silently drops NEW events when full.
    #[ffi_stream(item = AbFrameEvent)]
    pub fn events(&self) -> Arc<EventSubscription<AbFrameEvent>> {
        self.events.subscribe()
    }

    /// Produce `count` frames ~`period_ms` apart on an internal thread,
    /// bridging deliveries into the event stream.
    pub fn start(&self, count: u32, period_ms: u32) {
        let mut inner = self.inner.lock().unwrap();
        let mut subscribed = self.subscribed.lock().unwrap();
        if !*subscribed {
            let producer = self.events.clone();
            let mut index = 0u32;
            inner.subscribe(move |frame| {
                let event = AbFrameEvent {
                    index,
                    first_byte: frame.data()[0],
                    checksum: b::frame_checksum(&frame),
                };
                index += 1;
                let _ = producer.push(event);
            });
            *subscribed = true;
        }
        inner.stream(count, period_ms);
    }

    /// Wait for the active stream to finish (test convenience).
    /// NOTE: holds the object lock for the stream's remaining duration, so
    /// every other method stalls meanwhile — on a mobile main thread that is
    /// an ANR pattern. A production surface would keep the join handle
    /// outside the object lock. Also: the subscriber closure must never
    /// call back into this object (same lock — self-deadlock).
    pub fn join(&self) {
        self.inner.lock().unwrap().stream_join()
    }
}
