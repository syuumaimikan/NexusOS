//! Serial logging over the 16550-compatible UART at COM1.
//!
//! The serial port is the bootloader's primary diagnostic channel: it works
//! before the framebuffer is up, it survives `ExitBootServices`, and QEMU can
//! redirect it to a file for automated boot testing.
//!
//! This module is intentionally free of locking. The bootloader is
//! single-threaded with interrupts effectively quiescent, so a spinlock would
//! only add a way to deadlock inside a panic handler.

use core::fmt::{self, Write};

/// I/O port base of COM1 on a PC-compatible machine.
const COM1_BASE: u16 = 0x3F8;

const REG_DATA: u16 = 0;
const REG_INTERRUPT_ENABLE: u16 = 1;
const REG_DIVISOR_LOW: u16 = 0;
const REG_DIVISOR_HIGH: u16 = 1;
const REG_FIFO_CONTROL: u16 = 2;
const REG_LINE_CONTROL: u16 = 3;
const REG_MODEM_CONTROL: u16 = 4;
const REG_LINE_STATUS: u16 = 5;

/// `LCR` bit that remaps the first two registers onto the baud divisor.
const LCR_DLAB: u8 = 0x80;
/// 8 data bits, no parity, 1 stop bit.
const LCR_8N1: u8 = 0x03;
/// `LSR` bit: the transmit holding register is empty.
const LSR_THR_EMPTY: u8 = 0x20;

/// Write a byte to an I/O port.
///
/// # Safety
///
/// Port I/O is a side effect on arbitrary hardware; the caller must know that
/// `port` belongs to a device that tolerates the write.
#[inline]
unsafe fn outb(port: u16, value: u8) {
    // SAFETY: `out` with a `u8` operand is well-defined for any port number;
    // the caller has asserted this port is the UART.
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Read a byte from an I/O port.
///
/// # Safety
///
/// See [`outb`].
#[inline]
unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: as above; reading a UART register has no memory effects.
    unsafe {
        core::arch::asm!(
            "in al, dx",
            out("al") value,
            in("dx") port,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

/// A 16550 UART used as a byte sink.
pub struct SerialPort {
    base: u16,
}

impl SerialPort {
    /// Create a handle to the UART at `base` without touching the hardware.
    #[must_use]
    pub const fn new(base: u16) -> Self {
        Self { base }
    }

    /// Program the UART for 115200 baud, 8N1, with its FIFOs enabled.
    pub fn init(&mut self) {
        // SAFETY: `self.base` addresses a 16550 UART; this is the standard
        // initialisation sequence and touches only that device's registers.
        unsafe {
            outb(self.base + REG_INTERRUPT_ENABLE, 0x00);
            outb(self.base + REG_LINE_CONTROL, LCR_DLAB);
            // Divisor 1 selects 115200 baud from the 1.8432 MHz reference clock.
            outb(self.base + REG_DIVISOR_LOW, 0x01);
            outb(self.base + REG_DIVISOR_HIGH, 0x00);
            outb(self.base + REG_LINE_CONTROL, LCR_8N1);
            // Enable and clear both FIFOs, interrupt at 14 bytes.
            outb(self.base + REG_FIFO_CONTROL, 0xC7);
            // DTR + RTS + OUT2.
            outb(self.base + REG_MODEM_CONTROL, 0x0B);
        }
    }

    /// Send one byte, spinning until the transmit register drains.
    pub fn write_byte(&mut self, byte: u8) {
        // SAFETY: reads and writes are confined to this UART's register block.
        unsafe {
            while inb(self.base + REG_LINE_STATUS) & LSR_THR_EMPTY == 0 {
                core::hint::spin_loop();
            }
            outb(self.base + REG_DATA, byte);
        }
    }
}

impl Write for SerialPort {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // Terminals expect CRLF; the rest of the system emits bare LF.
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

/// The bootloader's single UART instance.
static mut COM1: SerialPort = SerialPort::new(COM1_BASE);

/// Bring up the logging UART. Called once, first thing in `efi_main`.
pub fn init() {
    // SAFETY: the bootloader is single-threaded and this runs before any other
    // code can reach `COM1`.
    unsafe {
        (*core::ptr::addr_of_mut!(COM1)).init();
    }
}

/// Write pre-formatted arguments to the UART. Used by the `log!` macros.
pub fn write_fmt(args: fmt::Arguments) {
    // SAFETY: single-threaded access, as in `init`.
    unsafe {
        let port = &mut *core::ptr::addr_of_mut!(COM1);
        // A serial write cannot fail in a way we could act on, and the logger
        // must never be the reason boot stops.
        let _ = port.write_fmt(args);
    }
}

/// Log a line to the serial port, prefixed with the bootloader tag.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::serial::write_fmt(format_args!("[nexus-boot] {}\n", format_args!($($arg)*)))
    };
}

/// Log a line without the tag, for continuation lines and banners.
#[macro_export]
macro_rules! log_raw {
    ($($arg:tt)*) => {
        $crate::serial::write_fmt(format_args!($($arg)*))
    };
}
