//! `update`: brings the machine up to date, or says why it did not.
//!
//! The counterpart to `install`. That program puts one package on the disk
//! because somebody named it; this one looks at every package the machine can
//! see, works out which of them are newer than what is already there, and
//! installs those. What it does *not* have is a different idea of what
//! installing means: it calls the same code, with the same verification and the
//! same rollback, because a second implementation of "write this file and put
//! it back if anything goes wrong" is a second one to get wrong.
//!
//! # Where updates come from
//!
//! `PKG/` on the store. That directory is seeded from the boot image, so a
//! machine is updated by being given a new image -- which is what updating an
//! operating system is, once the download is over. Fetching them across the
//! network belongs above this and not inside it: what arrives has to be checked
//! the same way regardless of how it got here, and that check is here.
//!
//! # What makes an update safe to install
//!
//! Three things, in this order, and a package that fails any of them is not
//! installed:
//!
//! 1. It is signed by the key this machine trusts. An unsigned package and a
//!    package signed by somebody else are both refused, and refused in
//!    different words, because they are different things to have found.
//! 2. Its contents hash to what its header says they do.
//! 3. Its version is *higher* than the version installed under the same name.
//!
//! The third is what makes this an update rather than a reinstall, and it is
//! the reason a downgrade is refused rather than performed: a machine handed an
//! old image should not quietly go backwards.
//!
//! # What it leaves behind
//!
//! Two files in the settings directory. `installed.txt` is what is on the
//! machine, by name and version -- the record every later run compares against.
//! `updates.txt` is what the last check found, which is what the desktop reads
//! in order to say "three updates available" without doing any of this itself.
//!
//! # Automatic, or asked
//!
//! `system.updates` decides. `automatic` installs what it finds, which is the
//! default because a machine that waits to be asked is a machine that stays
//! unpatched. `ask` does the whole check and writes it down and installs
//! nothing, for somebody who wants to choose.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;
use core::panic::PanicInfo;

use nexus_config::{key, Settings};
use nexus_install::Trouble;
use nexus_pkg::Package;
use nexus_update::{judge, Installed, Verdict, Version};
use nexus_user::{Handle, Kind};

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to whoever started this program.
const PARENT: Handle = Handle(1);

/// How much heap: several packages, their files, and the copies a rollback
/// keeps.
const HEAP: usize = 2 * 1024 * 1024;

/// The directory updates arrive in.
const PACKAGES: &str = "PKG";

/// What the settings directory holds, beyond the settings themselves.
const INSTALLED_NAME: &str = "installed.txt";
const AVAILABLE_NAME: &str = "updates.txt";
const SETTINGS_NAME: &str = "settings.txt";

/// Longest file this program will read out of the settings directory.
const SETTINGS_MAX: usize = 16 * 1024;

/// Most packages one run will consider.
///
/// A bound rather than a trust. The directory is on a disk this program did not
/// make, and a run that walked ten thousand entries would be a boot that never
/// finished.
const MAX_PACKAGES: usize = 64;

#[unsafe(naked)]
#[no_mangle]
#[link_section = ".text.entry"]
pub extern "C" fn _start() -> ! {
    core::arch::naked_asm!(
        "xor rbp, rbp",
        "call {main}",
        "ud2",
        main = sym main,
    )
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(HEAP) {
        failed("update: FAILED: could not get a heap");
        finish();
    }

    // Two handles: the filesystem to install into, and the directory the record
    // of what is installed lives in. Both are authority and neither is
    // ambient -- without them this program cannot read a package, install one,
    // or remember that it did.
    let mut buffer = [0u8; 64];
    let mut handles = [Handle(0); 2];
    let Ok(received) = nexus_user::receive(PARENT, &mut buffer, &mut handles) else {
        failed("update: FAILED: nothing arrived to work on");
        finish();
    };
    if received.handles != 2 {
        failed("update: FAILED: no filesystem and settings came with the message");
        finish();
    }
    let root = handles[0];
    let settings_directory = handles[1];

    let settings = read_settings(settings_directory);
    let automatic = nexus_update::installs_by_itself(settings.get(key::UPDATES));

    match run(root, settings_directory, automatic) {
        Ok(report) => nexus_user::log(&report).ok(),
        Err(why) => {
            failed(&format!("update: FAILED: {why}"));
            None
        }
    };
    finish()
}

/// What one package in the directory turned out to be.
struct Candidate {
    /// The file it came from, so it can be installed by name.
    file: String,
    name: String,
    version: Version,
    verdict: Verdict,
}

/// Look at everything available, and act on it.
fn run(root: Handle, settings_directory: Handle, automatic: bool) -> Result<String, String> {
    let mut installed = read_installed(settings_directory);
    nexus_user::log(&format!(
        "update: {} package(s) on record, installing {}",
        installed.len(),
        if automatic {
            "what is newer"
        } else {
            "nothing until asked"
        }
    ))
    .ok();

    let (candidates, turned_down) = look(root, &installed)?;
    if candidates.is_empty() {
        nexus_user::log("update: nothing to look at; the machine has no packages").ok();
    }

    let worth_doing: Vec<&Candidate> = candidates
        .iter()
        .filter(|candidate| candidate.verdict.worth_doing())
        .collect();

    // Written before anything is installed, not after. What this file says is
    // "here is what was found", and a machine that crashed half way through an
    // install should still be able to say what it had found -- which is also
    // what somebody comes back to when the answer is `ask`.
    write_available(settings_directory, &candidates, automatic, 0);

    if !automatic {
        let report = format!(
            "update: {} of {} package(s) are newer; not installing, because this machine asks first",
            worth_doing.len(),
            candidates.len()
        );
        remember_when(settings_directory);
        return Ok(report);
    }

    let mut done = 0usize;
    // Counted from the check as well as from the install. A package refused
    // before it was ever a candidate -- forged, unreadable, not a package --
    // is still a package this machine turned down, and a summary that said
    // "0 refused" underneath a line naming one would be a summary nobody
    // could trust.
    let mut refused = turned_down;
    for candidate in &worth_doing {
        match nexus_install::install(root, &candidate.file) {
            Ok(line) => {
                nexus_user::log(&line).ok();
                nexus_user::log(&format!(
                    "update: {} is now at {}",
                    candidate.name,
                    candidate.version.to_text()
                ))
                .ok();
                installed.set(&candidate.name, candidate.version);
                done += 1;
            }
            // A package the machine was right to turn down. Counted, said, and
            // not recorded as installed -- the record has to keep describing
            // what is actually on the disk.
            Err(Trouble::Refused(why)) => {
                refused += 1;
                nexus_user::log(&format!("update: refused {}: {why}", candidate.file)).ok();
            }
            // A package that should have installed and did not. The rollback
            // has already put things back; what is left is to say so and stop,
            // because carrying on would be installing the rest of an update
            // whose first half is missing.
            Err(Trouble::Broken(why)) => {
                write_installed(settings_directory, &installed)?;
                return Err(format!("{}: {why}", candidate.file));
            }
        }
    }

    write_installed(settings_directory, &installed)?;
    write_available(settings_directory, &candidates, automatic, done);
    remember_when(settings_directory);

    if done == 0 {
        Ok(format!(
            "update: nothing to do; {} package(s) checked, {refused} refused, the machine is up to date",
            candidates.len()
        ))
    } else {
        Ok(format!(
            "update: installed {done} update(s) of {} package(s) checked, {refused} refused",
            candidates.len()
        ))
    }
}

/// Read every package in the directory and decide what it is.
///
/// A file that is not a package, or is one this machine will not trust, is
/// skipped with a line saying so rather than stopping the run. One bad file in
/// a directory is not a reason to leave the machine unpatched.
fn look(root: Handle, installed: &Installed) -> Result<(Vec<Candidate>, usize), String> {
    let directory = match nexus_user::open(root, PACKAGES) {
        Ok(handle) => handle,
        // No package directory at all is an ordinary state for a machine that
        // was installed without one, not a failure.
        Err(_) => return Ok((Vec::new(), 0)),
    };

    let mut packed = [0u8; 4096];
    let length = nexus_user::list(directory, &mut packed)
        .map_err(|error| format!("cannot read {PACKAGES}: {error:?}"))?;
    let names: Vec<String> = nexus_user::entries(&packed[..length])
        .filter(|entry| entry.kind == Kind::File)
        .map(|entry| entry.name.to_string())
        .take(MAX_PACKAGES)
        .collect();
    nexus_user::close(directory).ok();

    let mut candidates = Vec::new();
    let mut turned_down = 0usize;
    for name in names {
        let path = format!("{PACKAGES}/{name}");
        let Ok(bytes) = nexus_install::read_whole(root, &path) else {
            turned_down += 1;
            nexus_user::log(&format!("update: skipped {path}: it would not read")).ok();
            continue;
        };
        let package = match Package::open(&bytes) {
            Ok(package) => package,
            Err(error) => {
                turned_down += 1;
                nexus_user::log(&format!("update: skipped {path}: {error}")).ok();
                continue;
            }
        };
        // Checked here as well as inside the install, because this is what
        // decides whether the machine *reports* an update as available. A
        // forged package that showed up in "2 updates available" and then
        // refused to install would be a machine lying to whoever read the
        // number.
        if let Err(error) = package.verify() {
            turned_down += 1;
            nexus_user::log(&format!("update: refused {path}: {error}")).ok();
            continue;
        }
        let (Ok(package_name), Ok(release)) = (package.name(), package.release()) else {
            turned_down += 1;
            nexus_user::log(&format!(
                "update: skipped {path}: it will not say what it is"
            ))
            .ok();
            continue;
        };
        let Some(version) = Version::parse(release) else {
            turned_down += 1;
            nexus_user::log(&format!(
                "update: skipped {path}: \"{release}\" is not a version"
            ))
            .ok();
            continue;
        };

        candidates.push(Candidate {
            file: path,
            name: package_name.to_string(),
            version,
            verdict: judge(installed.get(package_name), version),
        });
    }

    // One release per name, the highest. A directory holds two the moment an
    // update arrives beside the release it supersedes, and installing both
    // would leave the machine at whichever the filesystem happened to list
    // last. What is dropped here is said, because a package that is on the
    // disk and was not considered is a thing somebody will come looking for.
    let offered: Vec<(&str, Version)> = candidates
        .iter()
        .map(|candidate| (candidate.name.as_str(), candidate.version))
        .collect();
    let keep = nexus_update::newest_of_each(&offered);
    let mut chosen = Vec::with_capacity(keep.len());
    for (index, candidate) in candidates.into_iter().enumerate() {
        if keep.contains(&index) {
            nexus_user::log(&format!(
                "update: {} {} is {}",
                candidate.name,
                candidate.version.to_text(),
                candidate.verdict.describe()
            ))
            .ok();
            chosen.push(candidate);
        } else {
            nexus_user::log(&format!(
                "update: {} {} is superseded by another file on this disk",
                candidate.name,
                candidate.version.to_text()
            ))
            .ok();
        }
    }
    Ok((chosen, turned_down))
}

/// What is installed on this machine now.
fn read_installed(directory: Handle) -> Installed {
    match read_text(directory, INSTALLED_NAME) {
        Some(text) => Installed::parse(&text),
        None => Installed::new(),
    }
}

/// Write it back.
fn write_installed(directory: Handle, installed: &Installed) -> Result<(), String> {
    write_text(directory, INSTALLED_NAME, &installed.to_text())
}

/// Write down what this run found, for whoever shows it.
///
/// Best effort. A machine that could not write this file has still done the
/// thing that matters -- installing what was newer -- and turning a failure to
/// write a status file into a failed update would be getting the priorities the
/// wrong way round.
fn write_available(directory: Handle, candidates: &[Candidate], automatic: bool, done: usize) {
    let mut text = String::from(
        "# What the last check found.
",
    );
    text.push_str(&format!(
        "mode = {}
",
        if automatic { "automatic" } else { "ask" }
    ));
    let waiting = candidates
        .iter()
        .filter(|candidate| candidate.verdict.worth_doing())
        .count();
    // What is still waiting, which is what was found less what has since been
    // installed. The desktop reads this number and nothing else, so it has to
    // mean "how many updates is this machine still missing" at every moment
    // this file is written -- including half way through a run.
    text.push_str(&format!(
        "pending = {}
",
        waiting.saturating_sub(done)
    ));
    text.push_str(&format!(
        "installed = {done}
"
    ));
    text.push_str(&format!(
        "checked = {}
",
        candidates.len()
    ));
    for candidate in candidates {
        text.push_str(&format!(
            "{} = {} {}\n",
            candidate.name,
            candidate.version.to_text(),
            candidate.verdict.describe()
        ));
    }
    if write_text(directory, AVAILABLE_NAME, &text).is_err() {
        nexus_user::log("update: could not write down what it found").ok();
    }
}

/// Note in the settings when the machine last looked.
fn remember_when(directory: Handle) {
    let Ok(seconds) = nexus_user::now() else {
        return;
    };
    let Some(text) = read_text(directory, SETTINGS_NAME) else {
        return;
    };
    let mut settings = Settings::parse(&text);
    settings.set(key::UPDATES_CHECKED, &format!("{seconds}"));
    if write_text(directory, SETTINGS_NAME, &settings.to_text()).is_err() {
        nexus_user::log("update: could not note when it last looked").ok();
    }
}

/// The settings, or an empty set if they will not read.
fn read_settings(directory: Handle) -> Settings {
    match read_text(directory, SETTINGS_NAME) {
        Some(text) => Settings::parse(&text),
        None => Settings::new(),
    }
}

/// A whole text file out of a directory, if it is there and readable.
fn read_text(directory: Handle, name: &str) -> Option<String> {
    let file = nexus_user::open(directory, name).ok()?;
    let size = nexus_user::size(file).unwrap_or(0).min(SETTINGS_MAX);
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read_at(file, 0, &mut bytes).unwrap_or(0);
    nexus_user::close(file).ok();
    bytes.truncate(read);
    String::from_utf8(bytes).ok()
}

/// Replace a text file with this content.
fn write_text(directory: Handle, name: &str, text: &str) -> Result<(), String> {
    // Removed first: the filesystem has no truncate, so a shorter file written
    // over a longer one would keep the old ending.
    match nexus_user::remove(directory, name) {
        Ok(()) | Err(nexus_user::Error::NotFound) => {}
        Err(error) => return Err(format!("{name}: cannot replace: {error:?}")),
    }
    let file = nexus_user::create(directory, name, Kind::File)
        .map_err(|error| format!("{name}: cannot create: {error:?}"))?;
    let contents = text.as_bytes();
    let mut written = 0;
    while written < contents.len() {
        match nexus_user::write_at(file, written as u64, &contents[written..]) {
            Ok(0) | Err(_) => {
                nexus_user::close(file).ok();
                return Err(format!("{name}: the write stopped at {written} bytes"));
            }
            Ok(count) => written += count,
        }
    }
    nexus_user::close(file).ok();
    Ok(())
}

/// Whether anything has gone wrong, for the status this program exits with.
static FAILED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Say what happened and stop. Never returns.
fn finish() -> ! {
    if FAILED.load(core::sync::atomic::Ordering::Relaxed) {
        nexus_user::exit_with(1)
    } else {
        nexus_user::exit()
    }
}

/// Log a failure and remember it.
fn failed(what: &str) {
    FAILED.store(true, core::sync::atomic::Ordering::Relaxed);
    nexus_user::log(what).ok();
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    nexus_user::log("update: PANIC").ok();
    nexus_user::exit_with(2)
}
