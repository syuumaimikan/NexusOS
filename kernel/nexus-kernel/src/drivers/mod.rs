//! Device drivers.
//!
//! Everything here talks to real hardware. Drivers live in the kernel for now;
//! the Nexus driver model moves them into user space, behind IPC, once there is
//! a user space to move them into.
pub mod keyboard;
pub mod mouse;
pub mod pci;
pub mod rtc;
pub mod speaker;
pub mod virtio_blk;
pub mod virtio_net;
pub mod usb;
pub mod usb_storage;
pub mod xhci;
pub mod xhci_rings;
