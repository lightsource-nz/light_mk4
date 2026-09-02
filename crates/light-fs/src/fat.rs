//! FAT16/FAT32: mount, list, look up, read -- and write. Written to the Microsoft FAT
//! specification's layout rules. Names are 8.3 with a bounded long-name READ: LFN chains
//! up to 64 ASCII characters are decoded, checksum-verified against their 8.3 entry, and
//! usable both in listings and in path lookup; longer or non-ASCII names fall back to the
//! short alias, and files are CREATED with 8.3 names only. Matching is case-insensitive,
//! which is FAT's own rule.
//!
//! The implementation owns exactly one 512-byte sector buffer and allocates nothing:
//! directory entries are decoded into value types, and a [`File`] is a cursor that borrows
//! nothing -- reads, writes and seeks take the filesystem by `&mut`, so any number of open
//! files interleave. Writes are WRITE-THROUGH: every mutated sector goes to the medium
//! before the call returns, every FAT copy is kept in step, and the directory entry's size
//! and first cluster are rewritten at the end of each `write` call -- a pulled card loses
//! at most the call in flight.
//!
//! Mount reads sector 0 and takes what it finds: a bare FAT volume (a "superfloppy"), or
//! an MBR whose first FAT-typed partition points at one. exFAT -- the factory format of
//! every SDXC card -- and FAT12 are detected and named in the error rather than misparsed.

use light_core::hal::{BlockDevice, BlockError};

/// "NAME.EXT" at its longest: 8 + dot + 3.
pub const NAME_MAX: usize = 12;

/// The longest long name carried; anything longer falls back to its 8.3 alias.
pub const LONG_NAME_MAX: usize = 64;

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
        /// `create` on a name that already exists.
        Exists,
        /// The FAT holds no free cluster.
        NoSpace,
        /// A FAT16 root directory with no free slot cannot grow.
        DirFull,
        /// Not a legal 8.3 name (creation is 8.3-only) -- or an operation aimed at ".",
        /// "..", or a directory's own subtree.
        BadName,
        /// `rmdir` on a directory that still holds entries.
        NotEmpty,
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

/// Where a directory entry lives on the medium -- what a [`File`] rewrites when its size
/// or first cluster changes.
#[derive(Clone, Copy, Debug)]
struct EntrySlot {
        lba: u32,
        off: u16,
}

/// One directory entry, decoded: the formatted 8.3 name, the checksum-verified long name
/// when one fits, and the numbers a caller acts on.
#[derive(Clone, Copy, Debug)]
pub struct DirEntry {
        name: [u8; NAME_MAX],
        name_len: u8,
        long: [u8; LONG_NAME_MAX],
        long_len: u8,
        pub is_dir: bool,
        pub size: u32,
        first_cluster: u32,
}

impl DirEntry {
        /// The entry's short name, "NAME.EXT" form. 8.3 names are ASCII by construction
        /// here: any byte outside the printable range was replaced with '?' at decode.
        pub fn name(&self) -> &str {
                core::str::from_utf8(&self.name[..usize::from(self.name_len)]).unwrap_or("?")
        }

        /// The long name, when the entry had a valid LFN chain that fits
        /// [`LONG_NAME_MAX`] in ASCII (non-ASCII characters become '?').
        pub fn long_name(&self) -> Option<&str> {
                if self.long_len == 0 {
                        return None;
                }
                core::str::from_utf8(&self.long[..usize::from(self.long_len)]).ok()
        }

        fn matches(&self, s: &str) -> bool {
                if self.name().eq_ignore_ascii_case(s) {
                        return true;
                }
                match self.long_name() {
                        Some(l) => l.eq_ignore_ascii_case(s),
                        None => false,
                }
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
        fat_size: u32,
        num_fats: u32,
        sectors_per_cluster: u32,
        data_start: u32,
        cluster_count: u32,
        /// Where the next free-cluster scan starts -- rolls forward so sequential writes
        /// stay sequential on the medium.
        alloc_hint: u32,
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

/// The LFN checksum over an 8.3 name field, per the spec: rotate right, add.
fn lfn_checksum(name11: &[u8]) -> u8 {
        let mut sum = 0u8;
        for b in &name11[..11] {
                sum = (sum >> 1).wrapping_add((sum & 1) << 7).wrapping_add(*b);
        }
        sum
}

/// The accumulator for a long name's slots, which precede their 8.3 entry in reverse
/// order and may span sectors and clusters -- so this walks alongside the directory scan.
struct LfnState {
        buf: [u8; LONG_NAME_MAX],
        len: u8,
        checksum: u8,
        valid: bool,
}

impl LfnState {
        fn new() -> Self {
                Self { buf: [0; LONG_NAME_MAX], len: 0, checksum: 0, valid: false }
        }

        fn reset(&mut self) {
                self.valid = false;
                self.len = 0;
        }

        fn slot(&mut self, e: &[u8]) {
                let seq = e[0];
                if seq & 0x40 != 0 {
                        //   the chain's physically-first slot carries its highest sequence
                        self.buf = [0; LONG_NAME_MAX];
                        self.len = 0;
                        self.checksum = e[13];
                        self.valid = true;
                } else if !self.valid || e[13] != self.checksum {
                        self.valid = false;
                        return;
                }
                let idx = usize::from(seq & 0x1F);
                if idx == 0 {
                        self.valid = false;
                        return;
                }
                let base = (idx - 1) * 13;
                //   the 13 UCS-2 characters of one slot, at the spec's scattered offsets
                const OFFS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
                for (k, off) in OFFS.iter().enumerate() {
                        let ch = u16le(e, *off);
                        if ch == 0 || ch == 0xFFFF {
                                continue; // terminator and fill
                        }
                        let pos = base + k;
                        if pos >= LONG_NAME_MAX {
                                //   longer than this crate carries: the 8.3 alias stands in
                                self.valid = false;
                                return;
                        }
                        self.buf[pos] = if ch < 0x80 { ch as u8 } else { b'?' };
                        if (pos + 1) as u8 > self.len {
                                self.len = (pos + 1) as u8;
                        }
                }
        }
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
                        fat_size: 0,
                        num_fats: 0,
                        sectors_per_cluster: 1,
                        data_start: 0,
                        cluster_count: 0,
                        alloc_hint: 2,
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
                fs.fat_size = fat_size;
                fs.num_fats = nfats;
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

        fn eoc(&self) -> u32 {
                match self.kind {
                        Kind::Fat16 { .. } => 0xFFFF,
                        Kind::Fat32 { .. } => 0x0FFF_FFFF,
                }
        }

        /// The FAT entry for `cluster`, raw (0 = free).
        fn raw_fat(&mut self, cluster: u32) -> Result<u32, FsError> {
                match self.kind {
                        Kind::Fat16 { .. } => {
                                let byte = cluster * 2;
                                self.load(self.fat_start + byte / 512)?;
                                Ok(u16le(&self.buf, (byte % 512) as usize))
                        }
                        Kind::Fat32 { .. } => {
                                let byte = cluster * 4;
                                self.load(self.fat_start + byte / 512)?;
                                Ok(u32le(&self.buf, (byte % 512) as usize) & 0x0FFF_FFFF)
                        }
                }
        }

        /// Write the FAT entry for `cluster` -- in EVERY FAT copy, which is what keeps a
        /// volume checkable by other implementations.
        fn set_fat(&mut self, cluster: u32, value: u32) -> Result<(), FsError> {
                let (byte, wide) = match self.kind {
                        Kind::Fat16 { .. } => (cluster * 2, false),
                        Kind::Fat32 { .. } => (cluster * 4, true),
                };
                for copy in 0..self.num_fats {
                        let lba = self.fat_start + copy * self.fat_size + byte / 512;
                        self.load(lba)?;
                        let off = (byte % 512) as usize;
                        if wide {
                                //   FAT32's top 4 bits are reserved: preserved, per spec
                                let v = (u32le(&self.buf, off) & 0xF000_0000) | (value & 0x0FFF_FFFF);
                                self.buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
                        } else {
                                self.buf[off..off + 2].copy_from_slice(&(value as u16).to_le_bytes());
                        }
                        self.dev.write_block(lba, &self.buf)?;
                }
                Ok(())
        }

        /// The chain's verdict on `cluster`: the next one, or `None` at end-of-chain.
        fn next_cluster(&mut self, cluster: u32) -> Result<Option<u32>, FsError> {
                let v = self.raw_fat(cluster)?;
                let end = match self.kind {
                        Kind::Fat16 { .. } => v >= 0xFFF8,
                        Kind::Fat32 { .. } => v >= 0x0FFF_FFF8,
                };
                if end {
                        return Ok(None);
                }
                //   free, reserved and bad-cluster marks are all wrong in a chain
                if v < 2 || v - 2 >= self.cluster_count {
                        return Err(FsError::BadChain);
                }
                Ok(Some(v))
        }

        /// Claim a free cluster (marked end-of-chain), linked onto `link_from` when given.
        /// The scan rolls forward from the last allocation and wraps once.
        fn alloc_cluster(&mut self, link_from: Option<u32>) -> Result<u32, FsError> {
                let total = self.cluster_count;
                for step in 0..total {
                        let c = 2 + (self.alloc_hint - 2 + step) % total;
                        if self.raw_fat(c)? == 0 {
                                let eoc = self.eoc();
                                self.set_fat(c, eoc)?;
                                if let Some(prev) = link_from {
                                        self.set_fat(prev, c)?;
                                }
                                self.alloc_hint = 2 + (c - 2 + 1) % total;
                                return Ok(c);
                        }
                }
                Err(FsError::NoSpace)
        }

        fn zero_cluster(&mut self, cluster: u32) -> Result<(), FsError> {
                self.buf = [0; 512];
                let base = self.cluster_lba(cluster);
                for s in 0..self.sectors_per_cluster {
                        self.dev.write_block(base + s, &self.buf)?;
                }
                self.buf_lba = base + self.sectors_per_cluster - 1;
                Ok(())
        }

        /// Mark a directory slot deleted. Any LFN slots that preceded it become orphans,
        /// which the read side's checksum gate already ignores -- the same lint every FAT
        /// implementation accumulates.
        fn delete_slot(&mut self, slot: EntrySlot) -> Result<(), FsError> {
                self.load(slot.lba)?;
                self.buf[usize::from(slot.off)] = 0xE5;
                self.dev.write_block(slot.lba, &self.buf)?;
                Ok(())
        }

        /// Free a chain, first cluster to end. A broken link ends the walk: by the time
        /// this runs the entry is already gone, and a leaked tail is a checker's lint,
        /// not corruption.
        fn free_chain(&mut self, first: u32) -> Result<(), FsError> {
                let mut cluster = first;
                let mut hops = 0u32;
                loop {
                        let next = self.next_cluster(cluster).unwrap_or(None);
                        self.set_fat(cluster, 0)?;
                        match next {
                                Some(n) => {
                                        hops += 1;
                                        if hops > self.cluster_count {
                                                break;
                                        }
                                        cluster = n;
                                }
                                None => break,
                        }
                }
                Ok(())
        }

        /// Place a raw 32-byte entry into a free slot of `loc`, maintaining the
        /// end-of-directory marker. What `create`, `mkdir` and a cross-directory
        /// `rename` all share.
        fn insert_entry(&mut self, loc: DirLoc, raw: &[u8; 32]) -> Result<EntrySlot, FsError> {
                let (slot, was_end, next_lba) = self.free_slot(loc)?;
                self.load(slot.lba)?;
                let off = usize::from(slot.off);
                self.buf[off..off + 32].copy_from_slice(raw);
                //   consuming the end marker moves it to the following slot -- in this
                // sector now, in the next one below, or nowhere when the directory ends
                // exactly here
                if was_end && off + 32 < 512 {
                        self.buf[off + 32..off + 64].fill(0);
                }
                let lba = slot.lba;
                self.buf_lba = lba;
                self.dev.write_block(lba, &self.buf)?;
                if was_end && off + 32 == 512 {
                        if let Some(nlba) = next_lba {
                                self.load(nlba)?;
                                self.buf[..32].fill(0);
                                self.dev.write_block(nlba, &self.buf)?;
                        }
                }
                Ok(slot)
        }

        /// Rewrite a directory entry's first cluster and size in place.
        fn update_entry(&mut self, slot: EntrySlot, first_cluster: u32, size: u32) -> Result<(), FsError> {
                self.load(slot.lba)?;
                let off = usize::from(slot.off);
                self.buf[off + 20..off + 22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
                self.buf[off + 26..off + 28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
                self.buf[off + 28..off + 32].copy_from_slice(&size.to_le_bytes());
                self.dev.write_block(slot.lba, &self.buf)?;
                Ok(())
        }

        fn root(&self) -> DirLoc {
                match self.kind {
                        Kind::Fat16 { root_start, root_sectors } => DirLoc::Fixed { start: root_start, sectors: root_sectors },
                        Kind::Fat32 { root_cluster } => DirLoc::Chain { first: root_cluster },
                }
        }

        /// Walk `loc`'s entries in order, stopping at the end-of-directory mark or when
        /// `f` answers `true`. LFN slots feed the long-name accumulator; deleted and
        /// volume-label slots never reach `f`.
        fn for_each_entry(&mut self, loc: DirLoc, f: &mut dyn FnMut(&DirEntry, EntrySlot) -> bool) -> Result<(), FsError> {
                let mut lfn = LfnState::new();
                match loc {
                        DirLoc::Fixed { start, sectors } => {
                                for s in 0..sectors {
                                        if self.scan_sector(start + s, &mut lfn, f)? {
                                                return Ok(());
                                        }
                                }
                        }
                        DirLoc::Chain { first } => {
                                let mut cluster = first;
                                let mut hops = 0u32;
                                loop {
                                        for s in 0..self.sectors_per_cluster {
                                                if self.scan_sector(self.cluster_lba(cluster) + s, &mut lfn, f)? {
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
        fn scan_sector(&mut self, lba: u32, lfn: &mut LfnState, f: &mut dyn FnMut(&DirEntry, EntrySlot) -> bool) -> Result<bool, FsError> {
                self.load(lba)?;
                for i in 0..16 {
                        //   the 32 bytes copied out: the callback must not borrow the buffer
                        let mut e = [0u8; 32];
                        e.copy_from_slice(&self.buf[i * 32..i * 32 + 32]);
                        if e[0] == 0x00 {
                                return Ok(true);
                        }
                        if e[0] == 0xE5 {
                                lfn.reset();
                                continue;
                        }
                        let attr = e[11];
                        if attr & 0x0F == 0x0F {
                                lfn.slot(&e);
                                continue;
                        }
                        if attr & 0x08 != 0 {
                                lfn.reset();
                                continue;
                        }
                        let mut entry = decode_entry(&e);
                        //   a long name counts only when its checksum matches THIS entry:
                        // orphaned LFN slots (an old editor's crash, a deleted rename)
                        // otherwise attach to whatever entry follows them
                        if lfn.valid && lfn.len > 0 && lfn.checksum == lfn_checksum(&e) {
                                entry.long = lfn.buf;
                                entry.long_len = lfn.len;
                        }
                        lfn.reset();
                        if f(&entry, EntrySlot { lba, off: (i * 32) as u16 }) {
                                return Ok(true);
                        }
                }
                Ok(false)
        }

        /// The directory `path` names ("" or "/" is the root). Components are
        /// '/'-separated, matched case-insensitively against short and long names.
        fn resolve_dir(&mut self, path: &str) -> Result<DirLoc, FsError> {
                let mut loc = self.root();
                for part in path.split('/').filter(|p| !p.is_empty()) {
                        let (entry, _) = self.find_in(loc, part)?;
                        if !entry.is_dir {
                                return Err(FsError::NotADirectory);
                        }
                        //   cluster 0 in a ".." entry means the root, per the spec
                        loc = if entry.first_cluster == 0 { self.root() } else { DirLoc::Chain { first: entry.first_cluster } };
                }
                Ok(loc)
        }

        fn find_in(&mut self, loc: DirLoc, name: &str) -> Result<(DirEntry, EntrySlot), FsError> {
                let mut found: Option<(DirEntry, EntrySlot)> = None;
                self.for_each_entry(loc, &mut |e, slot| {
                        if e.matches(name) {
                                found = Some((*e, slot));
                                true
                        } else {
                                false
                        }
                })?;
                found.ok_or(FsError::NotFound)
        }

        /// The first free slot in `loc` -- a deleted entry, the end marker, or (for a
        /// chain) a freshly grown cluster. Answers (slot, consumed-the-end-marker, the
        /// LBA holding the FOLLOWING slot when that slot lives in a different sector).
        fn free_slot(&mut self, loc: DirLoc) -> Result<(EntrySlot, bool, Option<u32>), FsError> {
                match loc {
                        DirLoc::Fixed { start, sectors } => {
                                for s in 0..sectors {
                                        self.load(start + s)?;
                                        for i in 0..16 {
                                                let b = self.buf[i * 32];
                                                if b == 0x00 || b == 0xE5 {
                                                        let next = if i == 15 && s + 1 < sectors { Some(start + s + 1) } else { None };
                                                        return Ok((EntrySlot { lba: start + s, off: (i * 32) as u16 }, b == 0x00, next));
                                                }
                                        }
                                }
                                //   the FAT16 root cannot grow: it is a fixed region
                                Err(FsError::DirFull)
                        }
                        DirLoc::Chain { first } => {
                                let mut cluster = first;
                                let mut hops = 0u32;
                                loop {
                                        for s in 0..self.sectors_per_cluster {
                                                let lba = self.cluster_lba(cluster) + s;
                                                self.load(lba)?;
                                                for i in 0..16 {
                                                        let b = self.buf[i * 32];
                                                        if b == 0x00 || b == 0xE5 {
                                                                let next = if i == 15 && s + 1 < self.sectors_per_cluster { Some(lba + 1) } else { None };
                                                                return Ok((EntrySlot { lba, off: (i * 32) as u16 }, b == 0x00, next));
                                                        }
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
                                                None => {
                                                        //   full to the chain's end: grow it. The
                                                        // fresh cluster is zeroed, which terminates
                                                        // the directory after the new slot for free
                                                        let n = self.alloc_cluster(Some(cluster))?;
                                                        self.zero_cluster(n)?;
                                                        return Ok((EntrySlot { lba: self.cluster_lba(n), off: 0 }, false, None));
                                                }
                                        }
                                }
                        }
                }
        }

        /// Every entry of the directory at `path`, in directory order.
        pub fn list_dir(&mut self, path: &str, mut f: impl FnMut(&DirEntry)) -> Result<(), FsError> {
                let loc = self.resolve_dir(path)?;
                self.for_each_entry(loc, &mut |e, _| {
                        f(e);
                        false
                })
        }

        /// The entry `path` names -- file or directory.
        pub fn stat(&mut self, path: &str) -> Result<DirEntry, FsError> {
                let (dir, name) = split_path(path);
                if name.is_empty() {
                        return Err(FsError::NotFound);
                }
                let loc = self.resolve_dir(dir)?;
                Ok(self.find_in(loc, name)?.0)
        }

        /// Open the file at `path` for reading and writing, positioned at the start.
        pub fn open(&mut self, path: &str) -> Result<File, FsError> {
                let (dir, name) = split_path(path);
                if name.is_empty() {
                        return Err(FsError::NotFound);
                }
                let loc = self.resolve_dir(dir)?;
                let (entry, slot) = self.find_in(loc, name)?;
                if entry.is_dir {
                        return Err(FsError::IsADirectory);
                }
                Ok(File { size: entry.size, pos: 0, cluster: entry.first_cluster, cluster_byte: 0, start: entry.first_cluster, slot })
        }

        /// Create an empty file at `path` (its directory must exist; the name is 8.3).
        /// The first cluster is claimed lazily, by the first write.
        pub fn create(&mut self, path: &str) -> Result<File, FsError> {
                let (dir, name) = split_path(path);
                let name11 = format83(name)?;
                let loc = self.resolve_dir(dir)?;
                if self.find_in(loc, name).is_ok() {
                        return Err(FsError::Exists);
                }
                let slot = self.insert_entry(loc, &new_entry_bytes(&name11, 0x20, 0))?;
                Ok(File { size: 0, pos: 0, cluster: 0, cluster_byte: 0, start: 0, slot })
        }

        /// Open `path` positioned at its end -- the tail of a log, the next record.
        pub fn append(&mut self, path: &str) -> Result<File, FsError> {
                let mut f = self.open(path)?;
                let size = f.size;
                f.seek(self, size)?;
                Ok(f)
        }

        /// Delete the file at `path`: entry first (the commit point), then its chain.
        pub fn remove(&mut self, path: &str) -> Result<(), FsError> {
                let (dir, name) = split_path(path);
                let name = plain_name(name)?;
                let loc = self.resolve_dir(dir)?;
                let (entry, slot) = self.find_in(loc, name)?;
                if entry.is_dir {
                        return Err(FsError::IsADirectory);
                }
                self.delete_slot(slot)?;
                if entry.first_cluster >= 2 {
                        self.free_chain(entry.first_cluster)?;
                }
                Ok(())
        }

        /// Delete the EMPTY directory at `path` ("." and ".." do not count as content).
        pub fn rmdir(&mut self, path: &str) -> Result<(), FsError> {
                let (dir, name) = split_path(path);
                let name = plain_name(name)?;
                let loc = self.resolve_dir(dir)?;
                let (entry, slot) = self.find_in(loc, name)?;
                if !entry.is_dir {
                        return Err(FsError::NotADirectory);
                }
                if entry.first_cluster < 2 {
                        return Err(FsError::BadChain);
                }
                let mut occupied = false;
                self.for_each_entry(DirLoc::Chain { first: entry.first_cluster }, &mut |e, _| {
                        if e.name() != "." && e.name() != ".." {
                                occupied = true;
                                true
                        } else {
                                false
                        }
                })?;
                if occupied {
                        return Err(FsError::NotEmpty);
                }
                self.delete_slot(slot)?;
                self.free_chain(entry.first_cluster)?;
                Ok(())
        }

        /// Create the directory at `path`: one zeroed cluster holding "." and "..", and
        /// an entry in the (existing) parent.
        pub fn mkdir(&mut self, path: &str) -> Result<(), FsError> {
                let (dir, name) = split_path(path);
                let name11 = format83(name)?;
                let loc = self.resolve_dir(dir)?;
                if self.find_in(loc, name).is_ok() {
                        return Err(FsError::Exists);
                }
                //   ".." holds the parent's first cluster; 0 means the root, per the spec
                // -- which is also what the FAT16 root's fixed region gets
                let parent_first = match loc {
                        DirLoc::Chain { first } => first,
                        DirLoc::Fixed { .. } => 0,
                };
                let c = self.alloc_cluster(None)?;
                self.zero_cluster(c)?;
                let lba = self.cluster_lba(c);
                self.load(lba)?;
                self.buf[0..32].copy_from_slice(&new_entry_bytes(b".          ", 0x10, c));
                self.buf[32..64].copy_from_slice(&new_entry_bytes(b"..         ", 0x10, parent_first));
                self.dev.write_block(lba, &self.buf)?;
                self.insert_entry(loc, &new_entry_bytes(&name11, 0x10, c))?;
                Ok(())
        }

        /// Move and/or rename `from` to `to` -- across directories too. The entry's raw
        /// bytes travel whole (attributes and all); only the name changes. A moved
        /// directory's ".." is pointed at its new parent.
        pub fn rename(&mut self, from: &str, to: &str) -> Result<(), FsError> {
                //   into its own subtree would orphan the moved directory in a cycle
                let (from_t, to_t) = (from.trim_matches('/'), to.trim_matches('/'));
                if to_t.len() > from_t.len() && to_t[..from_t.len()].eq_ignore_ascii_case(from_t) && to_t.as_bytes()[from_t.len()] == b'/' {
                        return Err(FsError::BadName);
                }
                let (fdir, fname) = split_path(from);
                let fname = plain_name(fname)?;
                let (tdir, tname) = split_path(to);
                let tname11 = format83(tname)?;
                let floc = self.resolve_dir(fdir)?;
                let tloc = self.resolve_dir(tdir)?;
                let (entry, fslot) = self.find_in(floc, fname)?;
                if self.find_in(tloc, tname).is_ok() {
                        return Err(FsError::Exists);
                }
                self.load(fslot.lba)?;
                let mut raw = [0u8; 32];
                raw.copy_from_slice(&self.buf[usize::from(fslot.off)..usize::from(fslot.off) + 32]);
                raw[..11].copy_from_slice(&tname11);
                self.insert_entry(tloc, &raw)?;
                self.delete_slot(fslot)?;
                if entry.is_dir && entry.first_cluster >= 2 {
                        let parent_first = match tloc {
                                DirLoc::Chain { first } => first,
                                DirLoc::Fixed { .. } => 0,
                        };
                        let lba = self.cluster_lba(entry.first_cluster);
                        self.load(lba)?;
                        if &self.buf[32..34] == b".." {
                                self.buf[32 + 20..32 + 22].copy_from_slice(&((parent_first >> 16) as u16).to_le_bytes());
                                self.buf[32 + 26..32 + 28].copy_from_slice(&(parent_first as u16).to_le_bytes());
                                self.dev.write_block(lba, &self.buf)?;
                        }
                }
                Ok(())
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

/// A name an operation may act on: nonempty, and neither of the dot entries.
fn plain_name(name: &str) -> Result<&str, FsError> {
        if name.is_empty() {
                return Err(FsError::NotFound);
        }
        if name == "." || name == ".." {
                return Err(FsError::BadName);
        }
        Ok(name)
}

/// A fresh raw directory entry: name, attributes, first cluster; zero size and times.
fn new_entry_bytes(name11: &[u8; 11], attr: u8, cluster: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[..11].copy_from_slice(name11);
        e[11] = attr;
        e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        e
}

/// An 8.3 name field from "NAME.EXT", uppercased, or [`FsError::BadName`].
fn format83(name: &str) -> Result<[u8; 11], FsError> {
        fn ok_char(c: u8) -> bool {
                c.is_ascii_alphanumeric() || b"_-~!#$%&'()@^`{}".contains(&c)
        }
        let (base, ext) = match name.rfind('.') {
                Some(i) => (&name[..i], &name[i + 1..]),
                None => (name, ""),
        };
        if base.is_empty() || base.len() > 8 || ext.len() > 3 {
                return Err(FsError::BadName);
        }
        let mut out = [b' '; 11];
        for (i, c) in base.bytes().enumerate() {
                let c = c.to_ascii_uppercase();
                if !ok_char(c) {
                        return Err(FsError::BadName);
                }
                out[i] = c;
        }
        for (i, c) in ext.bytes().enumerate() {
                let c = c.to_ascii_uppercase();
                if !ok_char(c) {
                        return Err(FsError::BadName);
                }
                out[8 + i] = c;
        }
        Ok(out)
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
                long: [0; LONG_NAME_MAX],
                long_len: 0,
                is_dir: attr & 0x10 != 0,
                size: u32le(e, 28),
                first_cluster: (u16le(e, 20) << 16) | u16le(e, 26),
        }
}

/// A read/write cursor. Borrows nothing: every operation takes the filesystem, so an open
/// file is a few words and any number can exist at once.
#[derive(Clone, Copy, Debug)]
pub struct File {
        size: u32,
        pos: u32,
        /// The cluster `pos` sits in; 0 before the first cluster exists.
        cluster: u32,
        /// How far into that cluster `pos` is.
        cluster_byte: u32,
        /// The chain's first cluster; 0 for a still-empty file.
        start: u32,
        /// Where the directory entry lives, for the size/cluster write-back.
        slot: EntrySlot,
}

impl File {
        pub fn size(&self) -> u32 {
                self.size
        }

        pub fn pos(&self) -> u32 {
                self.pos
        }

        /// Fill `out` from the current position; the count is short only at end of file.
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

        /// Write `data` at the current position -- over existing content, and past the end
        /// with clusters allocated as needed. Write-through: sectors, every FAT copy and
        /// the directory entry (size, first cluster) are all on the medium when this
        /// returns. A mid-write allocation failure loses the call, not the file.
        pub fn write<D: BlockDevice>(&mut self, fs: &mut Fat<D>, data: &[u8]) -> Result<usize, FsError> {
                let cluster_bytes = fs.sectors_per_cluster * 512;
                let mut done = 0usize;
                while done < data.len() {
                        if self.cluster == 0 {
                                let c = fs.alloc_cluster(None)?;
                                self.cluster = c;
                                self.start = c;
                                self.cluster_byte = 0;
                        } else if self.cluster_byte == cluster_bytes {
                                self.cluster = match fs.next_cluster(self.cluster)? {
                                        Some(next) => next,
                                        None => fs.alloc_cluster(Some(self.cluster))?,
                                };
                                self.cluster_byte = 0;
                        }
                        let lba = fs.cluster_lba(self.cluster) + self.cluster_byte / 512;
                        let in_sector = (self.cluster_byte % 512) as usize;
                        let n = (512 - in_sector).min(data.len() - done);
                        if n < 512 {
                                //   a partial sector: read-modify-write
                                fs.load(lba)?;
                        } else {
                                //   a whole sector: nothing to preserve
                                fs.buf_lba = lba;
                        }
                        fs.buf[in_sector..in_sector + n].copy_from_slice(&data[done..done + n]);
                        fs.dev.write_block(lba, &fs.buf)?;
                        done += n;
                        self.pos += n as u32;
                        self.cluster_byte += n as u32;
                }
                if self.pos > self.size {
                        self.size = self.pos;
                }
                fs.update_entry(self.slot, self.start, self.size)?;
                Ok(done)
        }

        /// Shrink the file to `len` bytes (growing is what `write` does): the tail of the
        /// chain is freed, the new last cluster re-marked end-of-chain, and the entry
        /// rewritten. A cursor past the new end moves to it.
        pub fn truncate<D: BlockDevice>(&mut self, fs: &mut Fat<D>, len: u32) -> Result<(), FsError> {
                if len >= self.size {
                        return Ok(());
                }
                if len == 0 {
                        if self.start >= 2 {
                                fs.free_chain(self.start)?;
                        }
                        self.start = 0;
                        self.size = 0;
                        self.pos = 0;
                        self.cluster = 0;
                        self.cluster_byte = 0;
                        return fs.update_entry(self.slot, 0, 0);
                }
                let cluster_bytes = fs.sectors_per_cluster * 512;
                //   the last kept cluster is the one holding byte len-1
                let keep = len.div_ceil(cluster_bytes);
                let mut cluster = self.start;
                for _ in 0..keep - 1 {
                        match fs.next_cluster(cluster)? {
                                Some(next) => cluster = next,
                                None => break,
                        }
                }
                if let Ok(Some(tail)) = fs.next_cluster(cluster) {
                        fs.free_chain(tail)?;
                }
                let eoc = fs.eoc();
                fs.set_fat(cluster, eoc)?;
                self.size = len;
                if self.pos > len {
                        self.seek(fs, len)?;
                }
                fs.update_entry(self.slot, self.start, len)
        }

        /// Move the cursor; past-the-end clamps to the end. Costs a chain walk from the
        /// start -- FAT has no other way to a byte offset.
        pub fn seek<D: BlockDevice>(&mut self, fs: &mut Fat<D>, pos: u32) -> Result<(), FsError> {
                let pos = pos.min(self.size);
                if pos == 0 || self.start == 0 {
                        self.pos = 0;
                        self.cluster = self.start;
                        self.cluster_byte = 0;
                        return Ok(());
                }
                let cluster_bytes = fs.sectors_per_cluster * 512;
                //   a position on a cluster boundary belongs to the END of the previous
                // cluster: the read/write loops advance the chain themselves, and the
                // final cluster may not have a successor yet
                let (hops, byte) = if pos % cluster_bytes == 0 { (pos / cluster_bytes - 1, cluster_bytes) } else { (pos / cluster_bytes, pos % cluster_bytes) };
                let mut cluster = self.start;
                for _ in 0..hops {
                        match fs.next_cluster(cluster)? {
                                Some(next) => cluster = next,
                                None => return Err(FsError::BadChain),
                        }
                }
                self.pos = pos;
                self.cluster = cluster;
                self.cluster_byte = byte;
                Ok(())
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use std::format;
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

        /// One LFN slot for 13 characters of `part`, at sequence `seq`.
        fn lfn_slot_bytes(seq: u8, checksum: u8, part: &str) -> [u8; 32] {
                let mut e = [0u8; 32];
                e[0] = seq;
                e[11] = 0x0F;
                e[13] = checksum;
                const OFFS: [usize; 13] = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
                let bytes = part.as_bytes();
                for (k, off) in OFFS.iter().enumerate() {
                        let ch: u16 = match bytes.get(k) {
                                Some(b) => u16::from(*b),
                                //   the first slot past the name carries the terminator,
                                // the rest 0xFFFF fill
                                None if k == bytes.len() => 0,
                                None => 0xFFFF,
                        };
                        put16(&mut e, *off, ch);
                }
                e
        }

        //   the FAT16 fixture: 4200 total sectors, 1 reserved, TWO 17-sector FATs (so
        // mirroring is exercised), a 1-sector root (16 entries), 1 sector per cluster ->
        // 4164 clusters (>= 4085 and < 65525: FAT16 by count). Layout: FATs at 1 and 18,
        // root at 35, data from 36 (cluster N = sector 36 + N - 2). Contents: HELLO.TXT
        // (700 bytes across clusters 2-3), SUB/ (cluster 4) holding DEEP.BIN (5 bytes,
        // cluster 5, with a two-slot LFN "deep-file-long-name.bin") -- plus a volume
        // label, a deleted entry and an ORPHANED LFN slot the walk must skip.
        const FAT16_DATA0: usize = 36;

        fn build_fat16(d: &mut [u8], base: usize) {
                let s = &mut d[base * 512..];
                s[0] = 0xEB;
                s[1] = 0x3C;
                s[2] = 0x90;
                put16(s, 11, 512);
                s[13] = 1;
                put16(s, 14, 1);
                s[16] = 2;
                put16(s, 17, 16);
                put16(s, 19, 4200);
                s[21] = 0xF8;
                put16(s, 22, 17);
                s[510] = 0x55;
                s[511] = 0xAA;
                for fat in [base + 1, base + 18] {
                        let fat = fat * 512;
                        put16(&mut d[fat..], 0, 0xFFF8);
                        put16(&mut d[fat..], 2, 0xFFFF);
                        put16(&mut d[fat..], 4, 3); // cluster 2 -> 3
                        put16(&mut d[fat..], 6, 0xFFFF); // 3: end
                        put16(&mut d[fat..], 8, 0xFFFF); // 4 (SUB): end
                        put16(&mut d[fat..], 10, 0xFFFF); // 5 (DEEP.BIN): end
                }
                let root = (base + 35) * 512;
                d[root..root + 32].copy_from_slice(&dirent(b"VOLLABEL   ", 0x08, 0, 0));
                let mut deleted = dirent(b"OLD     TXT", 0x20, 9, 1);
                deleted[0] = 0xE5;
                d[root + 32..root + 64].copy_from_slice(&deleted);
                //   an orphaned LFN slot: its checksum (0) matches nothing that follows
                let mut orphan = [0u8; 32];
                orphan[0] = 0x41;
                orphan[11] = 0x0F;
                d[root + 64..root + 96].copy_from_slice(&orphan);
                d[root + 96..root + 128].copy_from_slice(&dirent(b"HELLO   TXT", 0x20, 2, 700));
                d[root + 128..root + 160].copy_from_slice(&dirent(b"SUB        ", 0x10, 4, 0));
                let c = |n: usize| (base + FAT16_DATA0 + (n - 2)) * 512;
                for b in d[c(2)..c(2) + 512].iter_mut() {
                        *b = b'A';
                }
                for b in d[c(3)..c(3) + 188].iter_mut() {
                        *b = b'B';
                }
                let sub = c(4);
                d[sub..sub + 32].copy_from_slice(&dirent(b".          ", 0x10, 4, 0));
                d[sub + 32..sub + 64].copy_from_slice(&dirent(b"..         ", 0x10, 0, 0));
                //   DEEP.BIN behind a valid two-slot LFN chain
                let deep = dirent(b"DEEP    BIN", 0x20, 5, 5);
                let ck = lfn_checksum(&deep);
                d[sub + 64..sub + 96].copy_from_slice(&lfn_slot_bytes(0x42, ck, "g-name.bin"));
                d[sub + 96..sub + 128].copy_from_slice(&lfn_slot_bytes(0x01, ck, "deep-file-lon"));
                d[sub + 128..sub + 160].copy_from_slice(&deep);
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
                assert_eq!(info.cluster_count, 4164);
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
                assert!(matches!(fs.open("NOPE.TXT"), Err(FsError::NotFound)));
                assert!(matches!(fs.open("HELLO.TXT/X"), Err(FsError::NotADirectory)));
                assert!(matches!(fs.open("SUB"), Err(FsError::IsADirectory)));
        }

        #[test]
        fn long_names_list_match_and_orphans_do_not_attach() {
                let mut fs = Fat::mount(fat16_superfloppy()).unwrap();
                //   the orphaned LFN slot in the root precedes HELLO.TXT: no long name
                let hello = fs.stat("HELLO.TXT").unwrap();
                assert_eq!(hello.long_name(), None);
                let mut long: Option<std::string::String> = None;
                fs.list_dir("SUB", |e| {
                        if e.name() == "DEEP.BIN" {
                                long = e.long_name().map(|s| s.into());
                        }
                })
                .unwrap();
                assert_eq!(long.as_deref(), Some("deep-file-long-name.bin"));
                //   and the long name resolves in a path, case-insensitively
                let mut f = fs.open("sub/DEEP-FILE-LONG-NAME.BIN").unwrap();
                let mut buf = [0u8; 8];
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 5);
                assert_eq!(&buf[..5], b"deep!");
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
                let mut dev = MemDev(d);
                {
                        let mut fs = Fat::mount(&mut dev).unwrap();
                        assert!(fs.volume_info().fat32);
                        let mut f = fs.open("BIG.TXT").unwrap();
                        let mut buf = [0u8; 8];
                        assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 3);
                        assert_eq!(&buf[..3], b"big");
                        //   a write on FAT32, for the wide FAT entry path
                        let mut f = fs.append("BIG.TXT").unwrap();
                        f.write(&mut fs, b"ger").unwrap();
                }
                let mut fs = Fat::mount(&mut dev).unwrap();
                let mut f = fs.open("BIG.TXT").unwrap();
                let mut buf = [0u8; 8];
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 6);
                assert_eq!(&buf[..6], b"bigger");
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

        #[test]
        fn creates_writes_and_reads_back_across_a_remount() {
                let mut dev = fat16_superfloppy();
                {
                        let mut fs = Fat::mount(&mut dev).unwrap();
                        let mut f = fs.create("NEW.TXT").unwrap();
                        assert_eq!(f.size(), 0);
                        f.write(&mut fs, b"hello new world").unwrap();
                        assert!(matches!(fs.create("NEW.TXT"), Err(FsError::Exists)));
                        assert!(matches!(fs.create("bad name.txt"), Err(FsError::BadName)));
                        assert!(matches!(fs.create("WAYTOOLONG.TXT"), Err(FsError::BadName)));
                }
                //   a fresh mount sees only the medium
                let mut fs = Fat::mount(&mut dev).unwrap();
                let mut f = fs.open("NEW.TXT").unwrap();
                assert_eq!(f.size(), 15);
                let mut buf = [0u8; 32];
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 15);
                assert_eq!(&buf[..15], b"hello new world");
                let mut names: Vec<std::string::String> = Vec::new();
                fs.list_dir("", |e| names.push(e.name().into())).unwrap();
                assert!(names.contains(&"NEW.TXT".into()), "listed: {names:?}");
        }

        #[test]
        fn append_grows_the_chain_and_updates_every_fat_copy() {
                let mut dev = fat16_superfloppy();
                {
                        let mut fs = Fat::mount(&mut dev).unwrap();
                        let mut f = fs.append("HELLO.TXT").unwrap();
                        assert_eq!(f.pos(), 700);
                        f.write(&mut fs, &[b'C'; 400]).unwrap();
                }
                let mut fs = Fat::mount(&mut dev).unwrap();
                let mut f = fs.open("HELLO.TXT").unwrap();
                assert_eq!(f.size(), 1100);
                let mut buf = std::vec![0u8; 2048];
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 1100);
                assert!(buf[..512].iter().all(|b| *b == b'A'));
                assert!(buf[512..700].iter().all(|b| *b == b'B'));
                assert!(buf[700..1100].iter().all(|b| *b == b'C'));
                //   1100 bytes needs a third cluster: cluster 3's FAT entry now links on,
                // identically in BOTH copies
                let fat0 = u16::from_le_bytes([dev.0[512 + 6], dev.0[512 + 7]]);
                let fat1 = u16::from_le_bytes([dev.0[18 * 512 + 6], dev.0[18 * 512 + 7]]);
                assert_ne!(fat0, 0xFFFF, "cluster 3 links to the new cluster");
                assert_eq!(fat0, fat1, "the second FAT mirrors the first");
        }

        #[test]
        fn overwrite_in_place_leaves_the_size_alone() {
                let mut dev = fat16_superfloppy();
                let mut fs = Fat::mount(&mut dev).unwrap();
                let mut f = fs.open("HELLO.TXT").unwrap();
                f.write(&mut fs, b"XYZ").unwrap();
                assert_eq!(f.size(), 700);
                let mut f = fs.open("HELLO.TXT").unwrap();
                let mut buf = [0u8; 8];
                f.read(&mut fs, &mut buf).unwrap();
                assert_eq!(&buf[..4], b"XYZA");
        }

        #[test]
        fn seek_lands_on_content_and_boundaries() {
                let mut fs = Fat::mount(fat16_superfloppy()).unwrap();
                let mut f = fs.open("HELLO.TXT").unwrap();
                f.seek(&mut fs, 510).unwrap();
                let mut buf = [0u8; 4];
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 4);
                assert_eq!(&buf, b"AABB", "the cluster boundary at 512");
                f.seek(&mut fs, 512).unwrap();
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 4);
                assert_eq!(&buf, b"BBBB");
                f.seek(&mut fs, 0).unwrap();
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 4);
                assert_eq!(&buf, b"AAAA");
                f.seek(&mut fs, 9999).unwrap();
                assert_eq!(f.pos(), 700, "past the end clamps");
        }

        #[test]
        fn a_full_fat16_root_answers_dirfull() {
                let mut dev = fat16_superfloppy();
                let mut fs = Fat::mount(&mut dev).unwrap();
                //   16 slots: label + orphan LFN + HELLO + SUB stay, the deleted slot and
                // the tail are free -- 12 creates fit, the 13th does not
                for i in 0..12 {
                        fs.create(&format!("F{i}.TXT")).unwrap();
                }
                assert!(matches!(fs.create("LAST.TXT"), Err(FsError::DirFull)));
        }

        #[test]
        fn a_chain_directory_grows_when_full() {
                let mut dev = fat16_superfloppy();
                {
                        let mut fs = Fat::mount(&mut dev).unwrap();
                        //   SUB holds ., .., two LFN slots and DEEP.BIN: 11 slots free in
                        // its single cluster; the 12th create grows the chain
                        for i in 0..12 {
                                fs.create(&format!("SUB/G{i}.TXT")).unwrap();
                        }
                }
                let mut fs = Fat::mount(&mut dev).unwrap();
                let mut count = 0u32;
                fs.list_dir("SUB", |_| count += 1).unwrap();
                //   . + .. + DEEP.BIN + 12 created
                assert_eq!(count, 15);
        }

        #[test]
        fn a_volume_with_no_free_cluster_answers_nospace() {
                let mut dev = fat16_superfloppy();
                //   mark every data cluster used, in both FAT copies
                for fat in [1usize, 18] {
                        for c in 6..(4164 + 2) {
                                let byte = fat * 512 + c * 2;
                                dev.0[byte] = 0xFF;
                                dev.0[byte + 1] = 0xFF;
                        }
                }
                let mut fs = Fat::mount(&mut dev).unwrap();
                let mut f = fs.append("HELLO.TXT").unwrap();
                assert!(matches!(f.write(&mut fs, &[0u8; 600]), Err(FsError::NoSpace)));
        }

        #[test]
        fn remove_frees_the_chain_and_the_name() {
                let mut dev = fat16_superfloppy();
                {
                        let mut fs = Fat::mount(&mut dev).unwrap();
                        fs.remove("HELLO.TXT").unwrap();
                        assert!(matches!(fs.open("HELLO.TXT"), Err(FsError::NotFound)));
                        assert!(matches!(fs.remove("SUB"), Err(FsError::IsADirectory)));
                        assert!(matches!(fs.remove("SUB/."), Err(FsError::BadName)));
                }
                //   clusters 2 and 3 free again, in BOTH FAT copies
                for fat in [1usize, 18] {
                        assert_eq!(&dev.0[fat * 512 + 4..fat * 512 + 8], &[0, 0, 0, 0], "FAT at sector {fat}");
                }
                //   and the space is reusable
                let mut fs = Fat::mount(&mut dev).unwrap();
                let mut f = fs.create("HELLO.TXT").unwrap();
                f.write(&mut fs, b"again").unwrap();
                let mut f = fs.open("HELLO.TXT").unwrap();
                let mut buf = [0u8; 8];
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 5);
                assert_eq!(&buf[..5], b"again");
        }

        #[test]
        fn truncate_frees_the_tail_and_survives_a_remount() {
                let mut dev = fat16_superfloppy();
                {
                        let mut fs = Fat::mount(&mut dev).unwrap();
                        let mut f = fs.open("HELLO.TXT").unwrap();
                        f.truncate(&mut fs, 300).unwrap();
                        assert_eq!(f.size(), 300);
                }
                //   cluster 3 freed, cluster 2 re-marked end-of-chain, in both copies
                for fat in [1usize, 18] {
                        assert_eq!(u16::from_le_bytes([dev.0[fat * 512 + 4], dev.0[fat * 512 + 5]]), 0xFFFF);
                        assert_eq!(u16::from_le_bytes([dev.0[fat * 512 + 6], dev.0[fat * 512 + 7]]), 0);
                }
                let mut fs = Fat::mount(&mut dev).unwrap();
                let mut f = fs.open("HELLO.TXT").unwrap();
                assert_eq!(f.size(), 300);
                let mut buf = [0u8; 512];
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 300);
                assert!(buf[..300].iter().all(|b| *b == b'A'));
                //   and to zero: the first cluster goes too
                let mut f = fs.open("HELLO.TXT").unwrap();
                f.truncate(&mut fs, 0).unwrap();
                let f = fs.open("HELLO.TXT").unwrap();
                assert_eq!(f.size(), 0);
        }

        #[test]
        fn rename_moves_within_and_across_directories() {
                let mut dev = fat16_superfloppy();
                {
                        let mut fs = Fat::mount(&mut dev).unwrap();
                        fs.rename("HELLO.TXT", "HI.TXT").unwrap();
                        assert!(matches!(fs.open("HELLO.TXT"), Err(FsError::NotFound)));
                        fs.rename("HI.TXT", "SUB/MOVED.TXT").unwrap();
                }
                let mut fs = Fat::mount(&mut dev).unwrap();
                let mut f = fs.open("SUB/MOVED.TXT").unwrap();
                assert_eq!(f.size(), 700);
                let mut buf = [0u8; 4];
                f.read(&mut fs, &mut buf).unwrap();
                assert_eq!(&buf, b"AAAA");
                //   collisions and cycles are refused
                assert!(matches!(fs.rename("SUB/MOVED.TXT", "SUB/DEEP.BIN"), Err(FsError::Exists)));
                assert!(matches!(fs.rename("SUB", "SUB/INSIDE"), Err(FsError::BadName)));
        }

        #[test]
        fn mkdir_nests_and_a_moved_directory_updates_its_dotdot() {
                let mut dev = fat16_superfloppy();
                {
                        let mut fs = Fat::mount(&mut dev).unwrap();
                        fs.mkdir("NEST").unwrap();
                        assert!(matches!(fs.mkdir("NEST"), Err(FsError::Exists)));
                        let mut f = fs.create("NEST/NOTE.TXT").unwrap();
                        f.write(&mut fs, b"nested").unwrap();
                        //   move SUB under NEST: its ".." must follow
                        fs.rename("SUB", "NEST/SUB2").unwrap();
                }
                let mut fs = Fat::mount(&mut dev).unwrap();
                let mut f = fs.open("NEST/NOTE.TXT").unwrap();
                let mut buf = [0u8; 8];
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 6);
                assert_eq!(&buf[..6], b"nested");
                let mut f = fs.open("NEST/SUB2/DEEP.BIN").unwrap();
                assert_eq!(f.read(&mut fs, &mut buf).unwrap(), 5);
                //   ".." resolves to NEST now: listing through it finds NOTE.TXT
                let mut names: Vec<std::string::String> = Vec::new();
                fs.list_dir("NEST/SUB2/..", |e| names.push(e.name().into())).unwrap();
                assert!(names.contains(&"NOTE.TXT".into()), "listed: {names:?}");
        }

        #[test]
        fn rmdir_refuses_content_then_removes() {
                let mut dev = fat16_superfloppy();
                let mut fs = Fat::mount(&mut dev).unwrap();
                assert!(matches!(fs.rmdir("SUB"), Err(FsError::NotEmpty)));
                assert!(matches!(fs.rmdir("HELLO.TXT"), Err(FsError::NotADirectory)));
                fs.remove("SUB/DEEP.BIN").unwrap();
                fs.rmdir("SUB").unwrap();
                assert!(matches!(fs.stat("SUB"), Err(FsError::NotFound)));
        }
}
