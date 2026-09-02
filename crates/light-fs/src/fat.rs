//! FAT16/FAT32, read-side: mount, list, look up, read. Written to the Microsoft FAT
//! specification's layout rules with one deliberate scope cut -- 8.3 names only. Long-name
//! entries are recognized and skipped, so a listing shows the short alias a card's own
//! directory carries; matching is case-insensitive, which is FAT's own rule.
//!
//! The implementation owns exactly one 512-byte sector buffer and allocates nothing:
//! directory entries are decoded into 32-byte value types, and a [`File`] is a cursor that
//! borrows nothing -- reads take the filesystem by `&mut`, so files and listings interleave
//! freely without aliasing the buffer.
//!
//! Mount reads sector 0 and takes what it finds: a bare FAT volume (a "superfloppy"), or
//! an MBR whose first FAT-typed partition points at one. exFAT -- the factory format of
//! every SDXC card -- and FAT12 are detected and named in the error rather than misparsed.

use light_core::hal::{BlockDevice, BlockError};

/// "NAME.EXT" at its longest: 8 + dot + 3.
pub const NAME_MAX: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
        Io(BlockError),
        /// Sector 0 is neither a FAT boot sector nor an MBR with a FAT partition.
        NotFat,
        /// The volume is exFAT -- what SDXC cards ship with. Reformat as FAT32 to use it here.
        ExFat,
        /// FAT12: floppy territory, below this crate's floor.
        Fat12,
        NotFound,
        /// A path component that must be a directory is a file.
        NotADirectory,
        /// The named entry is a directory, where a file was required.
        IsADirectory,
        /// A cluster chain left the volume, hit a bad-cluster mark, or looped.
        BadChain,
}

impl From<BlockError> for FsError {
        fn from(e: BlockError) -> Self {
                FsError::Io(e)
        }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VolumeInfo {
        pub fat32: bool,
        pub cluster_count: u32,
        pub bytes_per_cluster: u32,
}

/// One directory entry, decoded: the formatted 8.3 name and the numbers a caller acts on.
#[derive(Clone, Copy, Debug)]
pub struct DirEntry {
        name: [u8; NAME_MAX],
        name_len: u8,
        pub is_dir: bool,
        pub size: u32,
        first_cluster: u32,
}

impl DirEntry {
        /// The entry's name, "NAME.EXT" form. 8.3 names are ASCII by construction here:
        /// any byte outside the printable range was replaced with '?' at decode.
        pub fn name(&self) -> &str {
                core::str::from_utf8(&self.name[..usize::from(self.name_len)]).unwrap_or("?")
        }
}

#[derive(Clone, Copy)]
enum Kind {
        Fat16 { root_start: u32, root_sectors: u32 },
        Fat32 { root_cluster: u32 },
}

/// Where a directory's entries live: the FAT16 root is a fixed run of sectors; everything
/// else is a cluster chain.
#[derive(Clone, Copy)]
enum DirLoc {
        Fixed { start: u32, sectors: u32 },
        Chain { first: u32 },
}

pub struct Fat<D: BlockDevice> {
        dev: D,
        buf: [u8; 512],
        /// Which LBA `buf` holds; `u32::MAX` = nothing yet.
        buf_lba: u32,
        kind: Kind,
        fat_start: u32,
        sectors_per_cluster: u32,
        data_start: u32,
        cluster_count: u32,
}

fn u16le(b: &[u8], off: usize) -> u32 {
        u32::from(b[off]) | u32::from(b[off + 1]) << 8
}

fn u32le(b: &[u8], off: usize) -> u32 {
        u16le(b, off) | u16le(b, off + 2) << 16
}

/// Whether a boot sector reads as a BPB rather than an MBR: an x86 jump, 512-byte
/// sectors, a power-of-two cluster size and a nonzero reserved count. An MBR's bytes at
/// these offsets are partition-loader code, which fails the arithmetic checks.
fn looks_like_bpb(s: &[u8; 512]) -> bool {
        (s[0] == 0xEB || s[0] == 0xE9) && u16le(s, 11) == 512 && s[13] != 0 && s[13].is_power_of_two() && u16le(s, 14) != 0
}

fn is_exfat(s: &[u8; 512]) -> bool {
        &s[3..11] == b"EXFAT   "
}

impl<D: BlockDevice> Fat<D> {
        /// Mount the volume on `dev`: sector 0 directly, or through the first FAT-typed
        /// MBR partition. The device is owned; [`Self::device`] lends it back.
        pub fn mount(dev: D) -> Result<Self, FsError> {
                let mut fs = Self {
                        dev,
                        buf: [0; 512],
                        buf_lba: u32::MAX,
                        kind: Kind::Fat16 { root_start: 0, root_sectors: 0 },
                        fat_start: 0,
                        sectors_per_cluster: 1,
                        data_start: 0,
                        cluster_count: 0,
                };
                fs.load(0)?;
                if fs.buf[510] != 0x55 || fs.buf[511] != 0xAA {
                        return Err(FsError::NotFat);
                }
                if is_exfat(&fs.buf) {
                        return Err(FsError::ExFat);
                }
                let base = if looks_like_bpb(&fs.buf) {
                        0
                } else {
                        //   an MBR: the four partition entries at 446, 16 bytes each --
                        // type at +4, LBA start at +8. 0x07 is exFAT (or NTFS, equally
                        // unsupported); any other nonzero type gets the benefit of the
                        // doubt, since the VBR check below is the real gate
                        let mut base = None;
                        for i in 0..4 {
                                let e = 446 + i * 16;
                                let ptype = fs.buf[e + 4];
                                let start = u32le(&fs.buf, e + 8);
                                if ptype == 0x07 {
                                        return Err(FsError::ExFat);
                                }
                                if ptype != 0 && start != 0 {
                                        base = Some(start);
                                        break;
                                }
                        }
                        let Some(base) = base else { return Err(FsError::NotFat) };
                        fs.load(base)?;
                        if fs.buf[510] != 0x55 || fs.buf[511] != 0xAA {
                                return Err(FsError::NotFat);
                        }
                        if is_exfat(&fs.buf) {
                                return Err(FsError::ExFat);
                        }
                        if !looks_like_bpb(&fs.buf) {
                                return Err(FsError::NotFat);
                        }
                        base
                };

                let spc = u32::from(fs.buf[13]);
                let reserved = u16le(&fs.buf, 14);
                let nfats = u32::from(fs.buf[16]);
                let root_entries = u16le(&fs.buf, 17);
                let total16 = u16le(&fs.buf, 19);
                let total = if total16 != 0 { total16 } else { u32le(&fs.buf, 32) };
                let fat16_size = u16le(&fs.buf, 22);
                let fat_size = if fat16_size != 0 { fat16_size } else { u32le(&fs.buf, 36) };
                if nfats == 0 || fat_size == 0 || total == 0 {
                        return Err(FsError::NotFat);
                }
                let root_sectors = (root_entries * 32).div_ceil(512);
                let fat_start = base + reserved;
                let root_start = fat_start + nfats * fat_size;
                let data_start = root_start + root_sectors;
                let data_sectors = total.saturating_sub(reserved + nfats * fat_size + root_sectors);
                let clusters = data_sectors / spc;
                //   the spec's rule: the TYPE follows the cluster count, nothing else --
                // not the FAT string in the BPB, which lies on real cards
                if clusters < 4085 {
                        return Err(FsError::Fat12);
                }
                fs.kind = if clusters < 65525 {
                        Kind::Fat16 { root_start, root_sectors }
                } else {
                        Kind::Fat32 { root_cluster: u32le(&fs.buf, 44) }
                };
                fs.fat_start = fat_start;
                fs.sectors_per_cluster = spc;
                fs.data_start = data_start;
                fs.cluster_count = clusters;
                Ok(fs)
        }

        pub fn volume_info(&self) -> VolumeInfo {
                VolumeInfo {
                        fat32: matches!(self.kind, Kind::Fat32 { .. }),
                        cluster_count: self.cluster_count,
                        bytes_per_cluster: self.sectors_per_cluster * 512,
                }
        }

        /// The medium back, for anything block-level (a re-init, a raw dump).
        pub fn device(&mut self) -> &mut D {
                &mut self.dev
        }

        fn load(&mut self, lba: u32) -> Result<(), FsError> {
                if self.buf_lba != lba {
                        self.dev.read_block(lba, &mut self.buf)?;
                        self.buf_lba = lba;
                }
                Ok(())
        }

        fn cluster_lba(&self, cluster: u32) -> u32 {
                self.data_start + (cluster - 2) * self.sectors_per_cluster
        }

        /// The FAT's verdict on `cluster`: the next in the chain, or `None` at end-of-chain.
        fn next_cluster(&mut self, cluster: u32) -> Result<Option<u32>, FsError> {
                let (lba, off, value) = match self.kind {
                        Kind::Fat16 { .. } => {
                                let byte = cluster * 2;
                                let lba = self.fat_start + byte / 512;
                                self.load(lba)?;
                                let v = u16le(&self.buf, (byte % 512) as usize);
                                (lba, byte % 512, if v >= 0xFFF8 { None } else { Some(v) })
                        }
                        Kind::Fat32 { .. } => {
                                let byte = cluster * 4;
                                let lba = self.fat_start + byte / 512;
                                self.load(lba)?;
                                let v = u32le(&self.buf, (byte % 512) as usize) & 0x0FFF_FFFF;
                                (lba, byte % 512, if v >= 0x0FFF_FFF8 { None } else { Some(v) })
                        }
                };
                let _ = (lba, off);
                match value {
                        None => Ok(None),
                        //   free, reserved and bad-cluster marks are all wrong in a chain
                        Some(v) if v < 2 || v - 2 >= self.cluster_count => Err(FsError::BadChain),
                        Some(v) => Ok(Some(v)),
                }
        }

        fn root(&self) -> DirLoc {
                match self.kind {
                        Kind::Fat16 { root_start, root_sectors } => DirLoc::Fixed { start: root_start, sectors: root_sectors },
                        Kind::Fat32 { root_cluster } => DirLoc::Chain { first: root_cluster },
                }
        }

        /// Walk `loc`'s entries in order, stopping at the end-of-directory mark or when
        /// `f` answers `true`. LFN, deleted and volume-label slots never reach `f`.
        fn for_each_entry(&mut self, loc: DirLoc, f: &mut dyn FnMut(&DirEntry) -> bool) -> Result<(), FsError> {
                match loc {
                        DirLoc::Fixed { start, sectors } => {
                                for s in 0..sectors {
                                        if self.scan_sector(start + s, f)? {
                                                return Ok(());
                                        }
                                }
                        }
                        DirLoc::Chain { first } => {
                                let mut cluster = first;
                                let mut hops = 0u32;
                                loop {
                                        for s in 0..self.sectors_per_cluster {
                                                if self.scan_sector(self.cluster_lba(cluster) + s, f)? {
                                                        return Ok(());
                                                }
                                        }
                                        match self.next_cluster(cluster)? {
                                                Some(next) => {
                                                        hops += 1;
                                                        if hops > self.cluster_count {
                                                                return Err(FsError::BadChain);
                                                        }
                                                        cluster = next;
                                                }
                                                None => break,
                                        }
                                }
                        }
                }
                Ok(())
        }

        /// One directory sector; `true` = stop (end mark, or `f` said so).
        fn scan_sector(&mut self, lba: u32, f: &mut dyn FnMut(&DirEntry) -> bool) -> Result<bool, FsError> {
                self.load(lba)?;
                for i in 0..16 {
                        let e = &self.buf[i * 32..i * 32 + 32];
                        if e[0] == 0x00 {
                                return Ok(true);
                        }
                        if e[0] == 0xE5 {
                                continue;
                        }
                        let attr = e[11];
                        //   0x0F is the long-name signature; 0x08 the volume label
                        if attr & 0x0F == 0x0F || attr & 0x08 != 0 {
                                continue;
                        }
                        let entry = decode_entry(e);
                        if f(&entry) {
                                return Ok(true);
                        }
                }
                Ok(false)
        }

        /// The directory `path` names ("" or "/" is the root). Components are 8.3 names,
        /// '/'-separated, matched case-insensitively.
        fn resolve_dir(&mut self, path: &str) -> Result<DirLoc, FsError> {
                let mut loc = self.root();
                for part in path.split('/').filter(|p| !p.is_empty()) {
                        let entry = self.find_in(loc, part)?;
                        if !entry.is_dir {
                                return Err(FsError::NotADirectory);
                        }
                        loc = self.dir_loc_of(&entry);
                }
                Ok(loc)
        }

        /// A directory entry's own location as a directory. Cluster 0 in a ".." entry
        /// means the root, per the spec.
        fn dir_loc_of(&self, entry: &DirEntry) -> DirLoc {
                if entry.first_cluster == 0 { self.root() } else { DirLoc::Chain { first: entry.first_cluster } }
        }

        fn find_in(&mut self, loc: DirLoc, name: &str) -> Result<DirEntry, FsError> {
                let mut found: Option<DirEntry> = None;
                self.for_each_entry(loc, &mut |e| {
                        if e.name().eq_ignore_ascii_case(name) {
                                found = Some(*e);
                                true
                        } else {
                                false
                        }
                })?;
                found.ok_or(FsError::NotFound)
        }

        /// Every entry of the directory at `path`, in directory order.
        pub fn list_dir(&mut self, path: &str, mut f: impl FnMut(&DirEntry)) -> Result<(), FsError> {
                let loc = self.resolve_dir(path)?;
                self.for_each_entry(loc, &mut |e| {
                        f(e);
                        false
                })
        }

        /// The entry `path` names -- file or directory.
        pub fn stat(&mut self, path: &str) -> Result<DirEntry, FsError> {
                let (dir, name) = split_path(path);
                let name = if name.is_empty() { return Err(FsError::NotFound) } else { name };
                let loc = self.resolve_dir(dir)?;
                self.find_in(loc, name)
        }

        /// Open the file at `path` for sequential reading.
        pub fn open(&mut self, path: &str) -> Result<File, FsError> {
                let entry = self.stat(path)?;
                if entry.is_dir {
                        return Err(FsError::IsADirectory);
                }
                Ok(File { size: entry.size, pos: 0, cluster: entry.first_cluster, cluster_byte: 0 })
        }
}

/// "A/B/C.TXT" -> ("A/B", "C.TXT").
fn split_path(path: &str) -> (&str, &str) {
        let path = path.trim_matches('/');
        match path.rfind('/') {
                Some(i) => (&path[..i], &path[i + 1..]),
                None => ("", path),
        }
}

fn decode_entry(e: &[u8]) -> DirEntry {
        let mut name = [0u8; NAME_MAX];
        let mut n = 0;
        for i in 0..8 {
                let mut c = e[i];
                if c == b' ' {
                        break;
                }
                //   0x05 in the first byte escapes a real 0xE5; anything non-printable is
                // not worth carrying into a &str
                if i == 0 && c == 0x05 {
                        c = 0xE5;
                }
                name[n] = if c.is_ascii_graphic() { c } else { b'?' };
                n += 1;
        }
        if e[8] != b' ' {
                name[n] = b'.';
                n += 1;
                for i in 8..11 {
                        let c = e[i];
                        if c == b' ' {
                                break;
                        }
                        name[n] = if c.is_ascii_graphic() { c } else { b'?' };
                        n += 1;
                }
        }
        let attr = e[11];
        DirEntry {
                name,
                name_len: n as u8,
                is_dir: attr & 0x10 != 0,
                size: u32le(e, 28),
                first_cluster: (u16le(e, 20) << 16) | u16le(e, 26),
        }
}

/// A sequential read cursor. Borrows nothing: every read takes the filesystem, so an open
/// file costs 16 bytes and any number can exist at once.
#[derive(Clone, Copy, Debug)]
pub struct File {
        size: u32,
        pos: u32,
        /// The cluster `pos` sits in; 0 once past the last (or for an empty file).
        cluster: u32,
        /// How far into that cluster `pos` is.
        cluster_byte: u32,
}

impl File {
        pub fn size(&self) -> u32 {
                self.size
        }

        pub fn pos(&self) -> u32 {
                self.pos
        }

        /// Fill `out` from the current position; the count actually read is short only at
        /// end of file. Interleaves freely with other reads and listings on `fs`.
        pub fn read<D: BlockDevice>(&mut self, fs: &mut Fat<D>, out: &mut [u8]) -> Result<usize, FsError> {
                let cluster_bytes = fs.sectors_per_cluster * 512;
                let mut done = 0usize;
                while done < out.len() && self.pos < self.size {
                        if self.cluster_byte == cluster_bytes {
                                match fs.next_cluster(self.cluster)? {
                                        Some(next) => {
                                                self.cluster = next;
                                                self.cluster_byte = 0;
                                        }
                                        //   the chain ended before the size did: a
                                        // truncated file reads short rather than failing
                                        None => break,
                                }
                        }
                        if self.cluster < 2 {
                                break;
                        }
                        let lba = fs.cluster_lba(self.cluster) + self.cluster_byte / 512;
                        fs.load(lba)?;
                        let in_sector = (self.cluster_byte % 512) as usize;
                        let want = out.len() - done;
                        let n = (512 - in_sector).min(want).min((self.size - self.pos) as usize);
                        out[done..done + n].copy_from_slice(&fs.buf[in_sector..in_sector + n]);
                        done += n;
                        self.pos += n as u32;
                        self.cluster_byte += n as u32;
                }
                Ok(done)
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use std::vec::Vec;

        struct MemDev(Vec<u8>);
        impl BlockDevice for MemDev {
                fn block_count(&self) -> u32 {
                        (self.0.len() / 512) as u32
                }
                fn read_block(&mut self, lba: u32, out: &mut [u8; 512]) -> Result<(), BlockError> {
                        let s = lba as usize * 512;
                        if s + 512 > self.0.len() {
                                return Err(BlockError::OutOfRange);
                        }
                        out.copy_from_slice(&self.0[s..s + 512]);
                        Ok(())
                }
                fn write_block(&mut self, lba: u32, data: &[u8; 512]) -> Result<(), BlockError> {
                        let s = lba as usize * 512;
                        if s + 512 > self.0.len() {
                                return Err(BlockError::OutOfRange);
                        }
                        self.0[s..s + 512].copy_from_slice(data);
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

        /// A FAT16 volume laid out by hand at sector `base` of `d`, per the spec: 4200
        /// total sectors, 1 reserved, one 17-sector FAT, a 1-sector root (16 entries),
        /// 1 sector per cluster -> 4181 clusters (>= 4085 and < 65525: FAT16 by count).
        /// Contents: HELLO.TXT (700 bytes across clusters 2-3), SUB/ (cluster 4)
        /// containing DEEP.BIN (5 bytes, cluster 5) -- plus a volume label, a deleted
        /// entry and an LFN slot the walk must skip.
        fn build_fat16(d: &mut [u8], base: usize) {
                let s = &mut d[base * 512..];
                s[0] = 0xEB;
                s[1] = 0x3C;
                s[2] = 0x90;
                put16(s, 11, 512);
                s[13] = 1;
                put16(s, 14, 1);
                s[16] = 1;
                put16(s, 17, 16);
                put16(s, 19, 4200);
                s[21] = 0xF8;
                put16(s, 22, 17);
                s[510] = 0x55;
                s[511] = 0xAA;
                //   the FAT at sector base+1
                let fat = (base + 1) * 512;
                put16(&mut d[fat..], 0, 0xFFF8);
                put16(&mut d[fat..], 2, 0xFFFF);
                put16(&mut d[fat..], 4, 3); // cluster 2 -> 3
                put16(&mut d[fat..], 6, 0xFFFF); // 3: end
                put16(&mut d[fat..], 8, 0xFFFF); // 4 (SUB): end
                put16(&mut d[fat..], 10, 0xFFFF); // 5 (DEEP.BIN): end
                //   root at base+18 (1 + 17)
                let root = (base + 18) * 512;
                d[root..root + 32].copy_from_slice(&dirent(b"VOLLABEL   ", 0x08, 0, 0));
                let mut deleted = dirent(b"OLD     TXT", 0x20, 9, 1);
                deleted[0] = 0xE5;
                d[root + 32..root + 64].copy_from_slice(&deleted);
                let mut lfn = [0u8; 32];
                lfn[0] = 0x41;
                lfn[11] = 0x0F;
                d[root + 64..root + 96].copy_from_slice(&lfn);
                d[root + 96..root + 128].copy_from_slice(&dirent(b"HELLO   TXT", 0x20, 2, 700));
                d[root + 128..root + 160].copy_from_slice(&dirent(b"SUB        ", 0x10, 4, 0));
                //   data starts at base+19; cluster N is sector base+19+(N-2)
                let c = |n: usize| (base + 19 + (n - 2)) * 512;
                for b in d[c(2)..c(2) + 512].iter_mut() {
                        *b = b'A';
                }
                for b in d[c(3)..c(3) + 188].iter_mut() {
                        *b = b'B';
                }
                let sub = c(4);
                d[sub..sub + 32].copy_from_slice(&dirent(b".          ", 0x10, 4, 0));
                d[sub + 32..sub + 64].copy_from_slice(&dirent(b"..         ", 0x10, 0, 0));
                d[sub + 64..sub + 96].copy_from_slice(&dirent(b"DEEP    BIN", 0x20, 5, 5));
                d[c(5)..c(5) + 5].copy_from_slice(b"deep!");
        }

        fn fat16_superfloppy() -> MemDev {
                let mut d = std::vec![0u8; 4200 * 512];
                build_fat16(&mut d, 0);
                MemDev(d)
        }

        #[test]
        fn mounts_fat16_and_lists_the_root_without_label_lfn_or_deleted_entries() {
                let mut fs = Fat::mount(fat16_superfloppy()).unwrap();
                let info = fs.volume_info();
                assert!(!info.fat32);
                assert_eq!(info.cluster_count, 4181);
                let mut names: Vec<std::string::String> = Vec::new();
                fs.list_dir("", |e| names.push(e.name().into())).unwrap();
                assert_eq!(names, ["HELLO.TXT", "SUB"]);
        }

        #[test]
        fn reads_a_file_across_a_cluster_boundary_and_stops_at_its_size() {
                let mut fs = Fat::mount(fat16_superfloppy()).unwrap();
                let mut f = fs.open("HELLO.TXT").unwrap();
                assert_eq!(f.size(), 700);
                let mut buf = std::vec![0u8; 4096];
                let n = f.read(&mut fs, &mut buf).unwrap();
                assert_eq!(n, 700);
                assert!(buf[..512].iter().all(|b| *b == b'A'));
                assert!(buf[512..700].iter().all(|b| *b == b'B'));
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 0, "a second read answers EOF");
        }

        #[test]
        fn partial_reads_resume_where_they_left_off() {
                let mut fs = Fat::mount(fat16_superfloppy()).unwrap();
                let mut f = fs.open("HELLO.TXT").unwrap();
                let mut a = [0u8; 500];
                let mut b = [0u8; 500];
                assert_eq!(f.read(&mut fs, &mut a).unwrap(), 500);
                assert_eq!(f.read(&mut fs, &mut b).unwrap(), 200);
                assert!(a.iter().all(|x| *x == b'A'));
                assert!(b[..12].iter().all(|x| *x == b'A'));
                assert!(b[12..200].iter().all(|x| *x == b'B'));
        }

        #[test]
        fn paths_descend_directories_case_insensitively() {
                let mut fs = Fat::mount(fat16_superfloppy()).unwrap();
                let mut f = fs.open("sub/deep.bin").unwrap();
                let mut buf = [0u8; 16];
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 5);
                assert_eq!(&buf[..5], b"deep!");
                assert_eq!(fs.stat("SUB").unwrap().is_dir, true);
                assert_eq!(fs.open("NOPE.TXT").unwrap_err(), FsError::NotFound);
                assert_eq!(fs.open("HELLO.TXT/X").unwrap_err(), FsError::NotADirectory);
                assert_eq!(fs.open("SUB").unwrap_err(), FsError::IsADirectory);
        }

        #[test]
        fn an_mbr_partition_is_followed_to_its_volume() {
                let mut d = std::vec![0u8; (64 + 4200) * 512];
                //   the MBR: one 0x0C (FAT32 LBA) partition starting at sector 64
                d[446 + 4] = 0x0C;
                put32(&mut d, 446 + 8, 64);
                d[510] = 0x55;
                d[511] = 0xAA;
                build_fat16(&mut d, 64);
                let mut fs = Fat::mount(MemDev(d)).unwrap();
                let mut names: Vec<std::string::String> = Vec::new();
                fs.list_dir("/", |e| names.push(e.name().into())).unwrap();
                assert_eq!(names, ["HELLO.TXT", "SUB"]);
        }

        #[test]
        fn mounts_fat32_where_the_root_is_a_chain() {
                //   66200 total, 32 reserved, one 518-sector FAT, 1 sector per cluster:
                // 65650 clusters >= 65525 -> FAT32 by count. Root chain at cluster 2.
                let total = 66200usize;
                let mut d = std::vec![0u8; total * 512];
                d[0] = 0xEB;
                d[1] = 0x58;
                d[2] = 0x90;
                put16(&mut d, 11, 512);
                d[13] = 1;
                put16(&mut d, 14, 32);
                d[16] = 1;
                put16(&mut d, 17, 0);
                put16(&mut d, 19, 0);
                d[21] = 0xF8;
                put16(&mut d, 22, 0);
                put32(&mut d, 32, total as u32);
                put32(&mut d, 36, 518);
                put32(&mut d, 44, 2);
                d[510] = 0x55;
                d[511] = 0xAA;
                let fat = 32 * 512;
                put32(&mut d, fat, 0x0FFF_FFF8);
                put32(&mut d, fat + 4, 0x0FFF_FFFF);
                put32(&mut d, fat + 8, 0x0FFF_FFFF); // root, cluster 2
                put32(&mut d, fat + 12, 0x0FFF_FFFF); // BIG.TXT, cluster 3
                let data = (32 + 518) * 512;
                d[data..data + 32].copy_from_slice(&dirent(b"BIG     TXT", 0x20, 3, 3));
                d[data + 512..data + 512 + 3].copy_from_slice(b"big");
                let mut fs = Fat::mount(MemDev(d)).unwrap();
                assert!(fs.volume_info().fat32);
                let mut f = fs.open("BIG.TXT").unwrap();
                let mut buf = [0u8; 8];
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 3);
                assert_eq!(&buf[..3], b"big");
        }

        #[test]
        fn exfat_and_garbage_are_named_not_misparsed() {
                let mut d = std::vec![0u8; 512];
                d[3..11].copy_from_slice(b"EXFAT   ");
                d[510] = 0x55;
                d[511] = 0xAA;
                assert!(matches!(Fat::mount(MemDev(d)), Err(FsError::ExFat)));
                let mut d = std::vec![0u8; 512];
                d[446 + 4] = 0x07; // an exFAT/NTFS partition in the MBR
                put32(&mut d, 446 + 8, 64);
                d[510] = 0x55;
                d[511] = 0xAA;
                assert!(matches!(Fat::mount(MemDev(d)), Err(FsError::ExFat)));
                let d = std::vec![0u8; 512];
                assert!(matches!(Fat::mount(MemDev(d)), Err(FsError::NotFat)));
        }
}
