//! Read-only Nexus AI service. Started with only a parent channel, no I/O grants.
#![no_std]
#![no_main]

use core::panic::PanicInfo;
use nexus_ai_core::{wire, Response, Runtime, Snapshot, Status, SystemSource, Unavailable};
use nexus_user::Handle;

const PARENT: Handle = Handle(1);

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!("xor rbp, rbp", "call {main}", "ud2", main = sym main);
}

struct System;
impl SystemSource for System {
    fn snapshot(&mut self) -> Result<Snapshot, Unavailable> {
        Ok(Snapshot {
            uptime_ms: nexus_user::uptime(),
            thread_id: nexus_user::thread_id(),
        })
    }
}

extern "C" fn main() -> ! {
    nexus_user::log("nexus-ai: read-only service ready; protocol v1, session 1").ok();
    let Ok(set) = nexus_user::wait_set() else {
        nexus_user::exit_with(1)
    };
    if nexus_user::watch(set, PARENT, 1).is_err() {
        nexus_user::exit_with(1);
    }
    let mut runtime = Runtime::new(1);
    // Bound even malformed traffic. Idle clients cannot keep a worker forever.
    for _ in 0..64 {
        let mut keys = [0; 1];
        if !matches!(nexus_user::wait_any_until(set, &mut keys, 60_000), Ok(1)) {
            break;
        }
        let mut bytes = [0; nexus_user::MAX_MESSAGE];
        let mut handles = [Handle(0); nexus_user::MAX_HANDLES];
        let Ok(received) = nexus_user::receive(PARENT, &mut bytes, &mut handles) else {
            break;
        };
        // Unexpected capabilities are disposed, never installed into tools.
        for handle in &handles[..received.handles] {
            // CLOSE is optional on transferred handles. If disposal is denied,
            // exit so process teardown releases it; never retain such grants.
            if nexus_user::close(*handle).is_err() {
                nexus_user::exit_with(1);
            }
        }
        let input = &bytes[..received.bytes];
        let response = if received.handles != 0 {
            Response::error(0, 0, Status::Invalid)
        } else if let Some(request) = wire::decode_request(input) {
            runtime.execute(request, nexus_user::uptime(), &mut System)
        } else {
            Response::error(0, 0, Status::Invalid)
        };
        // Fixed numeric wire fields only; no request bodies enter the serial log.
        log_result(response);
        if nexus_user::send(PARENT, &wire::encode_response(response), &[]).is_err() {
            break;
        }
    }
    nexus_user::log("nexus-ai: service stopped").ok();
    nexus_user::exit()
}

fn log_result(response: Response) {
    use core::fmt::Write;
    struct Line {
        bytes: [u8; 128],
        len: usize,
    }
    impl Write for Line {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let end = self.len.checked_add(s.len()).ok_or(core::fmt::Error)?;
            if end > self.bytes.len() {
                return Err(core::fmt::Error);
            }
            self.bytes[self.len..end].copy_from_slice(s.as_bytes());
            self.len = end;
            Ok(())
        }
    }
    let mut line = Line {
        bytes: [0; 128],
        len: 0,
    };
    if write!(
        line,
        "nexus-ai: session={} task={} status={:?}",
        response.session, response.id, response.status
    )
    .is_ok()
    {
        if let Ok(text) = core::str::from_utf8(&line.bytes[..line.len]) {
            nexus_user::log(text).ok();
        }
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    nexus_user::log("nexus-ai: PANIC").ok();
    nexus_user::exit_with(2)
}
