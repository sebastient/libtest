//! Runtime helpers shared by A's cdylibs — see Cargo.toml for why this is a
//! standalone crate rather than part of `a-cshim`.
//!
//! Each Rust cdylib links its own libstd, so the panic hook installed here
//! is scoped to the module that calls `install_hook`. That is not a
//! limitation to work around; it is the same fact as "each cdylib carries
//! its own allocator state", and it means a process hosting several of
//! these modules gets several independent hooks, each quieting only its own
//! panics. Verified: with `a-py` and `b-py` loaded together, a panic raised
//! inside `b-py` is printed by libstd's default hook until `b-py` installs
//! its own — `a-py`'s makes no difference to it.

/// Error detail retrieval and the quiet panic hook.
///
/// A status code is the whole ABI contract, but a bare `-1` is a poor
/// debugging experience. The C convention for adding detail without adding
/// ABI surface is a thread-local last-error string (`dlerror`,
/// `strerror_r`, `SSL_get_error`): the status stays the contract, the
/// message is advisory and never parsed.
pub mod err {
    use core::ffi::c_char;
    use std::cell::RefCell;
    use std::ffi::CString;
    use std::sync::Once;

    thread_local! {
        /// Owned by the thread that set it; the pointer handed to C stays
        /// valid until this thread's NEXT failing call. Per-thread storage
        /// is what makes handing out the pointer safe at all — a global
        /// would be a data race the moment two threads failed at once.
        static LAST: RefCell<Option<CString>> = const { RefCell::new(None) };
    }

    /// Record advisory detail for the calling thread's most recent failure.
    pub fn set(msg: &str) {
        // A NUL inside the message would truncate it silently; say so.
        let c =
            CString::new(msg).unwrap_or_else(|_| c"error message contained an interior NUL".into());
        LAST.with(|slot| *slot.borrow_mut() = Some(c));
    }

    /// The calling thread's last-failure detail, or null if it has not
    /// failed. Valid until this thread's next failing call.
    pub fn last() -> *const c_char {
        LAST.with(|slot| {
            slot.borrow()
                .as_ref()
                .map_or(core::ptr::null(), |c| c.as_ptr())
        })
    }

    static INSTALL: Once = Once::new();

    /// Replace this module's default panic hook, which prints a backtrace
    /// to stderr — hostile in a library, where a shielded panic is an
    /// ordinary error return the host may handle silently. The message is
    /// captured into the thread-local instead, so nothing is lost.
    ///
    /// Two caveats worth naming. `set_hook` is global **to the cdylib that
    /// calls it** (see the crate docs): every module wanting quiet panics
    /// installs its own. And within that module it is process-wide, so a
    /// library imposing one on its host is rude — hence this runs lazily on
    /// first use rather than at load time, chains to the previous hook when
    /// `A_PANIC_VERBOSE` is set so debugging still works, and is idempotent.
    pub fn install_hook() {
        INSTALL.call_once(|| {
            let verbose = std::env::var_os("A_PANIC_VERBOSE").is_some();
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let payload = info.payload();
                let text = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("panic with a non-string payload");
                match info.location() {
                    Some(l) => set(&format!("panic at {}:{}: {text}", l.file(), l.line())),
                    None => set(&format!("panic: {text}")),
                }
                if verbose {
                    previous(info);
                }
            }));
        });
    }
}
