//! Minimal runtime-agnostic oneshot: adapts a completion callback into a
//! `Future`. Shared by every backend (generic over the backend's Frame).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

struct State<T> {
    value: Option<T>,
    waker: Option<Waker>,
}

pub(crate) struct OneShot<T>(Arc<Mutex<State<T>>>);

impl<T> Future for OneShot<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let mut st = self.0.lock().unwrap();
        if let Some(v) = st.value.take() {
            Poll::Ready(v)
        } else {
            st.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

/// Run `register` with a completion function; the returned future resolves
/// when the completion function is called (from any thread).
pub(crate) fn via<T: Send + 'static>(
    register: impl FnOnce(Box<dyn FnOnce(T) + Send>),
) -> OneShot<T> {
    let shared = Arc::new(Mutex::new(State {
        value: None,
        waker: None,
    }));
    let complete = {
        let shared = shared.clone();
        Box::new(move |v: T| {
            let waker = {
                let mut st = shared.lock().unwrap();
                st.value = Some(v);
                st.waker.take()
            };
            if let Some(w) = waker {
                w.wake();
            }
        })
    };
    register(complete);
    OneShot(shared)
}
