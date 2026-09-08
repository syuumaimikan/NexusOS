//! Device drivers.
//!
//! Everything here talks to real hardware. Drivers live in the kernel for now;
//! the Nexus driver model moves them into user space, behind IPC, once there is
//! a user space to move them into.
pub mod keyboard;
pub mod pci;
pub mod virtio_blk;
