//! Kernel serial console over the 16550-compatible UART at COM1.
//!
//! This is the kernel's earliest and most reliable diagnostic channel: it is
//! available before paging is reconfigured, before any driver exists, and
//! inside the panic handler. QEMU redirects it to a file, which is what the
//! automated boot tests read.

use core::fmt::{self, Write};

use crate::arch::io::{inb, outb};
use crate::sync::SpinLock;

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

const LCR_DLAB: u8 = 0x80;
const LCR_8N1: u8 = 0x03;
const LSR_THR_EMPTY: u8 = 0x20;

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
        // SAFETY: `self.base` addresses a 16550 UART; this touches only that
        // device's register block.
        unsafe {
            outb(self.base + REG_INTERRUPT_ENABLE, 0x00);
            outb(self.base + REG_LINE_CONTROL, LCR_DLAB);
            // Divisor 1 selects 115200 baud from the 1.8432 MHz reference clock.
            outb(self.base + REG_DIVISOR_LOW, 0x01);
            outb(self.base + REG_DIVISOR_HIGH, 0x00);
            outb(self.base + REG_LINE_CONTROL, LCR_8N1);
            outb(self.base + REG_FIFO_CONTROL, 0xC7);
            outb(self.base + REG_MODEM_CONTROL, 0x0B);
        }
    }

    /// Send one byte, spinning until the transmit holding register drains.
    pub fn write_byte(&mut self, byte: u8) {
        // SAFETY: confined to this UART's register block.
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
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

/// The kernel's COM1 instance.
static COM1: SpinLock<SerialPort> = SpinLock::new(SerialPort::new(COM1_BASE));

/// Bring up the serial console. Called once, first thing in `_start`.
pub fn init() {
    COM1.lock().init();
}

/// Write formatted output to the serial console.
pub fn write_fmt(args: fmt::Arguments) {
    // A failed serial write is not actionable and must never be the reason the
    // kernel stops making progress.
    let _ = COM1.lock().write_fmt(args);
}

/// Write formatted output without taking the lock.
///
/// Reserved for the panic handler, which must produce output even if it
/// interrupted a thread that was holding the serial lock.
///
/// # Safety
///
/// May interleave with another core's output. Only call once the system is
/// already stopping.
pub unsafe fn write_fmt_unlocked(args: fmt::Arguments) {
    // SAFETY: the caller accepts interleaved output; `SerialPort` itself has no
    // invariants that a concurrent writer could break, since every write is an
    // independent port access.
    let mut port = SerialPort::new(COM1_BASE);
    let _ = port.write_fmt(args);
}
