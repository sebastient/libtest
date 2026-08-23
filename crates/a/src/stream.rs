//! An async iterator over a subscription — the pull-shaped view of the
//! push-shaped callback API.
//!
//! Written once and compiled into all three backends: it is built purely on
//! `A::subscribe_with`, whose signature is identical in the static, dynamic
//! and vtable builds, so the whole file is backend-agnostic by construction.
//!
//! Dependency-free on purpose. `futures_core::Stream` would be the
//! conventional trait to implement, but the whole point of this codebase's
//! async story is that it composes with any executor without pulling one in
//! — so the surface is an inherent `async fn next()`, which is what
//! `Stream` is for anyway. Implementing the trait on top is a ten-line
//! adapter in whichever crate already depends on `futures-core`.

use crate::{Frame, A};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// Queue shared between the producer thread (pushing) and the consumer's
/// executor (popping).
struct Shared {
    queue: Mutex<VecDeque<Frame>>,
    waker: Mutex<Option<Waker>>,
    dropped: AtomicU64,
    capacity: usize,
}

impl Shared {
    fn push(&self, frame: Frame) {
        {
            let mut q = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // Drop-oldest at capacity: an async consumer that has fallen
            // behind wants the newest frames, and an unbounded queue would
            // turn a slow consumer into a memory leak.
            while q.len() >= self.capacity {
                q.pop_front();
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            q.push_back(frame);
        }
        // Wake AFTER releasing the queue lock: the woken task will want it.
        let waker = self
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        if let Some(w) = waker {
            w.wake();
        }
    }
}

/// Async iterator over frames delivered to one subscription.
///
/// Lifetime note worth stating plainly: this does NOT unsubscribe when
/// dropped. It cannot — the subscription lives in an `A` that this value
/// has no reference to (`A` is a plain owned object, not internally
/// refcounted), and a `'static` stream holding a borrow of it would not
/// compile. So the id is exposed and teardown is the caller's, exactly as
/// it is for a raw subscription. Dropping the stream without
/// unsubscribing is safe — deliveries just accumulate into a bounded queue
/// nobody reads — but the subscription lives until `A` is dropped.
///
/// Making drop-unsubscribe work would mean restructuring `A` to be
/// internally `Arc`-based so a stream could hold a weak handle. That is a
/// real design option, not an oversight; it trades the current
/// zero-overhead ownership model for convenience at this one surface.
pub struct FrameStream {
    shared: Arc<Shared>,
    id: u64,
}

impl FrameStream {
    /// The subscription id, for `A::unsubscribe`.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Frames discarded because this stream's queue was full.
    pub fn dropped(&self) -> u64 {
        self.shared.dropped.load(Ordering::Relaxed)
    }

    /// Frames waiting to be consumed right now.
    pub fn pending(&self) -> usize {
        self.shared
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// Await the next frame.
    ///
    /// Named `next` deliberately: this is an async iterator, and `next` is
    /// what every async-iteration protocol calls it (`Stream::poll_next`,
    /// Python's `__anext__`). It cannot collide with `Iterator::next` —
    /// this type is not an `Iterator`, and could not be: the whole point is
    /// that producing the next item requires awaiting.
    ///
    /// Never returns `None`: a subscription has no end-of-stream, it simply
    /// stops producing. `Option` is kept in the signature because that is
    /// the shape every async-iterator adapter expects, and a future version
    /// that models teardown as end-of-stream would use it.
    #[expect(
        clippy::should_implement_trait,
        reason = "async iterator, not Iterator"
    )]
    pub fn next(&mut self) -> impl Future<Output = Option<Frame>> + '_ {
        Next {
            shared: &self.shared,
        }
    }
}

struct Next<'a> {
    shared: &'a Arc<Shared>,
}

impl Future for Next<'_> {
    type Output = Option<Frame>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Frame>> {
        // Register the waker BEFORE the final queue check. The other order
        // races: a frame arriving between the check and the registration
        // would wake nobody and the task would sleep with work waiting.
        *self
            .shared
            .waker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cx.waker().clone());
        let frame = self
            .shared
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pop_front();
        match frame {
            Some(f) => Poll::Ready(Some(f)),
            None => Poll::Pending,
        }
    }
}

impl A {
    /// Subscribe and expose the deliveries as an async iterator.
    ///
    /// `capacity` bounds the queue; at capacity the oldest frame is dropped
    /// (counted by [`FrameStream::dropped`]). The underlying subscription
    /// uses blocking delivery, so the producer thread's only work per frame
    /// is a push onto this queue — the bound lives here rather than in the
    /// subscription's own pump, which would mean two queues in series.
    pub fn frames(&mut self, capacity: usize) -> FrameStream {
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            waker: Mutex::new(None),
            dropped: AtomicU64::new(0),
            capacity: capacity.max(1),
        });
        let sink = shared.clone();
        let id = self.subscribe_with(move |frame| sink.push(frame), a_abi::A_DELIVERY_BLOCKING);
        FrameStream { shared, id }
    }
}
