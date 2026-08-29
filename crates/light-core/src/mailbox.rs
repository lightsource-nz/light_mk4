//! A bounded, lock-protected queue of plain values, for one module to hand work to another.
//!
//! Modules are owned by the runtime and cannot reference each other, so anything crossing
//! between them goes through a static like this. It is the seed of the typed event bus the
//! assessment calls for: the same drop-and-count policy as the log queue, safe across cores
//! through the port's critical section, and `const`-constructible so it can be a `static`.

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};
use critical_section::Mutex;
use heapless::Deque;

pub struct Mailbox<E: Copy, const N: usize> {
        queue: Mutex<RefCell<Deque<E, N>>>,
        dropped: AtomicU32,
}

impl<E: Copy, const N: usize> Mailbox<E, N> {
        pub const fn new() -> Self {
                Self { queue: Mutex::new(RefCell::new(Deque::new())), dropped: AtomicU32::new(0) }
        }

        /// Queue an event. On a full mailbox the event comes back in the `Err`, and the drop is
        /// counted -- the producer decides whether that matters; the mailbox never blocks.
        pub fn push(&self, event: E) -> Result<(), E> {
                let r = critical_section::with(|cs| self.queue.borrow_ref_mut(cs).push_back(event));
                if r.is_err() {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                r
        }

        pub fn pop(&self) -> Option<E> {
                critical_section::with(|cs| self.queue.borrow_ref_mut(cs).pop_front())
        }

        pub fn len(&self) -> usize {
                critical_section::with(|cs| self.queue.borrow_ref(cs).len())
        }

        pub fn is_empty(&self) -> bool {
                self.len() == 0
        }

        /// Events refused since construction.
        pub fn dropped(&self) -> u32 {
                self.dropped.load(Ordering::Relaxed)
        }
}

impl<E: Copy, const N: usize> Default for Mailbox<E, N> {
        fn default() -> Self {
                Self::new()
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;

        #[test]
        fn fifo_with_a_counted_drop_on_full() {
                let m: Mailbox<u8, 2> = Mailbox::new();
                assert_eq!(m.push(1), Ok(()));
                assert_eq!(m.push(2), Ok(()));
                assert_eq!(m.push(3), Err(3), "the refused event comes back");
                assert_eq!(m.dropped(), 1);
                assert_eq!(m.pop(), Some(1));
                assert_eq!(m.pop(), Some(2));
                assert_eq!(m.pop(), None);
        }

        #[test]
        fn works_as_a_static_across_threads() {
                static M: Mailbox<u32, 64> = Mailbox::new();
                let producers: std::vec::Vec<_> = (0..4)
                        .map(|p| {
                                std::thread::spawn(move || {
                                        for i in 0..1000u32 {
                                                let _ = M.push(p * 1000 + i);
                                        }
                                })
                        })
                        .collect();
                let mut got = 0u32;
                while producers.iter().any(|h| !h.is_finished()) {
                        while M.pop().is_some() {
                                got += 1;
                        }
                }
                for h in producers {
                        h.join().unwrap();
                }
                while M.pop().is_some() {
                        got += 1;
                }
                assert_eq!(got + M.dropped(), 4000);
        }
}
