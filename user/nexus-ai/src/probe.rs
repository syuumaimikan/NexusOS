//! Guest integration test, used as INIT.ELF on an isolated test disk only.
#![no_std]
#![no_main]
use core::panic::PanicInfo;

// Required when the optional model feature links alloc; these binaries do not allocate.
#[cfg(feature = "model")]
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;
use nexus_ai_core::{wire, Request, Status, Tool};
use nexus_user::Handle;

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!("xor rbp, rbp", "call {main}", "ud2", main = sym main);
}
fn fail() -> ! {
    nexus_user::log("ai-probe: FAIL").ok();
    nexus_user::exit_with(1)
}
fn receive(channel: Handle, bytes: &mut [u8], handles: &mut [Handle]) -> nexus_user::Received {
    let set = nexus_user::wait_set().unwrap_or_else(|_| fail());
    nexus_user::watch(set, channel, 1).unwrap_or_else(|_| fail());
    if nexus_user::wait_any_until(set, &mut [0], 10_000) != Ok(1) {
        fail();
    }
    let received = nexus_user::receive(channel, bytes, handles).unwrap_or_else(|_| fail());
    nexus_user::close(set).unwrap_or_else(|_| fail());
    received
}
extern "C" fn main() -> ! {
    verify_context();
    nexus_user::log("ai-probe: starting actual service process").ok();
    nexus_user::send(Handle(1), b"BIN/AI.ELF", &[]).unwrap_or_else(|_| fail());
    let mut bytes = [0; 256];
    let mut handles = [Handle(0); 4];
    let got = receive(Handle(1), &mut bytes, &mut handles);
    if got.handles != 2 || !bytes[..got.bytes].starts_with(b"started ") {
        fail();
    }
    let channel = handles[0];
    let process = handles[1];
    let start = nexus_user::uptime();
    for (id, tool, status) in [
        (1, Tool::SystemInfo, Status::Verified),
        (2, Tool::TerminalExecute, Status::ConfirmRequired),
        (3, Tool::KernelMemory, Status::Blocked),
        (4, Tool::SettingsWrite, Status::Privileged),
        (5, Tool::FileRead, Status::Unsupported),
        (5, Tool::SystemInfo, Status::Invalid),
    ] {
        nexus_user::send(
            channel,
            &wire::encode_request(Request {
                session: 1,
                id,
                tool,
            }),
            &[],
        )
        .unwrap_or_else(|_| fail());
        let got = receive(channel, &mut bytes, &mut handles);
        if got.handles != 0 {
            fail();
        }
        let response = wire::decode_response(&bytes[..got.bytes]).unwrap_or_else(|| fail());
        if response.id != id || response.session != 1 || response.status != status {
            fail();
        }
        if status == Status::Verified
            && (response.snapshot.uptime_ms < start
                || response.snapshot.uptime_ms > nexus_user::uptime()
                || response.snapshot.thread_id == nexus_user::thread_id())
        {
            fail();
        }
    }
    nexus_user::log("ai-probe: verified OS observation, confirmations, privileged and blocked tools, replay rejection").ok();
    nexus_user::send(channel, b"malformed", &[]).unwrap_or_else(|_| fail());
    let got = receive(channel, &mut bytes, &mut handles);
    if got.handles != 0
        || wire::decode_response(&bytes[..got.bytes])
            != Some(nexus_ai_core::Response::error(0, 0, Status::Invalid))
    {
        fail();
    }
    let (lent, peer) = nexus_user::channel().unwrap_or_else(|_| fail());
    nexus_user::send(channel, b"unexpected handle", &[lent]).unwrap_or_else(|_| fail());
    let got = receive(channel, &mut bytes, &mut handles);
    if got.handles != 0
        || wire::decode_response(&bytes[..got.bytes])
            != Some(nexus_ai_core::Response::error(0, 0, Status::Invalid))
    {
        fail();
    }
    if nexus_user::send(peer, b"closed?", &[]) != Err(nexus_user::Error::Closed) {
        fail();
    }
    nexus_user::close(peer).unwrap_or_else(|_| fail());
    // A worker receives no authority to acquire filesystem/network/spawn access.
    // Closing the only peer must make it exit; wait readiness before ProcessWait.
    nexus_user::close(channel).unwrap_or_else(|_| fail());
    let set = nexus_user::wait_set().unwrap_or_else(|_| fail());
    nexus_user::watch(set, process, 1).unwrap_or_else(|_| fail());
    if nexus_user::wait_any_until(set, &mut [0], 10_000) != Ok(1) {
        fail();
    }
    let ending = nexus_user::wait(process).unwrap_or_else(|_| fail());
    if !matches!(ending, nexus_user::Ending::Exited(0)) {
        fail();
    }
    nexus_user::close(set).ok();
    nexus_user::close(process).ok();
    // A transferred capability without CLOSE cannot be retained: service exits.
    nexus_user::send(Handle(1), b"BIN/AI.ELF", &[]).unwrap_or_else(|_| fail());
    let got = receive(Handle(1), &mut bytes, &mut handles);
    if got.handles != 2 || !bytes[..got.bytes].starts_with(b"started ") {
        fail();
    }
    let channel = handles[0];
    let process = handles[1];
    let (lent, peer) = nexus_user::channel().unwrap_or_else(|_| fail());
    let restricted = nexus_user::duplicate(
        lent,
        nexus_user::rights::READ | nexus_user::rights::TRANSFER,
    )
    .unwrap_or_else(|_| fail());
    nexus_user::close(lent).unwrap_or_else(|_| fail());
    nexus_user::send(channel, b"unclosable handle", &[restricted]).unwrap_or_else(|_| fail());
    let set = nexus_user::wait_set().unwrap_or_else(|_| fail());
    nexus_user::watch(set, process, 1).unwrap_or_else(|_| fail());
    if nexus_user::wait_any_until(set, &mut [0], 10_000) != Ok(1) {
        fail();
    }
    if !matches!(nexus_user::wait(process), Ok(nexus_user::Ending::Exited(1))) {
        fail();
    }
    // Process completion precedes asynchronous scheduler reaping. Observe the
    // channel closing as separate evidence that the held capability was freed.
    nexus_user::unwatch(set, 1).unwrap_or_else(|_| fail());
    nexus_user::watch(set, peer, 2).unwrap_or_else(|_| fail());
    if nexus_user::wait_any_until(set, &mut [0], 10_000) != Ok(1) {
        fail();
    }
    if nexus_user::send(peer, b"released?", &[]) != Err(nexus_user::Error::Closed) {
        fail();
    }
    for handle in [peer, channel, process, set] {
        nexus_user::close(handle).unwrap_or_else(|_| fail());
    }
    nexus_user::log(
        "ai-probe: unexpected capabilities disposed; unclosable capability forced teardown",
    )
    .ok();
    nexus_user::log("ai-probe: PASS service IPC policy verification disconnect; kernel alive").ok();
    nexus_user::exit()
}
fn verify_context() {
    use nexus_ai_core::context::{
        ByteBpeBudget, Context, Destination, Entry, Origin, Policy, Sensitivity,
    };
    struct Source;
    impl nexus_ai_core::SystemSource for Source {
        fn snapshot(&mut self) -> Result<nexus_ai_core::Snapshot, nexus_ai_core::Unavailable> {
            Ok(nexus_ai_core::Snapshot {
                uptime_ms: nexus_user::uptime(),
                thread_id: nexus_user::thread_id(),
            })
        }
    }
    let mut runtime = nexus_ai_core::Runtime::new(1);
    let request = Request {
        session: 1,
        id: 1,
        tool: Tool::SystemInfo,
    };
    if runtime
        .execute(request, nexus_user::uptime(), &mut Source)
        .status
        != Status::Verified
    {
        fail();
    }
    let facts = runtime.context().unwrap_or_else(|| fail());
    let mut buffer = [0; 96];
    let text = facts.write(&mut buffer).unwrap_or_else(|_| fail());
    let item = |text| Entry {
        text,
        origin: Origin::System,
        scope: facts.scope(),
        collected_ms: facts.collected_ms(),
        priority: 1,
        relevance: 1,
        sensitivity: Sensitivity::Public,
        authorized: true,
        remote_allowed: false,
    };
    let mut japanese = item("日本語の確認🌸");
    japanese.origin = Origin::User;
    let entries = [item(text), japanese, item("password=probe-fixture")];
    let policy = Policy {
        scope: facts.scope(),
        now_ms: nexus_user::uptime(),
        max_age_ms: 500,
        destination: Destination::Local,
        allow_private: false,
        byte_budget: 256,
        token_budget: 258,
    };
    let mut context = Context::select(&entries, &policy, &ByteBpeBudget).unwrap_or_else(|_| fail());
    if context.selected().count() != 2
        || context.report().redacted != 1
        || !context.text().contains("日本語の確認🌸")
        || !context.text().starts_with(text)
    {
        fail();
    }
    let remote = Policy {
        destination: Destination::Remote,
        ..policy
    };
    if !Context::select(&entries, &remote, &ByteBpeBudget)
        .unwrap_or_else(|_| fail())
        .text()
        .is_empty()
    {
        fail();
    }
    context.clear();
    runtime.cancel();
    if !context.text().is_empty() || runtime.context().is_some() {
        fail();
    }
    nexus_user::log(
        "ai-probe: context PASS verified observations UTF-8 privacy budget cancellation",
    )
    .ok();
}
#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    fail()
}
