//! Filesystems.
//!
//! What turns a disk from a run of numbered sectors into files.
//!
//! Three things, of which two are about somebody else's disk. The partition
//! table says which part of the disk is which. FAT32 is what UEFI requires on
//! the partition a machine boots from, and so what NexusOS has to understand to
//! reach the files it was started from; it is read and never written, because
//! the boot partition is not ours to design.
//!
//! NexusFS is the one that is. It lives in its own partition, it is written as
//! well as read, and it is where the system keeps anything it means to still
//! have after a reboot. Underneath it is a cache, so that reading the same
//! block twice costs the disk once.

pub mod cache;
pub mod fat32;
pub mod gpt;
pub mod nexusfs;
pub mod store;
