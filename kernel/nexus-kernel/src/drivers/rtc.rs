//! The real-time clock, which is the only thing on this machine that knows what
//! day it is.
//!
//! Everything else measures *duration*: the timer counts ticks since boot, and
//! a system with only that can tell you it has been up for nine seconds but not
//! what time it is. A timezone is a correction to a wall clock, so without one
//! there is nothing for a timezone to correct — which is why this exists before
//! the settings that use it.
//!
//! # The two traps
//!
//! **The update in progress flag.** The chip advances its registers a field at
//! a time, so a read that lands in the middle of one can see 01:59:60 becoming
//! 02:00:00 and take the hour from before and the minute from after. Register B
//! bit 7 says an update is under way; reading is only safe when it is clear, and
//! the honest check is to read the whole time twice and require the two to
//! agree.
//!
//! **Binary-coded decimal.** By default the chip stores 25 as `0x25`, not as
//! 25. Which of the two it is doing is in register B bit 2, and a reader that
//! assumed either would be right on some machines and wrong on others — the
//! kind of bug that works on the developer's machine for a year.
//!
//! # What it is not
//!
//! Not a clock that is kept. The time is read once, at boot, and everything
//! afterwards is that reading plus the ticks since. The chip is not read again
//! because reading it costs port I/O and the timer is more accurate over
//! minutes anyway; what the chip is for is knowing where to start.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nexus_time::{from_bcd, Time};

use crate::arch::io::{inb, outb};
use crate::kprintln;

/// Where the register number is written.
const ADDRESS: u16 = 0x70;
/// Where the value is read.
const DATA: u16 = 0x71;

/// The registers this reads.
mod register {
    pub const SECONDS: u8 = 0x00;
    pub const MINUTES: u8 = 0x02;
    pub const HOURS: u8 = 0x04;
    pub const DAY: u8 = 0x07;
    pub const MONTH: u8 = 0x08;
    pub const YEAR: u8 = 0x09;
    /// Status B: how the values are encoded, and whether the hour is 12-hour.
    pub const STATUS_B: u8 = 0x0B;
    /// Status A: bit 7 says an update is in progress.
    pub const STATUS_A: u8 = 0x0A;
}

/// What the clock said at boot, as seconds since 1970.
static BOOT_UNIX: AtomicU64 = AtomicU64::new(0);
/// The tick count when it said it.
static BOOT_TICKS: AtomicU64 = AtomicU64::new(0);
/// Whether a clock was found at all.
static PRESENT: AtomicBool = AtomicBool::new(false);

/// Read one register.
///
/// # Safety
///
/// Uses the CMOS index and data ports, which nothing else on this system
/// touches.
unsafe fn read(index: u8) -> u8 {
    // SAFETY: upheld by the caller. The high bit of the index port is the
    // non-maskable-interrupt disable flag; leaving it clear is what every other
    // reader of this chip does, and setting it here would leave it set.
    unsafe {
        outb(ADDRESS, index & 0x7F);
        inb(DATA)
    }
}

/// Whether the chip is part way through advancing its registers.
///
/// # Safety
///
/// As [`read`].
unsafe fn updating() -> bool {
    // SAFETY: upheld by the caller.
    unsafe { read(register::STATUS_A) & 0x80 != 0 }
}

/// One reading, without regard for whether it is consistent.
///
/// # Safety
///
/// As [`read`].
unsafe fn sample() -> (Time, u8) {
    // SAFETY: upheld by the caller.
    unsafe {
        let status = read(register::STATUS_B);
        let time = Time {
            second: read(register::SECONDS),
            minute: read(register::MINUTES),
            hour: read(register::HOURS),
            day: read(register::DAY),
            month: read(register::MONTH),
            year: u16::from(read(register::YEAR)),
        };
        (time, status)
    }
}

/// Read the clock, correctly.
///
/// # Safety
///
/// Call once, before anything else uses the CMOS ports.
pub unsafe fn init() {
    // Twice, and again until two consecutive readings agree. That is what
    // catches a read straddling an update: the flag is checked first, but a
    // register can still advance between the first and last of the six reads,
    // and two identical readings cannot both have straddled the same boundary.
    let mut previous = None;
    let mut settled = None;
    for _ in 0..16 {
        // SAFETY: upheld by the caller.
        unsafe {
            while updating() {
                core::hint::spin_loop();
            }
            let (raw, status) = sample();
            if previous == Some(raw) {
                settled = Some((raw, status));
                break;
            }
            previous = Some(raw);
        }
    }

    let Some((raw, status)) = settled else {
        kprintln!("[rtc ] the clock would not settle; the system has no wall clock");
        return;
    };

    // Bit 2 clear means the values are binary-coded decimal, which is the
    // default on almost every machine including this one's firmware.
    let binary = status & 0x04 != 0;
    let twenty_four_hour = status & 0x02 != 0;

    let mut hour = raw.hour;
    // In twelve-hour mode the top bit of the hour register means afternoon, and
    // it survives the decimal conversion because it is not part of the digits.
    let afternoon = !twenty_four_hour && (hour & 0x80 != 0);
    hour &= 0x7F;

    let mut time = Time {
        second: if binary {
            raw.second
        } else {
            from_bcd(raw.second)
        },
        minute: if binary {
            raw.minute
        } else {
            from_bcd(raw.minute)
        },
        hour: if binary { hour } else { from_bcd(hour) },
        day: if binary { raw.day } else { from_bcd(raw.day) },
        month: if binary {
            raw.month
        } else {
            from_bcd(raw.month)
        },
        year: u16::from(if binary {
            raw.year as u8
        } else {
            from_bcd(raw.year as u8)
        }),
    };

    if afternoon && time.hour < 12 {
        time.hour += 12;
    } else if !afternoon && !twenty_four_hour && time.hour == 12 {
        // Midnight is twelve in twelve-hour mode, and zero in every other
        // arithmetic.
        time.hour = 0;
    }

    // The register holds two digits. There is no century register that can be
    // relied on -- the one at 0x32 means different things on different
    // firmware -- so the window is chosen instead: this system did not exist
    // before 2000 and will not be read by this code in 2100.
    time.year += if time.year < 70 { 2000 } else { 1900 };

    if time.month == 0 || time.month > 12 || time.day == 0 || time.day > 31 || time.hour > 23 {
        kprintln!(
            "[rtc ] the clock reported {:04}-{:02}-{:02} {:02}:{:02}, which is not a date",
            time.year,
            time.month,
            time.day,
            time.hour,
            time.minute
        );
        return;
    }

    let seconds = time.unix_seconds();
    if seconds < 0 {
        kprintln!("[rtc ] the clock is set before 1970; ignoring it");
        return;
    }

    BOOT_UNIX.store(seconds as u64, Ordering::Release);
    BOOT_TICKS.store(crate::arch::time::ticks(), Ordering::Release);
    PRESENT.store(true, Ordering::Release);

    kprintln!(
        "[rtc ] {:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC ({seconds} seconds since 1970), \
         {} encoding",
        time.year,
        time.month,
        time.day,
        time.hour,
        time.minute,
        time.second,
        if binary { "binary" } else { "packed decimal" }
    );
}

/// Whether the machine knows what time it is.
#[must_use]
pub fn is_present() -> bool {
    PRESENT.load(Ordering::Acquire)
}

/// The time now, as seconds since 1970 in UTC.
///
/// The clock at boot plus the ticks since. Not another read of the chip: port
/// I/O costs more than the timer does, and over the minutes a boot lasts the
/// timer is the more accurate of the two.
#[must_use]
pub fn now() -> Option<u64> {
    if !is_present() {
        return None;
    }
    let elapsed = crate::arch::time::ticks().saturating_sub(BOOT_TICKS.load(Ordering::Acquire));
    let hertz = crate::arch::time::frequency_hz().max(1);
    Some(BOOT_UNIX.load(Ordering::Acquire) + elapsed / hertz)
}
