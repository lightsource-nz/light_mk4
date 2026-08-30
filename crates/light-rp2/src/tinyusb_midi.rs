//! The USB-MIDI host transport: TinyUSB's `tuh_midi_*` API behind [`light_midi::Transport`],
//! and the class callbacks the stack expects the application to define.
//!
//! Everything here runs on core 0, in the context the shell's `light_shell_usb_host_task()` is
//! called from: the stack's mount callbacks fire from inside `tuh_task()`, so they arrive on the
//! same core as the packet reads and go through a mailbox only to get out of the callback and
//! into the module's own poll -- mk3's rule that a callback the stack is still unwinding does no
//! work of its own, kept.
//!
//! Only meaningful in a firmware whose shell was built for the host role; the symbols resolve
//! against `tinyusb_host` at link time.

use light_midi::{BusInfo, Mount, Packet, Transport};
use light_core::Mailbox;

#[repr(C)]
struct MountCbData {
        daddr: u8,
        itf_num: u8,
        rx_cable_count: u8,
        tx_cable_count: u8,
}

#[repr(C)]
#[derive(Default)]
struct TuhBusInfo {
        rhport: u8,
        hub_addr: u8,
        hub_port: u8,
        speed: u8,
}

unsafe extern "C" {
        fn light_shell_usb_host_init();
        fn light_shell_usb_host_task();
        fn light_shell_usb_host_reset();
        fn tuh_midi_packet_read_n(idx: u8, buffer: *mut u8, bufsize: u32) -> u32;
        fn tuh_midi_packet_write_n(idx: u8, buffer: *const u8, bufsize: u32) -> u32;
        fn tuh_midi_write_flush(idx: u8) -> u32;
        fn tuh_bus_info_get(daddr: u8, bus_info: *mut TuhBusInfo) -> bool;
}

/// A mount or unmount the stack reported, waiting for the application's poll.
#[derive(Clone, Copy, Debug)]
pub enum MidiEvent {
        Mounted { idx: u8, mount: Mount, bus: Option<BusInfo> },
        Unmounted { idx: u8 },
}

/// Mount events from the callbacks to the module. Eight deep: a hub coming up mounts its
/// instruments in one burst.
static EVENTS: Mailbox<MidiEvent, 8> = Mailbox::new();

#[unsafe(no_mangle)]
extern "C" fn tuh_midi_mount_cb(idx: u8, data: *const MountCbData) {
        // SAFETY: the stack passes a valid pointer for the duration of the call
        let d = unsafe { &*data };
        //   the bus position is read HERE, inside the callback, which is when the stack's own
        // information is available; a hub is never announced by any callback of its own
        let mut info = TuhBusInfo::default();
        let bus = if unsafe { tuh_bus_info_get(d.daddr, &mut info) } { Some(BusInfo { hub_addr: info.hub_addr, hub_port: info.hub_port }) } else { None };
        let _ = EVENTS.push(MidiEvent::Mounted { idx, mount: Mount { daddr: d.daddr, rx_cables: d.rx_cable_count, tx_cables: d.tx_cable_count }, bus });
}

#[unsafe(no_mangle)]
extern "C" fn tuh_midi_umount_cb(idx: u8) {
        let _ = EVENTS.push(MidiEvent::Unmounted { idx });
}

/// The host stack, owned by whichever module drives it.
pub struct UsbMidiHost {
        _private: (),
}

impl UsbMidiHost {
        /// Bring the host stack up on this core. Once.
        pub fn init() -> Self {
                unsafe { light_shell_usb_host_init() };
                Self { _private: () }
        }

        /// Run the stack: enumeration, transfers, and the callbacks above. Every pass.
        pub fn task(&mut self) {
                unsafe { light_shell_usb_host_task() }
        }

        /// What the callbacks reported since the last poll.
        pub fn next_event(&mut self) -> Option<MidiEvent> {
                EVENTS.pop()
        }

        pub fn dropped_events(&self) -> u32 {
                EVENTS.dropped()
        }

        /// Tear the controller down and bring it back: the root-port-empty workaround. Blocks
        /// for the settle delay.
        pub fn reset(&mut self) {
                unsafe { light_shell_usb_host_reset() }
        }
}

impl Transport for UsbMidiHost {
        fn read(&mut self, idx: u8) -> Option<Packet> {
                let mut p: Packet = [0; 4];
                if unsafe { tuh_midi_packet_read_n(idx, p.as_mut_ptr(), 4) } == 4 { Some(p) } else { None }
        }
        fn write(&mut self, idx: u8, packet: &Packet) {
                unsafe { tuh_midi_packet_write_n(idx, packet.as_ptr(), 4) };
        }
        fn flush(&mut self, idx: u8) {
                unsafe { tuh_midi_write_flush(idx) };
        }
}
