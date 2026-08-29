//! Logging: a bounded queue of records that never blocks the producer.
//!
//! mk3's message queue could deadlock the whole application when it filled on a single-core
//! path, and its consumers spent real effort keeping it from filling. This one has a stated
//! policy instead: when the queue is full the record is DROPPED and counted, and the next drain
//! reports how many were lost. A full log costs a lost line, never a stalled main loop.
//!
//! Records are formatted at the producer into a fixed buffer, so a record has a known size and
//! the queue has a known footprint (`DEPTH * size_of::<Record>()`, no heap). Deferred formatting
//! (`defmt`-style ids) can replace that later without changing the queue's contract.

use core::cell::RefCell;
use core::fmt::{self, Write};
use critical_section::Mutex;
use heapless::{Deque, String};

pub const TEXT_CAPACITY: usize = 96;
pub const DEPTH: usize = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
        Error,
        Warn,
        Info,
        Debug,
        Trace,
}

impl Level {
        pub fn as_str(self) -> &'static str {
                match self {
                        Level::Error => "error",
                        Level::Warn => "warn",
                        Level::Info => "info",
                        Level::Debug => "debug",
                        Level::Trace => "trace",
                }
        }
}

#[derive(Clone, Debug)]
pub struct Record {
        pub level: Level,
        pub ts_us: u64,
        /// The producing crate or module, from `module_path!()`.
        pub target: &'static str,
        pub text: String<TEXT_CAPACITY>,
}

impl fmt::Display for Record {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let ms = self.ts_us / 1000;
                write!(f, "[{:6}.{:03}] {:5} {}: {}", ms / 1000, ms % 1000, self.level.as_str(), self.target, self.text)
        }
}

struct State {
        queue: Deque<Record, DEPTH>,
        /// Records dropped since the last drain reported them.
        dropped: u32,
        clock: Option<fn() -> u64>,
        /// Records above this level are discarded at the producer, before formatting.
        max_level: Level,
}

static STATE: Mutex<RefCell<State>> = Mutex::new(RefCell::new(State {
        queue: Deque::new(),
        dropped: 0,
        clock: None,
        max_level: Level::Info,
}));

/// A `fmt::Write` that truncates instead of failing, so an over-long message loses its tail
/// rather than the whole record.
struct Truncating<'a, const N: usize>(&'a mut String<N>);

impl<const N: usize> Write for Truncating<'_, N> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
                let room = N - self.0.len();
                if s.len() <= room {
                        let _ = self.0.push_str(s);
                } else {
                        //   cut on a char boundary; a mid-char cut would make the String invalid
                        let mut cut = room;
                        while cut > 0 && !s.is_char_boundary(cut) {
                                cut -= 1;
                        }
                        let _ = self.0.push_str(&s[..cut]);
                }
                Ok(())
        }
}

/// Install the clock records are stamped with. Before this, timestamps are zero.
pub fn set_clock(clock: fn() -> u64) {
        critical_section::with(|cs| STATE.borrow_ref_mut(cs).clock = Some(clock));
}

pub fn set_max_level(level: Level) {
        critical_section::with(|cs| STATE.borrow_ref_mut(cs).max_level = level);
}

/// Whether a record at this level would be kept. Lets the macros skip formatting entirely.
pub fn enabled(level: Level) -> bool {
        critical_section::with(|cs| level <= STATE.borrow_ref(cs).max_level)
}

/// Queue a record. Never blocks; on a full queue the record is dropped and counted.
pub fn push(level: Level, target: &'static str, args: fmt::Arguments<'_>) {
        //   format OUTSIDE the critical section: formatting is the slow part, and holding the
        // lock through it would make every other producer -- including the other core -- wait
        // on this one's printf
        let mut text = String::new();
        let _ = Truncating(&mut text).write_fmt(args);
        critical_section::with(|cs| {
                let mut st = STATE.borrow_ref_mut(cs);
                if level > st.max_level {
                        return;
                }
                let ts_us = st.clock.map_or(0, |c| c());
                let record = Record { level, ts_us, target, text };
                if st.queue.push_back(record).is_err() {
                        st.dropped = st.dropped.saturating_add(1);
                }
        });
}

/// Take up to `max` records to `sink`, in order. If records were dropped since the last drain,
/// one synthesized record saying how many comes first. Returns how many the sink received.
pub fn drain(max: usize, mut sink: impl FnMut(&Record)) -> usize {
        let mut delivered = 0;
        while delivered < max {
                //   one record per critical section, so a slow sink (a USB CDC write) never
                // holds the lock against producers
                let next = critical_section::with(|cs| {
                        let mut st = STATE.borrow_ref_mut(cs);
                        if st.dropped > 0 {
                                let n = st.dropped;
                                st.dropped = 0;
                                let ts_us = st.clock.map_or(0, |c| c());
                                let mut text = String::new();
                                let _ = write!(Truncating(&mut text), "dropped {n} log records");
                                return Some(Record { level: Level::Warn, ts_us, target: "light_core::log", text });
                        }
                        st.queue.pop_front()
                });
                match next {
                        Some(r) => {
                                sink(&r);
                                delivered += 1;
                        }
                        None => break,
                }
        }
        delivered
}

/// How many records are waiting.
pub fn pending() -> usize {
        critical_section::with(|cs| STATE.borrow_ref(cs).queue.len())
}

#[macro_export]
macro_rules! log {
        ($level:expr, $($arg:tt)*) => {
                if $crate::log::enabled($level) {
                        $crate::log::push($level, module_path!(), format_args!($($arg)*));
                }
        };
}
#[macro_export]
macro_rules! error { ($($arg:tt)*) => { $crate::log!($crate::log::Level::Error, $($arg)*) }; }
#[macro_export]
macro_rules! warn { ($($arg:tt)*) => { $crate::log!($crate::log::Level::Warn, $($arg)*) }; }
#[macro_export]
macro_rules! info { ($($arg:tt)*) => { $crate::log!($crate::log::Level::Info, $($arg)*) }; }
#[macro_export]
macro_rules! debug { ($($arg:tt)*) => { $crate::log!($crate::log::Level::Debug, $($arg)*) }; }
#[macro_export]
macro_rules! trace { ($($arg:tt)*) => { $crate::log!($crate::log::Level::Trace, $($arg)*) }; }

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use std::sync::Mutex as StdMutex;
        use std::vec::Vec;

        //   one global queue, so tests that touch it must not interleave. serialised here
        // rather than with --test-threads=1, which nothing would remember to pass. a test
        // that panics poisons the lock; the others take it anyway, or one failure reads as four
        static SERIAL: StdMutex<()> = StdMutex::new(());

        fn reset() {
                critical_section::with(|cs| {
                        let mut st = STATE.borrow_ref_mut(cs);
                        st.queue.clear();
                        st.dropped = 0;
                        st.clock = None;
                        st.max_level = Level::Info;
                });
        }

        fn drain_all() -> Vec<Record> {
                let mut out = Vec::new();
                while drain(usize::MAX, |r| out.push(r.clone())) > 0 {}
                out
        }

        #[test]
        fn records_come_out_in_order_with_their_fields() {
                let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
                reset();
                set_clock(|| 1_234_567);
                crate::info!("first {}", 1);
                crate::warn!("second");
                let got = drain_all();
                assert_eq!(got.len(), 2);
                assert_eq!(got[0].text.as_str(), "first 1");
                assert_eq!(got[0].level, Level::Info);
                assert_eq!(got[0].ts_us, 1_234_567);
                assert_eq!(got[0].target, module_path!());
                assert_eq!(got[1].text.as_str(), "second");
                assert_eq!(std::format!("{}", got[0]), std::format!("[     1.234] info  {}: first 1", module_path!()));
        }

        #[test]
        fn a_full_queue_drops_and_reports_rather_than_blocking() {
                let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
                reset();
                for i in 0..(DEPTH + 5) {
                        crate::info!("record {i}");
                }
                assert_eq!(pending(), DEPTH);
                let got = drain_all();
                //   the drop notice leads, then exactly DEPTH real records, the earliest kept
                assert_eq!(got.len(), DEPTH + 1);
                assert_eq!(got[0].level, Level::Warn);
                assert_eq!(got[0].text.as_str(), "dropped 5 log records");
                assert_eq!(got[1].text.as_str(), "record 0");
                assert_eq!(got[DEPTH].text.as_str(), std::format!("record {}", DEPTH - 1));
                //   and the counter was consumed by the report
                assert!(drain_all().is_empty());
        }

        #[test]
        fn over_long_messages_truncate_on_a_char_boundary() {
                let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
                reset();
                //   one ASCII byte then two-byte chars, so the capacity falls mid-char: byte 96
                // is the second half of an 'é', and the cut must step back to 95
                let long: std::string::String =
                        core::iter::once('a').chain(core::iter::repeat('é').take(TEXT_CAPACITY)).collect();
                crate::info!("{long}");
                let got = drain_all();
                assert_eq!(got[0].text.len(), TEXT_CAPACITY - 1);
                assert!(got[0].text.starts_with('a'));
                assert!(got[0].text[1..].chars().all(|c| c == 'é'));
        }

        #[test]
        fn levels_above_the_maximum_cost_nothing() {
                let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
                reset();
                set_max_level(Level::Warn);
                crate::info!("not kept");
                crate::debug!("not kept");
                crate::warn!("kept");
                let got = drain_all();
                assert_eq!(got.len(), 1);
                assert_eq!(got[0].text.as_str(), "kept");
        }

        #[test]
        fn drain_hands_over_at_most_max_per_call() {
                let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
                reset();
                for i in 0..10 {
                        crate::info!("{i}");
                }
                let mut seen = 0;
                assert_eq!(drain(4, |_| seen += 1), 4);
                assert_eq!(pending(), 6);
                assert_eq!(drain(100, |_| seen += 1), 6);
                assert_eq!(seen, 10);
        }

        #[test]
        fn concurrent_producers_lose_nothing_uncounted() {
                let _g = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
                reset();
                //   four producers hammer the queue while the drainer runs; every record is
                // either delivered or counted in a drop notice. the total must balance exactly
                // -- the shape of test that found mk3's refcount races, where a lost update
                // shows up as a count that no longer adds up
                const PRODUCERS: u32 = 4;
                const EACH: u32 = 2_000;
                let mut delivered = 0u32;
                let mut dropped = 0u32;
                let handles: Vec<_> = (0..PRODUCERS)
                        .map(|p| {
                                std::thread::spawn(move || {
                                        for i in 0..EACH {
                                                crate::info!("p{p} {i}");
                                        }
                                })
                        })
                        .collect();
                let count = |r: &Record, delivered: &mut u32, dropped: &mut u32| {
                        if r.target == "light_core::log" {
                                let n: u32 = r.text.as_str().split(' ').nth(1).unwrap().parse().unwrap();
                                *dropped += n;
                        } else if r.target == module_path!() {
                                // only this test's producers: the queue is global, and other
                                // tests running alongside log records of their own
                                *delivered += 1;
                        }
                };
                while handles.iter().any(|h| !h.is_finished()) {
                        drain(usize::MAX, |r| count(r, &mut delivered, &mut dropped));
                }
                for h in handles {
                        h.join().unwrap();
                }
                drain(usize::MAX, |r| count(r, &mut delivered, &mut dropped));
                assert_eq!(delivered + dropped, PRODUCERS * EACH);
                assert!(delivered > 0);
        }
}
