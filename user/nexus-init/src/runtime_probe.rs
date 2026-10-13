//! Test-only init: execute actual libc-linked programs and inspect completion.
#![no_std]
#![no_main]
extern crate alloc;
use core::panic::PanicInfo;
use nexus_user::Handle;

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!("xor rbp, rbp", "call {main}", "ud2", main = sym main);
}

fn fail(message: &str) -> ! {
    nexus_user::log(message).ok();
    nexus_user::exit_with(1)
}

fn run(path: &[u8], label: &str) {
    run_status(path, label, 0);
}

fn run_status(path: &[u8], label: &str, expected: u32) {
    nexus_user::send(Handle(1), path, &[])
        .unwrap_or_else(|_| fail("runtime-probe: FAIL spawn request"));
    let mut bytes = [0u8; 256];
    let mut handles = [Handle(0); 4];
    let received = nexus_user::receive(Handle(1), &mut bytes, &mut handles)
        .unwrap_or_else(|_| fail("runtime-probe: FAIL spawn reply"));
    if received.handles != 2 {
        nexus_user::log(core::str::from_utf8(&bytes[..received.bytes]).unwrap_or("invalid reply"))
            .ok();
        fail("runtime-probe: FAIL executable refused");
    }
    nexus_user::close(handles[0]).ok();
    let set = nexus_user::wait_set().unwrap_or_else(|_| fail("runtime-probe: FAIL waitset"));
    nexus_user::watch(set, handles[1], 1).unwrap_or_else(|_| fail("runtime-probe: FAIL watch"));
    let mut completed = false;
    for _ in 0..4 {
        match nexus_user::wait_any_until(set, &mut [0], 30_000) {
            Ok(1) => {
                completed = true;
                break;
            }
            Ok(0) => {}
            result => fail(&alloc::format!("runtime-probe: FAIL wait {result:?}")),
        }
    }
    if !completed {
        fail("runtime-probe: FAIL runtime timed out");
    }
    let ending = nexus_user::wait(handles[1]);
    nexus_user::close(set).ok();
    nexus_user::close(handles[1]).ok();
    if ending != Ok(nexus_user::Ending::Exited(expected)) {
        fail(&alloc::format!("runtime-probe: FAIL {label} {ending:?}"));
    }
    nexus_user::log(&alloc::format!("runtime-probe: PASS {label}")).ok();
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(nexus_user::heap::DEFAULT_SIZE) {
        fail("runtime-probe: FAIL heap");
    }
    let root = nexus_user::open(Handle(2), "linux")
        .unwrap_or_else(|_| fail("runtime-probe: FAIL native Linux root"));
    let directory = nexus_user::create(root, "native-link-probe", nexus_user::Kind::Directory)
        .unwrap_or_else(|_| fail("runtime-probe: FAIL native link directory"));
    let link = nexus_user::symlink(directory, "link", "../tmp")
        .unwrap_or_else(|_| fail("runtime-probe: FAIL native symlink"));
    let mut target = [0u8; 64];
    let mut listed = [0u8; 64];
    let count = nexus_user::list(directory, &mut listed)
        .unwrap_or_else(|_| fail("runtime-probe: FAIL native link listing"));
    if nexus_user::read_link(link, &mut target) != Ok(6)
        || &target[..6] != b"../tmp"
        || nexus_user::entries(&listed[..count])
            .next()
            .map(|entry| entry.kind)
            != Some(nexus_user::Kind::Symlink)
        || nexus_user::read_link(link, &mut target[..2]) != Err(nexus_user::Error::TooBig)
    {
        fail("runtime-probe: FAIL native link bytes/type/bounds");
    }
    let readonly = nexus_user::duplicate(
        directory,
        nexus_user::rights::READ | nexus_user::rights::CLOSE,
    )
    .unwrap_or_else(|_| fail("runtime-probe: FAIL native attenuated directory"));
    if nexus_user::symlink(readonly, "denied", "/") != Err(nexus_user::Error::Denied) {
        fail("runtime-probe: FAIL native link authority");
    }
    nexus_user::close(readonly).ok();
    nexus_user::remove(directory, "link")
        .unwrap_or_else(|_| fail("runtime-probe: FAIL native link unlink"));
    if nexus_user::read_link(link, &mut target) != Ok(6) || &target[..6] != b"../tmp" {
        fail("runtime-probe: FAIL unlinked native link identity");
    }
    nexus_user::close(link).ok();
    nexus_user::close(directory).ok();
    nexus_user::remove(root, "native-link-probe")
        .unwrap_or_else(|_| fail("runtime-probe: FAIL native link directory cleanup"));
    nexus_user::close(root).ok();
    nexus_user::log(
        "runtime-probe: PASS native symlink type bytes authority and last-close identity",
    )
    .ok();
    const FIRST: usize = 0x0200_0000;
    const SECOND: usize = FIRST + 4096;
    let memory = nexus_user::memory_create(4096)
        .unwrap_or_else(|_| fail("runtime-probe: FAIL native memory create"));
    if nexus_user::memory_map(memory, FIRST, true) != Ok(4096)
        || nexus_user::memory_map(memory, SECOND, true) != Ok(4096)
    {
        fail("runtime-probe: FAIL native alias map");
    }
    unsafe {
        core::ptr::write_volatile(FIRST as *mut u64, 0x1234_5678);
    }
    nexus_user::memory_unmap(memory, FIRST)
        .unwrap_or_else(|_| fail("runtime-probe: FAIL native alias unmap"));
    nexus_user::close(memory).unwrap_or_else(|_| fail("runtime-probe: FAIL native memory close"));
    // Another allocation would reuse the freed frame if the remaining mapping
    // did not retain its object. It must not change the mapped alias's bytes.
    let other = nexus_user::memory_create(4096)
        .unwrap_or_else(|_| fail("runtime-probe: FAIL native other create"));
    if nexus_user::memory_map(other, FIRST, true) != Ok(4096) {
        fail("runtime-probe: FAIL native other map");
    }
    unsafe {
        core::ptr::write_volatile(FIRST as *mut u64, 0xaabb_ccdd);
    }
    if unsafe { core::ptr::read_volatile(SECOND as *const u64) } != 0x1234_5678 {
        fail("runtime-probe: FAIL native mapping lost its backing object");
    }
    nexus_user::close(other).ok();
    nexus_user::log("runtime-probe: PASS native aliases retain memory after handle close").ok();
    run(b"linux:/usr/bin/musl-probe", "real musl static");
    run(b"linux:/usr/bin/musl-threads", "real musl pthread");
    run(
        b"linux:/usr/bin/musl-dynamic",
        "real musl dynamic loader and dlopen",
    );
    #[cfg(feature = "runtime-glibc-probe")]
    {
        run(b"linux:/usr/bin/glibc-probe", "real GNU libc basic");
        run(b"linux:/usr/bin/glibc-threads", "real GNU libc pthread");
        run(
            b"linux:/usr/bin/glibc-dynamic",
            "real GNU libc dynamic loader and dlopen",
        );
    }
    #[cfg(feature = "runtime-mesa-probe")]
    run(b"linux:BIN/MESA.LX", "real OSMesa softpipe OpenGL and GLSL");
    #[cfg(feature = "runtime-glibc32-probe")]
    {
        run(
            b"linux32:/usr/bin/glibc32-probe",
            "real i386 GNU libc basic",
        );
        run(
            b"linux32:/usr/bin/glibc32-threads",
            "real i386 GNU libc pthread",
        );
        run(
            b"linux32:/usr/bin/glibc32-dynamic",
            "real i386 GNU libc dynamic loader and dlopen",
        );
    }
    #[cfg(feature = "runtime-vulkan-probe")]
    run(
        b"linux:BIN/VULKAN.LX",
        "real SwiftShader Vulkan and SPIR-V dispatch",
    );
    for _ in 0..8 {
        run_status(
            b"linux:/usr/bin/musl-exit-group",
            "musl group exit keeps status 42 with eight active siblings",
            42,
        );
        #[cfg(feature = "runtime-glibc-probe")]
        run_status(
            b"linux:/usr/bin/glibc-exit-group",
            "glibc group exit keeps status 42 with eight active siblings",
            42,
        );
        #[cfg(feature = "runtime-glibc32-probe")]
        run_status(
            b"linux32:/usr/bin/glibc32-exit-group",
            "glibc32 group exit keeps status 42 with eight active siblings",
            42,
        );
    }
    nexus_user::log("runtime-probe: PASS all configured runtimes").ok();
    nexus_user::exit()
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    fail("runtime-probe: PANIC")
}
