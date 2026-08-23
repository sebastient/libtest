//! Size probe: the smallest credible slice of A's C ABI, with no libstd.
//!
//! What A would have to give up to be `no_std`, and why this is a
//! measurement rather than a port:
//!
//! - `Arc` → refcounted snapshots, COW, and the whole zero-copy buffer
//!   model are built on it. A `no_std` version means hand-rolled atomic
//!   refcounts over a caller-supplied allocator.
//! - `Vec` → heap storage disappears; every buffer becomes caller-provided
//!   or comes from a fixed pool (as below).
//! - `std::thread` → the producer thread, streaming, and completion
//!   callbacks all go. Async capture goes with them.
//! - `Mutex`/`Condvar` → the delivery gate and the unsubscribe teardown
//!   guarantee are built on them; without them the callback contract cannot
//!   be upheld at all.
//! - `catch_unwind` → already traded away one rung up, at `panic = "abort"`.
//!
//! That is not a size knob applied to this component; it is a different
//! component. The number below is still worth having: it says what the
//! floor is, so a decision to pursue it is made against a real figure.

#![no_std]

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

// A cdylib without libstd still needs a panic handler and an abort path.
unsafe extern "C" {
    fn abort() -> !;
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // SAFETY: libc's abort is always callable and never returns.
    unsafe { abort() }
}

/// Fixed-capacity handle pool: the `no_std` stand-in for `Box::new`.
/// Allocation is a slot index, so there is no allocator at all.
const CAPACITY: usize = 64;

#[repr(C)]
struct Slot {
    live: AtomicU32,
    id: AtomicU64,
    counter: AtomicU64,
}

static POOL: [Slot; CAPACITY] = [const {
    Slot {
        live: AtomicU32::new(0),
        id: AtomicU64::new(0),
        counter: AtomicU64::new(0),
    }
}; CAPACITY];

/// Opaque handle, same contract as `AHandle`: never dereferenced by a
/// consumer, never sized, never stack-allocated.
#[repr(C)]
pub struct AnHandle {
    _data: [u8; 0],
    _marker: core::marker::PhantomData<(*mut u8, core::marker::PhantomPinned)>,
}

fn slot_of(p: *const AnHandle) -> Option<&'static Slot> {
    let idx = (p as usize).checked_sub(1)?;
    POOL.get(idx)
}

/// Claim a pool slot. Returns null when the pool is exhausted — the
/// `no_std` analogue of an allocation failure, and a reminder that a fixed
/// pool turns a soft failure into a hard capacity limit.
#[no_mangle]
pub extern "C" fn an_create(id: u64) -> *mut AnHandle {
    for (i, slot) in POOL.iter().enumerate() {
        if slot
            .live
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            slot.id.store(id, Ordering::Release);
            slot.counter.store(0, Ordering::Release);
            // Index+1 so a valid handle is never null.
            return (i + 1) as *mut AnHandle;
        }
    }
    core::ptr::null_mut()
}

/// # Safety
/// `p` must come from `an_create` and not have been destroyed.
#[no_mangle]
pub unsafe extern "C" fn an_destroy(p: *mut AnHandle) {
    if let Some(slot) = slot_of(p) {
        slot.live.store(0, Ordering::Release);
    }
}

/// # Safety
/// `p` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn an_id(p: *const AnHandle) -> u64 {
    slot_of(p).map_or(0, |s| s.id.load(Ordering::Acquire))
}

/// # Safety
/// `p` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn an_counter(p: *const AnHandle) -> u64 {
    slot_of(p).map_or(0, |s| s.counter.load(Ordering::Acquire))
}

/// # Safety
/// `p` must be a valid handle.
#[no_mangle]
pub unsafe extern "C" fn an_increment(p: *mut AnHandle) {
    if let Some(slot) = slot_of(p) {
        slot.counter.fetch_add(1, Ordering::AcqRel);
    }
}
