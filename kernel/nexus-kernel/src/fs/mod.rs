//! Filesystems.
//!
//! What turns a disk from a run of numbered sectors into files. Two readers so
//! far, and both are readers: the partition table, which says which part of the
//! disk is which, and FAT32, which is what UEFI requires on the partition a
//! machine boots from and therefore what NexusOS has to understand to reach the
//! files it was started from.
//!
//! NexusFS -- the filesystem this project means to have -- goes elsewhere on the
//! disk and is not started. This is the part that has to exist whatever that
//! turns out to be, because the boot partition is not ours to design.

pub mod fat32;
pub mod gpt;
