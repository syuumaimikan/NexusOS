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
        eprintln!("usage: nexus-linux-example <output.elf> <message> [rich]");
        return ExitCode::FAILURE;
    }
    let output = PathBuf::from(&arguments[0]);
    let mut message = arguments[1].clone().into_bytes();
    message.push(b'\n');
    // Two programs, from one generator. The plain one asks for `write` and
    // `exit_group` and nothing else; the rich one asks for the memory and the
    // scattered write a real libc asks for. See `rich_machine_code`.
    let rich = arguments.get(2).map(String::as_str) == Some("rich");

    let image = build(&message, rich);

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

/// Assemble the whole file.
fn build(message: &[u8], rich: bool) -> Vec<u8> {
    let assemble = if rich {
        rich_machine_code
    } else {
        machine_code
    };
    // The layout is decided first, because the code has to name the address of
    // the message and the headers have to name the address of the code.
    let code_offset = EHDR + PHDR;
    let code = assemble(0, 0); // to measure it
    let message_offset = code_offset + code.len();
    let message_address = BASE + message_offset as u64;
    let code = assemble(message_address, message.len() as u32);
    let entry = BASE + code_offset as u64;
    let total = message_offset + message.len();

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
