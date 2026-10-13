//! Diagnostic only: launch the genuine Valve ELF32 bootstrap and record its ending.
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

fn stop(message: &str, status: u32) -> ! {
    nexus_user::log(message).ok();
    nexus_user::exit_with(status)
}

fn prepare_links() -> Result<(), nexus_user::Error> {
    // The official pinned steam.sh creates these links to STEAMROOT/STEAMDATA.
    // This diagnostic supplies that layout without executing a host shell or
    // claiming that Steam's complete installer/launcher already runs here.
    let root = nexus_user::open(Handle(2), "linux")?;
    let home = nexus_user::create(root, "root", nexus_user::Kind::Directory)
        .or_else(|_| nexus_user::open(root, "root"))?;
    let config = nexus_user::create(home, ".steam", nexus_user::Kind::Directory)
        .or_else(|_| nexus_user::open(home, ".steam"))?;
    for (name, target) in [
        ("steam", "/usr/lib/steam"),
        ("root", "/usr/lib/steam"),
        ("bin32", "/usr/lib/steam/ubuntu12_32"),
        ("bin64", "/usr/lib/steam/ubuntu12_64"),
        ("sdk32", "/usr/lib/steam/linux32"),
        ("sdk64", "/usr/lib/steam/linux64"),
        ("bin", "/root/.steam/bin32"),
    ] {
        let link = nexus_user::symlink(config, name, target)?;
        let mut bytes = [0u8; 128];
        if nexus_user::read_link(link, &mut bytes)? != target.len()
            || &bytes[..target.len()] != target.as_bytes()
        {
            return Err(nexus_user::Error::Filesystem);
        }
        nexus_user::close(link)?;
    }
    nexus_user::close(config)?;
    nexus_user::close(home)?;
    nexus_user::close(root)?;
    Ok(())
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(nexus_user::heap::DEFAULT_SIZE) {
        stop("steam-diagnostic: could not initialise diagnostic heap", 1);
    }
    if let Err(error) = prepare_links() {
        stop(
            &alloc::format!("steam-diagnostic: launcher-link preparation failed: {error}"),
            1,
        );
    }
    nexus_user::log("steam-diagnostic: prepared official bootstrap launcher link layout using native persistent symlinks").ok();
    nexus_user::log("steam-diagnostic: launching official Steam ELF32 bootstrap; this is not an installation acceptance test").ok();
    if nexus_user::send(Handle(1), b"linux32:/usr/lib/steam/ubuntu12_32/steam", &[]).is_err() {
        stop("steam-diagnostic: spawn service unavailable", 1);
    }
    let mut bytes = [0u8; 256];
    let mut handles = [Handle(0); 4];
    let received = nexus_user::receive(Handle(1), &mut bytes, &mut handles)
        .unwrap_or_else(|_| stop("steam-diagnostic: no spawn response", 1));
    if received.handles != 2 {
        let reply = core::str::from_utf8(&bytes[..received.bytes]).unwrap_or("invalid reply");
        stop(
            &alloc::format!("steam-diagnostic: loader refused bootstrap: {reply}"),
            1,
        );
    }
    nexus_user::close(handles[0]).ok();
    let set = nexus_user::wait_set().unwrap_or_else(|_| stop("steam-diagnostic: no wait set", 1));
    nexus_user::watch(set, handles[1], 1)
        .unwrap_or_else(|_| stop("steam-diagnostic: cannot watch completion", 1));
    match nexus_user::wait_any_until(set, &mut [0], 30_000) {
        Ok(1) => {
            let ending = nexus_user::wait(handles[1]);
            nexus_user::log(&alloc::format!(
                "steam-diagnostic: bootstrap ending {ending:?}; client sign-in is not verified"
            ))
            .ok();
            nexus_user::close(handles[1]).ok();
            nexus_user::close(set).ok();
            nexus_user::exit()
        }
        other => stop(
            &alloc::format!("steam-diagnostic: bootstrap did not complete: {other:?}"),
            1,
        ),
    }
}

#[panic_handler]
fn panic(_: &PanicInfo) -> ! {
    stop("steam-diagnostic: PANIC", 1)
}
