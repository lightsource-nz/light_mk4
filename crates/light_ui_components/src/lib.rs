//! Higher-level UI components that augment [`light_ui`] by wiring it to other parts of the
//! platform. `light_ui` is deliberately UI-only -- it draws a list and reports which row was
//! tapped, but knows nothing of a filesystem, a clock, or a network. This crate is where those
//! meet the toolkit, so it depends, by design, on framework modules `light_ui` does not.
//!
//! The first component is [`FilePicker`]: it lists a directory through the filesystem API
//! ([`light_fs`]) and fills a [`light_ui` list](light_ui::file_list) with the result, turning a
//! tapped row index into the file to open. The application still owns the pieces the picker
//! connects -- it authors the list's rows and mounts the card -- keeping each layer's job its own.

#![no_std]

use light_core::hal::BlockDevice;
use light_fs::{DirEntry, Fat, FsError};
use light_ui::Ui;

/// Longest entry name the picker keeps; longer names are truncated at a char boundary. A list
/// row is far narrower than this anyway, so the cap only bounds storage.
const NAME_CAP: usize = 32;

/// One captured directory entry. Owned (the borrowed [`DirEntry`] does not outlive the scan),
/// fixed size, so a `FilePicker` needs no allocator.
#[derive(Clone, Copy)]
struct Item {
        name: [u8; NAME_CAP],
        name_len: u8,
        size: u32,
        is_dir: bool,
}

impl Item {
        fn new(name: &str, size: u32, is_dir: bool) -> Self {
                let mut buf = [0u8; NAME_CAP];
                let mut take = name.len().min(NAME_CAP);
                while !name.is_char_boundary(take) {
                        take -= 1;
                }
                buf[..take].copy_from_slice(&name.as_bytes()[..take]);
                Self { name: buf, name_len: take as u8, size, is_dir }
        }

        fn name(&self) -> &str {
                core::str::from_utf8(&self.name[..usize::from(self.name_len)]).unwrap_or("")
        }
}

/// The order a [`FilePicker`] keeps its entries in, which -- with a bounded capacity -- also
/// decides which entries survive when a directory holds more than fit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
        /// A..Z: keeps the first `CAP` names.
        NameAscending,
        /// Z..A: keeps the last `CAP` names -- newest first for zero-padded names like
        /// `REC_0007.WAV`.
        NameDescending,
}

/// A directory listing bound to a [`light_ui` list](light_ui::file_list): scan a directory,
/// fill the rows, and turn a tapped row index back into a filename. Holds at most `CAP` entries
/// -- size it to the list's row count -- with no allocator.
///
/// ```ignore
/// static mut PICKER: FilePicker<8> = FilePicker::new(Order::NameDescending);
/// // on open: mount the card, then
/// picker.scan(&mut fs, "/", |e| !e.is_dir && e.name().ends_with(".WAV"))?;
/// picker.fill(&mut ui, TAG_ROW_BASE, "-");
/// // on a row tap: picker.name(i) is the file to open
/// ```
pub struct FilePicker<const CAP: usize> {
        items: [Option<Item>; CAP],
        len: usize,
        order: Order,
}

impl<const CAP: usize> FilePicker<CAP> {
        pub const fn new(order: Order) -> Self {
                Self { items: [None; CAP], len: 0, order }
        }

        /// Drop every entry. `scan` does this first; call it directly to blank a list.
        pub fn clear(&mut self) {
                self.items = [None; CAP];
                self.len = 0;
        }

        pub fn len(&self) -> usize {
                self.len
        }

        pub fn is_empty(&self) -> bool {
                self.len == 0
        }

        /// The name at a row index -- the file to open for a tapped row -- or `None` past the end.
        pub fn name(&self, index: usize) -> Option<&str> {
                self.items.get(index).and_then(Option::as_ref).map(Item::name)
        }

        /// The size at a row index, or `None` past the end.
        pub fn size(&self, index: usize) -> Option<u32> {
                self.items.get(index).and_then(Option::as_ref).map(|i| i.size)
        }

        /// Whether the entry at a row index is a directory.
        pub fn is_dir(&self, index: usize) -> Option<bool> {
                self.items.get(index).and_then(Option::as_ref).map(|i| i.is_dir)
        }

        /// Insert into the ordered, capacity-bounded list, keeping the best `CAP` by [`Order`].
        /// Kept separate from the filesystem so ordering and capacity are testable on their own.
        fn offer(&mut self, item: Item) {
                let mut pos = self.len;
                for i in 0..self.len {
                        let existing = self.items[i].as_ref().expect("kept slots are contiguous");
                        let before = match self.order {
                                Order::NameAscending => item.name() < existing.name(),
                                Order::NameDescending => item.name() > existing.name(),
                        };
                        if before {
                                pos = i;
                                break;
                        }
                }
                if pos >= CAP {
                        return; // worse than every kept entry, and no room
                }
                // shift the tail down one; a full list drops its last (worst) entry off the end
                let mut j = self.len.min(CAP - 1);
                while j > pos {
                        self.items[j] = self.items[j - 1];
                        j -= 1;
                }
                self.items[pos] = Some(item);
                if self.len < CAP {
                        self.len += 1;
                }
        }

        /// List `dir` on a mounted filesystem, keeping the entries `keep` accepts, ordered and
        /// bounded to `CAP`. The display name (the long name when present, else the 8.3 name) and
        /// size are copied in; nothing borrows the transient entry. Replaces the previous listing.
        pub fn scan<D: BlockDevice>(&mut self, fs: &mut Fat<D>, dir: &str, keep: impl Fn(&DirEntry) -> bool) -> Result<(), FsError> {
                self.clear();
                fs.list_dir(dir, |e| {
                        if keep(e) {
                                let name = e.long_name().unwrap_or_else(|| e.name());
                                self.offer(Item::new(name, e.size, e.is_dir));
                        }
                })
        }

        /// Write the scanned names into a [`light_ui::file_list!`] at `tag_base`, one per row from
        /// row 0, `placeholder` for the rows past the end. Uses [`Ui::set_list_text`], the
        /// toolkit's row-fill primitive, so the empty-row convention lives in one place.
        pub fn fill<A: Copy, const N: usize>(&self, ui: &mut Ui<A, N>, tag_base: u8, placeholder: &'static str) {
                for i in 0..CAP {
                        ui.set_list_text(tag_base, i as u8, self.name(i).unwrap_or(""), placeholder);
                }
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use light_core::hal::BlockError;
        use std::vec;
        use std::vec::Vec;

        // --- the data logic, no filesystem ---

        fn offer_names<const CAP: usize>(order: Order, names: &[&str]) -> FilePicker<CAP> {
                let mut p = FilePicker::<CAP>::new(order);
                for n in names {
                        p.offer(Item::new(n, 0, false));
                }
                p
        }

        fn collected<const CAP: usize>(p: &FilePicker<CAP>) -> Vec<&str> {
                (0..p.len()).map(|i| p.name(i).unwrap()).collect()
        }

        #[test]
        fn offer_orders_ascending_and_descending() {
                let asc = offer_names::<8>(Order::NameAscending, &["C", "A", "B"]);
                assert_eq!(collected(&asc), ["A", "B", "C"]);
                let desc = offer_names::<8>(Order::NameDescending, &["C", "A", "B"]);
                assert_eq!(collected(&desc), ["C", "B", "A"]);
        }

        #[test]
        fn offer_keeps_the_best_cap_and_drops_the_overflow() {
                // descending, room for 3: the three largest, in order, whatever the arrival order
                let p = offer_names::<3>(Order::NameDescending, &["A", "E", "C", "B", "D"]);
                assert_eq!(p.len(), 3);
                assert_eq!(collected(&p), ["E", "D", "C"]);
                // ascending keeps the three smallest
                let p = offer_names::<3>(Order::NameAscending, &["A", "E", "C", "B", "D"]);
                assert_eq!(collected(&p), ["A", "B", "C"]);
        }

        #[test]
        fn a_name_past_the_end_reads_none() {
                let p = offer_names::<8>(Order::NameAscending, &["X"]);
                assert_eq!(p.name(0), Some("X"));
                assert_eq!(p.name(1), None);
                assert_eq!(p.size(9), None);
        }

        // --- a real scan over a mock BlockDevice + a small FAT16 image ---

        struct MemDev(Vec<u8>);
        impl BlockDevice for MemDev {
                fn block_count(&self) -> u32 {
                        (self.0.len() / 512) as u32
                }
                fn read_block(&mut self, lba: u32, out: &mut [u8; 512]) -> Result<(), BlockError> {
                        let s = lba as usize * 512;
                        out.copy_from_slice(self.0.get(s..s + 512).ok_or(BlockError::OutOfRange)?);
                        Ok(())
                }
                fn write_block(&mut self, lba: u32, data: &[u8; 512]) -> Result<(), BlockError> {
                        let s = lba as usize * 512;
                        self.0.get_mut(s..s + 512).ok_or(BlockError::OutOfRange)?.copy_from_slice(data);
                        Ok(())
                }
        }

        fn put16(d: &mut [u8], off: usize, v: u16) {
                d[off..off + 2].copy_from_slice(&v.to_le_bytes());
        }
        fn put32(d: &mut [u8], off: usize, v: u32) {
                d[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        fn dirent(name11: &[u8; 11], attr: u8, cluster: u32, size: u32) -> [u8; 32] {
                let mut e = [0u8; 32];
                e[..11].copy_from_slice(name11);
                e[11] = attr;
                put16(&mut e, 20, (cluster >> 16) as u16);
                put16(&mut e, 26, cluster as u16);
                put32(&mut e, 28, size);
                e
        }

        //   a FAT16 super-floppy: reserved sector, two 17-sector FATs, a one-sector root at
        // sector 35, data from 36 -- the same shape light-fs's own tests mount. The root holds
        // three .WAV files, a .TXT, and a subdirectory.
        fn fat16_with_recordings() -> MemDev {
                let mut d = vec![0u8; 4200 * 512];
                d[0] = 0xEB;
                d[1] = 0x3C;
                d[2] = 0x90;
                put16(&mut d, 11, 512); // bytes/sector
                d[13] = 1; // sectors/cluster
                put16(&mut d, 14, 1); // reserved sectors
                d[16] = 2; // FAT count
                put16(&mut d, 17, 16); // root entries
                put16(&mut d, 19, 4200); // total sectors
                d[21] = 0xF8; // media
                put16(&mut d, 22, 17); // sectors/FAT
                d[510] = 0x55;
                d[511] = 0xAA;
                for fat in [1usize, 18] {
                        let f = fat * 512;
                        put16(&mut d, f, 0xFFF8);
                        put16(&mut d, f + 2, 0xFFFF);
                        for c in 2..=6 {
                                put16(&mut d, f + c * 2, 0xFFFF); // each file/dir one end-of-chain cluster
                        }
                }
                let root = 35 * 512;
                let entries = [
                        dirent(b"REC_0001WAV", 0x20, 2, 100),
                        dirent(b"REC_0003WAV", 0x20, 3, 300),
                        dirent(b"NOTES   TXT", 0x20, 4, 50),
                        dirent(b"REC_0002WAV", 0x20, 5, 200),
                        dirent(b"SUB        ", 0x10, 6, 0),
                ];
                for (i, e) in entries.iter().enumerate() {
                        d[root + i * 32..root + i * 32 + 32].copy_from_slice(e);
                }
                MemDev(d)
        }

        #[test]
        fn scan_lists_matching_files_newest_first() {
                let mut fs = Fat::mount(fat16_with_recordings()).unwrap();
                let mut picker = FilePicker::<8>::new(Order::NameDescending);
                picker.scan(&mut fs, "", |e| !e.is_dir && e.name().ends_with(".WAV")).unwrap();
                // the three .WAV files, newest (highest number) first; the .TXT and the dir skipped
                assert_eq!(collected(&picker), ["REC_0003.WAV", "REC_0002.WAV", "REC_0001.WAV"]);
                assert_eq!(picker.size(0), Some(300));
        }

        #[test]
        fn scan_is_bounded_by_capacity() {
                let mut fs = Fat::mount(fat16_with_recordings()).unwrap();
                let mut picker = FilePicker::<2>::new(Order::NameDescending);
                picker.scan(&mut fs, "", |e| e.name().ends_with(".WAV")).unwrap();
                assert_eq!(collected(&picker), ["REC_0003.WAV", "REC_0002.WAV"]);
        }

        // --- filling a light_ui list ---

        #[derive(Clone, Copy)]
        enum Ev {
                Pick(u8),
        }

        light_ui::file_list! {
                FILE_ROWS,
                event: Ev,
                tag_base: 0x10u8,
                min_size: (0, 10),
                select: |i| Ev::Pick(i),
                indices: [0, 1, 2, 3],
        }
        static FILES_WIN: light_ui::Desc<Ev> = light_ui::Desc::window("Files").stack(0).children(FILE_ROWS);
        static FILES_PAGE: light_ui::Page<Ev> = light_ui::Page::new(&FILES_WIN, None);

        #[test]
        fn fill_writes_names_to_the_rows_and_a_placeholder_past_the_end() {
                let mut ui: Ui<Ev, 16> = Ui::new();
                ui.navigate(&FILES_PAGE).unwrap();
                let picker = offer_names::<4>(Order::NameDescending, &["REC_0003.WAV", "REC_0001.WAV"]);
                picker.fill(&mut ui, 0x10, "-");
                let row = |i: u8| ui.widget_text(ui.find(0x10 + i).unwrap()).unwrap();
                assert_eq!(row(0), "REC_0003.WAV");
                assert_eq!(row(1), "REC_0001.WAV");
                assert_eq!(row(2), "-", "rows past the end show the placeholder");
                assert_eq!(row(3), "-");
        }
}
