//! BoltFFI module `bmod` — B's surface as an independent module. It never
//! links amod: A-objects arrive as raw u64 handles (amod's `raw_handle` /
//! `raw_retained`), and every operation routes through liba's C ABI — the
//! one implementation home both modules share. This is the .NET
//! multi-library shape: two NuGet packages, two native libraries, one
//! shared implementation library.

use boltffi::export;
// SAFETY-BOUNDARY: the export macro forbids `unsafe` in exported bodies;
// the raw-handle plumbing lives here. Handles are UNCHECKED integers from
// the foreign side — the safety obligation is the .NET caller's (only
// handles from amod's raw_* methods, borrow/own semantics respected).
fn with_source<R>(handle: u64, f: impl FnOnce(&mut a::A) -> R) -> R {
    unsafe { a::A::with_raw(handle as a::Raw, f) }
}

fn borrow_frame<R>(handle: u64, f: impl FnOnce(&a::Frame) -> R) -> R {
    let frame = core::mem::ManuallyDrop::new(unsafe { a::Frame::from_raw(handle as a::RawBuf) });
    f(&frame)
}

fn own_frame(handle: u64) -> a::Frame {
    unsafe { a::Frame::from_raw(handle as a::RawBuf) }
}


/// Run B's opaque-access operation on an A borrowed from another module
/// (increments twice, returns id + counter).
///
/// `source_handle` is a BORROWED handle from `AmSource.raw_handle()`; it
/// must stay valid for the call and not be used concurrently with mutating
/// calls from its owning module.
#[export]
pub fn process_source(source_handle: u64) -> u64 {
    with_source(source_handle, |x| b::process(x))
}

/// B's zero-copy checksum of a borrowed A's payload.
#[export]
pub fn checksum_source(source_handle: u64) -> u64 {
    with_source(source_handle, |x| b::checksum(x))
}

/// Checksum a frame BORROWED from another module (`AmFrame.raw_retained`
/// gives an owned reference — pair this with a later consume, or use
/// `consume_frame_checksum` to do both at once).
#[export]
pub fn frame_checksum(frame_handle: u64) -> u64 {
    borrow_frame(frame_handle, b::frame_checksum)
}

/// Take OWNERSHIP of a retained frame handle, checksum it, and release the
/// reference — the release executes inside liba (allocation symmetry holds
/// across modules).
#[export]
pub fn consume_frame_checksum(frame_handle: u64) -> u64 {
    let frame = own_frame(frame_handle);
    b::frame_checksum(&frame)
}

/// B's derived async operation over a borrowed A: capture a frame and
/// resolve with its checksum (C# `Task<ulong>`).
#[export]
pub async fn capture_checksum(source_handle: u64) -> u64 {
    let fut = with_source(source_handle, |x| x.capture());
    let frame = fut.await;
    b::frame_checksum(&frame)
}
