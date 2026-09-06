//! The event bus: one application-defined event type, published by anyone, seen by every
//! subscriber, in order.
//!
//! This is assessment decision 6 -- "the command tree is the event bus, make it so on purpose".
//! In the predecessor C framework, UI events were routed through the CLI's line queue and the
//! boot command was a baked string; in the spike, modules reached each other through
//! per-consumer static mailboxes that had to know their consumer. Here the console parses a line into an event and publishes
//! it, a touch driver publishes the same way, a test injects events directly, and a module
//! subscribes and matches on what it cares about.
//!
//! Bounded, with a stated policy: the ring holds `N` events; an event that no subscriber has
//! consumed yet stays; when the ring is full because the slowest subscriber has not caught up,
//! `publish` refuses the new event -- returning it, and counting the refusal -- rather than
//! blocking or overwriting what someone has not seen. Static-constructible, safe across cores
//! through the port's critical section.

use core::cell::RefCell;
use portable_atomic::{AtomicU32, Ordering};
use critical_section::Mutex;
use heapless::Deque;

/// A subscriber's handle. Not `Copy`: one handle, one cursor.
pub struct Subscription {
        slot: usize,
}

struct Inner<E: Copy, const N: usize, const S: usize> {
        ring: Deque<(u32, E), N>,
        next_seq: u32,
        /// Per slot: the sequence number the subscriber will read next, or `None` when free.
        cursors: [Option<u32>; S],
}

impl<E: Copy, const N: usize, const S: usize> Inner<E, N, S> {
        /// Drop events every live subscriber has consumed.
        fn trim(&mut self) {
                let Some(slowest) = self.cursors.iter().flatten().min().copied() else {
                        self.ring.clear();
                        return;
                };
                while self.ring.front().is_some_and(|(seq, _)| *seq < slowest) {
                        self.ring.pop_front();
                }
        }
}

pub struct EventBus<E: Copy, const N: usize, const S: usize> {
        inner: Mutex<RefCell<Inner<E, N, S>>>,
        refused: AtomicU32,
}

impl<E: Copy, const N: usize, const S: usize> Default for EventBus<E, N, S> {
        fn default() -> Self {
                Self::new()
        }
}

impl<E: Copy, const N: usize, const S: usize> EventBus<E, N, S> {
        pub const fn new() -> Self {
                Self {
                        inner: Mutex::new(RefCell::new(Inner { ring: Deque::new(), next_seq: 0, cursors: [None; S] })),
                        refused: AtomicU32::new(0),
                }
        }

        /// Take a subscriber slot. Sees events published from now on. `None` when all `S`
        /// slots are taken -- a configuration error, reported rather than silently ignored.
        pub fn subscribe(&self) -> Option<Subscription> {
                critical_section::with(|cs| {
                        let mut inner = self.inner.borrow_ref_mut(cs);
                        let slot = inner.cursors.iter().position(|c| c.is_none())?;
                        inner.cursors[slot] = Some(inner.next_seq);
                        Some(Subscription { slot })
                })
        }

        pub fn unsubscribe(&self, sub: Subscription) {
                critical_section::with(|cs| {
                        let mut inner = self.inner.borrow_ref_mut(cs);
                        inner.cursors[sub.slot] = None;
                        inner.trim();
                });
        }

        /// Publish to every subscriber. With no subscribers the event is dropped silently --
        /// there is nobody to keep it for. On a full ring it comes back in the `Err`.
        pub fn publish(&self, event: E) -> Result<(), E> {
                let r = critical_section::with(|cs| {
                        let mut inner = self.inner.borrow_ref_mut(cs);
                        if inner.cursors.iter().all(|c| c.is_none()) {
                                return Ok(());
                        }
                        inner.trim();
                        let seq = inner.next_seq;
                        inner.ring.push_back((seq, event)).map_err(|(_, e)| e)?;
                        inner.next_seq = inner.next_seq.wrapping_add(1);
                        Ok(())
                });
                if r.is_err() {
                        self.refused.fetch_add(1, Ordering::Relaxed);
                }
                r
        }

        /// The subscriber's next unseen event, if any.
        pub fn poll(&self, sub: &Subscription) -> Option<E> {
                critical_section::with(|cs| {
                        let mut inner = self.inner.borrow_ref_mut(cs);
                        let cursor = inner.cursors[sub.slot]?;
                        let front_seq = inner.ring.front()?.0;
                        // the ring holds consecutive sequence numbers, so the entry is at a
                        // fixed offset from the front; a cursor before the front cannot happen
                        // because trim never drops past the slowest cursor
                        let index = cursor.wrapping_sub(front_seq) as usize;
                        let (_, event) = *inner.ring.iter().nth(index)?;
                        inner.cursors[sub.slot] = Some(cursor.wrapping_add(1));
                        Some(event)
                })
        }

        /// Events refused because the ring was full, since construction.
        pub fn refused(&self) -> u32 {
                self.refused.load(Ordering::Relaxed)
        }

        /// How many events are waiting for the slowest subscriber.
        pub fn backlog(&self) -> usize {
                critical_section::with(|cs| {
                        let mut inner = self.inner.borrow_ref_mut(cs);
                        inner.trim();
                        inner.ring.len()
                })
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use std::vec::Vec;

        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        enum Ev {
                A(u32),
                B,
        }

        fn drain(bus: &EventBus<Ev, 4, 2>, sub: &Subscription) -> Vec<Ev> {
                let mut v = Vec::new();
                while let Some(e) = bus.poll(sub) {
                        v.push(e);
                }
                v
        }

        #[test]
        fn every_subscriber_sees_every_event_in_order() {
                let bus: EventBus<Ev, 4, 2> = EventBus::new();
                let s1 = bus.subscribe().unwrap();
                let s2 = bus.subscribe().unwrap();
                assert!(bus.subscribe().is_none(), "two slots, two subscribers");
                bus.publish(Ev::A(1)).unwrap();
                bus.publish(Ev::B).unwrap();
                assert_eq!(drain(&bus, &s1), [Ev::A(1), Ev::B]);
                assert_eq!(bus.backlog(), 2, "s2 has not read yet, so nothing is trimmed");
                assert_eq!(drain(&bus, &s2), [Ev::A(1), Ev::B]);
                assert_eq!(bus.backlog(), 0);
                assert_eq!(bus.poll(&s1), None);
        }

        #[test]
        fn a_slow_subscriber_holds_events_and_the_ring_refuses_when_full() {
                let bus: EventBus<Ev, 4, 2> = EventBus::new();
                let fast = bus.subscribe().unwrap();
                let slow = bus.subscribe().unwrap();
                for i in 0..4 {
                        bus.publish(Ev::A(i)).unwrap();
                        assert_eq!(bus.poll(&fast), Some(Ev::A(i)));
                }
                //   the fast reader has consumed everything, the slow one nothing: full
                assert_eq!(bus.publish(Ev::B), Err(Ev::B));
                assert_eq!(bus.refused(), 1);
                //   the slow reader takes one and there is room for one
                assert_eq!(bus.poll(&slow), Some(Ev::A(0)));
                assert_eq!(bus.publish(Ev::B), Ok(()));
                assert_eq!(drain(&bus, &slow), [Ev::A(1), Ev::A(2), Ev::A(3), Ev::B]);
                assert_eq!(drain(&bus, &fast), [Ev::B]);
        }

        #[test]
        fn a_new_subscriber_sees_only_the_future_and_leaving_frees_the_ring() {
                let bus: EventBus<Ev, 4, 2> = EventBus::new();
                let s1 = bus.subscribe().unwrap();
                bus.publish(Ev::A(1)).unwrap();
                let s2 = bus.subscribe().unwrap();
                bus.publish(Ev::A(2)).unwrap();
                assert_eq!(drain(&bus, &s2), [Ev::A(2)]);
                //   s1 never reads; unsubscribing it must not strand what it was holding
                assert_eq!(bus.backlog(), 2);
                bus.unsubscribe(s1);
                assert_eq!(bus.backlog(), 0);
                let s3 = bus.subscribe().unwrap();
                bus.publish(Ev::B).unwrap();
                assert_eq!(drain(&bus, &s3), [Ev::B]);
        }

        #[test]
        fn with_nobody_listening_events_are_dropped_not_refused() {
                let bus: EventBus<Ev, 2, 1> = EventBus::new();
                for _ in 0..10 {
                        assert_eq!(bus.publish(Ev::B), Ok(()));
                }
                assert_eq!(bus.refused(), 0);
                assert_eq!(bus.backlog(), 0);
        }

        #[test]
        fn works_as_a_static_across_threads() {
                static BUS: EventBus<Ev, 64, 2> = EventBus::new();
                let sub = BUS.subscribe().unwrap();
                let producers: Vec<_> = (0..4)
                        .map(|p| {
                                std::thread::spawn(move || {
                                        for i in 0..1000u32 {
                                                let _ = BUS.publish(Ev::A(p * 1000 + i));
                                        }
                                })
                        })
                        .collect();
                let mut got = 0u32;
                while producers.iter().any(|h| !h.is_finished()) {
                        while BUS.poll(&sub).is_some() {
                                got += 1;
                        }
                }
                for h in producers {
                        h.join().unwrap();
                }
                while BUS.poll(&sub).is_some() {
                        got += 1;
                }
                assert_eq!(got + BUS.refused(), 4000);
        }
}
