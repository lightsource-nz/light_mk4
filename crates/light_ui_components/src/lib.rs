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
pub use light_fs::DirEntry;
use light_fs::{Fat, FsError};
use light_ui::Ui;

/// A predicate deciding whether a directory entry belongs in the listing, chosen by the app at
/// construction. A bare `fn` (not a boxed closure) so it stays `const`-constructible and nameable
/// in a `static` picker's type.
pub type Filter = fn(&DirEntry) -> bool;

/// The default [`Filter`]: keeps every entry. Pass it when the caller wants no filtering.
pub fn keep_all(_entry: &DirEntry) -> bool {
        true
}

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

impl Order {
        /// The opposite order. Paging a page *backward* selects with the reversed order (keep the
        /// entries nearest the current top rather than the far end), then flips the result back.
        const fn reversed(self) -> Order {
                match self {
                        Order::NameAscending => Order::NameDescending,
                        Order::NameDescending => Order::NameAscending,
                }
        }

        /// Whether `a` sorts before `b` in this display order -- the single comparison every
        /// ordering, windowing and paging decision goes through.
        fn sorts_before(self, a: &str, b: &str) -> bool {
                match self {
                        Order::NameAscending => a < b,
                        Order::NameDescending => a > b,
                }
        }
}

/// A directory listing bound to a [`light_ui` list](light_ui::file_list): scan a directory,
/// fill the rows, and turn a tapped row index back into a filename. Shows one page of at most
/// `CAP` entries -- size it to the list's row count -- with no allocator.
///
/// The app fixes the two policy choices at construction: the [`Order`] entries are kept in, and
/// the [`Filter`] deciding which entries belong (skipping directories, empty files, the wrong
/// extension -- whatever the app means). `scan` then just lists a directory against them.
///
/// A directory holding more than `CAP` matches is **paged**: [`scan`](Self::scan) shows the first
/// page, and [`next_page`](Self::next_page)/[`prev_page`](Self::prev_page) step through the rest so
/// every match is reachable regardless of `CAP`. [`has_next`](Self::has_next)/
/// [`has_prev`](Self::has_prev) say which steps exist, for showing or hiding the nav buttons.
/// Each step re-lists the directory and keeps only the one page's window, so a huge directory
/// still costs only `CAP` entries of storage.
///
/// ```ignore
/// fn keep_wav(e: &DirEntry) -> bool { !e.is_dir && e.name().ends_with(".WAV") }
/// static mut PICKER: FilePicker<8> = FilePicker::new(Order::NameDescending, keep_wav);
/// // on open: mount the card, then
/// picker.scan(&mut fs, "/")?;
/// picker.fill(&mut ui, TAG_ROW_BASE);
/// // on a row tap: picker.name(i) is the file to open; on a nav tap:
/// if forward { picker.next_page(&mut fs, "/")? } else { picker.prev_page(&mut fs, "/")? };
/// ```
pub struct FilePicker<const CAP: usize> {
        items: [Option<Item>; CAP],
        len: usize,
        order: Order,
        filter: Filter,
        /// Current page, 0-based; `scan` resets it to 0, `next_page`/`prev_page` step it.
        page: u32,
        /// Total entries the filter accepted across the whole directory, from the last list --
        /// what decides whether a next page exists.
        total: usize,
}

/// Which slice of the ordered listing a scan should keep: the first page, or the page just after
/// or just before a boundary entry (its owned name). The boundary is copied out of the picker
/// before the re-list, so nothing borrows across the mutation.
enum Window {
        First,
        After(Item),
        Before(Item),
}

impl<const CAP: usize> FilePicker<CAP> {
        pub const fn new(order: Order, filter: Filter) -> Self {
                Self { items: [None; CAP], len: 0, order, filter, page: 0, total: 0 }
        }

        /// Drop every entry and reset to an empty, single, first page. `scan` clears the window
        /// this way before each re-list; call it directly to blank a list (e.g. no card mounted).
        pub fn clear(&mut self) {
                self.items = [None; CAP];
                self.len = 0;
                self.page = 0;
                self.total = 0;
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

        /// Whether a next page of entries exists past the current window.
        pub fn has_next(&self) -> bool {
                self.total > (self.page as usize + 1) * CAP
        }

        /// Whether a previous page exists before the current window.
        pub fn has_prev(&self) -> bool {
                self.page > 0
        }

        /// The current page index, 0-based.
        pub fn page(&self) -> u32 {
                self.page
        }

        /// Total entries the filter accepted across the whole directory, from the last list --
        /// the count paged through, not the `CAP` shown at once.
        pub fn total(&self) -> usize {
                self.total
        }

        /// Insert into the ordered, capacity-bounded window, keeping the best `CAP` by `order`.
        /// The order is a parameter, not `self.order`, because paging backward selects under the
        /// reversed order (see [`build`](Self::build)). Kept separate from the filesystem so
        /// ordering and capacity are testable on their own.
        fn offer(&mut self, item: Item, order: Order) {
                let mut pos = self.len;
                for i in 0..self.len {
                        let existing = self.items[i].as_ref().expect("kept slots are contiguous");
                        if order.sorts_before(item.name(), existing.name()) {
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

        /// Re-list `dir` and keep the one page named by `window`, updating the total. The heart of
        /// both the first scan and every page turn.
        ///
        /// Paging is keyset (cursor), not offset: rather than skip N entries -- which would need
        /// the whole ordered listing in memory -- each page is defined relative to the boundary
        /// entry it adjoins. The *next* page keeps the best `CAP` entries that sort *after* the
        /// current window's last; the *previous* page wants the `CAP` that sort *before* the
        /// current first and lie *nearest* it, which is the best `CAP` under the reversed order,
        /// flipped back afterwards. So only `CAP` entries are ever held, whatever the page.
        fn build<D: BlockDevice>(&mut self, fs: &mut Fat<D>, dir: &str, window: Window) -> Result<(), FsError> {
                self.items = [None; CAP];
                self.len = 0;
                let order = self.order;
                let reversed = matches!(window, Window::Before(_));
                let sel_order = if reversed { order.reversed() } else { order };
                let filter = self.filter;
                let mut total = 0usize;
                fs.list_dir(dir, |e| {
                        if !filter(e) {
                                return;
                        }
                        total += 1;
                        let name = e.long_name().unwrap_or_else(|| e.name());
                        let in_window = match &window {
                                Window::First => true,
                                // "after b" and "before b" in the picker's own display order
                                Window::After(b) => order.sorts_before(b.name(), name),
                                Window::Before(b) => order.sorts_before(name, b.name()),
                        };
                        if in_window {
                                self.offer(Item::new(name, e.size, e.is_dir), sel_order);
                        }
                })?;
                if reversed {
                        self.items[..self.len].reverse();
                }
                self.total = total;
                Ok(())
        }

        /// List `dir` on a mounted filesystem, keeping the entries this picker's [`Filter`] accepts,
        /// ordered and bounded to `CAP`. The display name (the long name when present, else the 8.3
        /// name) and size are copied in; nothing borrows the transient entry. Shows the first page
        /// and resets paging; use [`next_page`](Self::next_page)/[`prev_page`](Self::prev_page) for
        /// the rest.
        pub fn scan<D: BlockDevice>(&mut self, fs: &mut Fat<D>, dir: &str) -> Result<(), FsError> {
                self.page = 0;
                self.build(fs, dir, Window::First)
        }

        /// Advance to the next page, re-listing `dir`; a no-op returning `Ok(false)` when
        /// [`has_next`](Self::has_next) is false.
        pub fn next_page<D: BlockDevice>(&mut self, fs: &mut Fat<D>, dir: &str) -> Result<bool, FsError> {
                if !self.has_next() {
                        return Ok(false);
                }
                let last = match self.items[self.len - 1] {
                        Some(item) => item,
                        None => return Ok(false),
                };
                let target = self.page + 1;
                self.build(fs, dir, Window::After(last))?;
                self.page = target;
                Ok(true)
        }

        /// Step back to the previous page, re-listing `dir`; a no-op returning `Ok(false)` when
        /// [`has_prev`](Self::has_prev) is false.
        pub fn prev_page<D: BlockDevice>(&mut self, fs: &mut Fat<D>, dir: &str) -> Result<bool, FsError> {
                if !self.has_prev() {
                        return Ok(false);
                }
                let first = match self.items[0] {
                        Some(item) => item,
                        None => return Ok(false),
                };
                let target = self.page - 1;
                self.build(fs, dir, Window::Before(first))?;
                self.page = target;
                Ok(true)
        }

        /// Write the scanned names into a [`light_ui::file_list!`] at `tag_base`, one per row from
        /// row 0, and HIDE the rows past the end so a partly-filled page (the last one) collapses
        /// to just its entries rather than trailing blank cells. Relays the tree so the freed
        /// space closes up. Uses [`Ui::set_list_row`], the toolkit's row-fill primitive.
        pub fn fill<A: Copy, const N: usize>(&self, ui: &mut Ui<A, N>, tag_base: u8) {
                for i in 0..CAP {
                        ui.set_list_row(tag_base, i as u8, self.name(i).unwrap_or(""));
                }
                ui.relayout();
        }

        /// Show or hide the paging buttons to match [`has_prev`](Self::has_prev)/
        /// [`has_next`](Self::has_next): the "previous" widget at `prev_tag` and the "next" widget
        /// at `next_tag` are made visible only when that step exists, then the tree is relaid so a
        /// hidden button reclaims its space. Pair with [`fill`](Self::fill) after each page turn.
        pub fn fill_nav<A: Copy, const N: usize>(&self, ui: &mut Ui<A, N>, prev_tag: u8, next_tag: u8) {
                if let Some(id) = ui.find(prev_tag) {
                        ui.set_visible(id, self.has_prev());
                }
                if let Some(id) = ui.find(next_tag) {
                        ui.set_visible(id, self.has_next());
                }
                ui.relayout();
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
                let mut p = FilePicker::<CAP>::new(order, keep_all);
                for n in names {
                        p.offer(Item::new(n, 0, false), order);
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

        fn keep_wav(e: &DirEntry) -> bool {
                !e.is_dir && e.name().ends_with(".WAV")
        }

        //   the same super-floppy shape, but five single-letter files A..E in clusters 2..6, for
        // exercising paging (capacity two gives three pages).
        fn fat16_five_letters() -> MemDev {
                let mut d = vec![0u8; 4200 * 512];
                d[0] = 0xEB;
                d[1] = 0x3C;
                d[2] = 0x90;
                put16(&mut d, 11, 512);
                d[13] = 1;
                put16(&mut d, 14, 1);
                d[16] = 2;
                put16(&mut d, 17, 16);
                put16(&mut d, 19, 4200);
                d[21] = 0xF8;
                put16(&mut d, 22, 17);
                d[510] = 0x55;
                d[511] = 0xAA;
                for fat in [1usize, 18] {
                        let f = fat * 512;
                        put16(&mut d, f, 0xFFF8);
                        put16(&mut d, f + 2, 0xFFFF);
                        for c in 2..=6 {
                                put16(&mut d, f + c * 2, 0xFFFF);
                        }
                }
                let root = 35 * 512;
                let names = [b"A          ", b"B          ", b"C          ", b"D          ", b"E          "];
                for (i, name) in names.iter().enumerate() {
                        let e = dirent(name, 0x20, (i + 2) as u32, 10);
                        d[root + i * 32..root + i * 32 + 32].copy_from_slice(&e);
                }
                MemDev(d)
        }

        #[test]
        fn scan_lists_matching_files_newest_first() {
                let mut fs = Fat::mount(fat16_with_recordings()).unwrap();
                let mut picker = FilePicker::<8>::new(Order::NameDescending, keep_wav);
                picker.scan(&mut fs, "").unwrap();
                // the three .WAV files, newest (highest number) first; the .TXT and the dir skipped
                assert_eq!(collected(&picker), ["REC_0003.WAV", "REC_0002.WAV", "REC_0001.WAV"]);
                assert_eq!(picker.size(0), Some(300));
        }

        #[test]
        fn scan_is_bounded_by_capacity() {
                let mut fs = Fat::mount(fat16_with_recordings()).unwrap();
                let mut picker = FilePicker::<2>::new(Order::NameDescending, keep_wav);
                picker.scan(&mut fs, "").unwrap();
                assert_eq!(collected(&picker), ["REC_0003.WAV", "REC_0002.WAV"]);
        }

        #[test]
        fn a_directory_that_fits_has_no_pages() {
                let mut fs = Fat::mount(fat16_with_recordings()).unwrap();
                let mut picker = FilePicker::<8>::new(Order::NameDescending, keep_wav);
                picker.scan(&mut fs, "").unwrap();
                assert_eq!(picker.total(), 3);
                assert!(!picker.has_prev());
                assert!(!picker.has_next(), "all three fit on one page");
                assert!(!picker.next_page(&mut fs, "").unwrap(), "next past the last page is a no-op");
        }

        #[test]
        fn next_and_prev_page_reach_every_entry() {
                let mut fs = Fat::mount(fat16_with_recordings()).unwrap();
                // room for two of the three recordings, so there is a second page of one
                let mut picker = FilePicker::<2>::new(Order::NameDescending, keep_wav);

                picker.scan(&mut fs, "").unwrap();
                assert_eq!(collected(&picker), ["REC_0003.WAV", "REC_0002.WAV"]);
                assert_eq!((picker.has_prev(), picker.has_next()), (false, true));

                assert!(picker.next_page(&mut fs, "").unwrap());
                assert_eq!(collected(&picker), ["REC_0001.WAV"], "the overflow the first page dropped");
                assert_eq!((picker.has_prev(), picker.has_next()), (true, false));
                assert!(!picker.next_page(&mut fs, "").unwrap(), "no page past the last");

                assert!(picker.prev_page(&mut fs, "").unwrap());
                assert_eq!(collected(&picker), ["REC_0003.WAV", "REC_0002.WAV"], "back to the first page, in order");
                assert_eq!((picker.has_prev(), picker.has_next()), (false, true));
                assert!(!picker.prev_page(&mut fs, "").unwrap(), "no page before the first");
        }

        #[test]
        fn paging_holds_the_middle_page_in_display_order() {
                // five names, capacity two: pages [E,D] [C,B] [A]; the middle page is the one that
                // exercises both a lower and an upper boundary and the reverse-select for prev.
                let mut fs = Fat::mount(fat16_five_letters()).unwrap();
                let mut picker = FilePicker::<2>::new(Order::NameDescending, keep_all);
                picker.scan(&mut fs, "").unwrap();
                assert_eq!(collected(&picker), ["E", "D"]);
                assert!(picker.next_page(&mut fs, "").unwrap());
                assert_eq!(collected(&picker), ["C", "B"], "middle page, still newest-first");
                assert_eq!((picker.has_prev(), picker.has_next()), (true, true));
                assert!(picker.next_page(&mut fs, "").unwrap());
                assert_eq!(collected(&picker), ["A"]);
                // and all the way back down through the middle
                assert!(picker.prev_page(&mut fs, "").unwrap());
                assert_eq!(collected(&picker), ["C", "B"], "middle page rebuilt from its upper edge");
                assert!(picker.prev_page(&mut fs, "").unwrap());
                assert_eq!(collected(&picker), ["E", "D"]);
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
        fn fill_writes_names_to_the_rows_and_hides_the_rest() {
                let mut ui: Ui<Ev, 16> = Ui::new();
                ui.navigate(&FILES_PAGE).unwrap();
                let picker = offer_names::<4>(Order::NameDescending, &["REC_0003.WAV", "REC_0001.WAV"]);
                picker.fill(&mut ui, 0x10);
                let text = |i: u8| ui.widget_text(ui.find(0x10 + i).unwrap()).unwrap();
                let shown = |i: u8| ui.get(ui.find(0x10 + i).unwrap()).unwrap().visible;
                assert_eq!(text(0), "REC_0003.WAV");
                assert_eq!(text(1), "REC_0001.WAV");
                assert!(shown(0) && shown(1), "the filled rows show");
                assert!(!shown(2) && !shown(3), "rows past the end are hidden, not blank cells");
        }
}
