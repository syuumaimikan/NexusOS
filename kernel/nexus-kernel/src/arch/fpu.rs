//! The floating-point and vector registers, and whose they are.
//!
//! This kernel does not use them. Its target says `+soft-float` and turns off
//! every SSE feature, which is the ordinary choice for kernel code: floating
//! point in an interrupt handler means saving the whole unit before you can add
//! two numbers, and there is nothing in a kernel worth that.
//!
//! Its own user programs are built the same way, for the same reason and one
//! more: a program that never touches `xmm` is a program whose context switch
//! is sixteen registers rather than sixteen registers and half a kilobyte.
//!
//! # And then a foreign program arrived
//!
//! A program built for Linux is not built that way and cannot be. The x86-64
//! System V ABI *is* SSE: `double` is passed in `xmm0`, a structure of two
//! floats comes back in `xmm0`, and every `memcpy`, `strlen` and `memset` in
//! every C library is written with it. A compiler targeting Linux emits
//! `xorps` to zero sixteen bytes of stack, because that is the cheapest way.
//!
//! On a machine that has never enabled SSE, the first of those instructions
//! raises **invalid opcode** — which is what it did, at the third system call
//! of the first compiled Linux program to run here:
//!
//! ```text
//! EXCEPTION 6: invalid opcode
//!   taken from : user mode
//!   rip        : 0x00000000004014ca      ; xorps %xmm0, %xmm0
//! ```
//!
//! Not a subtle failure, and not one a compatibility layer can translate
//! around: there is no system call involved. The processor has to be told the
//! registers exist.
//!
//! # What this does
//!
//! Two things, and the second is the one that is easy to forget.
//!
//! **Enables the unit**, per processor: `CR0.EM` cleared so SSE is not trapped
//! as an emulated coprocessor, `CR0.MP` set, `CR4.OSFXSR` set to say this
//! kernel can save the state, and `CR4.OSXMMEXCPT` set so an SSE numeric error
//! arrives as `#XM` rather than as a misleading `#UD`.
//!
//! **Makes the state per thread.** Enabling the unit without saving it would be
//! worse than leaving it off: two threads would share sixteen vector registers,
//! and a program's `double` would change value because something else was
//! scheduled. Every thread carries a [`State`], and the switch saves and
//! restores it.
//!
//! # Why `fxsave` and not `xsave`
//!
//! `fxsave` covers x87, MMX and SSE — the whole of the base x86-64 ABI, which
//! is what a Linux program is entitled to. `xsave` would be needed for AVX, and
//! AVX needs `XCR0` and `CR4.OSXSAVE` as well; a program built with `-mavx2`
//! will still take `#UD` here. That is a real limit and it is written down
//! rather than discovered: the guest programs in this repository are built for
//! the base `x86-64` target, which is SSE2 and no more.

use core::arch::asm;

/// `CR0.MP`: monitor coprocessor.
const CR0_MP: u64 = 1 << 1;
/// `CR0.EM`: emulate coprocessor. Set means every SSE instruction traps.
const CR0_EM: u64 = 1 << 2;
/// `CR0.TS`: task switched. Set means the *next* use of the unit traps, which
/// is how a kernel implements lazy switching. This one does not: see [`State`].
const CR0_TS: u64 = 1 << 3;
/// `CR4.OSFXSR`: the operating system saves SSE state with `fxsave`.
const CR4_OSFXSR: u64 = 1 << 9;
/// `CR4.OSXMMEXCPT`: an unmasked SSE numeric error raises `#XM`.
const CR4_OSXMMEXCPT: u64 = 1 << 10;

/// What one thread's floating-point and vector registers are worth.
///
/// Five hundred and twelve bytes, sixteen-byte aligned, which is what `fxsave`
/// writes and what `fxrstor` reads. Both fault if the alignment is wrong, so
/// the alignment is on the type rather than left to whoever allocates one.
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct State {
    bytes: [u8; 512],
}

/// Where the control word sits in an `fxsave` image.
const FCW: usize = 0;
/// And the SSE control and status word.
const MXCSR: usize = 24;

/// The x87 control word a thread starts with.
///
/// `0x037F`: round to nearest, extended precision, every exception masked. What
/// `finit` leaves behind, and what a program is entitled to assume.
const FCW_DEFAULT: u16 = 0x037F;
/// And the SSE one. `0x1F80`: every exception masked, round to nearest.
///
/// Not zero. A zeroed `MXCSR` unmasks every SSE exception, so the first
/// division that produced an inexact result — which is most of them — would
/// raise `#XM` in a program that had done nothing wrong.
const MXCSR_DEFAULT: u32 = 0x1F80;

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

impl State {
    /// The state a thread that has never used the unit should start with.
    ///
    /// Zeroed, with the two control words set to what `finit` would leave. A
    /// thread given a zeroed image would start with every SSE exception
    /// unmasked, which is not "no state" — it is a different and much less
    /// forgiving arithmetic.
    #[must_use]
    pub fn new() -> Self {
        let mut state = Self { bytes: [0u8; 512] };
        state.bytes[FCW..FCW + 2].copy_from_slice(&FCW_DEFAULT.to_le_bytes());
        state.bytes[MXCSR..MXCSR + 4].copy_from_slice(&MXCSR_DEFAULT.to_le_bytes());
        state
    }

    /// Write this processor's registers into `self`.
    ///
    /// # Safety
    ///
    /// The unit must be enabled on this processor — see [`enable`].
    pub unsafe fn save(&mut self) {
        // SAFETY: `self` is 512 bytes and 16-byte aligned by its type, which is
        // what `fxsave` requires; the caller promises the unit is enabled.
        unsafe {
            asm!("fxsave64 [{at}]", at = in(reg) self.bytes.as_mut_ptr(), options(nostack));
        }
    }

    /// Load `self` into this processor's registers.
    ///
    /// # Safety
    ///
    /// As [`save`](Self::save). The image must be one `fxsave` produced or
    /// [`new`](Self::new) built: `fxrstor` faults on a reserved bit pattern in
    /// `MXCSR`, so an image from anywhere else can fault here.
    pub unsafe fn restore(&self) {
        // SAFETY: as above.
        unsafe {
            asm!("fxrstor64 [{at}]", at = in(reg) self.bytes.as_ptr(), options(nostack));
        }
    }
}

/// Turn the unit on, on this processor.
///
/// Called once per processor, before anything runs in ring 3 on it. Until this
/// has run, an SSE instruction raises invalid opcode — which is the truth about
/// the machine rather than a bug, and is what it did before this existed.
///
/// # Safety
///
/// Called once per processor, early, with nothing using the unit yet.
pub unsafe fn enable() {
    // SAFETY: reading and writing this processor's own control registers, in
    // ring 0, before anything depends on their previous values.
    unsafe {
        let mut cr0: u64;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        // `EM` cleared: SSE is not to be trapped as an emulated coprocessor.
        // `TS` cleared: this kernel saves the state on every switch rather than
        // waiting to be told the unit was touched, so there is nothing for the
        // lazy path to do except make the first use of every thread fault.
        cr0 = (cr0 | CR0_MP) & !(CR0_EM | CR0_TS);
        asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack));

        let mut cr4: u64;
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        cr4 |= CR4_OSFXSR | CR4_OSXMMEXCPT;
        asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));

        // And a known starting state for whatever runs here first, so the very
        // first thread does not inherit whatever the firmware left.
        asm!("fninit", options(nomem, nostack));
    }
}

/// Whether this processor has the unit enabled.
///
/// Read back from the control registers rather than remembered, so that what it
/// reports is the processor's own answer.
#[must_use]
pub fn enabled() -> bool {
    // SAFETY: reading this processor's own control registers.
    let (cr0, cr4): (u64, u64) = unsafe {
        let mut cr0: u64;
        let mut cr4: u64;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
        (cr0, cr4)
    };
    cr0 & CR0_EM == 0 && cr4 & CR4_OSFXSR != 0
}
