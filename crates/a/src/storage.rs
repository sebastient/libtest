//! Frame storage backends: process-local heap, Linux DMA-BUF (`dma_heap`),
//! and macOS `IOSurface`. All expose the same CPU byte-slice view, so the
//! rest of the implementation (views, COW, refcounts) is storage-agnostic;
//! `export()` yields the cross-process descriptor.

// The dma-buf/iosurface constants are referenced via full paths inside
// platform-cfg'd arms, so only the common ones are imported here.
use a_abi::{AFrameDescV1, AStatus, A_STORAGE_HEAP};

pub(crate) enum Storage {
    Heap(Vec<u8>),
    #[cfg(any(target_os = "linux", target_os = "android"))]
    DmaBuf(dmabuf::DmaBuf),
    #[cfg(target_os = "macos")]
    IoSurface(iosurface::Surface),
}

impl Storage {
    /// Allocate zeroed storage of the given kind.
    ///
    /// The two failure modes are deliberately distinct, because consumers
    /// act on them differently: `InvalidArgument` means the kind is not one
    /// this build knows (a caller bug, and the same on every machine),
    /// while `Unavailable` means the kind is valid here but the runtime
    /// cannot supply one — no device node, no permission, allocator
    /// exhausted — which a caller may legitimately skip or fall back from.
    pub(crate) fn alloc(kind: u32, len: usize) -> Result<Storage, AStatus> {
        match kind {
            A_STORAGE_HEAP => Ok(Storage::Heap(vec![0; len])),
            #[cfg(any(target_os = "linux", target_os = "android"))]
            a_abi::A_STORAGE_DMABUF => dmabuf::DmaBuf::alloc(len)
                .map(Storage::DmaBuf)
                .ok_or(AStatus::Unavailable),
            #[cfg(target_os = "macos")]
            a_abi::A_STORAGE_IOSURFACE => iosurface::Surface::alloc(len)
                .map(Storage::IoSurface)
                .ok_or(AStatus::Unavailable),
            _ => Err(AStatus::InvalidArgument),
        }
    }

    pub(crate) fn kind(&self) -> u32 {
        match self {
            Storage::Heap(_) => A_STORAGE_HEAP,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Storage::DmaBuf(_) => a_abi::A_STORAGE_DMABUF,
            #[cfg(target_os = "macos")]
            Storage::IoSurface(_) => a_abi::A_STORAGE_IOSURFACE,
        }
    }

    /// UNSYNCHRONIZED view of the bytes.
    ///
    /// This is the accessor behind the zero-copy borrowed-view API
    /// (`a_data`, `a_buf_map`): it hands out a pointer whose validity window
    /// is the contract's, not a lock's. Cache maintenance for that window
    /// belongs to whoever reads through it — the same division of labour the
    /// kernel's dma-buf API assumes of an importer.
    ///
    /// For access performed by THIS library, use [`Storage::read`] /
    /// [`Storage::write`], which bracket it properly.
    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            Storage::Heap(v) => v,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Storage::DmaBuf(d) => d.bytes(),
            #[cfg(target_os = "macos")]
            Storage::IoSurface(s) => s.bytes(),
        }
    }

    /// UNSYNCHRONIZED mutable view — see [`Storage::bytes`].
    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        match self {
            Storage::Heap(v) => v,
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Storage::DmaBuf(d) => d.bytes_mut(),
            #[cfg(target_os = "macos")]
            Storage::IoSurface(s) => s.bytes_mut(),
        }
    }

    /// Bracketed CPU read. Issues `DMA_BUF_IOCTL_SYNC(START|READ)` on entry
    /// and `(END|READ)` on drop for dma-buf storage, and takes a read-only
    /// `IOSurfaceLock` for surfaces. A no-op for heap storage.
    pub(crate) fn read(&self) -> CpuRead<'_> {
        match self {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Storage::DmaBuf(d) => d.sync(dmabuf::SYNC_START | dmabuf::SYNC_READ),
            #[cfg(target_os = "macos")]
            Storage::IoSurface(s) => s.lock(iosurface::LOCK_READ_ONLY),
            Storage::Heap(_) => {}
        }
        CpuRead { storage: self }
    }

    /// Bracketed CPU write — the read/write counterpart of [`Storage::read`].
    pub(crate) fn write(&mut self) -> CpuWrite<'_> {
        match self {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Storage::DmaBuf(d) => d.sync(dmabuf::SYNC_START | dmabuf::SYNC_RW),
            #[cfg(target_os = "macos")]
            Storage::IoSurface(s) => s.lock(0),
            Storage::Heap(_) => {}
        }
        CpuWrite { storage: self }
    }

    /// The cross-process descriptor (see `AFrameDescV1` for ownership).
    /// Receivers must treat the buffer as READ-ONLY: writes through an
    /// exported fd/surface race Rust's `&[u8]` view of the same pages.
    pub(crate) fn export(&self) -> AFrameDescV1 {
        let mut desc = AFrameDescV1 {
            kind: self.kind(),
            _pad: 0,
            fd: -1,
            id: 0,
            offset: 0,
            len: self.bytes().len(),
        };
        match self {
            Storage::Heap(_) => {}
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Storage::DmaBuf(d) => desc.fd = d.dup_fd(),
            #[cfg(target_os = "macos")]
            Storage::IoSurface(s) => desc.id = s.id(),
        }
        desc
    }
}

/// Scoped CPU-read access. The sync/lock bracket is closed on drop, so the
/// access window is exactly this guard's lifetime.
pub(crate) struct CpuRead<'a> {
    storage: &'a Storage,
}

impl core::ops::Deref for CpuRead<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.storage.bytes()
    }
}

impl Drop for CpuRead<'_> {
    fn drop(&mut self) {
        match self.storage {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Storage::DmaBuf(d) => d.sync(dmabuf::SYNC_END | dmabuf::SYNC_READ),
            #[cfg(target_os = "macos")]
            Storage::IoSurface(s) => s.unlock(iosurface::LOCK_READ_ONLY),
            Storage::Heap(_) => {}
        }
    }
}

/// Scoped CPU-write access — the read/write counterpart of [`CpuRead`].
pub(crate) struct CpuWrite<'a> {
    storage: &'a mut Storage,
}

impl core::ops::Deref for CpuWrite<'_> {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.storage.bytes()
    }
}

impl core::ops::DerefMut for CpuWrite<'_> {
    fn deref_mut(&mut self) -> &mut [u8] {
        self.storage.bytes_mut()
    }
}

impl Drop for CpuWrite<'_> {
    fn drop(&mut self) {
        match &*self.storage {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            Storage::DmaBuf(d) => d.sync(dmabuf::SYNC_END | dmabuf::SYNC_RW),
            #[cfg(target_os = "macos")]
            Storage::IoSurface(s) => s.unlock(0),
            Storage::Heap(_) => {}
        }
    }
}

// COW support: Arc::make_mut clones storage of the SAME kind and copies.
// Platform-buffer allocation can legitimately fail at runtime (dma_heap
// exhausted, surface quota) — that must not panic across the C boundary, so
// the clone falls back to heap storage: the snapshot keeps its platform
// buffer, only the detached producer copy downgrades (its next export will
// say A_STORAGE_HEAP).
impl Clone for Storage {
    fn clone(&self) -> Self {
        let src = self.read();
        let len = src.len();
        let mut fresh =
            Storage::alloc(self.kind(), len).unwrap_or_else(|_| Storage::Heap(vec![0; len]));
        fresh.write().copy_from_slice(&src);
        fresh
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub mod dmabuf {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    #[repr(C)]
    struct DmaHeapAllocationData {
        len: u64,
        fd: u32,
        fd_flags: u32,
        heap_flags: u64,
    }
    // _IOWR('H', 0x0, struct dma_heap_allocation_data). Stored as u32 and
    // cast at the call site: glibc's ioctl takes c_ulong, bionic's c_int.
    const DMA_HEAP_IOCTL_ALLOC: u32 = 0xc018_4800;

    /// `struct dma_buf_sync { __u64 flags; }` — the CPU-access bracket.
    #[repr(C)]
    struct DmaBufSync {
        flags: u64,
    }
    // _IOW('b', 0, struct dma_buf_sync)
    const DMA_BUF_IOCTL_SYNC: u32 = 0x4008_6200;

    pub(crate) const SYNC_READ: u64 = 1 << 0;
    pub(crate) const SYNC_WRITE: u64 = 1 << 1;
    pub(crate) const SYNC_RW: u64 = SYNC_READ | SYNC_WRITE;
    pub(crate) const SYNC_START: u64 = 0;
    pub(crate) const SYNC_END: u64 = 1 << 2;

    /// A CPU-mapped DMA-BUF from /dev/dma_heap. The fd is the shareable
    /// handle; the mapping is our process's view of the same pages.
    pub struct DmaBuf {
        fd: OwnedFd,
        ptr: *mut u8,
        len: usize,
    }

    // SAFETY: the mapping is plain shared memory and in-process aliasing is
    // governed by the same Arc/COW rules as heap storage. CAVEAT that the
    // in-process rules cannot enforce: `dup_fd` exports an O_RDWR fd, so an
    // EXTERNAL process/device writing the pages while Rust reads them is a
    // data race on memory Rust types as &[u8]. The exported-descriptor
    // contract therefore declares exported buffers read-only for receivers;
    // device-written buffers additionally need DMA_BUF_IOCTL_SYNC (open
    // item, see ARCHITECTURE.md).
    unsafe impl Send for DmaBuf {}
    unsafe impl Sync for DmaBuf {}

    impl DmaBuf {
        pub fn alloc(len: usize) -> Option<DmaBuf> {
            for heap in ["/dev/dma_heap/system", "/dev/dma_heap/linux,cma"] {
                let Ok(h) = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(heap)
                else {
                    continue;
                };
                let mut req = DmaHeapAllocationData {
                    len: len as u64,
                    fd: 0,
                    fd_flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
                    heap_flags: 0,
                };
                if unsafe { libc::ioctl(h.as_raw_fd(), DMA_HEAP_IOCTL_ALLOC as _, &mut req) } != 0 {
                    continue;
                }
                let fd = unsafe { OwnedFd::from_raw_fd(req.fd as i32) };
                let ptr = unsafe {
                    libc::mmap(
                        std::ptr::null_mut(),
                        len,
                        libc::PROT_READ | libc::PROT_WRITE,
                        libc::MAP_SHARED,
                        fd.as_raw_fd(),
                        0,
                    )
                };
                if ptr == libc::MAP_FAILED {
                    return None;
                }
                return Some(DmaBuf {
                    fd,
                    ptr: ptr as *mut u8,
                    len,
                });
            }
            None
        }

        pub fn bytes(&self) -> &[u8] {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }

        pub fn bytes_mut(&mut self) -> &mut [u8] {
            unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
        }

        /// The kernel's CPU-access bracket: `SYNC_START` before touching
        /// the mapping and `SYNC_END` after, each tagged READ and/or WRITE.
        ///
        /// This is cache maintenance, not mutual exclusion — it does not
        /// stop a device from writing concurrently. It is what makes CPU
        /// reads of device-written data coherent on architectures where the
        /// device is not cache-coherent with the CPU (most arm64 SoCs, this
        /// prototype's imx95-pro included).
        ///
        /// Failure is deliberately ignored: some exporters do not implement
        /// the ioctl at all, and on those it is a no-op rather than an
        /// error worth failing a frame over.
        pub(crate) fn sync(&self, flags: u64) {
            let arg = DmaBufSync { flags };
            // SAFETY: `arg` outlives the call; the fd is ours and open.
            unsafe {
                libc::ioctl(self.fd.as_raw_fd(), DMA_BUF_IOCTL_SYNC as _, &raw const arg);
            }
        }

        /// Duplicate the fd for export; the caller owns the duplicate.
        pub fn dup_fd(&self) -> i32 {
            unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) }
        }
    }

    impl Drop for DmaBuf {
        fn drop(&mut self) {
            unsafe { libc::munmap(self.ptr as *mut _, self.len) };
        }
    }
}

#[cfg(target_os = "macos")]
pub(crate) mod iosurface {
    //! Hand-declared minimal `IOSurface` + CoreFoundation C API (both are
    //! plain C frameworks; no crate dependencies needed).
    // Hand-declared externs mirror the C framework's own spelling exactly;
    // renaming them to Rust conventions would obscure the correspondence.
    #![expect(
        non_upper_case_globals,
        reason = "names mirror the IOSurface/CoreFoundation C API verbatim"
    )]

    use core::ffi::{c_char, c_int, c_void};

    type CFTypeRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFNumberRef = *const c_void;
    type CFStringRef = *const c_void;
    type IOSurfaceRef = *mut c_void;

    const kCFNumberSInt64Type: isize = 4;

    extern "C" {
        static kCFTypeDictionaryKeyCallBacks: [usize; 6];
        static kCFTypeDictionaryValueCallBacks: [usize; 6];
        fn CFNumberCreate(alloc: *const c_void, ty: isize, value: *const c_void) -> CFNumberRef;
        fn CFDictionaryCreate(
            alloc: *const c_void,
            keys: *const CFTypeRef,
            values: *const CFTypeRef,
            num: isize,
            key_cbs: *const c_void,
            value_cbs: *const c_void,
        ) -> CFDictionaryRef;
        fn CFRelease(cf: CFTypeRef);

        static kIOSurfaceAllocSize: CFStringRef;
        static kIOSurfaceBytesPerElement: CFStringRef;
        fn IOSurfaceCreate(props: CFDictionaryRef) -> IOSurfaceRef;
        fn IOSurfaceGetBaseAddress(s: IOSurfaceRef) -> *mut c_void;
        fn IOSurfaceGetAllocSize(s: IOSurfaceRef) -> usize;
        fn IOSurfaceGetID(s: IOSurfaceRef) -> u32;
        fn IOSurfaceLock(s: IOSurfaceRef, options: u32, seed: *mut u32) -> c_int;
        fn IOSurfaceUnlock(s: IOSurfaceRef, options: u32, seed: *mut u32) -> c_int;
    }

    // Silence dead-code note for c_char (kept for future CFString use).
    fn _unused(_: c_char) {}

    /// `kIOSurfaceLockReadOnly` — a read that must not mark the surface
    /// dirty (which would force a GPU-side copy back on the next use).
    pub(crate) const LOCK_READ_ONLY: u32 = 1;

    /// An `IOSurface` used as CPU frame storage.
    ///
    /// It holds a **lifetime lock** taken at `alloc`, and per-access locks
    /// nest inside it (`IOSurface` lock counts are per process). The lifetime
    /// lock cannot simply be dropped in favour of per-access locking,
    /// because this architecture's zero-copy borrowed-view contract
    /// (`a_data` / `a_buf_map`) hands out a base-address pointer whose
    /// validity window is defined by the contract — "until the producer is
    /// mutated or destroyed" — not by a lock scope. A pointer handed out
    /// under a lock that has since been released is exactly the dangling
    /// case the contract promises not to produce.
    ///
    /// So the two mechanisms serve different consumers: the lifetime lock
    /// keeps exported views valid, and the per-access bracket
    /// ([`Surface::lock`]) is what a GPU pipeline needs around the
    /// library's own reads and writes. A design that wanted per-access
    /// locking *only* would have to replace the borrowed-view model with a
    /// scoped map/unmap API — a different C ABI, not an implementation
    /// detail.
    #[expect(
        clippy::struct_field_names,
        reason = "the field IS the IOSurfaceRef; any other name would be less clear"
    )]
    pub(crate) struct Surface {
        surface: IOSurfaceRef,
        ptr: *mut u8,
        len: usize,
    }

    // SAFETY: in-process aliasing follows the Arc/COW rules. CAVEAT: the
    // surface's global ID is exported by `a_buf_export`, and IOSurfaceLookup
    // in ANY process yields a writable mapping — the exported-descriptor
    // contract declares exported buffers read-only for receivers, but that
    // is convention, not enforcement.
    unsafe impl Send for Surface {}
    unsafe impl Sync for Surface {}

    impl Surface {
        pub(crate) fn alloc(len: usize) -> Option<Surface> {
            unsafe {
                // IOSurface's CFNumber properties are typed sInt64 by the
                // framework; our lengths are page-scale, never near 2^63.
                #[expect(clippy::cast_possible_wrap, reason = "IOSurface API takes int64")]
                let size = len as i64;
                let bpe = 1i64;
                let v_size = CFNumberCreate(
                    std::ptr::null(),
                    kCFNumberSInt64Type,
                    (&raw const size).cast::<c_void>(),
                );
                let v_bpe = CFNumberCreate(
                    std::ptr::null(),
                    kCFNumberSInt64Type,
                    (&raw const bpe).cast::<c_void>(),
                );
                let keys = [
                    kIOSurfaceAllocSize as CFTypeRef,
                    kIOSurfaceBytesPerElement as CFTypeRef,
                ];
                let vals = [v_size as CFTypeRef, v_bpe as CFTypeRef];
                let props = CFDictionaryCreate(
                    std::ptr::null(),
                    keys.as_ptr(),
                    vals.as_ptr(),
                    2,
                    kCFTypeDictionaryKeyCallBacks.as_ptr().cast::<c_void>(),
                    kCFTypeDictionaryValueCallBacks.as_ptr().cast::<c_void>(),
                );
                if v_size.is_null() || v_bpe.is_null() || props.is_null() {
                    // CF allocation failure: release whatever exists, bail.
                    for cf in [props as CFTypeRef, v_size as CFTypeRef, v_bpe as CFTypeRef] {
                        if !cf.is_null() {
                            CFRelease(cf);
                        }
                    }
                    return None;
                }
                let surface = IOSurfaceCreate(props);
                CFRelease(props);
                CFRelease(v_size as CFTypeRef);
                CFRelease(v_bpe as CFTypeRef);
                if surface.is_null() {
                    return None;
                }
                if IOSurfaceGetAllocSize(surface) < len
                    || IOSurfaceLock(surface, 0, std::ptr::null_mut()) != 0
                {
                    CFRelease(surface.cast_const());
                    return None;
                }
                let ptr = IOSurfaceGetBaseAddress(surface).cast::<u8>();
                if ptr.is_null() {
                    IOSurfaceUnlock(surface, 0, std::ptr::null_mut());
                    CFRelease(surface.cast_const());
                    return None;
                }
                std::ptr::write_bytes(ptr, 0, len);
                Some(Surface { surface, ptr, len })
            }
        }

        pub(crate) fn bytes(&self) -> &[u8] {
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }

        pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
            unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
        }

        /// Take the per-access lock. `options` is 0 for read/write or
        /// `LOCK_READ_ONLY` for reads (which skips dirtying the surface).
        ///
        /// `IOSurface` locks are COUNTED per process, so this nests inside
        /// the lifetime lock taken at `alloc` — see the note on that field
        /// for why the lifetime lock cannot simply be removed.
        pub(crate) fn lock(&self, options: u32) {
            // SAFETY: `self.surface` is a live IOSurfaceRef we own.
            unsafe { IOSurfaceLock(self.surface, options, std::ptr::null_mut()) };
        }

        /// Release a lock taken by [`Surface::lock`] with the same options.
        pub(crate) fn unlock(&self, options: u32) {
            // SAFETY: paired with a preceding lock of the same options.
            unsafe { IOSurfaceUnlock(self.surface, options, std::ptr::null_mut()) };
        }

        /// The surface's global ID — resolvable in any process via
        /// `IOSurfaceLookup`.
        pub(crate) fn id(&self) -> u32 {
            unsafe { IOSurfaceGetID(self.surface) }
        }
    }

    impl Drop for Surface {
        fn drop(&mut self) {
            unsafe {
                IOSurfaceUnlock(self.surface, 0, std::ptr::null_mut());
                CFRelease(self.surface.cast_const());
            }
        }
    }
}
