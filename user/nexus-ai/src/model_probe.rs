//! Isolated guest client; expected IDs obtained from pinned upstream C, not Rust.
#![no_std]
#![no_main]
use core::panic::PanicInfo;

// Required when the optional model feature links alloc; these binaries do not allocate.
#[cfg(feature = "model")]
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;
use nexus_user::Handle;
const EXPECTED: &[u32] = &[
    432, 383, 286, 261, 376, 298, 315, 421, 395, 317, 426, 338, 401, 396, 267, 337, 410, 408, 419,
    292, 411, 322, 265, 282, 295, 433, 426, 385, 328, 432, 358, 394,
];
const TEXT: &[u8]=b", there was a little girl named Lily. She loved to play outside in the park. One day, she saw";
#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!("xor rbp, rbp", "call {main}", "ud2", main = sym main);
}
fn fail() -> ! {
    nexus_user::log("model-probe: FAIL").ok();
    nexus_user::exit_with(1)
}
fn receive(
    channel: Handle,
    bytes: &mut [u8; 256],
    handles: &mut [Handle; 4],
) -> nexus_user::Received {
    let set = nexus_user::wait_set().unwrap_or_else(|_| fail());
    nexus_user::watch(set, channel, 1).unwrap_or_else(|_| fail());
    if nexus_user::wait_any_until(set, &mut [0], 30_000) != Ok(1) {
        fail();
    }
    let got = nexus_user::receive(channel, bytes, handles).unwrap_or_else(|_| fail());
    nexus_user::close(set).unwrap_or_else(|_| fail());
    got
}
fn spawn() -> (Handle, Handle) {
    nexus_user::send(Handle(1), b"BIN/MODEL.ELF", &[]).unwrap_or_else(|_| fail());
    let mut bytes = [0; 256];
    let mut handles = [Handle(0); 4];
    let got = receive(Handle(1), &mut bytes, &mut handles);
    if got.handles != 2 || !bytes[..got.bytes].starts_with(b"started ") {
        fail();
    }
    (handles[0], handles[1])
}
fn exchange(ch: Handle, request: &[u8], bytes: &mut [u8; 256]) -> usize {
    nexus_user::send(ch, request, &[]).unwrap_or_else(|_| fail());
    let got = receive(ch, bytes, &mut [Handle(0); 4]);
    if got.handles != 0 {
        fail();
    }
    got.bytes
}
fn joined(ch: Handle, process: Handle, expected: u32) {
    nexus_user::close(ch).unwrap_or_else(|_| fail());
    reap(process, expected);
}
fn reap(process: Handle, expected: u32) {
    let set = nexus_user::wait_set().unwrap_or_else(|_| fail());
    nexus_user::watch(set, process, 1).unwrap_or_else(|_| fail());
    if nexus_user::wait_any_until(set, &mut [0], 10_000) != Ok(1)
        || nexus_user::wait(process) != Ok(nexus_user::Ending::Exited(expected))
    {
        fail();
    }
    nexus_user::close(set).ok();
    nexus_user::close(process).ok();
}
extern "C" fn main() -> ! {
    nexus_user::log("model-probe: pretrained transformer via isolated worker IPC").ok();
    let (ch, process) = spawn();
    let mut bytes = [0; 256];
    let n = exchange(ch, b"gen1\x20\x00Once upon a time", &mut bytes);
    if &bytes[..n] != b"rdy1" {
        fail();
    }
    let mut count = 0;
    let mut text = [0; 256];
    let mut len = 0;
    let mut prefill = 0;
    let mut ended = false;
    for _ in 0..64 {
        let n = exchange(ch, b"next", &mut bytes);
        if n < 4 {
            fail();
        }
        match &bytes[..4] {
            b"pre1" => {
                if n != 4 {
                    fail();
                }
                prefill += 1;
            }
            b"tok1" => {
                if n < 8
                    || count >= EXPECTED.len()
                    || u32::from_le_bytes(bytes[4..8].try_into().unwrap()) != EXPECTED[count]
                {
                    fail();
                }
                count += 1;
                if len + n - 8 > text.len() {
                    fail();
                }
                text[len..len + n - 8].copy_from_slice(&bytes[8..n]);
                len += n - 8;
            }
            b"end1" => {
                if n != 5 || bytes[4] != 1 {
                    fail();
                }
                ended = true;
                break;
            }
            _ => fail(),
        }
    }
    if !ended || count != 32 || prefill != 4 || &text[..len] != TEXT {
        fail();
    }
    joined(ch, process, 0);
    nexus_user::log("model-probe: 32 generated token IDs and text match upstream C oracle").ok();
    // The fixed public test output may be logged. The worker never logs prompts.
    nexus_user::log(core::str::from_utf8(&text[..len]).unwrap_or_else(|_| fail())).ok();
    let (ch, process) = spawn();
    let n = exchange(ch, b"gen1\x20\x00hello", &mut bytes);
    if &bytes[..n] != b"rdy1" {
        fail();
    }
    let n = exchange(ch, b"stop", &mut bytes);
    if &bytes[..n] != b"can1" {
        fail();
    }
    joined(ch, process, 0);
    for request in [
        &b"gen1\x00\x00hello"[..],
        &b"gen1\x01\x01hello"[..],
        &b"gen1\x20\x00\xff"[..],
        &b"bad"[..],
    ] {
        let (ch, process) = spawn();
        let n = exchange(ch, request, &mut bytes);
        if &bytes[..n] != b"err1" {
            fail();
        }
        joined(ch, process, 1);
    }
    // A peer disconnect must terminate a worker waiting for a generation pull.
    let (ch, process) = spawn();
    let n = exchange(ch, b"gen1\x20\x00hello", &mut bytes);
    if &bytes[..n] != b"rdy1" {
        fail();
    }
    joined(ch, process, 1);
    // Transferred authority is never used, even when it cannot be closed.
    let (ch, process) = spawn();
    let (lent, peer) = nexus_user::channel().unwrap_or_else(|_| fail());
    let restricted = nexus_user::duplicate(
        lent,
        nexus_user::rights::READ | nexus_user::rights::TRANSFER,
    )
    .unwrap_or_else(|_| fail());
    nexus_user::close(lent).unwrap_or_else(|_| fail());
    nexus_user::send(ch, b"gen1\x20\x00hello", &[restricted]).unwrap_or_else(|_| fail());
    reap(process, 1);
    nexus_user::close(ch).unwrap_or_else(|_| fail());
    let set = nexus_user::wait_set().unwrap_or_else(|_| fail());
    nexus_user::watch(set, peer, 1).unwrap_or_else(|_| fail());
    if nexus_user::wait_any_until(set, &mut [0], 10_000) != Ok(1)
        || nexus_user::send(peer, b"closed?", &[]) != Err(nexus_user::Error::Closed)
    {
        fail();
    }
    nexus_user::close(set).ok();
    nexus_user::close(peer).ok();
    nexus_user::log(
        "model-probe: malformed UTF-8, limits, disconnect and unclosable capability rejected",
    )
    .ok();
    nexus_user::log(
        "model-probe: PASS pretrained inference IPC oracle cancellation limits; kernel alive",
    )
    .ok();
    nexus_user::exit()
}
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    fail()
}
