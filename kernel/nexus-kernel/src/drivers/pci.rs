//! PCI enumeration.
//!
//! Everything the machine has that is not on the processor is behind PCI: the
//! disk controller, the network card, the graphics adapter. Finding them is a
//! prerequisite for having any of them, and it is the first thing NexusOS does
//! that is about the *machine* rather than the processor.
//!
//! # Ports, not memory
//!
//! Configuration space is reached two ways. The legacy one is a pair of I/O
//! ports: write an address to one, read or write the data at the other. The
//! modern one is a memory-mapped window whose base ACPI reports in the MCFG
//! table, and it is the only way to reach the extended half of a device's
//! configuration space.
//!
//! The ports are used here. They reach the first 256 bytes of every device's
//! configuration space on every machine that has PCI at all, which is all of
//! what enumeration and a virtio driver need. The memory-mapped window buys
//! extended capabilities and buses beyond 255, and it can be added when
//! something wants one; adding it now would be a second path to test with
//! nothing yet asking for it.
//!
//! # Scanning
//!
//! Brute force: every function of every device on every bus. A recursive scan
//! that follows bridges is less work on a machine with many buses and more code
//! on one with a few; at 256 buses of 32 devices of 8 functions this is 65536
//! port reads, which happens once and takes a handful of milliseconds.

use alloc::vec::Vec;

use crate::arch::io::{inl, outl};
use crate::kprintln;

/// The port an address is written to.
const CONFIG_ADDRESS: u16 = 0x0CF8;
/// The port the data is read from.
const CONFIG_DATA: u16 = 0x0CFC;

/// Bit 31 of the address word: without it the access does not happen.
const ENABLE: u32 = 1 << 31;

/// A vendor identifier of all ones means no device answered.
const NO_DEVICE: u16 = 0xFFFF;

/// Where a device is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Address {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl core::fmt::Display for Address {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}

impl Address {
    /// The configuration-space address word for `offset`.
    ///
    /// The offset is aligned down to four bytes because the data port is a
    /// dword window; a caller wanting a byte or a word extracts it from the
    /// dword it lands in.
    fn word(self, offset: u8) -> u32 {
        ENABLE
            | (u32::from(self.bus) << 16)
            | (u32::from(self.device) << 11)
            | (u32::from(self.function) << 8)
            | u32::from(offset & 0xFC)
    }
}

/// Read a dword of configuration space.
///
/// # Safety
///
/// Reading configuration space has no side effects on any device this kernel
/// will meet, but it is a port access, so it is `unsafe` for the same reason
/// every port access is.
pub unsafe fn read_config(address: Address, offset: u8) -> u32 {
    // SAFETY: the address word is well formed and the data port is the one the
    // specification names.
    unsafe {
        outl(CONFIG_ADDRESS, address.word(offset));
        inl(CONFIG_DATA)
    }
}

/// Write a dword of configuration space.
///
/// # Safety
///
/// The caller must know what the register does. Writing a base address register
/// moves a device's window; writing the command register can disable it.
pub unsafe fn write_config(address: Address, offset: u8, value: u32) {
    // SAFETY: upheld by the caller.
    unsafe {
        outl(CONFIG_ADDRESS, address.word(offset));
        outl(CONFIG_DATA, value);
    }
}

/// A device found on the bus.
#[derive(Debug, Clone, Copy)]
pub struct Device {
    pub address: Address,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub interface: u8,
    pub header_type: u8,
    /// Subsystem device identifier, which is how a virtio device says what it
    /// is when its device identifier is a transitional one.
    pub subsystem: u16,
    /// The legacy interrupt line firmware connected it to, or `0xFF` for none.
    ///
    /// Firmware's answer and not the device's: the number means whichever input
    /// of the interrupt controller this device's pin was wired to, which is a
    /// fact about the board and not about the card.
    pub interrupt_line: u8,
}

impl Device {
    /// Read one of the six base address registers.
    ///
    /// The low bits are flags rather than address: bit 0 says whether the
    /// window is in I/O space or memory, and for memory windows bits 1 and 2
    /// say how wide the address is. They are masked off here, and a 64-bit
    /// memory window's upper half is read from the following register.
    ///
    /// # Safety
    ///
    /// `index` must be below six, and the device must be one this kernel is
    /// entitled to touch.
    pub unsafe fn base_address(&self, index: u8) -> BaseAddress {
        let offset = 0x10 + index * 4;
        // SAFETY: upheld by the caller.
        let low = unsafe { read_config(self.address, offset) };

        if low & 1 == 1 {
            return BaseAddress::Port((low & 0xFFFC) as u16);
        }

        let sixty_four = (low >> 1) & 0b11 == 0b10;
        let mut base = u64::from(low & 0xFFFF_FFF0);
        if sixty_four {
            // SAFETY: as above; a 64-bit window always has a following
            // register, which is why they come in pairs.
            let high = unsafe { read_config(self.address, offset + 4) };
            base |= u64::from(high) << 32;
        }
        BaseAddress::Memory { base, sixty_four }
    }

    /// Turn on the parts of the device the kernel intends to use.
    ///
    /// Bus mastering is the one that matters: a device that cannot master the
    /// bus cannot read the descriptor rings a driver builds for it, and a
    /// virtio device left this way simply never does anything.
    ///
    /// # Safety
    ///
    /// The caller must be about to drive this device.
    /// Walk the capability list, handing each one to `look`.
    ///
    /// A capability is a small structure somewhere in the device's own
    /// configuration space, linked into a chain: a byte at 0x34 says where the
    /// first one is, and each one's second byte says where the next is, with
    /// nought ending it. `look` is given the offset and the identifier, and
    /// returns `Some` to stop.
    ///
    /// This exists because modern virtio devices put *everything* here -- which
    /// bar their registers are in, and at what offset -- rather than at a fixed
    /// place in configuration space the way the legacy transport did. There is
    /// no other way to find a virtio 1.0 device's registers at all.
    ///
    /// # Safety
    ///
    /// The device must be one enumeration found, so its configuration space is
    /// readable, and nothing else may be writing it.
    pub unsafe fn capabilities<T>(&self, mut look: impl FnMut(u8, u8) -> Option<T>) -> Option<T> {
        /// Bit 4 of the status half of the command register: "this device has a
        /// capability list". A device without one has whatever was left in the
        /// byte at 0x34, and following it walks into somebody else's registers.
        const HAS_CAPABILITIES: u32 = 1 << 20;
        // SAFETY: upheld by the caller.
        if unsafe { read_config(self.address, 0x04) } & HAS_CAPABILITIES == 0 {
            return None;
        }

        // SAFETY: as above.
        let mut at = (unsafe { read_config(self.address, 0x34) } & 0xFC) as u8;
        // Bounded, because the list is a chain of pointers the *device* writes
        // and a device with a loop in it would otherwise stop the machine. The
        // space is 256 bytes and a capability is at least four, so there cannot
        // honestly be more than this many.
        for _ in 0..64 {
            if at < 0x40 {
                break;
            }
            // SAFETY: as above.
            let header = unsafe { read_config(self.address, at) };
            let identifier = (header & 0xFF) as u8;
            if let Some(found) = look(at, identifier) {
                return Some(found);
            }
            let next = ((header >> 8) & 0xFC) as u8;
            if next == 0 || next == at {
                break;
            }
            at = next;
        }
        None
    }

    pub unsafe fn enable(&self) {
        const IO_SPACE: u32 = 1 << 0;
        const MEMORY_SPACE: u32 = 1 << 1;
        const BUS_MASTER: u32 = 1 << 2;

        // SAFETY: upheld by the caller. The status half of the register is
        // write-one-to-clear, so it is masked out rather than written back.
        unsafe {
            let command = read_config(self.address, 0x04) & 0xFFFF;
            write_config(
                self.address,
                0x04,
                command | IO_SPACE | MEMORY_SPACE | BUS_MASTER,
            );
        }
    }

    /// A short description of what the device claims to be.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match (self.class, self.subclass) {
            (0x00, _) => "pre-class device",
            (0x01, 0x01) => "IDE controller",
            (0x01, 0x06) => "SATA controller",
            (0x01, 0x08) => "NVMe controller",
            (0x01, _) => "storage controller",
            (0x02, _) => "network controller",
            (0x03, _) => "display controller",
            (0x04, _) => "multimedia device",
            (0x06, 0x00) => "host bridge",
            (0x06, 0x01) => "ISA bridge",
            (0x06, 0x04) => "PCI-to-PCI bridge",
            (0x06, _) => "bridge",
            (0x0C, 0x03) => "USB controller",
            (0x0C, _) => "serial bus controller",
            _ => "device",
        }
    }
}

/// Where a device's registers are.
#[derive(Debug, Clone, Copy)]
pub enum BaseAddress {
    /// A window in I/O port space.
    Port(u16),
    /// A window in physical memory.
    Memory { base: u64, sixty_four: bool },
}

/// Every device the machine reports.
///
/// # Safety
///
/// Call once, early, before anything else drives a device.
#[must_use]
pub unsafe fn enumerate() -> Vec<Device> {
    let mut found = Vec::new();

    for bus in 0..=255u8 {
        for slot in 0..32u8 {
            // Function zero answers for every device that exists at all, and
            // its header type says whether there are more.
            let address = Address {
                bus,
                device: slot,
                function: 0,
            };
            // SAFETY: reading configuration space is side-effect free.
            let Some(first) = (unsafe { probe(address) }) else {
                continue;
            };

            let multifunction = first.header_type & 0x80 != 0;
            found.push(first);

            if !multifunction {
                continue;
            }
            for function in 1..8u8 {
                let address = Address {
                    bus,
                    device: slot,
                    function,
                };
                // SAFETY: as above.
                if let Some(device) = unsafe { probe(address) } {
                    found.push(device);
                }
            }
        }
    }

    found
}

/// Read one function, if anything answers there.
///
/// # Safety
///
/// See [`read_config`].
unsafe fn probe(address: Address) -> Option<Device> {
    // SAFETY: upheld by the caller.
    let identity = unsafe { read_config(address, 0x00) };
    let vendor = identity as u16;
    if vendor == NO_DEVICE {
        return None;
    }

    // SAFETY: as above.
    let classes = unsafe { read_config(address, 0x08) };
    // SAFETY: as above.
    let header = unsafe { read_config(address, 0x0C) };
    // SAFETY: as above. Only a type-zero header has a subsystem register; a
    // bridge's would name something else, so it is read as zero for one.
    let subsystem = if (header >> 16) as u8 & 0x7F == 0 {
        (unsafe { read_config(address, 0x2C) } >> 16) as u16
    } else {
        0
    };

    // The interrupt line is the low byte of the last register in a type-zero
    // header. A bridge does not have one, so it reads as "connected to nothing".
    // SAFETY: as above.
    let interrupt_line = if (header >> 16) as u8 & 0x7F == 0 {
        (unsafe { read_config(address, 0x3C) }) as u8
    } else {
        0xFF
    };

    Some(Device {
        address,
        vendor,
        device: (identity >> 16) as u16,
        class: (classes >> 24) as u8,
        subclass: (classes >> 16) as u8,
        interface: (classes >> 8) as u8,
        header_type: (header >> 16) as u8,
        subsystem,
        interrupt_line,
    })
}

/// Print what was found.
pub fn report(devices: &[Device]) {
    kprintln!("[pci ] {} devices", devices.len());
    for device in devices {
        kprintln!(
            "[pci ]   {} {:04x}:{:04x} class {:02x}.{:02x}.{:02x} -- {}",
            device.address,
            device.vendor,
            device.device,
            device.class,
            device.subclass,
            // The programming interface, which is what separates an AHCI
            // controller from an IDE one inside the same subclass.
            device.interface,
            device.describe()
        );
    }
}
