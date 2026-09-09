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
    if arguments.len() != 2 {
        eprintln!("usage: nexus-linux-example <output.elf> <message>");
        return ExitCode::FAILURE;
    }
    let output = PathBuf::from(&arguments[0]);
    let mut message = arguments[1].clone().into_bytes();
    message.push(b'\n');

    let image = build(&message);

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
fn build(message: &[u8]) -> Vec<u8> {
    // The layout is decided first, because the code has to name the address of
    // the message and the headers have to name the address of the code.
    let code_offset = EHDR + PHDR;
    let code = machine_code(0, 0); // to measure it
    let message_offset = code_offset + code.len();
    let message_address = BASE + message_offset as u64;
    let code = machine_code(message_address, message.len() as u32);
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
