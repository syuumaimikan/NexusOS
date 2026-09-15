//! Local inference worker: one request per process, parent channel only.
#![no_std]
#![no_main]
extern crate alloc;
use core::panic::PanicInfo;
use nexus_ai_core::model::{Event, Finish, Llama, ModelProvider, ModelSession, Tokenizer};
use nexus_user::Handle;
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;
const PARENT: Handle = Handle(1);
const WEIGHTS: &[u8] = include_bytes!("../../../build/models/stories260K.bin");
const VOCAB: &[u8] = include_bytes!("../../../build/models/tok512.bin");
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!("xor rbp, rbp", "call {main}", "ud2", main = sym main);
}
fn fail() -> ! {
    nexus_user::send(PARENT, b"err1", &[]).ok();
    nexus_user::exit_with(1)
}
fn receive(set: Handle, bytes: &mut [u8; 256]) -> usize {
    if nexus_user::wait_any_until(set, &mut [0], 10_000) != Ok(1) {
        nexus_user::exit_with(1);
    }
    let mut handles = [Handle(0); 4];
    let got = nexus_user::receive(PARENT, bytes, &mut handles)
        .unwrap_or_else(|_| nexus_user::exit_with(1));
    // Teardown releases even handles without CLOSE; never use lent capabilities.
    if got.handles != 0 {
        nexus_user::exit_with(1);
    }
    got.bytes
}
extern "C" fn main() -> ! {
    if !nexus_user::heap::init(4 * 1024 * 1024) {
        fail();
    }
    let set = nexus_user::wait_set().unwrap_or_else(|_| fail());
    nexus_user::watch(set, PARENT, 1).unwrap_or_else(|_| fail());
    let mut bytes = [0; 256];
    // gen1 | max_new_tokens:u16 | UTF-8 prompt. Never logged or persisted.
    let n = receive(set, &mut bytes);
    if n < 6 || &bytes[..4] != b"gen1" {
        fail();
    }
    let limit = u16::from_le_bytes(bytes[4..6].try_into().unwrap()) as usize;
    let prompt = core::str::from_utf8(&bytes[6..n]).unwrap_or_else(|_| fail());
    let model = Llama::load(WEIGHTS).unwrap_or_else(|_| fail());
    let tokenizer = Tokenizer::load(VOCAB, model.vocab_size()).unwrap_or_else(|_| fail());
    let tokens = tokenizer.encode(prompt).unwrap_or_else(|_| fail());
    let mut previous = *tokens.last().unwrap_or_else(|| fail());
    let mut session = ModelSession::new(&model, tokens, limit).unwrap_or_else(|_| fail());
    nexus_user::send(PARENT, b"rdy1", &[]).unwrap_or_else(|_| nexus_user::exit_with(1));
    let start = nexus_user::uptime();
    // Pull protocol provides backpressure. Each next performs one bounded pass.
    for _ in 0..1024 {
        let n = receive(set, &mut bytes);
        if nexus_user::uptime().saturating_sub(start) >= 120_000 {
            fail();
        }
        if &bytes[..n] == b"stop" {
            session.cancel();
            nexus_user::send(PARENT, b"can1", &[]).ok();
            nexus_user::exit();
        }
        if &bytes[..n] != b"next" {
            fail();
        }
        let len = match session.poll().unwrap_or_else(|_| fail()) {
            Event::Prefill => {
                bytes[..4].copy_from_slice(b"pre1");
                4
            }
            Event::Token(id) => {
                let piece = tokenizer.decode(previous, id).unwrap_or_else(|_| fail());
                previous = id;
                bytes[..4].copy_from_slice(b"tok1");
                bytes[4..8].copy_from_slice(&id.to_le_bytes());
                bytes[8..8 + piece.len()].copy_from_slice(piece);
                8 + piece.len()
            }
            Event::Finished(reason) => {
                let code = if reason == Finish::Eos { 0 } else { 1 };
                nexus_user::send(PARENT, &[b'e', b'n', b'd', b'1', code], &[]).ok();
                nexus_user::exit();
            }
        };
        nexus_user::send(PARENT, &bytes[..len], &[]).unwrap_or_else(|_| nexus_user::exit_with(1));
        nexus_user::yield_now();
    }
    fail()
}
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    nexus_user::log("nexus-model: PANIC").ok();
    nexus_user::exit_with(2)
}
