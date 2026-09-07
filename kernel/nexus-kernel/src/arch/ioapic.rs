//! The I/O APIC.
//!
//! The local APIC handles interrupts a processor raises for itself — its timer,
//! its errors, messages from other processors. Everything a *device* raises
//! arrives through an I/O APIC instead, which is what makes this the
//! precondition for any driver at all: without it the only interrupt source in
//! the system is the timer.
//!
//! # Programming model
//!
//! Two registers in the memory-mapped window, not one per setting: write a
//! register number to the selector, then read or write the data window. Each
//! input pin has a 64-bit redirection entry saying which vector to raise, on
//! which processor, and how the line behaves electrically. Every entry powers
//! up masked, so a pin is silent until something asks for it.
//!
//! # Why the routing is not the obvious one
//!
//! Legacy IRQ numbers are not I/O APIC pin numbers. Firmware describes the
//! difference in the ACPI interrupt source overrides, and a kernel that assumes
//! IRQ *n* means pin *n* programs the wrong pin on a great many machines. The
//! caller resolves that before getting here; this module takes a global system
//! interrupt, which is the number that actually identifies a pin.

use core::sync::atomic::{AtomicUsize, Ordering};

use nexus_abi::layout;

use crate::kprintln;
use crate::memory::paging;

/// Offset of the register selector within the window.
const REGISTER_SELECT: usize = 0x00;
/// Offset of the data window.
const REGISTER_WINDOW: usize = 0x10;

/// Register holding the identifier.
const REGISTER_ID: u32 = 0x00;
/// Register holding the version and the highest redirection entry.
const REGISTER_VERSION: u32 = 0x01;
/// First redirection entry. Each occupies two consecutive registers.
const REGISTER_REDIRECTION_BASE: u32 = 0x10;

/// Redirection entry: the interrupt is not delivered.
const ENTRY_MASKED: u64 = 1 << 16;
/// Redirection entry: level triggered rather than edge triggered.
const ENTRY_LEVEL_TRIGGERED: u64 = 1 << 15;
/// Redirection entry: the line is asserted low rather than high.
const ENTRY_ACTIVE_LOW: u64 = 1 << 13;

/// Virtual address the window is mapped at, or 0 before [`init`].
static REGISTERS: AtomicUsize = AtomicUsize::new(0);

/// The first global system interrupt this I/O APIC handles.
static GSI_BASE: AtomicUsize = AtomicUsize::new(0);

/// Number of pins it has.
static PIN_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Why the I/O APIC could not be brought up.
#[derive(Debug, Clone, Copy)]
pub enum IoApicError {
    /// Firmware reported no I/O APIC.
    NotPresent,
    /// The register window could not be mapped.
    MapFailed(paging::MapError),
    /// The window is mapped but reads as if it were not.
    NotResponding,
    /// A pin outside the range this I/O APIC serves.
    PinOutOfRange { pin: u32, count: usize },
}

impl core::fmt::Display for IoApicError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotPresent => f.write_str("firmware reported no I/O APIC"),
            Self::MapFailed(error) => write!(f, "could not map the I/O APIC: {error}"),
            Self::NotResponding => f.write_str("the I/O APIC window does not respond"),
            Self::PinOutOfRange { pin, count } => {
                write!(f, "pin {pin} is outside the {count} this I/O APIC has")
            }
        }
    }
}

/// Read an I/O APIC register.
///
/// # Safety
///
/// The window must be mapped.
unsafe fn read(register: u32) -> u32 {
    let base = REGISTERS.load(Ordering::Acquire);
    debug_assert!(base != 0, "the I/O APIC is not mapped");
    // SAFETY: the window is mapped uncached. The selector must be written
    // before the data window is read; they are not independent registers.
    unsafe {
        core::ptr::write_volatile((base + REGISTER_SELECT) as *mut u32, register);
        core::ptr::read_volatile((base + REGISTER_WINDOW) as *const u32)
    }
}

/// Write an I/O APIC register.
///
/// # Safety
///
/// See [`read`].
unsafe fn write(register: u32, value: u32) {
    let base = REGISTERS.load(Ordering::Acquire);
    debug_assert!(base != 0, "the I/O APIC is not mapped");
    // SAFETY: as above.
    unsafe {
        core::ptr::write_volatile((base + REGISTER_SELECT) as *mut u32, register);
        core::ptr::write_volatile((base + REGISTER_WINDOW) as *mut u32, value);
    }
}

/// Map the I/O APIC and mask every pin.
///
/// # Safety
///
/// Call once, after the virtual memory manager is running. `address` must be
/// the window firmware reported.
pub unsafe fn init(address: u32, gsi_base: u32) -> Result<(), IoApicError> {
    if address == 0 {
        return Err(IoApicError::NotPresent);
    }

    // One page past the local APIC in the kernel's device window. Uncached, for
    // the same reason as every device register: a cached write has not reached
    // the device, and a cached read is not the device's answer.
    let virtual_address = layout::KERNEL_MMIO_BASE + 4096;
    // SAFETY: the address is a device window from firmware, and this page of
    // the device window is used by nothing else.
    unsafe {
        paging::map_page(
            virtual_address,
            u64::from(address),
            paging::WRITABLE | paging::NO_CACHE | paging::NO_EXECUTE | paging::GLOBAL,
        )
        .map_err(IoApicError::MapFailed)?;
    }
    REGISTERS.store(virtual_address as usize, Ordering::Release);
    GSI_BASE.store(gsi_base as usize, Ordering::Release);

    // SAFETY: the window is mapped.
    let version = unsafe { read(REGISTER_VERSION) };
    if version == 0 || version == u32::MAX {
        REGISTERS.store(0, Ordering::Release);
        return Err(IoApicError::NotResponding);
    }

    // Bits 16..24 hold the highest entry number, so the count is one more.
    let pins = ((version >> 16) & 0xFF) as usize + 1;
    PIN_COUNT.store(pins, Ordering::Release);

    // Mask every pin. They power up masked, but a warm reset can leave stale
    // entries behind, and an interrupt into a vector with no handler is worse
    // than no interrupt at all.
    for pin in 0..pins as u32 {
        // SAFETY: `pin` is below the count just read.
        unsafe { write_entry(pin, ENTRY_MASKED) };
    }

    // SAFETY: the window is mapped.
    let id = unsafe { read(REGISTER_ID) } >> 24 & 0xF;
    kprintln!(
        "[ioapic] I/O APIC {id} version {:#x} at {:#010x}, {pins} pins from global interrupt {gsi_base}",
        version & 0xFF,
        address
    );

    Ok(())
}

/// Write a redirection entry.
///
/// # Safety
///
/// The window must be mapped and `pin` must be within range. The entry is
/// written high half first: the low half carries the mask bit, so writing it
/// last means the pin cannot deliver against a half-written destination.
unsafe fn write_entry(pin: u32, entry: u64) {
    let register = REGISTER_REDIRECTION_BASE + pin * 2;
    // SAFETY: upheld by the caller.
    unsafe {
        write(register + 1, (entry >> 32) as u32);
        write(register, entry as u32);
    }
}

/// Route a global system interrupt to `vector` on the processor with
/// `destination_apic_id`, and unmask it.
///
/// # Safety
///
/// A handler must already be registered for `vector`, or the first interrupt
/// lands on the catch-all and stops the machine.
pub unsafe fn route(
    global_system_interrupt: u32,
    vector: u8,
    destination_apic_id: u32,
    active_low: bool,
    level_triggered: bool,
) -> Result<(), IoApicError> {
    if REGISTERS.load(Ordering::Acquire) == 0 {
        return Err(IoApicError::NotPresent);
    }

    let base = GSI_BASE.load(Ordering::Acquire) as u32;
    let count = PIN_COUNT.load(Ordering::Acquire);
    let pin = global_system_interrupt.saturating_sub(base);
    if pin as usize >= count {
        return Err(IoApicError::PinOutOfRange { pin, count });
    }

    // Fixed delivery, physical destination mode, unmasked. The destination sits
    // in the top eight bits of the 64-bit entry.
    let mut entry = u64::from(vector) | (u64::from(destination_apic_id) << 56);
    if active_low {
        entry |= ENTRY_ACTIVE_LOW;
    }
    if level_triggered {
        entry |= ENTRY_LEVEL_TRIGGERED;
    }

    // SAFETY: `pin` was just bounds-checked, and the caller guarantees a
    // handler exists for `vector`.
    unsafe { write_entry(pin, entry) };
    Ok(())
}

/// Whether the I/O APIC is up.
#[must_use]
pub fn is_active() -> bool {
    REGISTERS.load(Ordering::Acquire) != 0
}
