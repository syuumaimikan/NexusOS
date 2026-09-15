//! Emits a static Linux x86-64 executable.
//!
//! Not a NexusOS program. What comes out of here is an ELF that a Linux kernel
//! would run without noticing anything unusual about it: `ET_EXEC`, one
//! `PT_LOAD`, `EM_X86_64`, no interpreter, no dynamic section, and machine code
//! that makes its requests with the `syscall` instruction and Linux's own call
//! numbers — 1 for `write`, 231 for `exit_group`.
//!
//! # Why it is generated rather than committed
//!
//! Because a committed binary is a binary nobody can read. Every byte here is
//! written out with the field it belongs to named beside it, so the thing being
//! claimed — that this is a real Linux executable and not something shaped to
//! suit NexusOS — can be checked by reading rather than taken on trust.
//!
//! It is also the only way to have one at all: building a Linux binary needs a
//! Linux toolchain, and the machine this repository is developed on does not
//! have one. A file this small does not need one.
//!
//! # Why not a compiler
//!
//! Twenty-nine bytes of machine code, hand-assembled and commented. A compiler
//! would produce something larger with a runtime attached, and the point of the
//! exercise is the *interface* — the instruction, the call numbers, the
//! register convention — not the language it was written in.

use std::path::PathBuf;
use std::process::ExitCode;

/// Where the image is loaded. The traditional address for a static x86-64
/// executable, and one the loader on the other side has no special knowledge of.
const BASE: u64 = 0x40_0000;

/// Bytes of ELF header, and of one program header.
const EHDR: usize = 64;
const PHDR: usize = 56;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.len() < 2 || arguments.len() > 3 {
        eprintln!("usage: nexus-linux-example <output.elf> <message> [rich|files]");
        return ExitCode::FAILURE;
    }
    let output = PathBuf::from(&arguments[0]);
    let mut message = arguments[1].clone().into_bytes();
    message.push(b'\n');
    // Three programs, from one generator. The plain one asks for `write` and
    // `exit_group` and nothing else; the rich one asks for the memory and the
    // scattered write a real libc asks for; the third opens a file, reads it
    // back and checks what it got. See `machine_code`, `rich_machine_code` and
    // `files_machine_code`.
    let flavour = match arguments.get(2).map(String::as_str) {
        None => Flavour::Plain,
        Some("rich") => Flavour::Rich,
        Some("files") => Flavour::Files,
        Some(other) => {
            eprintln!("nexus-linux-example: no such kind of program: {other}");
            return ExitCode::FAILURE;
        }
    };

    let image = build(&message, flavour);

    if let Err(error) = std::fs::write(&output, &image) {
        eprintln!(
            "nexus-linux-example: cannot write {}: {error}",
            output.display()
        );
        return ExitCode::FAILURE;
    }
    println!(
        "wrote {} bytes of static Linux x86-64 ELF to {}",
        image.len(),
        output.display()
    );
    ExitCode::SUCCESS
}


// ---------------------------------------------------------------------------
// Just enough assembler to write the third program by hand without going mad
// ---------------------------------------------------------------------------

/// A growing sequence of x86-64 instructions.
///
/// The first two programs here are thirty and sixty bytes and are written as
/// literal arrays with the mnemonic in a comment beside each one. The third is
/// three hundred, and at that length a miscounted jump distance is a program
/// that fails in a way nobody can read. So the encodings move into named
/// methods, each with the byte sequence it emits written down, and the program
/// below reads as the list of steps it is.
#[derive(Default)]
struct Assembler {
    code: Vec<u8>,
}

impl Assembler {
    fn at(&self) -> usize {
        self.code.len()
    }
    fn raw(&mut self, bytes: &[u8]) -> &mut Self {
        self.code.extend_from_slice(bytes);
        self
    }
    /// `mov eax, imm32` and friends: the register is in the opcode byte, and
    /// writing to a 32-bit register zeroes the top half, which is why these
    /// serve for small 64-bit values too.
    fn mov_eax(&mut self, value: u32) -> &mut Self {
        self.raw(&[0xB8]).raw(&value.to_le_bytes())
    }
    fn mov_edi(&mut self, value: u32) -> &mut Self {
        self.raw(&[0xBF]).raw(&value.to_le_bytes())
    }
    fn mov_esi(&mut self, value: u32) -> &mut Self {
        self.raw(&[0xBE]).raw(&value.to_le_bytes())
    }
    fn mov_edx(&mut self, value: u32) -> &mut Self {
        self.raw(&[0xBA]).raw(&value.to_le_bytes())
    }
    fn mov_r10d(&mut self, value: u32) -> &mut Self {
        self.raw(&[0x41, 0xBA]).raw(&value.to_le_bytes())
    }
    fn mov_r8d(&mut self, value: u32) -> &mut Self {
        self.raw(&[0x41, 0xB8]).raw(&value.to_le_bytes())
    }
    fn mov_r9d(&mut self, value: u32) -> &mut Self {
        self.raw(&[0x41, 0xB9]).raw(&value.to_le_bytes())
    }
    /// `mov rsi, imm64`, for an address that does not fit in thirty-two bits --
    /// and for one that does, because an address is worth writing whole.
    fn mov_rsi_imm(&mut self, value: u64) -> &mut Self {
        self.raw(&[0x48, 0xBE]).raw(&value.to_le_bytes())
    }
    fn mov_rcx_imm(&mut self, value: u64) -> &mut Self {
        self.raw(&[0x48, 0xB9]).raw(&value.to_le_bytes())
    }
    /// `mov rbx, rax` -- the scratch page, kept where a system call cannot
    /// clobber it. Linux's calling convention destroys only `rcx` and `r11`.
    fn rbx_from_rax(&mut self) -> &mut Self {
        self.raw(&[0x48, 0x89, 0xC3])
    }
    /// `mov r12, rax` -- the descriptor, kept for the same reason.
    fn r12_from_rax(&mut self) -> &mut Self {
        self.raw(&[0x49, 0x89, 0xC4])
    }
    /// `mov rdi, r12`
    fn rdi_from_r12(&mut self) -> &mut Self {
        self.raw(&[0x4C, 0x89, 0xE7])
    }
    /// `lea rsi, [rbx + offset]` -- somewhere inside the scratch page.
    fn rsi_in_scratch(&mut self, offset: u32) -> &mut Self {
        self.raw(&[0x48, 0x8D, 0xB3]).raw(&offset.to_le_bytes())
    }
    /// `mov rax, [rbx + offset]`
    fn rax_from_scratch(&mut self, offset: u32) -> &mut Self {
        self.raw(&[0x48, 0x8B, 0x83]).raw(&offset.to_le_bytes())
    }
    fn syscall(&mut self) -> &mut Self {
        self.raw(&[0x0F, 0x05])
    }

    /// `exit_group(code)`, as its own twelve bytes.
    fn stop(code: u32) -> Vec<u8> {
        let mut out = Assembler::default();
        out.mov_eax(231).mov_edi(code).syscall();
        out.code
    }

    /// Stop with `code` unless the condition holds, and carry on if it does.
    ///
    /// The jump goes *over* the stopping block, so its distance is that block's
    /// length and nothing has to be counted by hand. Every check in the program
    /// below is one of these, and each `code` is a different number -- so a
    /// failure says which step failed, in the exit status, without a word of
    /// explanation having to survive the journey.
    fn unless(&mut self, jump: u8, code: u32) -> &mut Self {
        let block = Self::stop(code);
        let over = u8::try_from(block.len()).expect("a stopping block is twelve bytes");
        self.raw(&[jump, over]).raw(&block)
    }
    /// Carry on if `rax` is not negative.
    fn expect_not_negative(&mut self, code: u32) -> &mut Self {
        self.raw(&[0x48, 0x85, 0xC0]).unless(0x79, code) // test rax, rax; jns
    }
    /// Carry on if `rax` is exactly `value`.
    fn expect_exactly(&mut self, value: u32, code: u32) -> &mut Self {
        self.raw(&[0x48, 0x3D]).raw(&value.to_le_bytes()).unless(0x74, code) // cmp; je
    }
    /// Carry on if `rax` is `value` or more.
    fn expect_at_least(&mut self, value: u32, code: u32) -> &mut Self {
        self.raw(&[0x48, 0x3D]).raw(&value.to_le_bytes()).unless(0x7D, code) // cmp; jge
    }
}

/// A program that writes a file, reads it back, and checks what it got.
///
/// The first two programs here prove the system-call boundary exists and is
/// wide enough for a libc's memory and output. This one is about the
/// filesystem, and it is written so that it *fails* rather than prints: every
/// step checks what came back, and a step that is wrong stops the program with
/// its own exit status. A translation layer that answered plausibly to
/// everything and did nothing would exit non-zero here, and the number would
/// say at which call.
///
/// | 10 | `mmap` for a page to work in |
/// | 11, 12 | `openat` for writing, and the descriptor it returned |
/// | 13, 14 | `write`, and `close` |
/// | 15 | `openat` again, for reading |
/// | 16, 17 | `fstat`, and the size it reported |
/// | 18, 19 | `read`, and whether the bytes are the ones written |
/// | 20 | `lseek` to the end |
/// | 21 | `getcwd` |
/// | 22 | `getrandom` |
/// | 23, 24, 25 | opening the root directory and `getdents64` |
/// | 26 | `AT_RANDOM` in the auxiliary vector |
///
/// Twelve is the interesting one. A Linux descriptor is a Nexus handle here,
/// and handle numbers start at one -- which is standard output. If the process
/// did not start its handles at three, the first `openat` would return 1 and
/// every check after it would still pass, because writing to the file and
/// writing to the log would be the same call. So the descriptor is checked
/// against three before anything is written through it.
fn files_machine_code(
    message: u64,
    length: u32,
    first_eight: u64,
    path: u64,
    root: u64,
) -> Vec<u8> {
    /// What the program asks `openat` for. `AT_FDCWD` is -100.
    const AT_FDCWD: u32 = (-100i32) as u32;
    const O_WRONLY_CREAT_TRUNC: u32 = 0o1 | 0o100 | 0o1000;
    const O_RDONLY: u32 = 0;
    const O_DIRECTORY: u32 = 0o200000;
    /// Offsets inside the scratch page: the `struct stat`, then the read
    /// buffer, then room for a path, then for random bytes, then the
    /// directory listing. Spread out so that one overrunning is visible as
    /// wrong data rather than as a quiet overlap.
    const STAT: u32 = 0;
    const BUFFER: u32 = 256;
    const CWD: u32 = 512;
    const SEED: u32 = 640;
    const DENTS: u32 = 1024;
    /// Where `st_size` is in a `struct stat` on x86-64. Not a guess: it is what
    /// the C library's header says, and it is what the kernel side fills in.
    const ST_SIZE: u32 = 48;
    /// The first descriptor that is not standard input, output or error.
    const FIRST_REAL_DESCRIPTOR: u32 = 3;

    let mut a = Assembler::default();

    // A page to work in. Everything after this writes into it, so a failure
    // here has to stop rather than carry on into a null pointer.
    a.mov_edi(0)
        .mov_esi(4096)
        .mov_edx(3) // PROT_READ | PROT_WRITE
        .mov_r10d(0x22) // MAP_PRIVATE | MAP_ANONYMOUS
        .mov_r8d(u32::MAX) // no file
        .mov_r9d(0)
        .mov_eax(9) // mmap
        .syscall()
        .expect_not_negative(10)
        .rbx_from_rax();

    // Create it, and check the descriptor before writing anything through it.
    a.mov_edi(AT_FDCWD)
        .mov_rsi_imm(path)
        .mov_edx(O_WRONLY_CREAT_TRUNC)
        .mov_r10d(0o644)
        .mov_eax(257) // openat
        .syscall()
        .expect_not_negative(11)
        .expect_at_least(FIRST_REAL_DESCRIPTOR, 12)
        .r12_from_rax();

    // Write the message, and insist on all of it.
    a.rdi_from_r12()
        .mov_rsi_imm(message)
        .mov_edx(length)
        .mov_eax(1) // write
        .syscall()
        .expect_exactly(length, 13);

    a.rdi_from_r12().mov_eax(3).syscall().expect_not_negative(14); // close

    // Open it again. This is the step that distinguishes a layer that wrote the
    // file from one that accepted the bytes and dropped them.
    a.mov_edi(AT_FDCWD)
        .mov_rsi_imm(path)
        .mov_edx(O_RDONLY)
        .mov_r10d(0)
        .mov_eax(257)
        .syscall()
        .expect_not_negative(15)
        .r12_from_rax();

    a.rdi_from_r12()
        .rsi_in_scratch(STAT)
        .mov_eax(5) // fstat
        .syscall()
        .expect_not_negative(16);
    a.rax_from_scratch(STAT + ST_SIZE).expect_exactly(length, 17);

    a.rdi_from_r12()
        .rsi_in_scratch(BUFFER)
        .mov_edx(length)
        .mov_eax(0) // read
        .syscall()
        .expect_exactly(length, 18);

    // The bytes themselves, not just the count. The first eight of them, which
    // is one comparison and is enough to tell "the file came back" from "a
    // buffer of zeroes came back with the right length".
    a.rax_from_scratch(BUFFER);
    a.mov_rcx_imm(first_eight);
    a.raw(&[0x48, 0x39, 0xC8]).unless(0x74, 19); // cmp rax, rcx; je

    a.rdi_from_r12()
        .mov_rsi_imm(0)
        .mov_edx(2) // SEEK_END
        .mov_eax(8) // lseek
        .syscall()
        .expect_exactly(length, 20);
    a.rdi_from_r12().mov_eax(3).syscall();

    a.rsi_in_scratch(CWD);
    a.raw(&[0x48, 0x89, 0xF7]); // mov rdi, rsi
    a.mov_esi(64).mov_eax(79).syscall().expect_exactly(2, 21); // getcwd -> "/\0"

    a.rsi_in_scratch(SEED);
    a.raw(&[0x48, 0x89, 0xF7]); // mov rdi, rsi
    a.mov_esi(16).mov_edx(0).mov_eax(318).syscall().expect_exactly(16, 22); // getrandom

    // And a directory, read the way a directory is read.
    a.mov_edi(AT_FDCWD)
        .mov_rsi_imm(root)
        .mov_edx(O_RDONLY | O_DIRECTORY)
        .mov_r10d(0)
        .mov_eax(257)
        .syscall()
        .expect_not_negative(23)
        .r12_from_rax();
    a.rdi_from_r12()
        .rsi_in_scratch(DENTS)
        .mov_edx(1024)
        .mov_eax(217) // getdents64
        .syscall()
        .expect_not_negative(24)
        .expect_at_least(1, 25); // an empty root would be a root that is not there

    // The auxiliary vector, walked from the stack pointer -- which still points
    // at `argc`, because nothing above has pushed anything. Past the arguments,
    // past the environment, then pair by pair looking for `AT_RANDOM`.
    a.raw(&[0x48, 0x89, 0xE6]); // mov rsi, rsp
    a.raw(&[0x48, 0x8B, 0x06]); // mov rax, [rsi]      -- argc
    a.raw(&[0x48, 0x8D, 0x74, 0xC6, 0x10]); // lea rsi, [rsi + rax*8 + 16]

    let environment = a.at();
    a.raw(&[0x48, 0x8B, 0x06]); // mov rax, [rsi]
    a.raw(&[0x48, 0x83, 0xC6, 0x08]); // add rsi, 8
    a.raw(&[0x48, 0x85, 0xC0]); // test rax, rax
    let back = -((a.at() + 2 - environment) as i64);
    a.raw(&[0x75, i8::try_from(back).expect("the environment loop is short") as u8]); // jnz

    // Each pair is a type and a value. Type zero ends the vector.
    let pairs = a.at();
    a.raw(&[0x48, 0x8B, 0x06]); // mov rax, [rsi]
    a.raw(&[0x48, 0x85, 0xC0]); // test rax, rax
    a.unless(0x75, 26); // a vector with no AT_RANDOM in it
    a.raw(&[0x48, 0x83, 0xF8, 0x19]); // cmp rax, 25 (AT_RANDOM)
    // Found: the value is the next word, and it has to be a real pointer. A
    // null one is exactly as fatal to a program with a stack guard as no entry
    // at all, so the two are the same failure here.
    let found = {
        let mut out = Assembler::default();
        out.raw(&[0x48, 0x8B, 0x46, 0x08]); // mov rax, [rsi+8]
        out.raw(&[0x48, 0x85, 0xC0]); // test rax, rax
        out.unless(0x75, 26);
        out.code
    };
    // `jne` over the found block and the jump that follows it, landing on the
    // step that moves to the next pair.
    let skip = u8::try_from(found.len() + 2).expect("the found block is short");
    a.raw(&[0x75, skip]);
    a.raw(&found);
    // Found and sound: past the loop's tail, to the message. Six bytes, which
    // is the `add` and the `jmp` below.
    a.raw(&[0xEB, 0x06]);
    a.raw(&[0x48, 0x83, 0xC6, 0x10]); // add rsi, 16
    let back = -((a.at() + 2 - pairs) as i64);
    a.raw(&[0xEB, i8::try_from(back).expect("the auxv loop is short") as u8]); // jmp

    // Everything held. Say so where the log will show it, and stop with zero.
    a.mov_edi(1)
        .mov_rsi_imm(message)
        .mov_edx(length)
        .mov_eax(1)
        .syscall();
    a.mov_eax(231).mov_edi(0).syscall();

    a.code
}

/// Which of the three programs to emit.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavour {
    Plain,
    Rich,
    Files,
}

/// The file the `files` program writes, and the directory it lists.
///
/// Both are paths inside what a translated program sees as `/`, which on this
/// machine is `linux/` in the store. They are null-terminated here because
/// `openat` takes a C string and the count is not passed with it.
const PROBE_PATH: &[u8] = b"/probe.txt\0";
const ROOT_PATH: &[u8] = b"/\0";

/// Assemble the whole file.
fn build(message: &[u8], flavour: Flavour) -> Vec<u8> {
    // The layout is decided first, because the code has to name the address of
    // the message and the headers have to name the address of the code. The
    // code is assembled twice: once to measure it, and once with the addresses
    // that measurement makes it possible to work out.
    let code_offset = EHDR + PHDR;
    let assemble = |message_address: u64, path: u64, root: u64| -> Vec<u8> {
        let length = message.len() as u32;
        match flavour {
            Flavour::Plain => machine_code(message_address, length),
            Flavour::Rich => rich_machine_code(message_address, length),
            Flavour::Files => {
                // The first eight bytes of the message, as one word, so the
                // program can compare what it read against what it wrote with
                // a single instruction.
                let mut first = [0u8; 8];
                let taken = message.len().min(8);
                first[..taken].copy_from_slice(&message[..taken]);
                files_machine_code(
                    message_address,
                    length,
                    u64::from_le_bytes(first),
                    path,
                    root,
                )
            }
        }
    };

    let code = assemble(0, 0, 0); // to measure it
    let message_offset = code_offset + code.len();
    let message_address = BASE + message_offset as u64;
    let path_offset = message_offset + message.len();
    let root_offset = path_offset + PROBE_PATH.len();
    let code = assemble(
        message_address,
        BASE + path_offset as u64,
        BASE + root_offset as u64,
    );
    // Assembling twice must not change the length, or every address worked out
    // from the first pass is wrong by the difference. Nothing here is
    // address-dependent in size -- every immediate is a fixed width -- and this
    // is what says so rather than hoping.
    assert_eq!(
        code.len(),
        message_offset - code_offset,
        "the second assembly came out a different length from the first"
    );
    let entry = BASE + code_offset as u64;
    let total = root_offset + ROOT_PATH.len();

    let mut image = Vec::with_capacity(total);

    // ---- the ELF header -------------------------------------------------
    image.extend_from_slice(&[0x7F, b'E', b'L', b'F']); // e_ident: magic
    image.push(2); //   EI_CLASS   = ELFCLASS64
    image.push(1); //   EI_DATA    = ELFDATA2LSB
    image.push(1); //   EI_VERSION = EV_CURRENT
    image.push(0); //   EI_OSABI   = ELFOSABI_SYSV, which is what Linux binaries
    image.push(0); //   EI_ABIVERSION      almost always carry: nothing in the
    image.extend_from_slice(&[0; 7]); // padding    file says "Linux" at all.
    image.extend_from_slice(&2u16.to_le_bytes()); // e_type = ET_EXEC
    image.extend_from_slice(&0x3Eu16.to_le_bytes()); // e_machine = EM_X86_64
    image.extend_from_slice(&1u32.to_le_bytes()); // e_version
    image.extend_from_slice(&entry.to_le_bytes()); // e_entry
    image.extend_from_slice(&(EHDR as u64).to_le_bytes()); // e_phoff
    image.extend_from_slice(&0u64.to_le_bytes()); // e_shoff: no sections
    image.extend_from_slice(&0u32.to_le_bytes()); // e_flags
    image.extend_from_slice(&(EHDR as u16).to_le_bytes()); // e_ehsize
    image.extend_from_slice(&(PHDR as u16).to_le_bytes()); // e_phentsize
    image.extend_from_slice(&1u16.to_le_bytes()); // e_phnum
    image.extend_from_slice(&0u16.to_le_bytes()); // e_shentsize
    image.extend_from_slice(&0u16.to_le_bytes()); // e_shnum
    image.extend_from_slice(&0u16.to_le_bytes()); // e_shstrndx
    assert_eq!(image.len(), EHDR);

    // ---- one program header ---------------------------------------------
    //
    // Mapped from offset zero, so the headers themselves are in the image the
    // program sees. That is what a real static binary does, and it is why the
    // entry point is not at the start of the file.
    image.extend_from_slice(&1u32.to_le_bytes()); // p_type  = PT_LOAD
    image.extend_from_slice(&5u32.to_le_bytes()); // p_flags = R | X
    image.extend_from_slice(&0u64.to_le_bytes()); // p_offset
    image.extend_from_slice(&BASE.to_le_bytes()); // p_vaddr
    image.extend_from_slice(&BASE.to_le_bytes()); // p_paddr
    image.extend_from_slice(&(total as u64).to_le_bytes()); // p_filesz
    image.extend_from_slice(&(total as u64).to_le_bytes()); // p_memsz
    image.extend_from_slice(&0x1000u64.to_le_bytes()); // p_align
    assert_eq!(image.len(), EHDR + PHDR);

    image.extend_from_slice(&code);
    image.extend_from_slice(message);
    image.extend_from_slice(PROBE_PATH);
    image.extend_from_slice(ROOT_PATH);
    assert_eq!(image.len(), total);
    image
}

/// The program itself.
///
/// ```text
///     mov  eax, 1          ; __NR_write
///     mov  edi, 1          ; fd 1, standard output
///     movabs rsi, message  ; the buffer
///     mov  edx, length     ; how many bytes
///     syscall
///     mov  eax, 231        ; __NR_exit_group
///     xor  edi, edi        ; status 0
///     syscall
/// ```
///
/// Nothing here is NexusOS's. The call numbers are Linux's, the registers are
/// the ones Linux's system call convention names, and `syscall` is the
/// instruction Linux expects to be used — which is exactly why running this
/// proves something: the other side has to understand a foreign interface, not
/// a familiar one wearing a hat.
fn machine_code(message: u64, length: u32) -> Vec<u8> {
    let mut code = Vec::new();
    code.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00]); // mov eax, 1
    code.extend_from_slice(&[0xBF, 0x01, 0x00, 0x00, 0x00]); // mov edi, 1
    code.extend_from_slice(&[0x48, 0xBE]); // movabs rsi, imm64
    code.extend_from_slice(&message.to_le_bytes());
    code.push(0xBA); // mov edx, imm32
    code.extend_from_slice(&length.to_le_bytes());
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    code.extend_from_slice(&[0xB8, 0xE7, 0x00, 0x00, 0x00]); // mov eax, 231
    code.extend_from_slice(&[0x31, 0xFF]); // xor edi, edi
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    code
}

/// A program that asks for what a real libc asks for.
///
/// The twenty-nine byte one above proves the boundary exists. This one proves
/// it is wide enough to be useful, by making the three calls that every
/// statically linked C or Rust program makes before it prints anything:
///
/// ```text
///     ; p = mmap(NULL, 4096, PROT_READ|PROT_WRITE, MAP_PRIVATE|MAP_ANONYMOUS, -1, 0)
///     xor  edi, edi
///     mov  esi, 4096
///     mov  edx, 3            ; PROT_READ | PROT_WRITE
///     mov  r10d, 0x22        ; MAP_PRIVATE | MAP_ANONYMOUS
///     mov  r8d, -1           ; no file
///     xor  r9d, r9d
///     mov  eax, 9            ; __NR_mmap
///     syscall
///     test rax, rax
///     js   failed            ; a negative return is an error
///     mov  rbx, rax
///
///     ; build two iovecs in it, pointing at the same message twice
///     movabs rcx, message
///     mov  [rbx], rcx        ; iov[0].iov_base
///     mov  eax, length
///     mov  [rbx+8], rax      ; iov[0].iov_len
///     mov  [rbx+16], rcx     ; iov[1].iov_base
///     mov  [rbx+24], rax     ; iov[1].iov_len
///
///     ; writev(1, iov, 2)
///     mov  eax, 20           ; __NR_writev
///     mov  edi, 1
///     mov  rsi, rbx
///     mov  edx, 2
///     syscall
///
///     ; arch_prctl(ARCH_SET_FS, p) -- what sets up thread-local storage
///     mov  eax, 158
///     mov  edi, 0x1002
///     mov  rsi, rbx
///     syscall
///
///     mov  eax, 231          ; __NR_exit_group
///     xor  edi, edi
///     syscall
/// failed:
///     mov  eax, 231
///     mov  edi, 1            ; status 1, so a failed mmap is not a pass
///     syscall
/// ```
///
/// The `test`/`js` matters: if `mmap` is refused, the program exits **1** and
/// the test that runs it fails. Without it the store into `[rbx]` would fault
/// instead, which is a worse failure to read and one that could be mistaken for
/// a kernel bug rather than a refused call.
///
/// Writing the *same* message twice through two vectors is deliberate: a
/// `writev` that only walked the first vector would produce output that looks
/// almost right, and "almost right" is what a test has to be able to catch.
fn rich_machine_code(message: u64, length: u32) -> Vec<u8> {
    // The body that runs when `mmap` worked. Assembled first so that the
    // conditional jump over it can be given the right distance.
    let mut body: Vec<u8> = Vec::new();
    body.extend_from_slice(&[0x48, 0x89, 0xC3]); // mov rbx, rax

    body.extend_from_slice(&[0x48, 0xB9]); // movabs rcx, imm64
    body.extend_from_slice(&message.to_le_bytes());
    body.extend_from_slice(&[0x48, 0x89, 0x0B]); // mov [rbx], rcx
    body.push(0xB8); // mov eax, imm32
    body.extend_from_slice(&length.to_le_bytes());
    body.extend_from_slice(&[0x48, 0x89, 0x43, 0x08]); // mov [rbx+8], rax
    body.extend_from_slice(&[0x48, 0x89, 0x4B, 0x10]); // mov [rbx+16], rcx
    body.extend_from_slice(&[0x48, 0x89, 0x43, 0x18]); // mov [rbx+24], rax

    body.extend_from_slice(&[0xB8, 0x14, 0x00, 0x00, 0x00]); // mov eax, 20
    body.extend_from_slice(&[0xBF, 0x01, 0x00, 0x00, 0x00]); // mov edi, 1
    body.extend_from_slice(&[0x48, 0x89, 0xDE]); // mov rsi, rbx
    body.extend_from_slice(&[0xBA, 0x02, 0x00, 0x00, 0x00]); // mov edx, 2
    body.extend_from_slice(&[0x0F, 0x05]); // syscall

    body.extend_from_slice(&[0xB8, 0x9E, 0x00, 0x00, 0x00]); // mov eax, 158
    body.extend_from_slice(&[0xBF, 0x02, 0x10, 0x00, 0x00]); // mov edi, 0x1002
    body.extend_from_slice(&[0x48, 0x89, 0xDE]); // mov rsi, rbx
    body.extend_from_slice(&[0x0F, 0x05]); // syscall

    body.extend_from_slice(&[0xB8, 0xE7, 0x00, 0x00, 0x00]); // mov eax, 231
    body.extend_from_slice(&[0x31, 0xFF]); // xor edi, edi
    body.extend_from_slice(&[0x0F, 0x05]); // syscall

    let mut code: Vec<u8> = Vec::new();
    code.extend_from_slice(&[0x31, 0xFF]); // xor edi, edi
    code.extend_from_slice(&[0xBE, 0x00, 0x10, 0x00, 0x00]); // mov esi, 4096
    code.extend_from_slice(&[0xBA, 0x03, 0x00, 0x00, 0x00]); // mov edx, 3
    code.extend_from_slice(&[0x41, 0xBA, 0x22, 0x00, 0x00, 0x00]); // mov r10d, 0x22
    code.extend_from_slice(&[0x41, 0xB8, 0xFF, 0xFF, 0xFF, 0xFF]); // mov r8d, -1
    code.extend_from_slice(&[0x45, 0x31, 0xC9]); // xor r9d, r9d
    code.extend_from_slice(&[0xB8, 0x09, 0x00, 0x00, 0x00]); // mov eax, 9
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    code.extend_from_slice(&[0x48, 0x85, 0xC0]); // test rax, rax

    // `js` takes a signed byte, which bounds the body at 127 instructions'
    // worth. It is about sixty, and this asserts rather than silently
    // assembling a jump to the wrong place if that ever stops being true.
    let over = i8::try_from(body.len()).expect("the success path must fit an eight-bit jump");
    code.extend_from_slice(&[0x78, over as u8]); // js failed

    code.extend_from_slice(&body);

    // failed:
    code.extend_from_slice(&[0xB8, 0xE7, 0x00, 0x00, 0x00]); // mov eax, 231
    code.extend_from_slice(&[0xBF, 0x01, 0x00, 0x00, 0x00]); // mov edi, 1
    code.extend_from_slice(&[0x0F, 0x05]); // syscall
    code
}
