//! Native package manager. Handle 4 is the installation root; handle 5 is the
//! source directory. Both are explicitly lent by the terminal.
#![no_std]
#![no_main]
extern crate alloc;
use alloc::format;
use core::panic::PanicInfo;
use nexus_install::{Trouble, TRUSTED_KEY};
use nexus_pkg::Package;
use nexus_user::Handle;

#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;
const ROOT: Handle = Handle(4);
const SOURCE: Handle = Handle(5);

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!("xor rbp, rbp", "call {main}", "ud2", main = sym main);
}
extern "C" fn main() -> ! {
    if !nexus_user::heap::init(8 * 1024 * 1024) {
        nexus_user::exit_with(101);
    }
    let arguments = nexus_api::arguments();
    let mut words = arguments.split_whitespace();
    let command = words.next().unwrap_or("help");
    let path = words.next();
    let status = if words.next().is_some() {
        say("usage: npkg list | info PATH | check PATH | install PATH");
        2
    } else {
        match (command, path) {
            ("help", None) => {
                say("npkg: signed NexusOS package manager");
                say("npkg list | info PATH | check PATH | install PATH");
                say("Paths are relative to the current directory; installs use the lent system root.");
                0
            }
            ("list", None) => match nexus_install::installed(ROOT) {
                Ok(record) => {
                    say(&record.to_text());
                    0
                }
                Err(why) => {
                    say(&why);
                    2
                }
            },
            ("info" | "check" | "install", Some(path)) => package(command, path),
            _ => {
                say("usage: npkg list | info PATH | check PATH | install PATH");
                2
            }
        }
    };
    nexus_user::exit_with(status)
}
fn package(command: &str, path: &str) -> u32 {
    let bytes = match nexus_install::read_whole(SOURCE, path) {
        Ok(bytes) => bytes,
        Err(why) => {
            say(&why);
            return 2;
        }
    };
    let package = match Package::open(&bytes) {
        Ok(package) => package,
        Err(why) => {
            say(&format!("refused: {why}"));
            return 1;
        }
    };
    if let Err(why) = package.verify_signed_by(&TRUSTED_KEY) {
        say(&format!("refused: {why}"));
        return 1;
    }
    if let Err(why) = nexus_pkg::transaction::validate(&package) {
        say(&format!("refused: {why}"));
        return 1;
    }
    let app = match package.application() {
        Ok(app) => app,
        Err(why) => {
            say(&format!("refused application metadata: {why}"));
            return 1;
        }
    };
    let name = package.name().unwrap_or("?");
    let version = package.release().unwrap_or("?");
    say(&format!(
        "{name} {version}: {} files, trusted signature",
        package.len()
    ));
    let platform = app.map(|a| a.platform);
    if let Some(app) = app {
        say(&format!(
            "platform={} arch={} entry={}",
            app.platform.name(),
            app.architecture.name(),
            app.entry
        ));
    }
    if command == "info" {
        for i in 0..package.len() {
            if let Some(entry) = package.entry(i) {
                say(&format!(
                    "{} ({} bytes)",
                    entry.name().unwrap_or("?"),
                    entry.length
                ));
            }
        }
    }
    if command != "install" {
        return 0;
    }
    match nexus_install::install_recorded_bytes(ROOT, &bytes) {
        Ok(report) => {
            say(&report);
            say("installed and recorded");
            if platform == Some(nexus_pkg::manifest::Platform::Windows) {
                say("Windows payload stored; Win32 execution is not available yet.");
            } else if platform == Some(nexus_pkg::manifest::Platform::Linux) {
                say("Linux execution uses the compatibility layer; required libraries and syscalls must be supported.");
            }
            0
        }
        Err(Trouble::Refused(why)) => {
            say(&format!("refused: {why}"));
            1
        }
        Err(Trouble::Broken(why)) => {
            say(&format!("FAILED: {why}"));
            2
        }
    }
}
fn say(text: &str) {
    nexus_user::log(&format!("npkg: {text}")).ok();
    nexus_user::println(text).ok();
}
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    nexus_user::log(&format!("npkg: PANIC: {info}")).ok();
    nexus_user::exit_with(101)
}
