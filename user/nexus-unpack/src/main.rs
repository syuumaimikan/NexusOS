//! Unpacking software that was downloaded, and saying what it would still need
//! to run.
//!
//! The ask was to download a Linux installer in the browser and install it.
//! This is that, and the second half of the sentence is the honest part: it
//! unpacks, it says exactly what came out, and then it says what each program
//! inside would need before it could start. Usually that is a dynamic loader
//! belonging to a C library, which is not on this machine and cannot be built
//! here -- and naming it is worth a great deal more than mapping the segments
//! and jumping into a program that immediately reaches for a symbol table
//! nothing filled in.
//!
//! # What it is lent
//!
//! Two directories and nothing else. The one downloads go into, **read only**,
//! because an unpacker has no business editing what it was asked to read; and
//! one to unpack into, read and write. It cannot reach the disk, the network,
//! or any other folder, so an archive that is hostile can at worst fill the
//! folder it was being unpacked into -- and it cannot even leave that, because
//! every name inside it is checked before it is used.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use core::panic::PanicInfo;

use nexus_archive::{Kind, Trouble};
use nexus_user::Handle;

/// Say so and stop.
///
/// A distinct status, so that a machine reading the log can tell a program
/// that refused an archive from one that fell over reading it.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    nexus_user::log(&format!("unpack: PANIC: {info}")).ok();
    nexus_user::exit_with(101)
}

/// Where this program's allocations come from.
#[global_allocator]
static ALLOCATOR: nexus_user::heap::Allocator = nexus_user::heap::Allocator;

/// The channel to whoever started this program.
const PARENT: Handle = Handle(1);

/// How much heap.
///
/// An archive is read whole and then decompressed whole, so this has to hold
/// both at once. Twelve megabytes is a great deal by this machine's standards
/// and still smaller than plenty of real packages -- which is a limit worth
/// stating rather than discovering, so a file too large is refused by name.
const HEAP: usize = 12 * 1024 * 1024;

/// The largest archive this will read.
///
/// Half the heap, because the compressed bytes and what comes out of them are
/// both held.
const LARGEST: usize = HEAP / 2;

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

/// Whether anything went wrong, for the status this program exits with.
static FAILED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn failed(message: &str) {
    FAILED.store(true, core::sync::atomic::Ordering::Relaxed);
    nexus_user::log(message).ok();
}

fn finish() -> ! {
    if FAILED.load(core::sync::atomic::Ordering::Relaxed) {
        nexus_user::exit_with(1)
    } else {
        nexus_user::exit_with(0)
    }
}

extern "C" fn main() -> ! {
    if !nexus_user::heap::init(HEAP) {
        failed("unpack: FAILED: could not get a heap");
        finish();
    }

    // One message: the name of the file to unpack, and the two directories.
    // Both are required, and which is which is decided by position rather than
    // by what they turn out to contain -- a program that guessed would be a
    // program that could write into the downloads folder on a machine where
    // the two arrived the other way round.
    let mut buffer = [0u8; 256];
    let mut handles = [Handle(0); 2];
    let Ok(received) = nexus_user::receive(PARENT, &mut buffer, &mut handles) else {
        failed("unpack: FAILED: nothing arrived to unpack");
        finish();
    };
    if received.handles != 2 {
        failed("unpack: FAILED: the downloads and destination folders did not both arrive");
        finish();
    }
    let downloads = handles[0];
    let destination = handles[1];
    let Ok(name) = core::str::from_utf8(&buffer[..received.bytes]) else {
        failed("unpack: FAILED: the file name is not text");
        finish();
    };

    match unpack(downloads, destination, name) {
        Ok(report) => nexus_user::log(&report).ok(),
        Err(why) => {
            failed(&format!("unpack: FAILED: {name}: {why}"));
            None
        }
    };
    finish()
}

/// Read one downloaded file and write out what is inside it.
fn unpack(downloads: Handle, destination: Handle, name: &str) -> Result<String, String> {
    let archive = read_whole(downloads, name)?;

    // Where it goes: a folder named after the file, so two packages cannot
    // overwrite each other's files and so what came from where stays visible
    // after the fact.
    let folder = stem(name);
    let root = make_directory(destination, &folder)?;

    let (files, description) = contents(&archive, name)?;
    if files.is_empty() {
        return Err(String::from("there is nothing in it"));
    }

    let mut written = 0usize;
    let mut bytes = 0usize;
    let mut needs = Vec::new();
    for (path, kind, data) in &files {
        match kind {
            Kind::Directory => {
                make_directory(root, path)?;
            }
            // Recorded by the archive reader and not followed. This system has
            // no links, and inventing one by copying the target turns one file
            // into two that then disagree.
            Kind::Link => continue,
            Kind::File => {
                if let Some(parent) = path.rsplit_once('/').map(|(head, _)| head) {
                    make_directory(root, parent)?;
                }
                write_file(root, path, data)?;
                written += 1;
                bytes += data.len();

                // And the part that matters more than any of the above: if this
                // is a program, what would it need before it could run here.
                if let Ok(Some(loader)) = nexus_abi::elf::interpreter(data) {
                    needs.push((path.clone(), String::from(loader)));
                }
            }
        }
    }

    nexus_user::log(&format!(
        "unpack: {name} is {description}; {written} file(s), {bytes} bytes into {folder}/"
    ))
    .ok();

    // Said once per distinct loader rather than once per program, because a
    // package holding forty binaries needs the same one forty times and a
    // person reading forty identical lines learns nothing from the last
    // thirty-nine.
    let mut said: Vec<&str> = Vec::new();
    for (program, loader) in &needs {
        if said.contains(&loader.as_str()) {
            continue;
        }
        said.push(loader);
        nexus_user::log(&format!(
            "unpack: {program} cannot run here: it asks for {loader}, which this machine does not have"
        ))
        .ok();
    }
    if !needs.is_empty() {
        // Once, plainly, rather than implied by the lines above. A folder full
        // of files that will not start is a worse outcome than an error, if
        // nobody says so.
        nexus_user::log(
            "unpack: those programs are dynamically linked against a C library this system has \
             none of; the files are unpacked and are not runnable",
        )
        .ok();
    }

    Ok(format!(
        "unpack: {folder}/ holds {written} file(s), {} of which need a loader that is not here",
        needs.len()
    ))
}

/// One thing that came out of an archive: where it goes, what it is, and its
/// bytes.
///
/// A name rather than the triple written out, because it appears in three
/// signatures and the shape of it says nothing the name does not.
type Unpacked = (String, Kind, Vec<u8>);

/// What kind of archive it is, and everything inside it.
///
/// Sniffed from the bytes rather than taken from the name. A file downloaded
/// from somewhere else is named by somebody else, and a `.tar.gz` that is
/// really a Debian package is an ordinary mistake rather than an attack.
fn contents(archive: &[u8], name: &str) -> Result<(Vec<Unpacked>, String), String> {
    if archive.starts_with(b"!<arch>\n") {
        let package = nexus_archive::deb(archive, LARGEST).map_err(|trouble| match trouble {
            Trouble::NotThisFormat => match nexus_archive::compression(archive) {
                // The common case by far, and the refusal has to name the
                // compression: "compressed with xz, which this system does not
                // read" is something a person can act on, and "could not
                // install" is not.
                Some(how) if how != "gzip" && how != "none" => format!(
                    "a Debian package compressed with {how}, which this system does not read"
                ),
                _ => String::from("not a Debian package this system can read"),
            },
            other => describe(other),
        })?;
        let what = match (package.field("Package"), package.field("Version")) {
            (Some(named), Some(version)) => format!("the Debian package {named} {version}"),
            (Some(named), None) => format!("the Debian package {named}"),
            _ => String::from("a Debian package"),
        };
        return Ok((package.files, what));
    }

    let (body, what) = if archive.starts_with(&[0x1F, 0x8B]) {
        let out = nexus_inflate::gzip(archive, LARGEST)
            .map_err(|_| String::from("the gzip in it could not be read; it may be truncated"))?;
        (out, String::from("a gzipped tar archive"))
    } else {
        (archive.to_vec(), String::from("a tar archive"))
    };

    let entries = nexus_archive::tar(&body).map_err(describe)?;
    let mut files = Vec::new();
    for entry in entries {
        let data = entry.bytes(&body).map_err(describe)?.to_vec();
        files.push((entry.name, entry.kind, data));
    }
    let _ = name;
    Ok((files, what))
}

/// A reason, in words a person can act on.
fn describe(trouble: Trouble) -> String {
    match trouble {
        Trouble::NotThisFormat => String::from("not a format this system reads"),
        Trouble::Truncated => String::from("it stops in the middle; the download did not finish"),
        Trouble::Corrupt => String::from("a header in it does not make sense"),
        Trouble::UnsafeName => String::from(
            "it holds a name that would write outside the folder it is being unpacked into, so \
             none of it was unpacked",
        ),
        Trouble::Compressed(_) => String::from("the compressed part of it could not be read"),
    }
}

/// The part of a file name to use as a folder.
fn stem(name: &str) -> String {
    let base = name.rsplit_once('/').map_or(name, |(_, tail)| tail);
    let mut out = base;
    for suffix in [".deb", ".tar.gz", ".tgz", ".tar"] {
        if let Some(head) = out.strip_suffix(suffix) {
            out = head;
            break;
        }
    }
    if out.is_empty() {
        String::from("unpacked")
    } else {
        String::from(out)
    }
}

/// Read a whole file out of a directory.
fn read_whole(directory: Handle, name: &str) -> Result<Vec<u8>, String> {
    let file = nexus_api::path::open(directory, name)
        .map_err(|_| format!("there is no {name} in the downloads folder"))?;
    let size = nexus_user::size(file).unwrap_or(0) as usize;
    if size == 0 {
        nexus_user::close(file).ok();
        return Err(String::from("it is empty"));
    }
    if size > LARGEST {
        nexus_user::close(file).ok();
        return Err(format!(
            "it is {size} bytes, and this unpacker reads at most {LARGEST}"
        ));
    }
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read(file, &mut bytes).map_err(|_| String::from("it could not be read"));
    nexus_user::close(file).ok();
    let read = read?;
    bytes.truncate(read);
    Ok(bytes)
}

/// Make a directory, and every directory above it, and give back a handle.
///
/// Each component is opened if it is there and made if it is not, because an
/// archive lists `usr/bin/x` without necessarily listing `usr/bin` first.
fn make_directory(root: Handle, path: &str) -> Result<Handle, String> {
    let mut at = nexus_user::duplicate(
        root,
        nexus_user::rights::READ | nexus_user::rights::WRITE | nexus_user::rights::TRANSFER,
    )
    .map_err(|_| String::from("could not reach the destination folder"))?;

    for part in path.split('/').filter(|part| !part.is_empty()) {
        let next = match nexus_user::open(at, part) {
            Ok(handle) => handle,
            Err(_) => nexus_user::create(at, part, nexus_user::Kind::Directory)
                .map_err(|_| format!("could not make the folder {part}"))?,
        };
        nexus_user::close(at).ok();
        at = next;
    }
    Ok(at)
}

/// Write one file, making the folders above it first.
fn write_file(root: Handle, path: &str, data: &[u8]) -> Result<(), String> {
    let (folder, name) = match path.rsplit_once('/') {
        Some((head, tail)) => (make_directory(root, head)?, tail),
        None => (
            nexus_user::duplicate(
                root,
                nexus_user::rights::READ | nexus_user::rights::WRITE | nexus_user::rights::TRANSFER,
            )
            .map_err(|_| String::from("could not reach the destination folder"))?,
            path,
        ),
    };

    let file = match nexus_user::open(folder, name) {
        Ok(handle) => handle,
        Err(_) => nexus_user::create(folder, name, nexus_user::Kind::File)
            .map_err(|_| format!("could not make {path}"))?,
    };
    let written = nexus_user::write(file, data);
    nexus_user::close(file).ok();
    nexus_user::close(folder).ok();
    match written {
        Ok(count) if count == data.len() => Ok(()),
        Ok(count) => Err(format!(
            "only {count} of {} bytes of {path} were written",
            data.len()
        )),
        Err(_) => Err(format!("{path} could not be written")),
    }
}
