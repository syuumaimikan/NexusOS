//! The rings an xHCI controller and its driver pass work across.
//!
//! Split from `xhci.rs` because bringing a controller up and *talking* to one
//! are different jobs, and because this half is where every subtle rule lives.
//!
//! # A TRB
//!
//! Sixteen bytes: a 64-bit parameter, a 32-bit status, and a 32-bit control
//! word whose low bits are the cycle bit and whose bits 10 to 15 are the type.
//! Everything either side says to the other is one of these.
//!
//! # The cycle bit, which is the whole protocol
//!
//! A ring has no head pointer. Instead the producer writes each TRB with the
//! current cycle value and flips that value each time it wraps around, and the
//! consumer reads until it meets a TRB whose cycle is *not* what it expects.
//! One bit, and it is how both sides know where the other has got to.
//!
//! Getting it wrong does not fail loudly. A driver that forgot to flip its own
//! cycle on wrap would quietly stop being read; one that forgot to flip the
//! *expected* cycle on the event ring would read the same events for ever. Both
//! look like a controller that has stopped answering, which is why the flip
//! happens in exactly one place for each ring.
//!
//! # The link TRB
//!
//! A ring is a fixed buffer, so the last entry is a link back to the start. It
//! carries a **toggle** bit that tells the consumer to flip its expected cycle
//! when it follows the link -- which is what makes the cycle bit mean anything
//! across a wrap.

use core::sync::atomic::{AtomicU64, Ordering};

use nexus_abi::layout;

use crate::kprintln;

/// TRBs per ring.
///
/// Two hundred and fifty-six, which is exactly one page of sixteen-byte
/// entries. Chosen for that: every structure here is one frame from the frame
/// allocator, so nothing needs contiguous multi-page allocation.
pub const RING_SIZE: usize = 256;

/// The types of TRB this driver writes or reads.
pub mod trb {
    /// Data stages and the like, on a transfer ring.
    pub const NORMAL: u32 = 1;
    pub const SETUP: u32 = 2;
    pub const DATA: u32 = 3;
    pub const STATUS: u32 = 4;
    /// The link back to the start of a ring.
    pub const LINK: u32 = 6;

    /// Commands, on the command ring.
    pub const ENABLE_SLOT: u32 = 9;
    pub const ADDRESS_DEVICE: u32 = 11;
    pub const CONFIGURE_ENDPOINT: u32 = 12;

    /// Events, on the event ring.
    pub const TRANSFER_EVENT: u32 = 32;
    pub const COMMAND_COMPLETE: u32 = 33;
    pub const PORT_STATUS_CHANGE: u32 = 34;
}

/// Completion codes, as an event reports them.
pub mod completion {
    pub const SUCCESS: u32 = 1;
    /// The transfer moved less than was asked for, which for a `GET_DESCRIPTOR`
    /// that asked for more than the descriptor is the ordinary case and not an
    /// error.
    pub const SHORT_PACKET: u32 = 13;
}

/// One TRB, as the controller reads and writes it.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct Trb {
    pub parameter: u64,
    pub status: u32,
    pub control: u32,
}

impl Trb {
    /// What kind it is.
    #[must_use]
    pub const fn kind(self) -> u32 {
        (self.control >> 10) & 0x3F
    }

    /// Its cycle bit.
    #[must_use]
    pub const fn cycle(self) -> bool {
        self.control & 1 != 0
    }

    /// The completion code, for an event.
    #[must_use]
    pub const fn completion(self) -> u32 {
        (self.status >> 24) & 0xFF
    }

    /// How many bytes were *not* transferred, for a transfer event.
    ///
    /// Twenty-four bits. The mask matters: the completion code sits in bits 24
    /// to 31, so a mask one bit too wide picks up the code's low bit and a
    /// successful transfer of everything reports a residue of 16777216. That
    /// was the first bug this driver had, and it looked like the device
    /// returning no data rather than like an arithmetic mistake.
    #[must_use]
    pub const fn residue(self) -> u32 {
        self.status & 0x00FF_FFFF
    }

    /// The slot this event is about.
    #[must_use]
    pub const fn slot(self) -> u8 {
        ((self.control >> 24) & 0xFF) as u8
    }
}

/// A ring the driver writes and the controller reads.
///
/// The command ring and every transfer ring are one of these.
pub struct Producer {
    /// The frame it lives in.
    physical: u64,
    /// Where the next TRB goes.
    at: usize,
    /// What cycle value to stamp on it.
    cycle: bool,
}

impl Producer {
    /// Take a frame and make a ring in it.
    ///
    /// The last entry is a link back to the first, with the toggle bit set, so
    /// the controller flips its expected cycle when it wraps.
    pub fn new() -> Option<Self> {
        let physical = crate::memory::allocate_frame()?;
        // SAFETY: the frame came from the allocator, is this ring's alone, and
        // is reachable through the direct map.
        unsafe {
            core::ptr::write_bytes(layout::phys_to_virt(physical) as *mut u8, 0, 4096);
        }
        let ring = Self {
            physical,
            at: 0,
            cycle: true,
        };
        // The link, written once and never moved.
        let link = Trb {
            parameter: physical,
            status: 0,
            // Type 6, toggle cycle. Its own cycle bit is set to match the
            // ring's initial cycle so the controller follows it the first time
            // round.
            control: (trb::LINK << 10) | (1 << 1) | 1,
        };
        // SAFETY: the last slot is inside the frame.
        unsafe { ring.write_at(RING_SIZE - 1, link) };
        Some(ring)
    }

    /// Where the controller should be told to look.
    #[must_use]
    pub const fn physical(&self) -> u64 {
        self.physical
    }

    /// The cycle value the controller should start with.
    #[must_use]
    pub const fn initial_cycle(&self) -> bool {
        true
    }

    /// Write a TRB into a slot.
    ///
    /// # Safety
    ///
    /// `index` must be below [`RING_SIZE`].
    unsafe fn write_at(&self, index: usize, value: Trb) {
        let at = layout::phys_to_virt(self.physical) + (index * 16) as u64;
        // SAFETY: the offset is inside the ring's own frame, which is mapped.
        // Written as two halves with the control word last, because the control
        // word carries the cycle bit and the controller may read the TRB the
        // instant that bit says it is ready.
        unsafe {
            core::ptr::write_volatile(at as *mut u64, value.parameter);
            core::ptr::write_volatile((at + 8) as *mut u32, value.status);
            core::sync::atomic::compiler_fence(Ordering::SeqCst);
            core::ptr::write_volatile((at + 12) as *mut u32, value.control);
        }
    }

    /// Put a TRB on the ring.
    ///
    /// Returns the physical address it was written at, which is what a transfer
    /// event names when it reports on it.
    pub fn push(&mut self, parameter: u64, status: u32, control: u32) -> u64 {
        // The cycle bit is this ring's, not the caller's: a caller that had to
        // remember it would be a caller that could get it wrong.
        let control = (control & !1) | u32::from(self.cycle);
        let index = self.at;
        // SAFETY: `self.at` is always below `RING_SIZE - 1`; see the wrap below.
        unsafe {
            self.write_at(
                index,
                Trb {
                    parameter,
                    status,
                    control,
                },
            );
        }
        let written_at = self.physical + (index * 16) as u64;

        self.at += 1;
        // The last slot is the link and is never written by `push`. Reaching it
        // means wrapping -- and the cycle flips exactly here, which is the one
        // place in this file it may.
        if self.at == RING_SIZE - 1 {
            // The link's own cycle has to match what the controller expects
            // *now*, or it will stop at the link rather than follow it.
            let link = Trb {
                parameter: self.physical,
                status: 0,
                control: (trb::LINK << 10) | (1 << 1) | u32::from(self.cycle),
            };
            // SAFETY: the last slot is inside the frame.
            unsafe { self.write_at(RING_SIZE - 1, link) };
            self.at = 0;
            self.cycle = !self.cycle;
        }
        written_at
    }
}

/// The ring the controller writes and the driver reads.
pub struct Consumer {
    physical: u64,
    at: usize,
    /// What cycle value marks a TRB the controller has written.
    cycle: bool,
}

impl Consumer {
    /// Take a frame and make an event ring in it.
    pub fn new() -> Option<Self> {
        let physical = crate::memory::allocate_frame()?;
        // SAFETY: the frame came from the allocator and is this ring's alone.
        unsafe {
            core::ptr::write_bytes(layout::phys_to_virt(physical) as *mut u8, 0, 4096);
        }
        Some(Self {
            physical,
            at: 0,
            // The controller starts at one, and the ring was zeroed -- so every
            // entry reads as cycle 0 until it writes one.
            cycle: true,
        })
    }

    #[must_use]
    pub const fn physical(&self) -> u64 {
        self.physical
    }

    /// Where the controller should be told the driver has read up to.
    #[must_use]
    pub const fn dequeue(&self) -> u64 {
        self.physical + (self.at * 16) as u64
    }

    /// Take the next event, if the controller has written one.
    pub fn pop(&mut self) -> Option<Trb> {
        let at = layout::phys_to_virt(self.physical) + (self.at * 16) as u64;
        // SAFETY: the offset is inside the ring's frame, which is mapped
        // uncached -- so this read sees what the controller wrote rather than
        // what a cache line remembers.
        let event = unsafe {
            Trb {
                parameter: core::ptr::read_volatile(at as *const u64),
                status: core::ptr::read_volatile((at + 8) as *const u32),
                control: core::ptr::read_volatile((at + 12) as *const u32),
            }
        };
        if event.cycle() != self.cycle {
            return None;
        }

        self.at += 1;
        // The event ring has no link TRB -- the controller wraps it itself,
        // using the segment table -- so the flip happens on the buffer's end.
        if self.at == RING_SIZE {
            self.at = 0;
            self.cycle = !self.cycle;
        }
        Some(event)
    }
}

/// Everything the controller was given, kept so it is never freed.
///
/// The controller reads these frames for as long as it is running. Dropping
/// them would hand the frame allocator memory a device is still writing into,
/// which is the kind of fault that shows up somewhere else entirely -- so they
/// are held for the life of the machine and that is said out loud rather than
/// left to a comment on a `Vec`.
pub struct Rings {
    pub commands: Producer,
    pub events: Consumer,
    /// The device context base address array.
    pub contexts: u64,
    /// The event ring segment table.
    pub segments: u64,
    /// Scratchpad buffers, if the controller asked for any.
    ///
    /// Held and never read again. The controller writes its own state into
    /// these pages for as long as it runs, so what this field does is keep
    /// them from being freed -- which is a use, and is the reason the count is
    /// reported through [`Rings::scratchpad_count`] rather than from the
    /// register that asked for them.
    pub scratchpad: Option<u64>,
    /// How many pages are behind that array.
    pages: u64,
}

impl Rings {
    /// How many scratchpad pages the controller was given.
    #[must_use]
    pub const fn scratchpad_count(&self) -> u64 {
        if self.scratchpad.is_some() {
            self.pages
        } else {
            0
        }
    }
}

/// Where the device context array is, for a driver adding a slot to it.
static CONTEXTS: AtomicU64 = AtomicU64::new(0);

/// The device context base address array's physical address.
#[must_use]
pub fn contexts() -> u64 {
    CONTEXTS.load(Ordering::Relaxed)
}

/// Build everything the controller needs, and hand it over.
///
/// # Errors
///
/// `None` when a frame could not be had.
pub fn build(slots: u64, scratchpads: u64) -> Option<Rings> {
    let commands = Producer::new()?;
    let events = Consumer::new()?;

    // The device context array: one entry per slot, plus entry zero, which
    // points at the scratchpad array rather than at a device.
    let contexts = crate::memory::allocate_frame()?;
    // SAFETY: a fresh frame, this array's alone.
    unsafe { core::ptr::write_bytes(layout::phys_to_virt(contexts) as *mut u8, 0, 4096) };
    if (slots + 1) * 8 > 4096 {
        // 511 slots would be needed to reach this, and the maximum is 255.
        kprintln!("[usb ] {slots} device slots will not fit one page of contexts");
        return None;
    }

    // Scratchpad buffers: pages the controller keeps its own state in. How many
    // it wants is in HCSPARAMS2, and a controller that asked for some and did
    // not get them misbehaves in ways that are very hard to read.
    let scratchpad = if scratchpads > 0 {
        let array = crate::memory::allocate_frame()?;
        // SAFETY: a fresh frame, this array's alone.
        unsafe { core::ptr::write_bytes(layout::phys_to_virt(array) as *mut u8, 0, 4096) };
        if scratchpads * 8 > 4096 {
            kprintln!("[usb ] the controller wants {scratchpads} scratchpad pages, which is absurd");
            return None;
        }
        for index in 0..scratchpads {
            let page = crate::memory::allocate_frame()?;
            // SAFETY: a fresh frame, handed to the controller and never touched
            // again by this driver.
            unsafe {
                core::ptr::write_bytes(layout::phys_to_virt(page) as *mut u8, 0, 4096);
                core::ptr::write_volatile(
                    (layout::phys_to_virt(array) + index * 8) as *mut u64,
                    page,
                );
            }
        }
        // Entry zero of the device context array points at it.
        // SAFETY: inside the array's own frame.
        unsafe { core::ptr::write_volatile(layout::phys_to_virt(contexts) as *mut u64, array) };
        Some(array)
    } else {
        None
    };

    // The event ring segment table: one segment, which is the event ring.
    let segments = crate::memory::allocate_frame()?;
    // SAFETY: a fresh frame, this table's alone. The entry is a base address
    // and a size, and the rest is reserved and must be zero -- which it is,
    // because the frame was zeroed.
    unsafe {
        core::ptr::write_bytes(layout::phys_to_virt(segments) as *mut u8, 0, 4096);
        core::ptr::write_volatile(layout::phys_to_virt(segments) as *mut u64, events.physical());
        core::ptr::write_volatile(
            (layout::phys_to_virt(segments) + 8) as *mut u32,
            RING_SIZE as u32,
        );
    }

    CONTEXTS.store(contexts, Ordering::Relaxed);
    Some(Rings {
        commands,
        events,
        contexts,
        segments,
        scratchpad,
        pages: scratchpads,
    })
}
