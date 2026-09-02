//! Filesystems over [`light_core::hal::BlockDevice`] -- the framework's portable FS
//! layer. The media dependency is one trait, so the same filesystem code mounts an SD
//! card on an RP2 board and a byte vector in a host test.
//!
//! The first (and so far only) filesystem is FAT16/FAT32 (`fat`), because that is what
//! memory cards carry and what every desktop OS reads back. Read-side first: mount,
//! directory listing, path lookup, sequential file read -- no_std, no alloc, one owned
//! 512-byte buffer. exFAT (what SDXC cards ship with from the factory) is detected and
//! named in the error, not silently misparsed.

#![no_std]

pub mod fat;
pub use fat::{DirEntry, Fat, File, FsError, VolumeInfo};
