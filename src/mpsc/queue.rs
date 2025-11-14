/* Copyright (c) 2010-2011 Dmitry Vyukov. All rights reserved.
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 *
 *    1. Redistributions of source code must retain the above copyright notice,
 *       this list of conditions and the following disclaimer.
 *
 *    2. Redistributions in binary form must reproduce the above copyright
 *       notice, this list of conditions and the following disclaimer in the
 *       documentation and/or other materials provided with the distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY DMITRY VYUKOV "AS IS" AND ANY EXPRESS OR IMPLIED
 * WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT
 * SHALL DMITRY VYUKOV OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT,
 * INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT
 * LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
 * PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
 * LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE
 * OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF
 * ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
 *
 * The views and conclusions contained in the software and documentation are
 * those of the authors and should not be interpreted as representing official
 * policies, either expressed or implied, of Dmitry Vyukov.
 */

//! A mostly lock-free multi-producer, single consumer queue for sending
//! messages between asynchronous tasks.
//!
//! The queue implementation is essentially the same one used for mpsc channels
//! in the standard library.
//!
//! Note that the current implementation of this queue has a caveat of the `pop`
//! method, and see the method for more information about it. Due to this
//! caveat, this queue may not be appropriate for all use-cases.

// http://www.1024cores.net/home/lock-free-algorithms
//                         /queues/non-intrusive-mpsc-node-based-queue

// NOTE: this implementation is lifted from the standard library and only
//       slightly modified

use std::{
    cell::UnsafeCell,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
    thread,
};

const MAX_SPIN_TIME: usize = 32;

struct Node<T> {
    next: AtomicPtr<Self>,
    value: Option<T>,
}

pub(super) struct Queue<T> {
    head: AtomicPtr<Node<T>>,
    tail: UnsafeCell<*mut Node<T>>,
}

enum PopResult<T> {
    Value(T),
    Empty,
    Inconsistent,
}

/*
 *
 * ===== impl Node =====
 *
 */

impl<T> Node<T> {
    unsafe fn new(value: Option<T>) -> *mut Self {
        Box::into_raw(Box::new(Self {
            next: AtomicPtr::new(ptr::null_mut()),
            value,
        }))
    }
}

/*
 *
 * ===== impl Queue =====
 *
 */

impl<T> Queue<T> {
    pub(super) fn new() -> Self {
        let stub_ptr = unsafe { Node::new(None) };
        Self {
            head: AtomicPtr::new(stub_ptr),
            tail: UnsafeCell::new(stub_ptr),
        }
    }

    pub(super) fn push(&self, value: T) {
        unsafe {
            let next_node = Node::new(Some(value));

            let prev_node = self.head.swap(next_node, Ordering::Acquire);
            (*prev_node).next.store(next_node, Ordering::Release);
        }
    }

    fn pop(&self) -> PopResult<T> {
        unsafe {
            let tail = *self.tail.get();
            let next = (*tail).next.load(Ordering::Acquire);

            if !next.is_null() {
                *self.tail.get() = next;
                let val = (*next).value.take().unwrap();
                drop(Box::from_raw(tail));
                return PopResult::Value(val);
            }

            if self.head.load(Ordering::Acquire) == tail {
                PopResult::Empty
            } else {
                PopResult::Inconsistent
            }
        }
    }

    pub(super) fn pop_spin(&self) -> Option<T> {
        let mut spin_times = 0;
        loop {
            match self.pop() {
                PopResult::Value(val) => return Some(val),
                PopResult::Empty => return None,
                PopResult::Inconsistent => {
                    if spin_times > MAX_SPIN_TIME {
                        thread::yield_now();
                    } else {
                        spin_times += 1;
                    }
                }
            }
        }
    }
}

impl<T> Drop for Queue<T> {
    fn drop(&mut self) {
        unsafe {
            let mut curr = *self.tail.get();
            while !curr.is_null() {
                let next = (*curr).next.load(Ordering::Relaxed);
                drop(Box::from_raw(curr));
                curr = next;
            }
        }
    }
}

unsafe impl<T: Send> Send for Queue<T> {}
unsafe impl<T: Sync> Sync for Queue<T> {}
