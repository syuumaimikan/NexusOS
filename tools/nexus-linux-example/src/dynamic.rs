//! A dynamically linked Linux program, and the interpreter it names.
//!
//! The static fixtures next door answer "does the system-call boundary work".
//! This pair answers a different question, and it is the one standing between
//! this machine and any real Linux software: **is a program with a `PT_INTERP`
//! loaded the way Linux loads one.**
//!
//! Real Linux programs are almost all dynamically linked. Loading one is not
//! loading a file — it is loading *two* images into one address space, entering
//! the second, and telling it four numbers it cannot work out for itself. Every
//! one of those numbers is a thing that can be wrong in a way that produces no
//! message: a wrong `AT_BASE` is a dynamic linker relocating itself against the
//! wrong address, and what that looks like is a fault at a nonsense address
//! inside a library with no symbols loaded.
//!
//! # Why this is not `ld-linux-x86-64.so.2`
//!
//! Because glibc's loader is part of glibc, glibc needs a Linux toolchain to
//! build, and the machine this repository is developed on does not have one.
//!
//! So this is an interpreter in the *only* sense the kernel has to care about:
//! an `ET_DYN` image, named in another image's `PT_INTERP`, loaded at a bias it
//! does not choose, entered instead of the program, and left to reach the
//! program itself. It does not resolve symbols, because symbol resolution is
//! not something the kernel does or can get wrong. It does check every number
//! the kernel is required to have handed it, and it does use the memory calls a
//! real loader uses to map a library — `MAP_FIXED`, a file mapping at an
//! offset, and `mprotect` — because those were the three refusals that stood
//! between here and running one.
//!
//! What passing this proves is exactly: the kernel honours `PT_INTERP`, places
//! two images correctly, and hands over a correct auxiliary vector. What it
//! does not prove is that glibc's loader works, and that difference is written
//! down in `docs/steam-graphics.md` rather than left for a reader to assume.
//!
//! # The handshake
//!
//! A kernel that ignored `PT_INTERP` entirely and jumped straight to the
//! program would run the program, which would print its message and exit zero —
//! a pass, for the one thing being tested. So the interpreter puts a known
//! value in `r15` before it jumps, and the program refuses to run without it.
//! That is a convention between these two files and nothing a real loader does;
//! it is here because the alternative is a test that cannot fail.

use crate::Assembler;

/// Where the interpreter has to be, as the program's `PT_INTERP` names it.
///
/// Inside what a translated program sees as `/`, which on this machine is
/// `linux/` in the store. A real program names `/lib64/ld-linux-x86-64.so.2`;
/// this names its own file, because claiming to be glibc's loader while not
/// being it is the one thing worse than not having one.
pub const INTERPRETER_PATH: &[u8] = b"/lib/ld-nexus-x86-64.so.1\0";

/// What the interpreter leaves in `r15`. See the note above.
const HANDSHAKE: u64 = 0x4E58_5553_4C44_3031; // "NXUSLD01"

/// An address no image is loaded at, for the `MAP_FIXED` check.
///
/// Between where anonymous mappings are handed out and where the interpreter
/// is put, so a mapping that lands here collided with nothing — and if the
/// kernel ever did put something here, the check fails loudly rather than
/// quietly overwriting it.
const FIXED_AT: u64 = 0x0000_3000_0000_0000;

/// Bytes of ELF header, and of one program header.
const EHDR: usize = 64;
const PHDR: usize = 56;

/// The instructions that find the auxiliary vector, leaving it in `r13`.
///
/// At the entry point of any System V program `rsp` points at `argc`, then the
/// argument pointers, a null, the environment pointers, another null, and then
/// the pairs. Nothing says how many of the first two there are, so both are
/// walked to their null rather than counted.
fn find_auxiliary_vector(a: &mut Assembler) {
    a.raw(&[0x48, 0x89, 0xE6]); // mov rsi, rsp
    a.raw(&[0x48, 0x83, 0xC6, 0x08]); // add rsi, 8   -- past argc, at argv[0]
    for _ in 0..2 {
        // Once for the arguments and once for the environment.
        let top = a.at();
        a.raw(&[0x48, 0x8B, 0x06]); // mov rax, [rsi]
        a.raw(&[0x48, 0x83, 0xC6, 0x08]); // add rsi, 8
        a.raw(&[0x48, 0x85, 0xC0]); // test rax, rax
        let back = -((a.at() + 2 - top) as i64);
        a.raw(&[
            0x75,
            i8::try_from(back).expect("the list walk is short") as u8,
        ]); // jnz
    }
    a.raw(&[0x49, 0x89, 0xF5]); // mov r13, rsi
}

/// One auxiliary vector entry, by type, left in `rax`.
///
/// Stops the program with `missing` if the vector ends without it, which is the
/// failure that matters: an entry the kernel did not fill in is not an entry
/// with a wrong value, it is one a real program dereferences as null.
fn find_entry(a: &mut Assembler, kind: u32, missing: u32) {
    a.raw(&[0x4C, 0x89, 0xEE]); // mov rsi, r13
    let top = a.at();
    a.raw(&[0x48, 0x8B, 0x06]); // mov rax, [rsi]
    a.raw(&[0x48, 0x85, 0xC0]); // test rax, rax
    a.unless(0x75, missing); // AT_NULL: the vector ended
    a.raw(&[0x48, 0x3D]).raw(&kind.to_le_bytes()); // cmp rax, kind
    a.raw(&[0x74, 0x06]); // je over the step below
    a.raw(&[0x48, 0x83, 0xC6, 0x10]); // add rsi, 16
    let back = -((a.at() + 2 - top) as i64);
    a.raw(&[
        0xEB,
        i8::try_from(back).expect("the auxv walk is short") as u8,
    ]); // jmp
    a.raw(&[0x48, 0x8B, 0x46, 0x08]); // mov rax, [rsi + 8]
}

/// The interpreter's code.
///
/// `code_offset` is where these bytes start in the file, which for an image
/// whose single segment is mapped from offset zero is also their link-time
/// address — and is what the first instruction subtracts to learn the bias.
///
/// | 40 | the auxiliary vector ended before an entry it must have |
/// | 41 | `AT_BASE` is not where this image actually is |
/// | 42 | `AT_ENTRY` is zero, or is this image |
/// | 43 | `AT_PHDR` is zero |
/// | 44 | `mmap` of an anonymous page |
/// | 45 | what was written into it did not read back |
/// | 46 | `munmap` |
/// | 47 | `MAP_FIXED` did not return the address it was given |
/// | 48 | the fixed page did not hold what was written into it |
/// | 49 | `mprotect` to read-only |
/// | 50 | the page stopped holding its contents when it was protected |
/// | 51 | `openat` of the program's own file, named by `argv[0]` |
/// | 52 | `mmap` of that file |
/// | 53 | the file mapping does not begin with `\x7fELF` |
fn interpreter_code(code_offset: usize) -> Vec<u8> {
    let mut a = Assembler::default();

    // Where am I? A dynamic linker's first problem, and it has exactly one
    // answer: ask the processor. `lea rbp, [rip+0]` is the address of the
    // instruction *after* it, and subtracting that instruction's link-time
    // address leaves the amount the whole image moved by. Every real loader
    // starts with some form of this, because until it knows this number it
    // cannot read its own data.
    a.raw(&[0x48, 0x8D, 0x2D, 0x00, 0x00, 0x00, 0x00]); // lea rbp, [rip+0]
    a.raw(&[0x48, 0x81, 0xED])
        .raw(&((code_offset + 7) as u32).to_le_bytes()); // sub rbp, imm32

    find_auxiliary_vector(&mut a);

    // `AT_BASE` is where the kernel says this image is. It has to agree with
    // where the processor says this image is, and nothing else can check that:
    // the kernel chose the address and the loader has to trust it.
    find_entry(&mut a, 7, 40);
    a.raw(&[0x48, 0x39, 0xE8]); // cmp rax, rbp
    a.unless(0x74, 41);

    // `AT_ENTRY` is the program's own entry point -- the thing this interpreter
    // exists to reach. Zero means the kernel did not record it, and equal to
    // this image's base means it recorded the wrong one of the two images.
    find_entry(&mut a, 9, 40);
    a.raw(&[0x48, 0x85, 0xC0]); // test rax, rax
    a.unless(0x75, 42);
    a.raw(&[0x48, 0x39, 0xE8]); // cmp rax, rbp
    a.unless(0x75, 42); // must *not* be equal
    a.raw(&[0x49, 0x89, 0xC6]); // mov r14, rax  -- keep it to jump to

    // `AT_PHDR` is the *program's* headers, which is where its `PT_DYNAMIC` is
    // and therefore the only way a real loader finds anything to relocate.
    find_entry(&mut a, 3, 40);
    a.raw(&[0x48, 0x85, 0xC0]); // test rax, rax
    a.unless(0x75, 43);

    // An anonymous page, written and read back. The oldest check there is, and
    // the one that says the memory handed over is really there.
    a.mov_edi(0)
        .mov_esi(4096)
        .mov_edx(3) // PROT_READ | PROT_WRITE
        .mov_r10d(0x22) // MAP_PRIVATE | MAP_ANONYMOUS
        .mov_r8d(u32::MAX) // no descriptor
        .mov_r9d(0)
        .mov_eax(9)
        .syscall();
    a.expect_not_negative(44);
    a.rbx_from_rax();
    a.raw(&[0xC6, 0x03, 0x5A]); // mov byte [rbx], 0x5A
    a.raw(&[0x0F, 0xB6, 0x03]); // movzx eax, byte [rbx]
    a.expect_exactly(0x5A, 45);

    a.raw(&[0x48, 0x89, 0xDF]); // mov rdi, rbx
    a.mov_esi(4096).mov_eax(11).syscall(); // munmap
    a.expect_exactly(0, 46);

    // And now the one a loader actually depends on: a mapping at an address it
    // chose. A library's segments are mapped at the addresses the file names,
    // over a span reserved for them, and an address other than the one asked
    // for is not a smaller version of the answer.
    a.raw(&[0x48, 0xBB]).raw(&FIXED_AT.to_le_bytes()); // movabs rbx, FIXED_AT
    a.raw(&[0x48, 0x89, 0xDF]); // mov rdi, rbx
    a.mov_esi(4096)
        .mov_edx(3)
        .mov_r10d(0x32) // MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED
        .mov_r8d(u32::MAX)
        .mov_r9d(0)
        .mov_eax(9)
        .syscall();
    a.raw(&[0x48, 0x39, 0xD8]); // cmp rax, rbx
    a.unless(0x74, 47);
    a.raw(&[0xC6, 0x03, 0x77]); // mov byte [rbx], 0x77
    a.raw(&[0x0F, 0xB6, 0x03]); // movzx eax, byte [rbx]
    a.expect_exactly(0x77, 48);

    // Taking the write permission away, which is the other half of what a
    // loader does to every library it relocates. The page has to survive it:
    // an `mprotect` implemented as an unmap and a map would lose the contents,
    // and one implemented as nothing at all would pass this and fail the day
    // something wrote through a page it thought was read-only.
    a.raw(&[0x48, 0x89, 0xDF]); // mov rdi, rbx
    a.mov_esi(4096).mov_edx(1).mov_eax(10).syscall(); // mprotect PROT_READ
    a.expect_exactly(0, 49);
    a.raw(&[0x0F, 0xB6, 0x03]); // movzx eax, byte [rbx]
    a.expect_exactly(0x77, 50);

    // A file mapping, of the program's own file -- whose path is `argv[0]`,
    // which is at `[rsp + 8]` and has not moved, because nothing here pushes.
    a.raw(&[0x48, 0x8B, 0x7C, 0x24, 0x08]); // mov rdi, [rsp + 8]
    a.raw(&[0x48, 0x89, 0xFE]); // mov rsi, rdi
    a.mov_edi((-100i32) as u32) // AT_FDCWD
        .mov_edx(0) // O_RDONLY
        .mov_r10d(0)
        .mov_eax(257) // openat
        .syscall();
    a.expect_at_least(3, 51);
    a.r12_from_rax();

    a.mov_edi(0)
        .mov_esi(4096)
        .mov_edx(1) // PROT_READ
        .mov_r10d(0x02) // MAP_PRIVATE, and no MAP_ANONYMOUS
        .raw(&[0x4D, 0x89, 0xE0]) // mov r8, r12   -- the descriptor
        .mov_r9d(0) // at offset zero
        .mov_eax(9)
        .syscall();
    a.expect_not_negative(52);
    a.rbx_from_rax();

    // The first four bytes of the file this program was loaded from, read back
    // through a mapping of it. A file mapping that quietly handed over zeroed
    // anonymous pages would pass every check above and fail this one.
    a.raw(&[0x8B, 0x03]); // mov eax, [rbx]
    a.raw(&[0x3D]).raw(&0x464C_457Fu32.to_le_bytes()); // cmp eax, "\x7fELF"
    a.unless(0x74, 53);

    // Everything the kernel had to get right, it got right. Hand over.
    a.raw(&[0x49, 0xBF]).raw(&HANDSHAKE.to_le_bytes()); // movabs r15, HANDSHAKE
    a.raw(&[0x41, 0xFF, 0xE6]); // jmp r14

    a.code
}

/// The program's code.
///
/// Entered by the interpreter, not by the kernel, with `rsp` exactly as the
/// kernel left it -- which is what makes it possible for this to read its own
/// auxiliary vector after the jump.
///
/// | 60 | the interpreter did not run: nothing set the handshake |
/// | 61 | the auxiliary vector ended before `AT_ENTRY` |
/// | 62 | `AT_ENTRY` is not where this code actually is |
fn program_code(code_offset: usize, message: u64, length: u32) -> Vec<u8> {
    let mut a = Assembler::default();

    // Did the interpreter run at all? A kernel that ignored `PT_INTERP` and
    // jumped straight here would otherwise print the message and exit zero,
    // which is the failure this whole fixture exists to notice.
    a.raw(&[0x49, 0xBE]).raw(&HANDSHAKE.to_le_bytes()); // movabs r14, HANDSHAKE
    a.raw(&[0x4D, 0x39, 0xF7]); // cmp r15, r14
    a.unless(0x74, 60);

    // Where this code actually is, worked out the way the interpreter worked
    // out where it was: the address after a `lea` less that instruction's
    // link-time address. `rbx` holds the bias from here on, and every address
    // this program uses is an offset from it -- which is the whole of what
    // "position independent" means in practice.
    let lea_at = a.at();
    a.raw(&[0x48, 0x8D, 0x1D, 0x00, 0x00, 0x00, 0x00]); // lea rbx, [rip+0]
    a.raw(&[0x48, 0x81, 0xEB])
        .raw(&((code_offset + lea_at + 7) as u32).to_le_bytes()); // sub rbx, imm32

    // This image's entry point is the bias plus where its code starts, and the
    // kernel put that same number in `AT_ENTRY` -- from the header, by a
    // different route. They have to agree.
    a.raw(&[0x48, 0x8D, 0xAB])
        .raw(&(code_offset as u32).to_le_bytes()); // lea rbp, [rbx + code_offset]

    find_auxiliary_vector(&mut a);
    find_entry(&mut a, 9, 61);
    a.raw(&[0x48, 0x39, 0xE8]); // cmp rax, rbp
    a.unless(0x74, 62);

    // The message, through the same `write` the static fixtures use -- except
    // that its address is not a constant here, because this image does not know
    // where it is until it has run.
    a.raw(&[0x48, 0x8D, 0xB3])
        .raw(&(message as u32).to_le_bytes()); // lea rsi, [rbx + message]
    a.mov_edi(1).mov_edx(length).mov_eax(1).syscall();

    a.mov_eax(231).mov_edi(0).syscall();
    a.code
}

/// Which of the pair to emit.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Piece {
    /// The `ET_DYN` executable with a `PT_INTERP`.
    Program,
    /// The `ET_DYN` image it names.
    Interpreter,
}

/// Assemble one of them.
///
/// Both are `ET_DYN`: neither names an address, and each is correct wherever it
/// is put. That is the whole difference from the static fixtures, and it is why
/// every address either of them uses is worked out at run time from `rip`.
#[must_use]
pub fn build(piece: Piece, message: &[u8]) -> Vec<u8> {
    // One program header for the interpreter, two for the program -- the second
    // being the `PT_INTERP` that names the first.
    let headers = match piece {
        Piece::Program => 2,
        Piece::Interpreter => 1,
    };
    let code_offset = EHDR + headers * PHDR;

    // Assembled once to measure it, then again knowing where the message went.
    // Nothing here changes size with the value of an address -- every immediate
    // is a fixed width -- and the assertion below is what says so.
    let code = match piece {
        Piece::Interpreter => interpreter_code(code_offset),
        Piece::Program => program_code(code_offset, 0, message.len() as u32),
    };
    let message_offset = code_offset + code.len();
    let code = match piece {
        Piece::Interpreter => code,
        Piece::Program => program_code(code_offset, message_offset as u64, message.len() as u32),
    };
    assert_eq!(
        code.len(),
        message_offset - code_offset,
        "the second assembly came out a different length from the first"
    );

    let interpreter_offset = message_offset + message.len();
    let total = match piece {
        Piece::Program => interpreter_offset + INTERPRETER_PATH.len(),
        Piece::Interpreter => interpreter_offset,
    };
    let entry = code_offset as u64;

    let mut image: Vec<u8> = Vec::with_capacity(total);

    // ---- the ELF header -------------------------------------------------
    image.extend_from_slice(&[0x7F, b'E', b'L', b'F']);
    image.push(2); // EI_CLASS   = ELFCLASS64
    image.push(1); // EI_DATA    = ELFDATA2LSB
    image.push(1); // EI_VERSION = EV_CURRENT
    image.push(0); // EI_OSABI   = ELFOSABI_SYSV
    image.push(0); // EI_ABIVERSION
    image.extend_from_slice(&[0; 7]);
    // ET_DYN, which is what a shared object is and what a position-independent
    // executable is. The header does not distinguish the two; whether anything
    // else loads it is what makes it one or the other.
    image.extend_from_slice(&3u16.to_le_bytes()); // e_type
    image.extend_from_slice(&0x3Eu16.to_le_bytes()); // e_machine = EM_X86_64
    image.extend_from_slice(&1u32.to_le_bytes()); // e_version
    image.extend_from_slice(&entry.to_le_bytes()); // e_entry, relative to nothing
    image.extend_from_slice(&(EHDR as u64).to_le_bytes()); // e_phoff
    image.extend_from_slice(&0u64.to_le_bytes()); // e_shoff
    image.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    image.extend_from_slice(&(EHDR as u16).to_le_bytes()); // e_ehsize
    image.extend_from_slice(&(PHDR as u16).to_le_bytes()); // e_phentsize
    image.extend_from_slice(&(headers as u16).to_le_bytes()); // e_phnum
    image.extend_from_slice(&0u16.to_le_bytes()); // e_shentsize
    image.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
    image.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx
    assert_eq!(image.len(), EHDR);

    // ---- PT_LOAD --------------------------------------------------------
    //
    // From offset zero, at virtual address zero: the whole file, including its
    // own headers, which is what `AT_PHDR` has to be able to point into.
    image.extend_from_slice(&1u32.to_le_bytes()); // p_type  = PT_LOAD
    image.extend_from_slice(&5u32.to_le_bytes()); // p_flags = R | X
    image.extend_from_slice(&0u64.to_le_bytes()); // p_offset
    image.extend_from_slice(&0u64.to_le_bytes()); // p_vaddr
    image.extend_from_slice(&0u64.to_le_bytes()); // p_paddr
    image.extend_from_slice(&(total as u64).to_le_bytes()); // p_filesz
    image.extend_from_slice(&(total as u64).to_le_bytes()); // p_memsz
    image.extend_from_slice(&0x1000u64.to_le_bytes()); // p_align

    // ---- PT_INTERP ------------------------------------------------------
    //
    // The one segment that makes a program dynamically linked. A kernel that
    // skipped it would load this image, jump to its entry point, and run a
    // program that has not been relocated and whose libraries are not there.
    if piece == Piece::Program {
        image.extend_from_slice(&3u32.to_le_bytes()); // p_type  = PT_INTERP
        image.extend_from_slice(&4u32.to_le_bytes()); // p_flags = R
        image.extend_from_slice(&(interpreter_offset as u64).to_le_bytes()); // p_offset
        image.extend_from_slice(&(interpreter_offset as u64).to_le_bytes()); // p_vaddr
        image.extend_from_slice(&(interpreter_offset as u64).to_le_bytes()); // p_paddr
        image.extend_from_slice(&(INTERPRETER_PATH.len() as u64).to_le_bytes()); // p_filesz
        image.extend_from_slice(&(INTERPRETER_PATH.len() as u64).to_le_bytes()); // p_memsz
        image.extend_from_slice(&1u64.to_le_bytes()); // p_align
    }
    assert_eq!(image.len(), code_offset);

    image.extend_from_slice(&code);
    image.extend_from_slice(message);
    if piece == Piece::Program {
        image.extend_from_slice(INTERPRETER_PATH);
    }
    assert_eq!(image.len(), total);
    image
}
