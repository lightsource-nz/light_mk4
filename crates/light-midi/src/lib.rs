//! USB-MIDI forwarding: crossfire's engine, ported.
//!
//! One rule, mk3's: incoming data on cable C of any mounted device is forwarded to cable C of
//! every other mounted device that has a cable C. Devices are slots indexed the way the host
//! stack indexes its MIDI interfaces; what sits behind a slot -- TinyUSB, an SPI-linked peer
//! board, a test mock -- is a [`Transport`], and the engine sees only 4-byte USB-MIDI event
//! packets, a fixed-size self-framing format every transport shares without any MIDI parsing
//! of its own.
//!
//! Hub mode is position: the host stack's mount index says nothing about which socket a cable
//! is in and is not even stable across a reconnect, so a device also records the hub port it
//! arrived on, which is the physical identity a status display can show.
//!
//! The engine holds no clock and calls nothing back: mounts and unmounts come in as calls,
//! traffic goes through the transport, and what an application must react to -- a display to
//! update, a host controller to reset -- comes out as return values.

#![no_std]

use heapless::Vec;

/// A USB-MIDI event packet: byte 0 is `(cable << 4) | code_index_number`, bytes 1..4 the MIDI
/// data.
pub type Packet = [u8; 4];

/// Cables per device the engine forwards between. Real USB-MIDI devices virtually always
/// expose one; this is headroom, not a spec limit.
pub const MAX_CABLES: usize = 4;

/// How long the RX/TX activity indicators stay lit after the most recent traffic.
pub const INDICATOR_MS: u32 = 150;

/// "No hub port": a device attached straight to the root port, or not mounted.
pub const HUB_PORT_NONE: u8 = 0;

/// Where a device sits on the bus. `hub_addr` 0 means the root port; otherwise the address of
/// the hub in front of it and that hub's 1-based downstream port.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BusInfo {
        pub hub_addr: u8,
        pub hub_port: u8,
}

/// What a host stack reports when a MIDI interface mounts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mount {
        pub daddr: u8,
        pub rx_cables: u8,
        pub tx_cables: u8,
}

/// The packet path behind the device slots.
pub trait Transport {
        /// The next packet waiting from slot `idx`, if any. Drained until `None` per service.
        fn read(&mut self, idx: u8) -> Option<Packet>;
        fn write(&mut self, idx: u8, packet: &Packet);
        /// Called once per service for every slot written to. A transport whose writes are
        /// already complete bursts (the SPI link) ignores it.
        fn flush(&mut self, idx: u8);
}

/// A mount or unmount a host stack reported, waiting for the application's poll.
#[derive(Clone, Copy, Debug)]
pub enum MidiEvent {
        Mounted { idx: u8, mount: Mount, bus: Option<BusInfo> },
        Unmounted { idx: u8 },
}

/// A USB host stack driving MIDI devices: the packet path plus the lifecycle around it.
/// What lets an application own its forwarding loop without naming the stack -- the port
/// crate implements this for the real controller, a test mock for the bench.
pub trait Host: Transport {
        /// Run the stack: enumeration, transfers, callbacks. Every pass.
        fn task(&mut self);
        /// What the stack reported since the last poll.
        fn next_event(&mut self) -> Option<MidiEvent>;
        /// Tear the controller down and bring it back. May block for a settle delay.
        fn reset(&mut self);
        /// Mount reports lost to a full queue since boot.
        fn dropped_events(&self) -> u32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
        Usb,
        /// A peer board over an SPI link, mounted for the life of the program and never on the
        /// USB bus -- not counted by [`Forwarder::usb_mounted_count`].
        Link,
}

#[derive(Clone, Copy, Debug)]
pub struct Device {
        pub mounted: bool,
        pub kind: Kind,
        pub daddr: u8,
        pub rx_cables: u8,
        pub tx_cables: u8,
        /// 1-based hub port, or `HUB_PORT_NONE`.
        pub hub_port: u8,
}

const NO_DEVICE: Device = Device { mounted: false, kind: Kind::Usb, daddr: 0, rx_cables: 0, tx_cables: 0, hub_port: HUB_PORT_NONE };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Target {
        idx: u8,
        cable: u8,
}

/// What a mount or unmount asks of the application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Change {
        /// Something in the mounted set changed: the status display is stale.
        pub status_changed: bool,
        /// Whether anything USB is still mounted -- what an activity LED shows.
        pub any_usb_mounted: bool,
        /// A disconnect left the root port EMPTY: the moment to reset the host controller.
        /// Only then, because a controller reset drops every other device on the bus, and
        /// behind a hub that would turn unplugging one instrument into losing all four.
        pub reset_host: bool,
}

/// What one service pass saw.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Activity {
        pub received: bool,
        pub forwarded: bool,
}

/// `N` device slots: at least the host stack's MIDI interface count, plus one for a linked peer.
pub struct Forwarder<const N: usize> {
        devices: [Device; N],
        /// Forwarding targets per (source slot, source cable). Rebuilt on every mount change.
        table: [[Vec<Target, N>; MAX_CABLES]; N],
        /// The hub's address, learned from the first device that mounts behind one; 0 until
        /// then. There is no "hub mounted" callback: an empty hub is indistinguishable from
        /// none, which is fine -- it has nothing to forward.
        hub_addr: u8,
        last_rx_ms: u32,
        last_tx_ms: u32,
        rx_shown: bool,
        tx_shown: bool,
        /// Packets dropped for a cable number past `MAX_CABLES`, for diagnostics.
        pub dropped: u32,
}

impl<const N: usize> Default for Forwarder<N> {
        fn default() -> Self {
                Self::new()
        }
}

impl<const N: usize> Forwarder<N> {
        pub const fn new() -> Self {
                Self { devices: [NO_DEVICE; N], table: [const { [const { Vec::new() }; MAX_CABLES] }; N], hub_addr: 0, last_rx_ms: 0, last_tx_ms: 0, rx_shown: false, tx_shown: false, dropped: 0 }
        }

        pub fn device(&self, idx: u8) -> Option<&Device> {
                self.devices.get(usize::from(idx)).filter(|d| d.mounted)
        }

        pub fn mounted_count(&self) -> usize {
                self.devices.iter().filter(|d| d.mounted).count()
        }

        pub fn usb_mounted_count(&self) -> usize {
                self.devices.iter().filter(|d| d.mounted && d.kind == Kind::Usb).count()
        }

        pub fn hub_addr(&self) -> u8 {
                self.hub_addr
        }

        pub fn hub_port_of(&self, idx: u8) -> u8 {
                self.device(idx).map_or(HUB_PORT_NONE, |d| d.hub_port)
        }

        pub fn hub_port_occupied(&self, port: u8) -> bool {
                port != HUB_PORT_NONE && self.devices.iter().any(|d| d.mounted && d.kind == Kind::Usb && d.hub_port == port)
        }

        /// A USB-MIDI interface mounted in slot `idx`, with where the host stack says it sits.
        /// A slot past `N` is refused (`None`) rather than written: the host stack's interface
        /// count and this engine's slot count are configured in two places, and the guard is
        /// against them drifting.
        pub fn mount(&mut self, idx: u8, mount: Mount, bus: Option<BusInfo>) -> Option<Change> {
                let d = self.devices.get_mut(usize::from(idx))?;
                *d = Device { mounted: true, kind: Kind::Usb, daddr: mount.daddr, rx_cables: mount.rx_cables, tx_cables: mount.tx_cables, hub_port: HUB_PORT_NONE };
                match bus {
                        Some(b) if b.hub_addr != 0 => {
                                //   one hub is what the host stack enumerates; a second address
                                // would conflate port numbers, which the caller can log
                                if self.hub_addr == 0 {
                                        self.hub_addr = b.hub_addr;
                                }
                                d.hub_port = b.hub_port;
                        }
                        // straight into the root port, or no bus info at all: still forwards,
                        // we just cannot say where it is plugged in
                        _ => {}
                }
                self.rebuild();
                Some(Change { status_changed: true, any_usb_mounted: true, reset_host: false })
        }

        /// The SPI-linked peer, in its reserved slot: a forwarding participant unconditionally,
        /// with no discovery or handshake.
        pub fn mount_link(&mut self, idx: u8) -> Option<Change> {
                let d = self.devices.get_mut(usize::from(idx))?;
                *d = Device { mounted: true, kind: Kind::Link, daddr: 0, rx_cables: 1, tx_cables: 1, hub_port: HUB_PORT_NONE };
                self.rebuild();
                Some(Change { status_changed: true, any_usb_mounted: self.usb_mounted_count() > 0, reset_host: false })
        }

        pub fn unmount(&mut self, idx: u8) -> Option<Change> {
                let d = self.devices.get_mut(usize::from(idx))?;
                if !d.mounted {
                        return None;
                }
                d.mounted = false;
                d.hub_port = HUB_PORT_NONE;
                self.rebuild();
                let usb_left = self.usb_mounted_count();
                //   the hub is forgotten only once nothing is left behind it, so pulling one
                // instrument does not make a display claim the hub went away
                if usb_left == 0 {
                        self.hub_addr = 0;
                }
                Some(Change { status_changed: true, any_usb_mounted: usb_left > 0, reset_host: usb_left == 0 })
        }

        fn rebuild(&mut self) {
                for src in 0..N {
                        for cable in 0..MAX_CABLES {
                                self.table[src][cable].clear();
                        }
                        let s = self.devices[src];
                        if !s.mounted {
                                continue;
                        }
                        let src_cables = usize::from(s.rx_cables).min(MAX_CABLES);
                        for cable in 0..src_cables {
                                for dst in 0..N {
                                        let d = self.devices[dst];
                                        if dst == src || !d.mounted || cable >= usize::from(d.tx_cables) {
                                                continue;
                                        }
                                        // cannot overflow: at most N-1 other devices
                                        let _ = self.table[src][cable].push(Target { idx: dst as u8, cable: cable as u8 });
                                }
                        }
                }
        }

        /// Drain every mounted source and forward what arrived. Call once per pass.
        pub fn service<T: Transport>(&mut self, transport: &mut T, now_ms: u32) -> Activity {
                let mut activity = Activity::default();
                let mut wrote: [bool; N] = [false; N];
                for src in 0..N {
                        if !self.devices[src].mounted {
                                continue;
                        }
                        while let Some(packet) = transport.read(src as u8) {
                                activity.received = true;
                                self.last_rx_ms = now_ms;
                                //   DROP RESERVED CODE INDEX NUMBERS, which in practice means
                                // padding: a bulk transfer carries 16 four-byte slots and a
                                // device with one event zero-fills the other 15. CIN 0 and 1
                                // never carry data. Forwarding them was not harmless -- up to
                                // 15 of every 16 bytes on the SPI link were nothing at all
                                let cin = packet[0] & 0x0F;
                                if cin < 0x2 {
                                        continue;
                                }
                                let cable = usize::from(packet[0] >> 4);
                                if cable >= MAX_CABLES {
                                        self.dropped += 1;
                                        continue;
                                }
                                for t in self.table[src][cable].iter() {
                                        // the destination's own cable number re-embedded in byte
                                        // 0; the CIN and the MIDI bytes pass through unchanged
                                        let out = [(t.cable << 4) | cin, packet[1], packet[2], packet[3]];
                                        transport.write(t.idx, &out);
                                        wrote[usize::from(t.idx)] = true;
                                }
                        }
                }
                for (idx, w) in wrote.iter().enumerate() {
                        if *w {
                                activity.forwarded = true;
                                self.last_tx_ms = now_ms;
                                transport.flush(idx as u8);
                        }
                }
                activity
        }

        /// The RX/TX indicators as they should be shown, and whether that changed since the
        /// last call -- so a display redraws on a transition, not every pass. Call every pass:
        /// an indicator turns off by time passing, not by traffic.
        pub fn indicators(&mut self, now_ms: u32) -> (bool, bool, bool) {
                let rx = now_ms.wrapping_sub(self.last_rx_ms) < INDICATOR_MS && self.last_rx_ms != 0;
                let tx = now_ms.wrapping_sub(self.last_tx_ms) < INDICATOR_MS && self.last_tx_ms != 0;
                let changed = rx != self.rx_shown || tx != self.tx_shown;
                self.rx_shown = rx;
                self.tx_shown = tx;
                (rx, tx, changed)
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use std::collections::VecDeque;
        use std::vec::Vec as StdVec;

        /// Fake devices and their queues, standing in for TinyUSB's tuh_midi_* API.
        #[derive(Default)]
        struct Mock {
                inbound: StdVec<VecDeque<Packet>>,
                written: StdVec<VecDeque<Packet>>,
                flushes: StdVec<u32>,
        }

        impl Mock {
                fn new(slots: usize) -> Self {
                        Self { inbound: (0..slots).map(|_| VecDeque::new()).collect(), written: (0..slots).map(|_| VecDeque::new()).collect(), flushes: std::vec![0; slots] }
                }
                fn feed(&mut self, idx: u8, p: Packet) {
                        self.inbound[usize::from(idx)].push_back(p);
                }
                fn take_written(&mut self, idx: u8) -> Option<Packet> {
                        self.written[usize::from(idx)].pop_front()
                }
        }

        impl Transport for Mock {
                fn read(&mut self, idx: u8) -> Option<Packet> {
                        self.inbound[usize::from(idx)].pop_front()
                }
                fn write(&mut self, idx: u8, packet: &Packet) {
                        self.written[usize::from(idx)].push_back(*packet);
                }
                fn flush(&mut self, idx: u8) {
                        self.flushes[usize::from(idx)] += 1;
                }
        }

        const HUB: u8 = 5;

        fn connect(f: &mut Forwarder<4>, idx: u8, rx: u8, tx: u8) -> Change {
                f.mount(idx, Mount { daddr: idx + 1, rx_cables: rx, tx_cables: tx }, None).unwrap()
        }

        fn connect_hub(f: &mut Forwarder<4>, count: u8) {
                for idx in 0..count {
                        f.mount(idx, Mount { daddr: idx + 1, rx_cables: 1, tx_cables: 1 }, Some(BusInfo { hub_addr: HUB, hub_port: idx + 1 })).unwrap();
                }
        }

        const NOTE_ON: Packet = [0x09, 0x90, 0x3C, 0x64];
        const CC: Packet = [0x0B, 0xB0, 0x07, 0x7F];

        #[test]
        fn broadcast_between_two_devices_never_loops_back() {
                let mut f: Forwarder<4> = Forwarder::new();
                let mut m = Mock::new(4);
                connect(&mut f, 0, 1, 1);
                connect(&mut f, 1, 1, 1);
                m.feed(0, NOTE_ON);
                let a = f.service(&mut m, 10);
                assert_eq!(a, Activity { received: true, forwarded: true });
                assert_eq!(m.take_written(1), Some(NOTE_ON));
                assert_eq!(m.take_written(0), None, "never back to its own source");
                assert_eq!(m.flushes, [0, 1, 0, 0]);
        }

        #[test]
        fn one_source_broadcasts_to_every_other_mounted_device() {
                let mut f: Forwarder<4> = Forwarder::new();
                let mut m = Mock::new(4);
                for i in 0..3 {
                        connect(&mut f, i, 1, 1);
                }
                m.feed(0, CC);
                f.service(&mut m, 0);
                assert_eq!(m.take_written(1), Some(CC));
                assert_eq!(m.take_written(2), Some(CC));
        }

        #[test]
        fn a_cable_the_destination_lacks_is_skipped_and_cables_are_re_embedded() {
                let mut f: Forwarder<4> = Forwarder::new();
                let mut m = Mock::new(4);
                connect(&mut f, 0, 2, 2);
                connect(&mut f, 1, 1, 1);
                // cable 1 from device 0: device 1 has only cable 0
                m.feed(0, [0x19, 0x91, 0x40, 0x50]);
                f.service(&mut m, 0);
                assert_eq!(m.take_written(1), None);
                // cable 1 from a third device reaches device 0 (cables 0 and 1) and not device
                // 1 (cable 0 only); the cable number in byte 0 is the destination's
                connect(&mut f, 2, 3, 3);
                m.feed(2, [0x19, 0x92, 0x40, 0x50]);
                f.service(&mut m, 0);
                assert_eq!(m.take_written(0), Some([0x19, 0x92, 0x40, 0x50]));
                assert_eq!(m.take_written(1), None, "device 1 has no cable 1");
        }

        #[test]
        fn padding_slots_are_dropped_not_forwarded() {
                let mut f: Forwarder<4> = Forwarder::new();
                let mut m = Mock::new(4);
                connect(&mut f, 0, 1, 1);
                connect(&mut f, 1, 1, 1);
                m.feed(0, [0x00, 0, 0, 0]);
                m.feed(0, [0x01, 0, 0, 0]);
                let a = f.service(&mut m, 0);
                assert!(a.received && !a.forwarded);
                assert_eq!(m.take_written(1), None);
        }

        #[test]
        fn an_unmounted_device_is_dropped_from_the_table() {
                let mut f: Forwarder<4> = Forwarder::new();
                let mut m = Mock::new(4);
                connect(&mut f, 0, 1, 1);
                connect(&mut f, 1, 1, 1);
                let c = f.unmount(1).unwrap();
                assert!(c.status_changed && c.any_usb_mounted && !c.reset_host);
                m.feed(0, [0x08, 0x80, 0x3C, 0x40]);
                f.service(&mut m, 0);
                assert_eq!(m.take_written(1), None);
                assert_eq!(f.unmount(1), None, "already gone");
                assert_eq!(f.mount(9, Mount { daddr: 1, rx_cables: 1, tx_cables: 1 }, None), None, "a slot past the table is refused");
        }

        #[test]
        fn hub_devices_broadcast_and_report_their_ports() {
                let mut f: Forwarder<4> = Forwarder::new();
                let mut m = Mock::new(4);
                connect_hub(&mut f, 4);
                m.feed(0, NOTE_ON);
                f.service(&mut m, 0);
                for idx in 1..4 {
                        assert_eq!(m.take_written(idx), Some(NOTE_ON));
                }
                assert_eq!(m.take_written(0), None);
                assert_eq!(f.hub_addr(), HUB, "learned from the devices behind it");
                for idx in 0..4u8 {
                        assert_eq!(f.hub_port_of(idx), idx + 1);
                        assert!(f.hub_port_occupied(idx + 1));
                }
                assert!(!f.hub_port_occupied(HUB_PORT_NONE));
        }

        #[test]
        fn a_root_attached_device_has_no_port_and_claims_no_hub() {
                let mut f: Forwarder<4> = Forwarder::new();
                connect(&mut f, 0, 1, 1);
                assert_eq!(f.hub_port_of(0), HUB_PORT_NONE);
                assert_eq!(f.hub_addr(), 0);
        }

        #[test]
        fn unplugging_one_hub_device_keeps_its_siblings_and_asks_no_reset() {
                let mut f: Forwarder<4> = Forwarder::new();
                let mut m = Mock::new(4);
                connect_hub(&mut f, 4);
                let c = f.unmount(1).unwrap();
                //   THE POINT: a controller reset drops every device on the bus, so behind a
                // hub it must wait until the last one has gone
                assert!(!c.reset_host && c.any_usb_mounted);
                assert!(!f.hub_port_occupied(2));
                assert!(f.hub_port_occupied(1) && f.hub_port_occupied(3) && f.hub_port_occupied(4));
                assert_eq!(f.hub_addr(), HUB, "still known while devices remain behind it");
                m.feed(0, CC);
                f.service(&mut m, 0);
                assert!(m.take_written(2).is_some() && m.take_written(3).is_some());
                assert_eq!(m.take_written(1), None);
        }

        #[test]
        fn the_reset_is_requested_by_the_disconnect_that_empties_the_bus() {
                let mut f: Forwarder<4> = Forwarder::new();
                connect_hub(&mut f, 4);
                for idx in 0..3 {
                        assert!(!f.unmount(idx).unwrap().reset_host);
                }
                let last = f.unmount(3).unwrap();
                assert!(last.reset_host && !last.any_usb_mounted);
                assert_eq!(f.hub_addr(), 0, "forgotten once nothing is mounted behind it");
        }

        #[test]
        fn a_linked_peer_forwards_but_is_not_on_the_usb_bus() {
                let mut f: Forwarder<4> = Forwarder::new();
                let mut m = Mock::new(4);
                f.mount_link(3).unwrap();
                connect(&mut f, 0, 1, 1);
                assert_eq!(f.usb_mounted_count(), 1);
                assert_eq!(f.mounted_count(), 2);
                m.feed(3, NOTE_ON);
                f.service(&mut m, 0);
                assert_eq!(m.take_written(0), Some(NOTE_ON));
                // the last USB device leaving still empties the bus, peer or no peer
                let c = f.unmount(0).unwrap();
                assert!(c.reset_host && !c.any_usb_mounted);
        }

        #[test]
        fn indicators_light_on_traffic_and_go_out_by_time() {
                let mut f: Forwarder<4> = Forwarder::new();
                let mut m = Mock::new(4);
                connect(&mut f, 0, 1, 1);
                connect(&mut f, 1, 1, 1);
                assert_eq!(f.indicators(1000), (false, false, false));
                m.feed(0, NOTE_ON);
                f.service(&mut m, 1000);
                assert_eq!(f.indicators(1010), (true, true, true));
                assert_eq!(f.indicators(1100), (true, true, false), "no transition, no redraw");
                assert_eq!(f.indicators(1000 + INDICATOR_MS), (false, false, true));
        }
}
