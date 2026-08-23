//! The real implementation. `#[repr(Rust)]` — private layout, free to drift
//! between versions — except the embedded `ASharedV1` window, frozen by the
//! a-abi contract.

use a_abi::{AFrameCb, AFrameInfoV1, ASharedV1, AStatus, A_ABI_VERSION};
use core::ffi::c_void;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// A frame's geometry: 32 rows of 30 pixels × 4 channels, with rows padded
/// to a 128-byte stride — deliberately NON-contiguous so consumers must
/// honour strides (row 0 uses bytes 0..120 of 0..128, etc.).
const FRAME_INFO: AFrameInfoV1 = AFrameInfoV1 {
    rows: 32,
    cols: 30,
    channels: 4,
    row_stride: 128,
};

/// Refcounted payload: geometry + storage travel together. Clone is what
/// `Arc::make_mut` uses for the copy-on-write detach (same storage kind).
#[derive(Clone)]
pub struct BufData {
    info: AFrameInfoV1,
    storage: crate::storage::Storage,
}

impl BufData {
    fn bytes(&self) -> &[u8] {
        self.storage.bytes()
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        self.storage.bytes_mut()
    }
}

/// Build a fresh frame whose bytes are `seed.wrapping_add(offset)`.
/// Produce a frame in `kind`, falling back to heap if that kind cannot be
/// allocated right now. The fallback matches the COW policy: a transient
/// allocation failure downgrades storage rather than failing the frame, and
/// the frame's own descriptor reports what it actually got.
fn make_frame(seed: u8, kind: u32) -> Frame {
    let len = FRAME_INFO.rows * FRAME_INFO.row_stride;
    let mut storage = crate::storage::Storage::alloc(kind, len)
        .unwrap_or_else(|| crate::storage::Storage::Heap(vec![0; len]));
    {
        let mut w = storage.write();
        for (i, b) in w.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
    }
    Frame(Arc::new(BufData {
        info: FRAME_INFO,
        storage,
    }))
}

struct SendPtr(*mut c_void);
unsafe impl Send for SendPtr {}
unsafe impl Sync for SendPtr {}

enum SubCb {
    C { cb: AFrameCb, user: SendPtr },
    Rust(Mutex<Box<dyn FnMut(Frame) + Send>>),
}

/// The bounded queue behind `A_DELIVERY_LATEST`: depth 1, drop-oldest.
///
/// Depth 1 is not a simplification — for a live consumer, a queue deeper
/// than one is a latency buffer that fills with frames nobody will ever
/// want. What a real pipeline needs is "the newest frame, when you are
/// ready", which is exactly a one-slot mailbox with overwrite.
struct Pump {
    slot: Mutex<Option<Frame>>,
    ready: Condvar,
    dropped: AtomicU64,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// One subscription. `gate` is held for the duration of each invocation;
/// teardown clears `active` and then takes the gate once, which yields the
/// contract "no invocation after unsubscribe returns" (and the matching
/// restriction: never unsubscribe from inside the callback).
struct SubEntry {
    id: u64,
    active: AtomicBool,
    gate: Mutex<()>,
    cb: SubCb,
    /// `None` for blocking delivery — the producer thread invokes the
    /// callback itself, as it always did.
    pump: Option<Pump>,
}

impl SubEntry {
    /// Invoke the callback. Always runs under `gate`, whichever thread
    /// calls it, so the teardown guarantee covers both delivery modes.
    fn invoke(&self, frame: &Frame) {
        // Poison-tolerant locks: a panicking subscriber callback must not
        // poison delivery or teardown for every other subscriber.
        let _held = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.active.load(Ordering::Acquire) {
            return;
        }
        match &self.cb {
            // Streaming delivery: the frame is BORROWED for the call.
            SubCb::C { cb, user } => unsafe {
                cb(user.0, Arc::as_ptr(&frame.0) as *mut a_abi::ABuf);
            },
            SubCb::Rust(f) => {
                (f.lock().unwrap_or_else(std::sync::PoisonError::into_inner))(frame.clone());
            }
        }
    }

    /// Called on the PRODUCER thread for every frame.
    fn deliver(&self, frame: &Frame) {
        let Some(pump) = &self.pump else {
            self.invoke(frame);
            return;
        };
        // Hand off and return immediately — the producer must not be held
        // up by this subscriber, which is the whole point of the policy.
        let mut slot = pump
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.replace(frame.clone()).is_some() {
            // The previous frame was never picked up. Count it: dropping
            // silently would make a struggling consumer indistinguishable
            // from a healthy one.
            pump.dropped.fetch_add(1, Ordering::Relaxed);
        }
        drop(slot);
        pump.ready.notify_one();
    }

    fn dropped(&self) -> u64 {
        self.pump
            .as_ref()
            .map_or(0, |p| p.dropped.load(Ordering::Relaxed))
    }

    /// The teardown protocol, shared by `unsubscribe` and `drop`: after it
    /// returns, no callback for this subscription is running or will run.
    fn teardown(&self) {
        self.active.store(false, Ordering::Release);
        if let Some(pump) = &self.pump {
            // Wake the pump so it observes !active and exits, then join it
            // — joining is what makes "will never run again" true rather
            // than merely likely.
            pump.ready.notify_all();
            let handle = pump
                .thread
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(h) = handle {
                let _ = h.join();
            }
        }
        // Blocking mode: block until any in-flight invocation returns.
        drop(
            self.gate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
}

/// One in-flight cancellable capture.
struct CaptureEntry {
    id: u64,
    cancelled: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// The pump thread body: wait for a frame, deliver it, repeat until torn
/// down. Holds an `Arc<SubEntry>`, so the entry outlives the producer's
/// view of it and teardown is a join rather than a race.
fn run_pump(entry: &Arc<SubEntry>) {
    let Some(pump) = entry.pump.as_ref() else {
        // Only spawned for pumped subscriptions; nothing to do otherwise.
        return;
    };
    loop {
        let mut slot = pump
            .slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while slot.is_none() && entry.active.load(Ordering::Acquire) {
            slot = pump
                .ready
                .wait(slot)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        let frame = slot.take();
        drop(slot);
        match frame {
            // Deliver even a frame that arrived just before teardown began;
            // `invoke` re-checks `active` under the gate, which is where
            // the guarantee actually lives.
            Some(f) => entry.invoke(&f),
            None => return,
        }
    }
}

pub struct A {
    shared: ASharedV1,
    // Extra private state for size realism (~128 bytes of primitives,
    // buffers, and pointers/handles).
    #[expect(
        dead_code,
        reason = "private padding: exists to make impl_size realistic"
    )]
    name: [u8; 32],
    #[expect(
        dead_code,
        reason = "private padding: exists to make impl_size realistic"
    )]
    name_len: usize,
    // Refcounted payload: snapshots (`frame`) share it zero-copy, and
    // Arc::make_mut gives copy-on-write when A mutates while snapshots
    // are outstanding.
    payload: Arc<BufData>,
    // Streaming state: subscriber list shared with the producer thread.
    subs: Arc<Mutex<Vec<Arc<SubEntry>>>>,
    /// Storage kind for frames this producer EMITS (capture and stream) —
    /// independent of `payload`'s own kind, which `set_storage` controls.
    /// A capture pool feeding a GPU importer wants dma-buf frames even when
    /// the producer's working buffer is heap, and vice versa.
    frame_kind: u32,
    next_sub: u64,
    /// In-flight cancellable captures, so `cancel_capture` can reach them.
    captures: Arc<Mutex<Vec<Arc<CaptureEntry>>>>,
    stream: Option<JoinHandle<()>>,
    // A v2-only private field in the MIDDLE of the struct: shifts internal
    // offsets and grows size_of::<A>() — the drift that breaks any consumer
    // holding a stale copy of this layout.
    #[cfg(feature = "v2")]
    #[expect(
        dead_code,
        reason = "drift-test field: its presence is the point, not its use"
    )]
    debug_flags: u64,
}

/// The C-facing handle representation for this backend.
pub type Raw = *mut A;

impl A {
    pub fn new(id: u64) -> Self {
        #[cfg_attr(
            not(feature = "v2"),
            expect(unused_mut, reason = "only the v2 build mutates this after init")
        )]
        let mut shared = ASharedV1 {
            abi_version: A_ABI_VERSION,
            struct_size: size_of::<ASharedV1>() as u32,
            id,
            counter: 0,
            scale: 1.5,
            fd: -1,
            _pad: 0,
            _reserved: [0; 24],
        };
        // v2 defines a new field carved out of the reserved area. A v1
        // reader of ASharedV1 never looks there, so it is unaffected.
        #[cfg(feature = "v2")]
        {
            shared._reserved[..8].copy_from_slice(&0xDEAD_BEEF_CAFE_F00Du64.to_le_bytes());
        }
        Self {
            shared,
            name: [0; 32],
            name_len: 0,
            payload: Arc::new(BufData {
                info: FRAME_INFO,
                storage: crate::storage::Storage::Heap(vec![
                    0;
                    FRAME_INFO.rows * FRAME_INFO.row_stride
                ]),
            }),
            subs: Arc::new(Mutex::new(Vec::new())),
            frame_kind: a_abi::A_STORAGE_HEAP,
            next_sub: 0,
            captures: Arc::new(Mutex::new(Vec::new())),
            stream: None,
            #[cfg(feature = "v2")]
            debug_flags: 0,
        }
    }

    pub fn id(&self) -> u64 {
        self.shared.id
    }

    pub fn counter(&self) -> u64 {
        self.shared.counter
    }

    pub fn increment(&mut self) {
        self.shared.counter += 1;
    }

    pub fn scale(&self) -> f64 {
        self.shared.scale
    }

    pub fn shared(&self) -> &ASharedV1 {
        &self.shared
    }

    /// Diagnostic: the private size of the implementation struct.
    pub fn impl_size() -> usize {
        size_of::<A>()
    }

    /// Borrow an `A` from a raw C handle for the duration of a callback.
    ///
    /// # Safety
    /// `p` must be a valid, exclusively borrowed handle for the call.
    pub unsafe fn with_raw<R>(p: Raw, f: impl FnOnce(&mut A) -> R) -> R {
        // SAFETY: the caller guarantees `p` is valid and exclusively
        // borrowed for this call, so the &mut cannot alias.
        f(unsafe { &mut *p })
    }

    /// This object's C-facing handle (borrowed; no ownership transfer).
    /// The result must only be used for READ access; for a handle that will
    /// be mutated through, use [`A::as_raw_mut`] so the pointer's provenance
    /// derives from an exclusive borrow.
    pub fn as_raw(&self) -> Raw {
        std::ptr::from_ref::<A>(self).cast_mut()
    }

    /// This object's C-facing handle with write provenance (borrowed).
    pub fn as_raw_mut(&mut self) -> Raw {
        std::ptr::from_mut::<A>(self)
    }

    /// Borrowed view of the payload (zero-copy; lifetime tied to `&self`).
    pub fn data(&self) -> &[u8] {
        self.payload.bytes()
    }

    /// Refill the payload with a deterministic pattern. Copy-on-write: if
    /// snapshots are outstanding, they keep the old bytes untouched.
    pub fn fill(&mut self, seed: u8) {
        let d = Arc::make_mut(&mut self.payload);
        // Bracketed write: on dma-buf this is DMA_BUF_IOCTL_SYNC
        // START/END, on IOSurface a per-access lock (see storage.rs).
        let mut w = d.storage.write();
        for (i, b) in w.iter_mut().enumerate() {
            *b = seed.wrapping_add(i as u8);
        }
    }

    /// Fallible fill. Seed 0 is reserved (demonstrates the error-code
    /// convention); otherwise identical to `fill`.
    pub fn try_fill(&mut self, seed: u8) -> Result<(), AStatus> {
        if seed == 0 {
            return Err(AStatus::InvalidArgument);
        }
        self.fill(seed);
        Ok(())
    }

    /// Caller-provided buffer model: copies up to `out.len()` bytes.
    pub fn copy_data(&self, out: &mut [u8]) -> usize {
        let src = self.payload.storage.read();
        let n = out.len().min(src.len());
        out[..n].copy_from_slice(&src[..n]);
        n
    }

    /// Select the storage kind for frames this producer subsequently
    /// EMITS (capture, stream) — orthogonal to `set_storage`, which
    /// reallocates the producer's own working payload.
    ///
    /// Validated eagerly with a probe allocation so an unsupported kind is
    /// an error here rather than a silent heap downgrade on every frame
    /// later, when no one is watching the return value.
    pub fn set_frame_storage(&mut self, kind: u32) -> Result<(), AStatus> {
        if crate::storage::Storage::alloc(kind, 1).is_none() {
            return Err(AStatus::InvalidArgument);
        }
        self.frame_kind = kind;
        Ok(())
    }

    /// The storage kind emitted frames are allocated in.
    pub fn frame_storage(&self) -> u32 {
        self.frame_kind
    }

    /// Zero-copy refcounted snapshot of the current payload.
    pub fn frame(&self) -> Frame {
        Frame(self.payload.clone())
    }

    /// Reallocate the payload in the given storage kind, carrying the
    /// current contents over. `InvalidArgument` if the kind is unsupported
    /// on this platform.
    pub fn set_storage(&mut self, kind: u32) -> Result<(), AStatus> {
        let mut storage = crate::storage::Storage::alloc(kind, self.payload.bytes().len())
            .ok_or(AStatus::InvalidArgument)?;
        storage
            .write()
            .copy_from_slice(&self.payload.storage.read());
        self.payload = Arc::new(BufData {
            info: self.payload.info,
            storage,
        });
        Ok(())
    }

    fn add_sub(&mut self, cb: SubCb, policy: u32) -> u64 {
        self.next_sub += 1;
        let id = self.next_sub;
        let pump = (policy == a_abi::A_DELIVERY_LATEST).then(|| Pump {
            slot: Mutex::new(None),
            ready: Condvar::new(),
            dropped: AtomicU64::new(0),
            thread: Mutex::new(None),
        });
        let entry = Arc::new(SubEntry {
            id,
            active: AtomicBool::new(true),
            gate: Mutex::new(()),
            cb,
            pump,
        });
        if let Some(pump) = entry.pump.as_ref() {
            let for_thread = entry.clone();
            let handle = std::thread::spawn(move || run_pump(&for_thread));
            *pump
                .thread
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
        }
        self.subs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(entry);
        id
    }

    /// Subscribe a Rust callback; invoked from the producer thread with an
    /// owned (retained) frame per delivery. Losing the returned id makes the
    /// subscription permanent until this object is dropped.
    #[must_use]
    pub fn subscribe(&mut self, f: impl FnMut(Frame) + Send + 'static) -> u64 {
        self.subscribe_with(f, a_abi::A_DELIVERY_BLOCKING)
    }

    /// Subscribe with an explicit delivery policy (`A_DELIVERY_*`).
    /// Unknown policies fall back to blocking — a newer library must never
    /// silently stop delivering to an older consumer.
    #[must_use]
    pub fn subscribe_with(&mut self, f: impl FnMut(Frame) + Send + 'static, policy: u32) -> u64 {
        self.add_sub(SubCb::Rust(Mutex::new(Box::new(f))), policy)
    }

    /// Frames this subscription missed because it was still busy when the
    /// next one arrived. Always 0 under blocking delivery, which drops
    /// nothing (and slows the producer instead).
    pub fn dropped(&self, id: u64) -> u64 {
        self.subs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|e| e.id == id)
            .map_or(0, |e| e.dropped())
    }

    /// Subscribe a C callback (borrowed-frame contract, see `AFrameCb`).
    ///
    /// # Safety
    /// `cb`/`user` must be callable from another thread for the life of the
    /// subscription.
    #[must_use]
    pub unsafe fn subscribe_c(&mut self, cb: AFrameCb, user: *mut c_void, policy: u32) -> u64 {
        self.add_sub(
            SubCb::C {
                cb,
                user: SendPtr(user),
            },
            policy,
        )
    }

    /// Remove a subscription; on return, no invocation is in flight and
    /// none will follow. Must not be called from within the callback.
    pub fn unsubscribe(&mut self, id: u64) {
        let entry = {
            let mut subs = self
                .subs
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            subs.iter().position(|e| e.id == id).map(|i| subs.remove(i))
        };
        if let Some(e) = entry {
            e.teardown();
        }
    }

    /// One-shot async capture: `f` is invoked exactly once, from an
    /// internal thread, with an owned frame.
    pub fn capture_cb(&self, f: impl FnOnce(Frame) + Send + 'static) {
        let kind = self.frame_kind;
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2));
            f(make_frame(0xCA, kind));
        });
    }

    /// Cancellable capture. The callback is invoked **exactly once**, with
    /// `Some(frame)` on completion or `None` if the capture was cancelled
    /// first — preserving the exactly-once invariant every trampoline in
    /// this codebase relies on to free its context box.
    ///
    /// Returns an id for [`A::cancel_capture`]. Losing the id makes the
    /// capture uncancellable, exactly as losing a subscription id does.
    #[must_use]
    pub fn capture_cb_cancellable(
        &mut self,
        f: impl FnOnce(Option<Frame>) + Send + 'static,
    ) -> u64 {
        self.next_sub += 1;
        let id = self.next_sub;
        let kind = self.frame_kind;
        let entry = Arc::new(CaptureEntry {
            id,
            cancelled: AtomicBool::new(false),
            thread: Mutex::new(None),
        });
        let for_thread = entry.clone();
        let registry = self.captures.clone();
        let handle = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2));
            // The cancellation point: sample the flag once, then commit.
            let frame = if for_thread.cancelled.load(Ordering::Acquire) {
                None
            } else {
                Some(make_frame(0xCA, kind))
            };
            f(frame);
            registry
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .retain(|e| e.id != for_thread.id);
        });
        *entry
            .thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
        self.captures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(entry);
        id
    }

    /// Cancel an in-flight capture and BLOCK until its callback has run —
    /// with `None` if the cancellation won the race, or with the frame if
    /// the capture had already completed.
    ///
    /// Blocking is deliberate and matches `unsubscribe`: after this
    /// returns, nothing is in flight and the caller may free whatever the
    /// callback captured. A non-blocking cancel would leave the consumer
    /// with no moment at which its context is provably dead.
    ///
    /// Returns `InvalidArgument` for an unknown id (already completed, or
    /// never existed) — not an error worth panicking over, since a capture
    /// completing just before you cancel it is an ordinary race.
    pub fn cancel_capture(&mut self, id: u64) -> Result<(), AStatus> {
        let entry = {
            let mut caps = self
                .captures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            caps.iter().position(|e| e.id == id).map(|i| caps.remove(i))
        };
        let Some(entry) = entry else {
            return Err(AStatus::InvalidArgument);
        };
        entry.cancelled.store(true, Ordering::Release);
        let handle = entry
            .thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(h) = handle {
            let _ = h.join();
        }
        Ok(())
    }

    /// First-class async capture, built on the completion callback.
    pub fn capture(&self) -> impl Future<Output = Frame> + Send + 'static {
        crate::oneshot::via(|complete| self.capture_cb(complete))
    }

    /// Produce `count` frames ~`period_ms` apart on an internal thread,
    /// delivering each to all current subscribers (frame i's bytes start
    /// at i).
    pub fn stream(&mut self, count: u32, period_ms: u32) {
        self.stream_join();
        let subs = self.subs.clone();
        let kind = self.frame_kind;
        self.stream = Some(std::thread::spawn(move || {
            for i in 0..count {
                let frame = make_frame(i as u8, kind);
                let snapshot: Vec<Arc<SubEntry>> = subs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                for e in &snapshot {
                    e.deliver(&frame);
                }
                std::thread::sleep(Duration::from_millis(period_ms as u64));
            }
        }));
    }

    /// Block until the active stream completes. Must not be called from
    /// within a callback.
    pub fn stream_join(&mut self) {
        if let Some(h) = self.stream.take() {
            let _ = h.join();
        }
    }
}

impl Drop for A {
    /// Destroying an A is a full teardown: join any active stream, then
    /// deactivate every remaining subscription with the same gate protocol
    /// as `unsubscribe` — after drop returns, no callback is in flight and
    /// none will ever fire again. C consumers get this via `a_destroy`.
    fn drop(&mut self) {
        self.stream_join();
        let entries: Vec<Arc<SubEntry>> = self
            .subs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect();
        for e in entries {
            e.teardown();
        }
        // Outstanding cancellable captures are part of "destroy is full
        // teardown" too: cancel each and join, so nothing is in flight when
        // this returns.
        let caps: Vec<Arc<CaptureEntry>> = self
            .captures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
            .collect();
        for c in caps {
            c.cancelled.store(true, Ordering::Release);
            let handle = c
                .thread
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(h) = handle {
                let _ = h.join();
            }
        }
    }
}

impl core::fmt::Debug for A {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("A")
            .field("id", &self.shared.id)
            .field("counter", &self.shared.counter)
            .field("storage_kind", &self.payload.storage.kind())
            .finish_non_exhaustive()
    }
}

/// Refcounted, immutable buffer snapshot. In this backend it is literally an
/// `Arc<BufData>`; the C ABI's retain/release map onto Arc's strong count.
#[derive(Clone)]
pub struct Frame(Arc<BufData>);

impl core::fmt::Debug for Frame {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Frame")
            .field("len", &self.0.bytes().len())
            .field("storage_kind", &self.0.storage.kind())
            .finish()
    }
}

/// C-facing handle for a refcounted buffer in this backend.
pub type RawBuf = *const BufData;

impl Frame {
    pub fn data(&self) -> &[u8] {
        self.0.bytes()
    }

    /// The frame's geometry (shape and strides).
    pub fn info(&self) -> AFrameInfoV1 {
        self.0.info
    }

    /// Cross-process storage descriptor (see `AFrameDescV1` for ownership).
    pub fn export(&self) -> a_abi::AFrameDescV1 {
        self.0.storage.export()
    }

    /// Descriptor v2: the v1 fields plus the pixel-format vocabulary an
    /// importer needs (fourcc, modifier, geometry). Composed here rather
    /// than in `Storage` because format and geometry belong to the payload,
    /// not to how its pages were allocated.
    pub fn export2(&self) -> a_abi::AFrameDescV2 {
        let v1 = self.0.storage.export();
        let info = self.0.info;
        a_abi::AFrameDescV2 {
            kind: v1.kind,
            _pad: 0,
            fd: v1.fd,
            id: v1.id,
            offset: v1.offset,
            len: v1.len,
            fourcc: a_abi::A_FOURCC_RGBA8888,
            _pad2: 0,
            modifier: a_abi::A_MODIFIER_LINEAR,
            width: info.cols as u32,
            height: info.rows as u32,
            stride: info.row_stride as u32,
            plane_count: 1,
        }
    }

    /// Exclusive write access: `Some` only while this is the sole reference
    /// (`Arc::get_mut` semantics).
    pub fn data_mut(&mut self) -> Option<&mut [u8]> {
        Arc::get_mut(&mut self.0).map(BufData::bytes_mut)
    }

    /// C-ABI helper for `a_buf_map_mut`: exclusive view through a raw
    /// handle, or `None` if the buffer is shared.
    ///
    /// # Safety
    /// `p` must be a live buffer handle; the returned pointer is valid only
    /// while the caller's reference remains the unique one.
    pub unsafe fn map_mut_raw(p: RawBuf) -> Option<(*mut u8, usize)> {
        // SAFETY: caller guarantees `p` is a live handle. ManuallyDrop
        // keeps the reconstructed Arc from consuming the caller's
        // reference — this is a borrow, not a transfer.
        let mut arc = core::mem::ManuallyDrop::new(unsafe { Arc::from_raw(p) });
        Arc::get_mut(&mut arc).map(|v| {
            let b = v.bytes_mut();
            (b.as_mut_ptr(), b.len())
        })
    }

    /// Transfer this reference to a raw C handle (no refcount change).
    pub fn into_raw(self) -> RawBuf {
        Arc::into_raw(self.0)
    }

    /// Take ownership of one reference from a raw C handle.
    ///
    /// # Safety
    /// `p` must come from `into_raw`/`a_frame` and carry a reference the
    /// caller is entitled to consume.
    pub unsafe fn from_raw(p: RawBuf) -> Frame {
        // SAFETY: caller transfers a reference it owns; the count is
        // unchanged, this Frame now owns it.
        Frame(unsafe { Arc::from_raw(p) })
    }

    /// Bump the refcount on a raw handle.
    ///
    /// # Safety
    /// `p` must be a live buffer handle.
    pub unsafe fn retain_raw(p: RawBuf) {
        // SAFETY: caller guarantees `p` is a live handle.
        unsafe { Arc::increment_strong_count(p) }
    }

    /// Drop one reference on a raw handle, freeing the buffer at zero.
    ///
    /// # Safety
    /// `p` must carry a reference the caller is entitled to consume.
    pub unsafe fn release_raw(p: RawBuf) {
        // SAFETY: caller transfers a reference it owns; at zero the
        // payload (and its platform storage) is freed inside this library.
        unsafe { Arc::decrement_strong_count(p) }
    }
}
