use crate::{mpsc::queue::Queue, oneshot::RecvError};
use futures::{Stream, StreamExt, task::AtomicWaker};

use std::{
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

mod queue;

struct BoundedSenderInner<T> {
    inner: Arc<BoundedInner<T>>,
    sender_task: Arc<Mutex<SenderTask>>,
    maybe_parked: bool,
}

pub struct Sender<T>(Option<BoundedSenderInner<T>>);

pub struct SendFut<'a, T> {
    sender: &'a mut Sender<T>,
    msg: Option<T>,
}

pub struct Receiver<T> {
    inner: Option<Arc<BoundedInner<T>>>,
}

pub struct Recv<'a, St: ?Sized> {
    stream: &'a mut St,
}

struct BoundedInner<T> {
    buffer: usize,
    state: AtomicUsize,
    queue: Queue<T>,
    parked_queue: Queue<Arc<Mutex<SenderTask>>>,
    num_senders: AtomicUsize,
    recv_task: AtomicWaker,
}

struct SenderTask {
    waker: Option<Waker>,
    is_parked: bool,
}

// indicate channel is open
const OPEN_MASK: usize = usize::MAX - (usize::MAX >> 1);

// init value
const INIT_STATE: usize = OPEN_MASK;

// indicate number of messages
const CAPACITY_MASK: usize = !(OPEN_MASK);

#[derive(Debug)]
struct State {
    is_open: bool,
    num_msg: usize,
}

#[derive(Debug)]
pub struct TrySendError<T> {
    pub kind: SendErrorKind,
    pub value: T,
}

#[derive(Debug)]
pub struct SendError {
    pub kind: SendErrorKind,
}

#[derive(Debug)]
pub enum SendErrorKind {
    Full,
    Closed,
    Disconnected,
}

#[derive(Debug)]
pub enum TryRecvError {
    Closed,
    Emptiy,
}

pub fn channel<T>(buffer: usize) -> (Sender<T>, Receiver<T>) {
    assert!(buffer > 0);

    let inner = Arc::new(BoundedInner {
        buffer,
        state: AtomicUsize::new(INIT_STATE),
        queue: Queue::new(),
        parked_queue: Queue::new(),
        num_senders: AtomicUsize::new(1),
        recv_task: AtomicWaker::new(),
    });

    let sender_inner = BoundedSenderInner {
        inner: inner.clone(),
        sender_task: Arc::new(Mutex::new(SenderTask::new())),
        maybe_parked: false,
    };

    (
        Sender(Some(sender_inner)),
        Receiver {
            inner: Some(inner.clone()),
        },
    )
}

/*
 *
 * ===== impl Sender =====
 *
 */

impl<T> BoundedSenderInner<T> {
    fn try_send(&mut self, msg: T) -> Result<(), TrySendError<T>> {
        match self.poll_unparked(None) {
            Poll::Ready(()) => {}
            Poll::Pending => {
                return Err(TrySendError {
                    kind: SendErrorKind::Full,
                    value: msg,
                });
            }
        }
        self.send_b(msg)
    }

    fn send_b(&mut self, msg: T) -> Result<(), TrySendError<T>> {
        let should_park = match self.inc_num_msg() {
            Some(num) => num > self.inner.buffer,
            None => {
                return Err(TrySendError {
                    kind: SendErrorKind::Closed,
                    value: msg,
                });
            }
        };

        if should_park {
            self.park();
        }

        self.send_queue_and_wake(msg);
        Ok(())
    }

    fn inc_num_msg(&mut self) -> Option<usize> {
        let mut curr = self.inner.state.load(Ordering::SeqCst);

        loop {
            let mut state = decode_state(curr);

            if !state.is_open {
                return None;
            }

            state.num_msg += 1;
            let next_state = encode_state(&state);

            match self.inner.state.compare_exchange(
                curr,
                next_state,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return Some(state.num_msg),
                Err(actual_val) => curr = actual_val,
            }
        }
    }

    fn send_queue_and_wake(&mut self, msg: T) {
        self.inner.queue.push(msg);

        self.inner.recv_task.wake();
    }

    fn park(&mut self) {
        {
            let mut sender_task = self.sender_task.lock().unwrap();
            sender_task.is_parked = true;
            sender_task.waker = None;
        }

        let task = self.sender_task.clone();
        self.inner.parked_queue.push(task);

        let state = decode_state(self.inner.state.load(Ordering::SeqCst));
        self.maybe_parked = state.is_open;
    }

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), SendError>> {
        // check not receiver closed
        let state = decode_state(self.inner.state.load(Ordering::SeqCst));
        if !state.is_open {
            return Poll::Ready(Err(SendError {
                kind: SendErrorKind::Closed,
            }));
        }

        self.poll_unparked(Some(cx)).map(Ok)
    }

    fn poll_unparked(&mut self, cx: Option<&mut Context<'_>>) -> Poll<()> {
        if !self.maybe_parked {
            return Poll::Ready(());
        }

        // maybe_parked true case
        let mut sender_task = self.sender_task.lock().unwrap();
        if sender_task.is_parked {
            sender_task.waker = cx.map(|cx| cx.waker().clone());
            Poll::Pending
        } else {
            self.maybe_parked = false;
            Poll::Ready(())
        }
    }

    fn close_channel(&self) {
        self.inner.close();
        self.inner.recv_task.wake();
    }
}

impl<T> Sender<T> {
    pub fn send(&mut self, msg: T) -> SendFut<'_, T> {
        SendFut {
            sender: self,
            msg: Some(msg),
        }
    }

    pub fn try_send(&mut self, msg: T) -> Result<(), TrySendError<T>> {
        match &mut self.0 {
            Some(inner) => inner.try_send(msg),
            None => Err(TrySendError {
                kind: SendErrorKind::Disconnected,
                value: msg,
            }),
        }
    }

    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), SendError>> {
        match &mut self.0 {
            Some(inner) => inner.poll_ready(cx),
            None => Poll::Ready(Err(SendError {
                kind: SendErrorKind::Disconnected,
            })),
        }
    }

    pub fn start_send(&mut self, msg: T) -> Result<(), SendError> {
        self.try_send(msg).map_err(|e| SendError { kind: e.kind })
    }

    pub fn num_senders(&self) -> usize {
        match self.0.as_ref() {
            Some(inner) => inner.inner.num_senders.load(Ordering::SeqCst),
            None => 0,
        }
    }

    pub fn channel_close(&self) {
        if let Some(inner) = &self.0 {
            inner.close_channel();
        }
    }

    pub fn disconnect(&mut self) {
        if self.0.is_some() {
            self.0 = None;
        }
    }
}

impl<T> Clone for BoundedSenderInner<T> {
    fn clone(&self) -> Self {
        let mut curr = self.inner.num_senders.load(Ordering::SeqCst);

        loop {
            let next = curr + 1;
            match self.inner.num_senders.compare_exchange(
                curr,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    return Self {
                        inner: self.inner.clone(),
                        sender_task: Arc::new(Mutex::new(SenderTask::new())),
                        maybe_parked: false,
                    };
                }
                Err(actual) => curr = actual,
            }
        }
    }
}

impl<T> Drop for BoundedSenderInner<T> {
    fn drop(&mut self) {
        let prev = self.inner.num_senders.fetch_sub(1, Ordering::SeqCst);

        if prev == 1 {
            self.close_channel();
        }
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Future for SendFut<'_, T>
where
    T: Send + Unpin,
{
    type Output = Result<(), SendError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        match this.sender.poll_ready(cx) {
            Poll::Ready(Ok(())) => {
                if let Some(v) = this.msg.take() {
                    let res = this.sender.start_send(v);
                    Poll::Ready(res.map_err(|v| SendError { kind: v.kind }))
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(SendError { kind: e.kind })),
            Poll::Pending => Poll::Pending,
        }
    }
}

/*
 *
 * ===== impl Receiver =====
 *
 */

impl<T> Receiver<T> {
    pub fn recv(&mut self) -> Recv<'_, Self> {
        Recv { stream: self }
    }

    pub fn try_recv(&mut self) -> Result<T, TryRecvError> {
        match self.next_message() {
            Poll::Ready(Some(v)) => Ok(v),
            Poll::Ready(None) => Err(TryRecvError::Closed),
            Poll::Pending => Err(TryRecvError::Emptiy),
        }
    }

    fn next_message(&mut self) -> Poll<Option<T>> {
        // check no close
        let inner = match self.inner.as_mut() {
            Some(inner) => inner,
            None => return Poll::Ready(None),
        };
        // pop data
        match inner.queue.pop_spin() {
            Some(v) => {
                self.unpark_one();
                self.dec_num_msg();
                Poll::Ready(Some(v))
            }
            None => {
                let state = decode_state(inner.state.load(Ordering::SeqCst));
                if !state.is_open {
                    self.inner = None;
                    return Poll::Ready(None);
                }
                Poll::Pending
            }
        }
    }

    fn unpark_one(&self) {
        if let Some(inner) = self.inner.as_ref() {
            #[warn(clippy::collapsible_if)]
            if let Some(task) = inner.parked_queue.pop_spin() {
                task.lock().unwrap().notify()
            }
        }
    }

    fn dec_num_msg(&self) {
        if let Some(inner) = &self.inner {
            inner.state.fetch_sub(1, Ordering::SeqCst);
        }
    }

    pub fn close(&self) {
        if let Some(inner) = self.inner.as_ref() {
            inner.close();

            while let Some(task) = inner.parked_queue.pop_spin() {
                task.lock().unwrap().notify();
            }
        }
    }
}

impl<T> Stream for Receiver<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.next_message() {
            Poll::Ready(msg) => {
                if msg.is_none() {
                    self.inner = None;
                }
                Poll::Ready(msg)
            }
            Poll::Pending => {
                self.inner.as_ref().unwrap().recv_task.register(cx.waker());
                Poll::Pending
            }
        }
    }
}

impl<St> Future for Recv<'_, St>
where
    St: Stream + ?Sized + Unpin,
{
    type Output = Result<St::Item, RecvError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Box::pin(&mut self.stream).poll_next_unpin(cx) {
            Poll::Ready(Some(msg)) => Poll::Ready(Ok(msg)),
            Poll::Ready(None) => Poll::Ready(Err(RecvError)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/*
 *
 * ===== impl BoundedInner =====
 *
 */

impl<T> BoundedInner<T> {
    fn close(&self) {
        let curr = self.state.load(Ordering::SeqCst);
        if !decode_state(curr).is_open {
            return;
        }

        self.state.fetch_and(!OPEN_MASK, Ordering::SeqCst);
    }
}

unsafe impl<T: Send> Send for BoundedInner<T> {}
unsafe impl<T: Sync> Sync for BoundedInner<T> {}

/*
 *
 * ===== impl SenderTask =====
 *
 */

impl SenderTask {
    fn new() -> Self {
        Self {
            is_parked: false,
            waker: None,
        }
    }

    fn notify(&mut self) {
        self.is_parked = false;
        if let Some(waker) = self.waker.take() {
            waker.wake();
        }
    }
}

/*
 *
 * ===== helpers =====
 *
 */

fn encode_state(state: &State) -> usize {
    let mut num = state.num_msg;

    if state.is_open {
        num |= OPEN_MASK;
    }
    num
}

fn decode_state(num: usize) -> State {
    State {
        is_open: num & OPEN_MASK == OPEN_MASK,
        num_msg: num & CAPACITY_MASK,
    }
}
