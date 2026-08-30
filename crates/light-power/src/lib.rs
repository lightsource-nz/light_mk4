//! Power supply management: mk3's `light_power`, ported with its hardest-won property
//! intact -- SAFE BY DEFAULT. Selecting a profile moves a real rail, and what hangs off that
//! rail is a fact about the board this layer cannot see. The failure is not theoretical and
//! not recoverable: on the bench this was written against, the sink's output was hardwired to
//! a Pico's 5V input, and a single successful request for 9V would have ended the Pico. So a
//! device starts with its request ceiling at the USB-C default rail, and anything more is an
//! explicit decision made through [`Power::set_max_millivolts`] by whoever knows the wiring.
//!
//! The other lesson baked in: a selection is not a function call that succeeds or fails --
//! it is a message handed to a negotiation that answers later, or does not answer at all.
//! A HUSB238 was asked for 15V and for 20V, both advertised by the source; both writes
//! landed, both returned success, and the source granted neither. So the outcome is a
//! [`RequestState`] the poll path resolves, never a return value.

#![no_std]

pub mod husb238;

use light_core::{error, info, warn};

/// The most operating points one device may offer. Eight covers USB PD's fixed supply
/// voltages (5/9/12/15/18/20V) with room to spare; a device offering more reports the first
/// `MAX_PROFILES` of them rather than overrunning.
pub const MAX_PROFILES: usize = 8;

/// The ceiling a device starts with: the USB-C default rail, the only voltage a source
/// supplies before anyone negotiates for more. See the crate doc for why this is the point.
pub const SAFE_MAX_MV: u16 = 5000;

/// How long a request may stay `Pending` before it is called refused. USB PD negotiation
/// completes in well under a second -- 12V was measured landing inside 800 ms on the mk3
/// bench -- and a request still unanswered at this point was measured still unanswered at
/// 2 s, so waiting longer only delays the news.
pub const REQUEST_TIMEOUT_MS: u32 = 1500;

/// How often the hardware is actually read unless overridden: a power contract changes when
/// someone plugs or unplugs something, so this only has to beat a person's patience, and
/// every poll faster is bus traffic against a chip with nothing new to say.
pub const DEFAULT_POLL_INTERVAL_MS: u16 = 500;

/// One selectable operating point: a voltage the source can be asked to supply, and the most
/// current it will provide there.
///
/// `available` is separate from the entry existing at all, and that distinction is the whole
/// reason profiles are indexed rather than compacted. A USB PD source advertises a FIXED set
/// of voltages and marks which it actually offers -- a 45W charger with no 18V rail still
/// has an 18V entry, it is simply not on offer. Compacting would renumber every profile
/// above the missing one whenever a different supply was plugged in, silently turning a
/// stored "profile 4" into a request for a different voltage. So indices are stable and
/// availability is a property of the entry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Profile {
        pub millivolts: u16,
        pub milliamps: u16,
        pub available: bool,
}

/// What became of the last request, resolved by the poll path -- see the crate doc for why
/// it cannot be a return value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestState {
        /// Nothing outstanding.
        None,
        /// Asked, not yet answered either way.
        Pending,
        /// The contract in force is the one asked for.
        Active,
        /// The source did not grant it within [`REQUEST_TIMEOUT_MS`] -- or could not be
        /// reached at all.
        Refused,
}

/// What the rail is doing, as one hardware read reports it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Reading {
        /// What the rail is supplying right now, agreed or not. Zero when nothing is known.
        pub active_mv: u16,
        pub active_ma: u16,
        /// Whether that is a NEGOTIATED contract as opposed to the bus's default. The two
        /// really are independent: a USB-C sink with nothing negotiated still sits at 5V,
        /// so "5V present" is not "5V agreed". Conflating them was a bug in mk3's first cut.
        pub contract_active: bool,
}

/// A part that supplies (or negotiates for) power. The driver reads and writes registers;
/// every judgement -- throttling, ceilings, what became of a request -- lives in [`Power`],
/// the same for every device.
pub trait PowerSource {
        /// Whether the part genuinely implements USB Power Delivery, as opposed to merely
        /// offering voltages to select between. Not cosmetic: on a PD device the profiles
        /// are a remote source's advertised capabilities and can change or be withdrawn
        /// without this device being asked; on a non-PD device they are local settings that
        /// stay put.
        fn is_pd(&self) -> bool;

        /// Fill in the profile list's fixed SHAPE -- every voltage the part can ever name,
        /// all initially unavailable -- and return how many entries that is. Called once;
        /// [`PowerSource::poll`] then only updates availability and current, which is what
        /// varies with what is plugged in.
        fn init(&mut self, profiles: &mut [Profile; MAX_PROFILES]) -> u8;

        /// Refresh availability and currents, and read the rail. `None` when the device did
        /// not answer -- which for a bus-powered sink with nothing plugged into it is its
        /// ORDINARY resting state, not an error.
        fn poll(&mut self, profiles: &mut [Profile; MAX_PROFILES]) -> Option<Reading>;

        /// Ask the hardware to switch to `profiles[index]`, already validated by the caller.
        /// `true` means the request was ACCEPTED FOR SENDING -- not that the new voltage is
        /// in force; only a later poll can say that. `None`-like devices that report but
        /// cannot be asked return `false` from [`PowerSource::can_select`] instead.
        fn select(&mut self, index: u8) -> bool;

        /// Whether the part can be asked to change its supply at all.
        fn can_select(&self) -> bool {
                true
        }
}

/// One power device: a driver plus every judgement mk3 made once, made once again here.
pub struct Power<S: PowerSource> {
        source: S,
        profiles: [Profile; MAX_PROFILES],
        profile_count: u8,
        reading: Reading,
        /// Whether the last poll reached the chip, so transitions can be logged without a
        /// line per poll -- silence is a normal state for a bus-powered sink.
        answering: bool,
        /// The profile last requested -- not necessarily the one in force. Kept so a
        /// consumer can tell "asked for 12V and got it" from "asked and still at 5V".
        requested: Option<u8>,
        request_state: RequestState,
        request_started_ms: u32,
        /// The highest voltage this device may be ASKED for -- a property of what is wired
        /// downstream, not of what the source offers.
        max_millivolts: u16,
        last_poll_ms: u32,
        poll_interval_ms: u16,
        ever_polled: bool,
}

impl<S: PowerSource> Power<S> {
        pub fn new(mut source: S) -> Self {
                let mut profiles = [Profile::default(); MAX_PROFILES];
                let profile_count = source.init(&mut profiles).min(MAX_PROFILES as u8);
                Self {
                        source,
                        profiles,
                        profile_count,
                        reading: Reading::default(),
                        answering: false,
                        requested: None,
                        request_state: RequestState::None,
                        request_started_ms: 0,
                        max_millivolts: SAFE_MAX_MV,
                        last_poll_ms: 0,
                        poll_interval_ms: DEFAULT_POLL_INTERVAL_MS,
                        ever_polled: false,
                }
        }

        /// Refresh from the hardware if the poll interval has elapsed. Returns `true` when
        /// the hardware was actually read AND answered; `false` both when the interval has
        /// not elapsed and when the read failed -- deliberately not distinguished, since
        /// neither is news, and a device that has never answered simply reports no profiles.
        pub fn poll(&mut self, now_ms: u32) -> bool {
                if self.ever_polled
                        && self.poll_interval_ms != 0
                        && now_ms.wrapping_sub(self.last_poll_ms) < u32::from(self.poll_interval_ms)
                {
                        return false;
                }
                self.ever_polled = true;
                self.last_poll_ms = now_ms;

                let answered = match self.source.poll(&mut self.profiles) {
                        Some(reading) => {
                                if !self.answering {
                                        self.answering = true;
                                        info!("power source answering");
                                }
                                self.reading = reading;
                                true
                        }
                        None => {
                                if self.answering {
                                        self.answering = false;
                                        info!("power source stopped answering -- unplugged?");
                                        //   everything known about the source goes with it.
                                        // Leaving the last-seen capabilities in place would
                                        // let a consumer select a profile from a charger
                                        // that is no longer attached
                                        self.reading = Reading::default();
                                        self.requested = None;
                                        for p in &mut self.profiles {
                                                p.available = false;
                                                p.milliamps = 0;
                                        }
                                }
                                false
                        }
                };
                //   resolved even when the read FAILED, so a request outstanding against a
                // device that has stopped answering still times out into Refused rather than
                // staying pending forever. A source that cannot be reached has not granted
                // anything
                self.resolve_request(now_ms);
                answered
        }

        /// Decides what became of an outstanding request, from the state just read: did the
        /// thing we asked for turn up, and if not, has long enough passed to call it refused.
        fn resolve_request(&mut self, now_ms: u32) {
                //   nothing outstanding. Also the path a disconnect takes: losing the source
                // clears `requested`, which retires whatever was in flight -- resolving a
                // request against a source that has gone away would be inventing news
                let Some(requested) = self.requested else {
                        self.request_state = RequestState::None;
                        return;
                };
                if self.request_state != RequestState::Pending {
                        return;
                }
                //   granted: there is a contract AND it is the one asked for. Both halves
                // matter -- a contract at a DIFFERENT voltage means an earlier request is
                // still standing and this one was ignored, which is exactly what a refused
                // HUSB238 request looks like
                let asked_mv = self.profiles[usize::from(requested)].millivolts;
                if self.reading.contract_active && self.reading.active_mv == asked_mv {
                        self.request_state = RequestState::Active;
                        info!("power: profile {requested} ({asked_mv} mV) is in force");
                        return;
                }
                if now_ms.wrapping_sub(self.request_started_ms) < REQUEST_TIMEOUT_MS {
                        return;
                }
                //   refused, and said plainly: a source may advertise a profile and decline
                // to supply it -- measured on a charger offering 15V and 20V that granted
                // neither -- and a caller told only that the write succeeded would carry on
                // believing it had them
                self.request_state = RequestState::Refused;
                warn!("power: profile {requested} ({asked_mv} mV) was NOT granted; still at {} mV", self.reading.active_mv);
        }

        // --- what the source offers ---

        pub fn profile_count(&self) -> u8 {
                self.profile_count
        }

        /// `None` for an out-of-range index.
        pub fn profile(&self, index: u8) -> Option<Profile> {
                (index < self.profile_count).then(|| self.profiles[usize::from(index)])
        }

        /// The highest-voltage AVAILABLE profile at or below `max_millivolts`, or `None`.
        /// The common ask -- "give me as much as I can safely take" -- expressed once here.
        /// The device's own ceiling applies too, whichever is lower, so this can never name
        /// a profile that [`Power::select_profile`] would then refuse: a find-then-select
        /// pair that disagrees with itself is a trap.
        pub fn find_profile(&self, max_millivolts: u16) -> Option<u8> {
                let ceiling = max_millivolts.min(self.max_millivolts);
                let mut best: Option<u8> = None;
                let mut best_mv = 0u16;
                for i in 0..usize::from(self.profile_count) {
                        let p = &self.profiles[i];
                        if !p.available || p.millivolts > ceiling {
                                continue;
                        }
                        if best.is_some() && p.millivolts <= best_mv {
                                continue;
                        }
                        best = Some(i as u8);
                        best_mv = p.millivolts;
                }
                best
        }

        // --- what this board can survive ---

        /// Raise or lower the ceiling on what may be REQUESTED. Raising it asserts that
        /// everything downstream of this rail tolerates the new voltage -- a claim about the
        /// board that only the board's own wiring code is in a position to make, so this
        /// call belongs beside the device's creation, not at a call site that wants power.
        pub fn set_max_millivolts(&mut self, millivolts: u16) {
                //   at WARNING when the ceiling goes UP: raising it is the moment someone
                // asserted the downstream can take more, and if that assertion is wrong the
                // evidence is a dead board with nothing in the log to say who claimed
                // otherwise
                if millivolts > self.max_millivolts {
                        warn!("power: request ceiling raised {} mV -> {millivolts} mV", self.max_millivolts);
                } else {
                        info!("power: request ceiling set to {millivolts} mV");
                }
                self.max_millivolts = millivolts;
        }

        pub fn max_millivolts(&self) -> u16 {
                self.max_millivolts
        }

        // --- what is actually in force ---

        /// What the rail is supplying right now, and whether anyone agreed to it. The
        /// reading is always meaningful -- zeros if the device has reported nothing yet --
        /// and `contract_active: false` does not mean "no answer": it means "this is what
        /// you are running on, and nobody negotiated it".
        pub fn active(&self) -> Reading {
                self.reading
        }

        pub fn requested(&self) -> Option<u8> {
                self.requested
        }

        pub fn request_state(&self) -> RequestState {
                self.request_state
        }

        pub fn is_pd(&self) -> bool {
                self.source.is_pd()
        }

        pub fn set_poll_interval(&mut self, interval_ms: u16) {
                self.poll_interval_ms = interval_ms;
        }

        // --- changing it ---

        /// Ask for `profiles[index]`. THIS CHANGES THE VOLTAGE ON A LIVE RAIL, which is what
        /// makes it unlike everything above: every other call observes, this one acts on
        /// hardware that may be powering the caller.
        ///
        /// Returns only that the request was SENT -- see the crate doc for the measured case
        /// that makes anything stronger a lie. The outcome arrives through
        /// [`Power::request_state`], resolved by the poll path.
        pub fn select_profile(&mut self, index: u8, now_ms: u32) -> bool {
                if !self.source.can_select() {
                        warn!("power: this device reports its supply but cannot be asked to change it");
                        return false;
                }
                if index >= self.profile_count {
                        error!("power: no profile {index} (there are {})", self.profile_count);
                        return false;
                }
                let p = self.profiles[usize::from(index)];
                //   refused rather than attempted: an unavailable profile is a request the
                // source has already said it cannot honour. Sending it anyway would at best
                // be ignored and at worst trigger a renegotiation that drops the rail to
                // nothing on the way to failing
                if !p.available {
                        error!("power: profile {index} ({} mV) is not currently offered", p.millivolts);
                        return false;
                }
                //   the ceiling, checked last because it is the one refusal about the BOARD
                // rather than the source: "can what is downstream survive it" is a fact this
                // layer was told once and the caller may never have known
                if p.millivolts > self.max_millivolts {
                        error!("power: REFUSED profile {index} ({} mV) -- this device is limited to {} mV. Raise it with set_max_millivolts() only if everything on this rail tolerates the higher voltage", p.millivolts, self.max_millivolts);
                        return false;
                }
                //   logged unconditionally: this is the one call that moves a live rail, and
                // a rail that moved with no record of anyone asking is a bad thing to debug
                info!("power: requesting profile {index} ({} mV, {} mA)", p.millivolts, p.milliamps);
                //   recorded BEFORE the driver call: the request has been made whether or not
                // the hardware accepts it, and "did what I asked for happen" needs the asking
                // recorded even -- especially -- when it did not. The clock starts here, not
                // at the first poll: timing from when somebody next looked would call a slow
                // answer refused
                self.requested = Some(index);
                self.request_state = RequestState::Pending;
                self.request_started_ms = now_ms;

                if self.source.select(index) {
                        return true;
                }
                //   the write itself failed -- the one refusal knowable immediately: nothing
                // reached the source, so there is nothing to wait for
                self.request_state = RequestState::Refused;
                false
        }

        /// The driver, for anything device-specific the model does not cover.
        pub fn source(&mut self) -> &mut S {
                &mut self.source
        }
}

#[cfg(test)]
mod tests {
        use super::*;

        /// A scriptable source: what the "hardware" offers and answers is set by the test.
        struct Fake {
                answering: bool,
                reading: Reading,
                available: [bool; 3],
                selected: Option<u8>,
                select_ok: bool,
                selectable: bool,
        }

        impl Default for Fake {
                fn default() -> Self {
                        Self { answering: true, reading: Reading::default(), available: [true; 3], selected: None, select_ok: true, selectable: true }
                }
        }

        const MV: [u16; 3] = [5000, 9000, 12000];

        impl PowerSource for Fake {
                fn is_pd(&self) -> bool {
                        true
                }
                fn init(&mut self, profiles: &mut [Profile; MAX_PROFILES]) -> u8 {
                        for (i, p) in profiles.iter_mut().take(3).enumerate() {
                                p.millivolts = MV[i];
                        }
                        3
                }
                fn poll(&mut self, profiles: &mut [Profile; MAX_PROFILES]) -> Option<Reading> {
                        if !self.answering {
                                return None;
                        }
                        for (i, p) in profiles.iter_mut().take(3).enumerate() {
                                p.available = self.available[i];
                                p.milliamps = if self.available[i] { 3000 } else { 0 };
                        }
                        Some(self.reading)
                }
                fn select(&mut self, index: u8) -> bool {
                        self.selected = Some(index);
                        self.select_ok
                }
                fn can_select(&self) -> bool {
                        self.selectable
                }
        }

        fn powered() -> Power<Fake> {
                let mut p = Power::new(Fake::default());
                p.poll(0);
                p
        }

        #[test]
        fn the_ceiling_starts_safe_and_binds_selection_and_find() {
                let mut p = powered();
                assert_eq!(p.max_millivolts(), SAFE_MAX_MV);
                //   find can only name what select would accept -- the pair must not disagree
                assert_eq!(p.find_profile(12000), Some(0), "ceiling caps find at 5V");
                assert!(!p.select_profile(2, 0), "12V refused below the ceiling");
                assert_eq!(p.source().selected, None, "nothing reached the wire");
                p.set_max_millivolts(12000);
                assert_eq!(p.find_profile(12000), Some(2));
                assert!(p.select_profile(2, 0));
                assert_eq!(p.source().selected, Some(2));
        }

        #[test]
        fn find_profile_picks_the_highest_available_within_the_ask() {
                let mut p = powered();
                p.set_max_millivolts(20000);
                assert_eq!(p.find_profile(20000), Some(2));
                assert_eq!(p.find_profile(9000), Some(1));
                p.source().available = [true, false, true];
                p.set_poll_interval(0);
                p.poll(1);
                assert_eq!(p.find_profile(9000), Some(0), "unavailable 9V is skipped, not absent");
                p.source().available = [false; 3];
                p.poll(2);
                assert_eq!(p.find_profile(20000), None);
        }

        #[test]
        fn a_request_is_pending_until_the_contract_lands() {
                let mut p = powered();
                p.set_max_millivolts(12000);
                p.set_poll_interval(0);
                assert!(p.select_profile(2, 100));
                assert_eq!(p.request_state(), RequestState::Pending);
                //   the write landing proves nothing -- the poll that sees the contract does
                p.poll(200);
                assert_eq!(p.request_state(), RequestState::Pending);
                p.source().reading = Reading { active_mv: 12000, active_ma: 3000, contract_active: true };
                p.poll(300);
                assert_eq!(p.request_state(), RequestState::Active);
        }

        #[test]
        fn an_unanswered_request_times_out_into_refused() {
                let mut p = powered();
                p.set_max_millivolts(12000);
                p.set_poll_interval(0);
                assert!(p.select_profile(2, 100));
                //   a contract at a DIFFERENT voltage is not this request granted: it is the
                // refused-HUSB238 shape, an earlier contract still standing
                p.source().reading = Reading { active_mv: 5000, active_ma: 1500, contract_active: true };
                p.poll(100 + REQUEST_TIMEOUT_MS - 1);
                assert_eq!(p.request_state(), RequestState::Pending);
                p.poll(100 + REQUEST_TIMEOUT_MS);
                assert_eq!(p.request_state(), RequestState::Refused);
        }

        #[test]
        fn a_disconnect_retires_the_request_and_the_capabilities() {
                let mut p = powered();
                p.set_max_millivolts(12000);
                p.set_poll_interval(0);
                p.source().reading = Reading { active_mv: 5000, active_ma: 1500, contract_active: false };
                p.poll(1);
                assert!(p.select_profile(2, 2));
                p.source().answering = false;
                assert!(!p.poll(3));
                assert_eq!(p.requested(), None);
                assert_eq!(p.request_state(), RequestState::None, "no news is invented about a source that left");
                assert_eq!(p.active().active_mv, 0);
                assert_eq!(p.profile(2).unwrap().available, false);
                //   a request against a device that then never answers must not stay pending
                // forever either: ask again while silent, and the timeout still runs
                p.source().answering = true;
                p.poll(4);
                assert!(p.select_profile(2, 5));
                p.source().answering = false;
                p.poll(5 + REQUEST_TIMEOUT_MS);
                assert_eq!(p.request_state(), RequestState::None);
        }

        #[test]
        fn refusals_that_are_knowable_never_reach_the_wire() {
                let mut p = powered();
                assert!(!p.select_profile(7, 0), "out of range");
                p.source().available = [true, false, true];
                p.set_poll_interval(0);
                p.poll(1);
                assert!(!p.select_profile(1, 1), "not currently offered");
                assert_eq!(p.source().selected, None);
                p.source().selectable = false;
                assert!(!p.select_profile(0, 2), "reports but cannot be asked");
                //   and a write that fails is refused immediately: nothing to wait for
                p.source().selectable = true;
                p.source().select_ok = false;
                assert!(!p.select_profile(0, 3));
                assert_eq!(p.request_state(), RequestState::Refused);
        }

        #[test]
        fn polling_is_throttled_but_the_first_poll_is_free() {
                let mut p = Power::new(Fake::default());
                assert!(p.poll(0), "first poll reads regardless of the interval");
                assert!(!p.poll(u32::from(DEFAULT_POLL_INTERVAL_MS) - 1));
                assert!(p.poll(u32::from(DEFAULT_POLL_INTERVAL_MS)));
        }

        #[test]
        fn the_default_rail_is_not_a_contract() {
                let mut p = powered();
                p.set_poll_interval(0);
                p.source().reading = Reading { active_mv: 5000, active_ma: 1500, contract_active: false };
                p.poll(1);
                let r = p.active();
                assert_eq!(r.active_mv, 5000, "the rail's state is always reported");
                assert!(!r.contract_active, "nobody agreed to it");
        }
}
