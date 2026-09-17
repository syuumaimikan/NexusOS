//! A Linux program that draws a window.
//!
//! It opens `/dev/nexus/display`, asks how big its window is, maps the buffer
//! `MAP_SHARED`, fills it, and tells the compositor which rectangle changed --
//! which is, in outline, what every X11 or Wayland client does, with the
//! connection and the object protocol taken away.
//!
//! What makes this a test rather than a demonstration is the last part: the
//! colours it writes are two specific numbers, in two bands whose boundary is a
//! third of the way down the window. A screenshot taken from outside the
//! machine can be checked for both, which is a claim no amount of "the call
//! returned zero" can make -- a display device that accepted every request and
//! showed nothing would pass every check inside the program.
//!
//! | 80 | `mmap` for a page to work in |
//! | 81 | `openat` of the display device |
//! | 82 | the `INFO` request |
//! | 83 | `mmap` of the window buffer, shared |
//! | 84 | the `PRESENT` request |
//! | 85 | the `EVENT` request |
//!
//! Eighty-one is the interesting one. A translated program only has a window if
//! something gave it one: the device is opened through the channel to the
//! compositor that the process was started with, so a program started by `init`
//! rather than by the compositor is told it has no window instead of being
//! handed the screen.

use crate::Assembler;

/// The device, as a Linux program names it.
pub const DEVICE_PATH: &[u8] = b"/dev/nexus/display\0";

/// The requests, which are the kernel's own numbers -- see
/// `compat/linux_display.rs`.
const INFO: u32 = 0x4E58_0001;
const PRESENT: u32 = 0x4E58_0002;
const EVENT: u32 = 0x4E58_0003;

/// The two colours, packed as the surface holds them: `0x00RRGGBB`.
///
/// Chosen to be far apart in every channel, so that a screenshot check is not
/// deciding between two shades of the same thing -- and so that a buffer that
/// arrived half-written is obvious rather than plausible.
const BACKGROUND: u32 = 0x001E_5AA8;
const BAND: u32 = 0x00E8_B23C;

/// Offsets in the scratch page.
///
/// The information the device writes first, then the rectangle handed back to
/// it, then somewhere for an event, then the frame counter and the word the
/// sleep waits on. Spread out rather than packed, so one overrunning shows up
/// as wrong data rather than as a quiet overlap.
const RECT: u8 = 0;
const INFO_AT: u8 = 0;
const EVENT_AT: u8 = 64;
const FRAMES_AT: u8 = 80;
const SLEEP_AT: u8 = 96;
const TIMEOUT_AT: u8 = 112;

/// How many frames to draw before stopping.
///
/// Large enough that the window is still there whenever a screenshot is taken,
/// and finite because a program that could never stop is one the machine can
/// only be rid of by being turned off.
const FRAMES: u32 = 100_000;

/// How long to wait between frames, in seconds and nanoseconds.
///
/// A tenth of a second, which is a calm refresh rate for a window that is not
/// animating, and is slow enough that the loop is not a program spinning.
///
/// The sleep is a `FUTEX_WAIT` with a timeout on a word that is never woken,
/// which is a real sleep and not a busy loop -- and is the only one this layer
/// has, because `nanosleep` is not translated. A program doing this on Linux
/// would be doing something slightly odd; a program doing it here is using the
/// one call that does the job.
const SLEEP_NANOSECONDS: u64 = 100_000_000;

/// `FUTEX_WAIT_PRIVATE`.
const FUTEX_WAIT: u32 = 128;

/// The program.
///
/// `path` is where the device's name ended up in the image.
pub fn machine_code(message: u64, length: u32, path: u64) -> Vec<u8> {
    let mut a = Assembler::default();

    // A page to work in: the device writes its answers here and reads the
    // rectangle back out of it.
    a.mov_edi(0)
        .mov_esi(4096)
        .mov_edx(3) // PROT_READ | PROT_WRITE
        .mov_r10d(0x22) // MAP_PRIVATE | MAP_ANONYMOUS
        .mov_r8d(u32::MAX)
        .mov_r9d(0)
        .mov_eax(9)
        .syscall();
    a.expect_not_negative(80);
    a.rbx_from_rax();

    // The window. A program that was not started by the compositor has no
    // channel to it and is refused here, which is the truth rather than a
    // failure: it has no window because nobody gave it one.
    a.mov_edi((-100i32) as u32) // AT_FDCWD
        .mov_rsi_imm(path)
        .mov_edx(2) // O_RDWR
        .mov_r10d(0)
        .mov_eax(257)
        .syscall();
    a.expect_at_least(3, 81);
    a.r12_from_rax();

    // How big it is. Asked rather than assumed: the compositor decides, and a
    // program that guessed would draw a diagonal smear the first time it was
    // wrong.
    ioctl(&mut a, INFO, INFO_AT);
    a.expect_exactly(0, 82);
    a.raw(&[0x44, 0x8B, 0x33]); // mov r14d, [rbx]      -- width
    a.raw(&[0x44, 0x8B, 0x7B, 0x04]); // mov r15d, [rbx+4]    -- height
    a.raw(&[0x44, 0x89, 0xF0]); // mov eax, r14d
    a.raw(&[0x41, 0x0F, 0xAF, 0xC7]); // imul eax, r15d
    a.raw(&[0x89, 0xC5]); // mov ebp, eax         -- pixels

    // And the buffer itself. `MAP_SHARED`, which is the point: the compositor
    // reads the same frames this writes. Every other mapping in this layer is
    // private, and a shared file mapping is refused -- this is the one place
    // the promise can actually be kept.
    a.mov_edi(0);
    a.raw(&[0x89, 0xEE]); // mov esi, ebp
    a.raw(&[0xC1, 0xE6, 0x02]); // shl esi, 2           -- four bytes a pixel
    a.mov_edx(3) // PROT_READ | PROT_WRITE
        .mov_r10d(0x01); // MAP_SHARED
    a.raw(&[0x4D, 0x89, 0xE0]); // mov r8, r12          -- the window
    a.mov_r9d(0).mov_eax(9).syscall();
    a.expect_not_negative(83);
    a.raw(&[0x49, 0x89, 0xC5]); // mov r13, rax         -- the pixels

    // How many frames are left, kept in the scratch page because every register
    // that is not already spoken for is destroyed by a system call.
    a.raw(&[0xC7, 0x43, FRAMES_AT]).raw(&FRAMES.to_le_bytes());
    // The word the sleep waits on, which nothing ever wakes, and the timeout.
    a.raw(&[0xC7, 0x43, SLEEP_AT]).raw(&0u32.to_le_bytes());
    a.raw(&[0x48, 0x31, 0xC0]); // xor rax, rax
    a.raw(&[0x48, 0x89, 0x43, TIMEOUT_AT]); // seconds: none
    a.raw(&[0x48, 0xB8]).raw(&SLEEP_NANOSECONDS.to_le_bytes());
    a.raw(&[0x48, 0x89, 0x43, TIMEOUT_AT + 8]);

    let frame = a.at();

    // The background, every pixel of it. `rep stosd` writes one dword at a time
    // from `eax` to `[rdi]`, `ecx` times -- which is the whole of "fill a
    // buffer with a colour" and is what a compiler emits for it too.
    a.raw(&[0xFC]); // cld  -- forwards, whatever the kernel left set
    a.raw(&[0x4C, 0x89, 0xEF]); // mov rdi, r13
    a.mov_eax(BACKGROUND);
    a.raw(&[0x89, 0xE9]); // mov ecx, ebp
    a.raw(&[0xF3, 0xAB]); // rep stosd

    // A band across the middle third. Its top edge is at a third of the height,
    // which makes it a landmark a screenshot can be checked against: the colour
    // alone would be satisfied by a window filled with it.
    a.raw(&[0x44, 0x89, 0xF8]); // mov eax, r15d        -- height
    a.mov_ecx(3);
    a.raw(&[0x31, 0xD2]); // xor edx, edx
    a.raw(&[0xF7, 0xF1]); // div ecx              -- eax = height / 3
    a.raw(&[0x89, 0xC1]); // mov ecx, eax
    a.raw(&[0x41, 0x0F, 0xAF, 0xCE]); // imul ecx, r14d       -- pixels in a third
    a.raw(&[0x4C, 0x89, 0xEF]); // mov rdi, r13
    a.raw(&[0x89, 0xC8]); // mov eax, ecx
    a.raw(&[0xC1, 0xE0, 0x02]); // shl eax, 2
    a.raw(&[0x48, 0x01, 0xC7]); // add rdi, rax         -- past the first third
    a.mov_eax(BAND);
    a.raw(&[0xF3, 0xAB]); // rep stosd

    // The whole window changed, which is what a program that redraws all of it
    // has to say. A smaller rectangle would be a lie the compositor would
    // believe.
    a.raw(&[0xC7, 0x43, RECT]).raw(&0u32.to_le_bytes()); // x
    a.raw(&[0xC7, 0x43, RECT + 4]).raw(&0u32.to_le_bytes()); // y
    a.raw(&[0x44, 0x89, 0x73, RECT + 8]); // mov [rbx+8], r14d
    a.raw(&[0x44, 0x89, 0x7B, RECT + 12]); // mov [rbx+12], r15d
    ioctl(&mut a, PRESENT, RECT);
    a.expect_exactly(0, 84);

    // Anything that happened while that frame was in flight. Not acted on --
    // this program has nothing to do with a keystroke -- but asked for, because
    // a program that never drained its events would be one whose queue this
    // layer has to bound, and because the call working is worth knowing.
    ioctl(&mut a, EVENT, EVENT_AT);
    a.expect_exactly(0, 85);

    // A real sleep between frames. See `SLEEP_NANOSECONDS`.
    a.raw(&[0x48, 0x8D, 0x7B, SLEEP_AT]); // lea rdi, [rbx+SLEEP_AT]
    a.mov_esi(FUTEX_WAIT);
    a.raw(&[0x48, 0x31, 0xD2]); // xor rdx, rdx     -- expecting zero
    a.raw(&[0x4C, 0x8D, 0x53, TIMEOUT_AT]); // lea r10, [rbx+TIMEOUT_AT]
    a.mov_eax(202).syscall();
    // Not checked: a timed wait that times out answers ETIMEDOUT, which is the
    // expected outcome here and is an error number. Checking it would be
    // checking that the sleep did not get woken.

    a.raw(&[0xFF, 0x4B, FRAMES_AT]); // dec dword [rbx+FRAMES_AT]
    let back = (frame as i64) - (a.at() as i64 + 6);
    a.raw(&[0x0F, 0x85]).raw(
        &i32::try_from(back)
            .expect("the frame loop is shorter than two gigabytes")
            .to_le_bytes(),
    ); // jnz frame

    // Every frame drawn. Say so and stop.
    a.mov_edi(1)
        .mov_rsi_imm(message)
        .mov_edx(length)
        .mov_eax(1)
        .syscall();
    a.mov_eax(231).mov_edi(0).syscall();

    a.code
}

/// `ioctl(fd, request, rbx + offset)`.
fn ioctl(a: &mut Assembler, request: u32, offset: u8) {
    a.raw(&[0x4C, 0x89, 0xE7]); // mov rdi, r12
    a.mov_esi(request);
    a.raw(&[0x48, 0x8D, 0x53, offset]); // lea rdx, [rbx + offset]
    a.mov_eax(16).syscall();
}
