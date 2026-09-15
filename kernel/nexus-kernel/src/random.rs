//! Unpredictable bytes, or an honest refusal.
//!
//! # Why this file is careful out of proportion to its length
//!
//! A TLS connection's security rests entirely on the client's ephemeral private
//! key being unguessable. A key derived from the uptime counter is a key an
//! attacker who knows roughly when the machine booted can search in seconds —
//! and the handshake would *succeed*, the padlock would appear, and nothing
//! would be protected.
//!
//! So this either produces bytes from the processor's hardware generator or it
//! produces nothing, and [`bytes`] returns `None` rather than something
//! plausible. Everything above it is written so that `None` means "no TLS",
//! with a sentence saying why, rather than "TLS with a guessable key".
//!
//! # `RDSEED` before `RDRAND`
//!
//! Both are hardware instructions. `RDRAND` is the output of a cryptographic
//! generator seeded from an entropy source; `RDSEED` is closer to the source
//! itself and is what you want for seeding a key. `RDSEED` fails more often --
//! it is rate-limited by how fast the machine gathers entropy -- so both are
//! tried, in that order, and each is retried as Intel's own guidance says.
//!
//! # What this is not
//!
//! It is not a pool, it is not a CSPRNG, and it does not mix in timing jitter,
//! disk latency or interrupt arrival. Those are real sources and a serious
//! system uses them, particularly on hardware with no `RDRAND` — but a *badly*
//! built pool is worse than none, because it looks like an answer. When this
//! machine needs to work on a processor without these instructions, the honest
//! next step is a jitter-based seeder with a real analysis behind it, not a
//! hash of whatever was lying around.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::kprintln;

/// Whether the processor has `RDRAND`, and whether it has `RDSEED`.
static HAS_RDRAND: AtomicBool = AtomicBool::new(false);
static HAS_RDSEED: AtomicBool = AtomicBool::new(false);

/// How many bytes have been handed out, and how many requests were refused.
static GIVEN: AtomicU64 = AtomicU64::new(0);
static REFUSED: AtomicU64 = AtomicU64::new(0);

/// How many times each instruction may say "not ready" before giving up.
///
/// Intel's guidance is ten for `RDRAND`; `RDSEED` is slower to replenish and is
/// given more room. Both are small enough that a processor whose generator has
/// genuinely failed does not hang the machine.
const RDRAND_TRIES: u32 = 10;
const RDSEED_TRIES: u32 = 100;

/// Ask the processor what it has. Called once, from the boot path.
pub fn init() {
    let (has_rdrand, has_rdseed) = features();
    HAS_RDRAND.store(has_rdrand, Ordering::Relaxed);
    HAS_RDSEED.store(has_rdseed, Ordering::Relaxed);

    match (has_rdseed, has_rdrand) {
        (true, _) => kprintln!("[rand] RDSEED and RDRAND: this machine can make keys"),
        (false, true) => kprintln!("[rand] RDRAND only; no RDSEED. Usable, and noted."),
        (false, false) => kprintln!(
            "[rand] this processor has neither RDSEED nor RDRAND. \
             Nothing here will make a key, and TLS will refuse rather than \
             use a guessable one."
        ),
    }
}

/// Whether this machine can produce unpredictable bytes at all.
#[must_use]
pub fn available() -> bool {
    HAS_RDSEED.load(Ordering::Relaxed) || HAS_RDRAND.load(Ordering::Relaxed)
}

/// How many bytes have been given out, and how many requests were refused.
#[must_use]
pub fn statistics() -> (u64, u64) {
    (
        GIVEN.load(Ordering::Relaxed),
        REFUSED.load(Ordering::Relaxed),
    )
}

/// Fill `out` with unpredictable bytes, or say that it cannot.
///
/// `false` means the buffer is untouched and the caller must not proceed. It is
/// never a partial fill: half a key is not a weaker key, it is a key with a
/// known half.
#[must_use]
pub fn bytes(out: &mut [u8]) -> bool {
    if !available() {
        REFUSED.fetch_add(1, Ordering::Relaxed);
        return false;
    }

    // Into a scratch buffer, and copied over only once every word has been
    // had. A generator that fails halfway must not leave the caller holding
    // something that looks filled.
    let mut filled = 0usize;
    let mut scratch = [0u8; 64];
    let mut taken = alloc::vec::Vec::with_capacity(out.len());

    while filled < out.len() {
        let Some(word) = word() else {
            REFUSED.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        let piece = word.to_le_bytes();
        let take = piece.len().min(out.len() - filled);
        scratch[..take].copy_from_slice(&piece[..take]);
        taken.extend_from_slice(&scratch[..take]);
        filled += take;
    }

    out.copy_from_slice(&taken);
    GIVEN.fetch_add(out.len() as u64, Ordering::Relaxed);
    true
}

/// One sixty-four-bit word, from whichever instruction this processor has.
fn word() -> Option<u64> {
    if HAS_RDSEED.load(Ordering::Relaxed) {
        for _ in 0..RDSEED_TRIES {
            // SAFETY: `rdseed` was reported by CPUID. It writes a register and
            // sets the carry flag, and has no other effect.
            if let Some(value) = unsafe { rdseed() } {
                return Some(value);
            }
        }
        // Falling through rather than failing: a processor whose RDSEED is
        // merely busy still has a perfectly good RDRAND behind it, which is
        // seeded from the same source.
    }
    if HAS_RDRAND.load(Ordering::Relaxed) {
        for _ in 0..RDRAND_TRIES {
            // SAFETY: as above.
            if let Some(value) = unsafe { rdrand() } {
                return Some(value);
            }
        }
    }
    None
}

/// What CPUID says about the two instructions.
fn features() -> (bool, bool) {
    // Leaf 1, ECX bit 30: RDRAND.
    let ecx: u32;
    // SAFETY: leaf 1 is available on every processor that reaches long mode.
    // `rbx` is saved and restored because LLVM reserves it.
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "pop rbx",
            inout("eax") 1u32 => _,
            out("ecx") ecx,
            out("edx") _,
            options(nostack, preserves_flags),
        );
    }
    let has_rdrand = ecx & (1 << 30) != 0;

    // Leaf 7 subleaf 0, EBX bit 18: RDSEED. Only asked for if the processor
    // says leaf 7 exists, because CPUID with too high a leaf returns the
    // highest one instead and the answer would be some other leaf's bits.
    let highest: u32;
    // SAFETY: as above.
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "pop rbx",
            inout("eax") 0u32 => highest,
            out("ecx") _,
            out("edx") _,
            options(nostack, preserves_flags),
        );
    }
    if highest < 7 {
        return (has_rdrand, false);
    }

    let ebx: u32;
    // SAFETY: leaf 7 exists, as just checked. `rbx` carries the answer here, so
    // it is moved out before being restored.
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {out:e}, ebx",
            "pop rbx",
            out = out(reg) ebx,
            inout("eax") 7u32 => _,
            inout("ecx") 0u32 => _,
            out("edx") _,
            options(nostack, preserves_flags),
        );
    }
    (has_rdrand, ebx & (1 << 18) != 0)
}

/// `RDRAND`, once. `None` when the processor says it is not ready.
///
/// # Safety
///
/// The processor must have the instruction, which [`features`] checked.
unsafe fn rdrand() -> Option<u64> {
    let value: u64;
    let ok: u8;
    // SAFETY: upheld by the caller. `rdrand` writes its operand and the carry
    // flag and has no other effect; `setc` reads the flag.
    unsafe {
        core::arch::asm!(
            "rdrand {value}",
            "setc {ok}",
            value = out(reg) value,
            ok = out(reg_byte) ok,
            options(nostack),
        );
    }
    (ok != 0).then_some(value)
}

/// `RDSEED`, once. `None` when the entropy source has not caught up.
///
/// # Safety
///
/// The processor must have the instruction, which [`features`] checked.
unsafe fn rdseed() -> Option<u64> {
    let value: u64;
    let ok: u8;
    // SAFETY: upheld by the caller, and as above.
    unsafe {
        core::arch::asm!(
            "rdseed {value}",
            "setc {ok}",
            value = out(reg) value,
            ok = out(reg_byte) ok,
            options(nostack),
        );
    }
    (ok != 0).then_some(value)
}

/// Check the generator at boot, and say what was found.
///
/// Not a test of randomness -- no short test can be -- but of the two failures
/// that actually happen: an instruction that reports success and writes
/// nothing, and one that returns the same value every time. Both have been
/// seen on real silicon with errata, and both would be invisible without a
/// check because the output still *looks* like bytes.
pub fn self_test() {
    if !available() {
        return;
    }

    let mut first = [0u8; 32];
    let mut second = [0u8; 32];
    if !bytes(&mut first) || !bytes(&mut second) {
        kprintln!("[rand] FAILED: the generator stopped answering during its own check");
        return;
    }

    if first == second {
        // Two reads the same is either a stuck generator or one that is not
        // running at all. Turned off rather than reported and used: a
        // generator that repeats is worse than none, because callers believe
        // it.
        HAS_RDRAND.store(false, Ordering::Relaxed);
        HAS_RDSEED.store(false, Ordering::Relaxed);
        kprintln!("[rand] FAILED: two reads gave the same bytes; the generator is turned off");
        return;
    }
    if first.iter().all(|byte| *byte == 0) || first.iter().all(|byte| *byte == 0xFF) {
        HAS_RDRAND.store(false, Ordering::Relaxed);
        HAS_RDSEED.store(false, Ordering::Relaxed);
        kprintln!("[rand] FAILED: the generator returned a constant; it is turned off");
        return;
    }

    // A count of set bits, which for 256 random bits sits near 128. The bound
    // is deliberately loose: this is looking for a generator that is broken in
    // an obvious way, not doing statistics, and a tight bound on one sample
    // would fail on a working machine now and then.
    let ones: u32 = first.iter().map(|byte| byte.count_ones()).sum();
    if !(64..=192).contains(&ones) {
        kprintln!("[rand] {ones} bits set in 256; that is odd but not impossible, carrying on");
    }

    kprintln!("[rand] the generator answers, and twice running gave different bytes");
}
