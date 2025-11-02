use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

use crate::lock::Lock;

pub struct Sender<T> {
    inner: Arc<Inner<T>>,
}

pub struct Receiver<T> {
    inner: Arc<Inner<T>>,
}

struct Inner<T> {
    complete: AtomicBool,
    data: Lock<Option<T>>,
    rx_waker: Lock<Option<Waker>>,
}

#[derive(Debug)]
pub struct RecvError;

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Inner::new());
    (
        Sender {
            inner: inner.clone(),
        },
        Receiver { inner },
    )
}

impl<T> Inner<T> {
    fn new() -> Self {
        Inner {
            complete: AtomicBool::new(false),
            data: Lock::new(None),
            rx_waker: Lock::new(None),
        }
    }

    fn send(&self, t: T) -> Result<(), T> {
        // check complete not true
        // true indicate receiver is droped
        if self.complete.load(Ordering::SeqCst) {
            return Err(t);
        }

        if let Some(mut slot) = self.data.try_lock() {
            assert!(slot.is_none());
            *slot = Some(t);
            drop(slot);
            if self.complete.load(Ordering::SeqCst)
                && let Some(mut slot) = self.data.try_lock()
                && let Some(data) = slot.take()
            {
                return Err(data);
            }

            Ok(())
        } else {
            Err(t)
        }
    }

    fn drop_tx(&self) {
        self.complete.store(true, Ordering::SeqCst);

        if let Some(mut slot) = self.rx_waker.try_lock()
            && let Some(waker) = slot.take()
        {
            drop(slot);
            waker.wake();
        }
    }

    fn try_recv(&self) -> Result<Option<T>, RecvError> {
        if self.complete.load(Ordering::SeqCst) {
            if let Some(mut slot) = self.data.try_lock() {
                match slot.take() {
                    Some(data) => Ok(Some(data)),
                    None => Err(RecvError),
                }
            } else {
                Err(RecvError)
            }
        } else {
            Ok(None)
        }
    }

    fn recv(&self, cx: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        let done = if self.complete.load(Ordering::SeqCst) {
            true
        } else {
            let waker = cx.waker().clone();
            match self.rx_waker.try_lock() {
                Some(mut slot) => {
                    *slot = Some(waker);
                    false
                }
                None => true,
            }
        };

        if done || self.complete.load(Ordering::SeqCst) {
            if let Some(mut slot) = self.data.try_lock() {
                if let Some(data) = slot.take() {
                    return Poll::Ready(Ok(data));
                } else {
                    return Poll::Ready(Err(RecvError));
                }
            }
            Poll::Ready(Err(RecvError))
        } else {
            Poll::Pending
        }
    }

    fn drop_rx(&self) {
        self.complete.store(true, Ordering::SeqCst);

        if let Some(mut slot) = self.rx_waker.try_lock()
            && let Some(waker) = slot.take()
        {
            drop(slot);
            drop(waker);
        }
    }
}

impl<T> Sender<T> {
    pub fn send(self, t: T) -> Result<(), T> {
        self.inner.send(t)
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.inner.drop_tx()
    }
}

unsafe impl<T: Send> Send for Sender<T> {}
unsafe impl<T: Sync> Sync for Sender<T> {}

impl<T> Unpin for Sender<T> {}

impl<T> Receiver<T> {
    pub fn try_recv(&self) -> Result<Option<T>, RecvError> {
        self.inner.try_recv()
    }
}

impl<T> Future for Receiver<T> {
    type Output = Result<T, RecvError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.inner.recv(cx)
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.inner.drop_rx()
    }
}

unsafe impl<T: Send> Send for Receiver<T> {}
unsafe impl<T: Sync> Sync for Receiver<T> {}

impl<T> Unpin for Receiver<T> {}
