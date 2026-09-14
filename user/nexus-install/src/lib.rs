//! The part of installing a package that is not a program.
//!
//! Split out from the program it began as, because there are now two callers:
//! `install`, which puts one package on the filesystem because somebody asked,
//! and `update`, which puts several on because they are newer than what is
//! there. Two copies of a rollback is one copy that is wrong.
//!
//! # What installing means here
//!
//! Read the package. Check it. Write it. Check what was written. If any of
//! those fails, put back exactly what was there before.
//!
//! The order matters. The whole package is verified before a single byte is
//! written, so a package that was corrupted in transit never touches the
//! filesystem. Each file is then verified *again* after it has been written and
//! read back, because the thing being guarded against at that point is not a
//! bad package -- it is a bad disk.
//!
//! # Rolling back
//!
//! Every file that is about to be overwritten is copied to a name beside it
//! first, and every file that is created is remembered. If anything goes wrong,
//! the copies go back and the new files are removed. What that buys is the
//! guarantee that matters: an install either happened or did not, and there is
//! no third state where half a package is on the disk and the machine will not
//! boot.
//!
//! It is not atomic against losing power -- that needs the filesystem's journal
//! to cover a whole sequence of operations, and it does not yet. What it is is
//! atomic against the install failing, which is what actually happens.

#![no_std]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use nexus_pkg::Package;
use nexus_user::{Handle, Kind};

include!(concat!(env!("OUT_DIR"), "/trusted_key.rs"));

/// What a saved copy of a replaced file is called while an install is running.
const SAVED: &str = ".OLD";

/// What went wrong, and whose fault it was.
///
/// The distinction is not pedantry. One of these means the package should not
/// be installed and the machine is fine; the other means the machine could not
/// install a package it had no reason to reject. Reporting them the same way
/// would make a correct refusal look like a fault.
pub enum Trouble {
    /// The package is not acceptable.
    Refused(String),
    /// Something that should have worked did not.
    Broken(String),
}

/// Read a package, check it, write it, and check what was written.
pub fn install(root: Handle, path: &str) -> Result<String, Trouble> {
    let bytes = read_whole(root, path).map_err(Trouble::Broken)?;

    let package =
        Package::open(&bytes).map_err(|error| Trouble::Refused(alloc::format!("{error}")))?;

    // The signature first, and then everything the package says about itself.
    // An unsigned package is refused as unsigned rather than as broken: nobody
    // claimed anything about it, which is a different thing to report than a
    // claim that does not hold.
    package
        .verify_signed_by(&TRUSTED_KEY)
        .map_err(|error| Trouble::Refused(alloc::format!("{error}")))?;

    let name = package
        .name()
        .map_err(|error| Trouble::Refused(alloc::format!("{error}")))?;
    let release = package
        .release()
        .map_err(|error| Trouble::Refused(alloc::format!("{error}")))?;
    nexus_user::log(&format!(
        "install: {name} {release}, {} files, signed by the key this machine trusts",
        package.len()
    ))
    .ok();

    // What has been done so far, so that it can be undone. Kept in the order it
    // happened, because a rollback has to run backwards through it.
    let mut done: Vec<Step> = Vec::new();

    for index in 0..package.len() {
        let Some(entry) = package.entry(index) else {
            break;
        };
        let target = entry
            .name()
            .map_err(|error| Trouble::Refused(alloc::format!("{error}")))?;
        let contents = package.contents(&entry).ok_or_else(|| {
            Trouble::Refused(String::from("an entry runs past the end of the package"))
        })?;

        if let Err(why) = place(root, target, contents, &mut done) {
            // Everything back the way it was, and then the failure. The order
            // is deliberate: a rollback that reported before it had finished
            // would be a machine claiming to be consistent while it is not.
            let undone = rewind(root, &mut done);
            return Err(Trouble::Broken(format!(
                "{why}; rolled back {undone} changes"
            )));
        }
    }

    let files = done.iter().filter(|step| !step.is_directory).count();
    // The saved copies go only now, when there is nothing left that could send
    // this back to them. Tidying earlier would be an install that cannot be
    // rolled back from its last step.
    tidy(&done);
    Ok(format!("install: wrote {files} files for {name} {release}"))
}

/// One thing that was done and may have to be undone.
struct Step {
    /// The directory it happened in, and what it was called.
    directory: Handle,
    name: String,
    /// Whether a copy of what was there is sitting beside it.
    saved: bool,
    /// Whether this made a directory rather than a file.
    is_directory: bool,
}

/// Write one file, saving whatever was there.
fn place(root: Handle, target: &str, contents: &[u8], done: &mut Vec<Step>) -> Result<(), String> {
    // A path is at most one directory deep. Deeper would need this to make
    // every level and remember every level for the rollback, and nothing this
    // installs is deeper -- a package that wanted to be would be refused here
    // rather than half-installed.
    let (directory, file) = match target.split_once('/') {
        Some((directory, rest)) => {
            if rest.contains('/') {
                return Err(format!("{target}: paths may be at most one level deep"));
            }
            (Some(directory), rest)
        }
        None => (None, target),
    };

    let mut where_to = root;
    if let Some(directory) = directory {
        where_to = match nexus_user::open(root, directory) {
            Ok(handle) => handle,
            Err(_) => {
                let handle = nexus_user::create(root, directory, Kind::Directory)
                    .map_err(|error| format!("{directory}: cannot create: {error:?}"))?;
                done.push(Step {
                    directory: root,
                    name: String::from(directory),
                    saved: false,
                    is_directory: true,
                });
                handle
            }
        };
    }

    // Save what is there, if anything is. A copy under another name rather than
    // a rename, because there is no rename: the filesystem has create, write
    // and remove, and a copy built from those is what a rename would be.
    let saved = match nexus_user::open(where_to, file) {
        Ok(existing) => {
            let old = read_handle(existing)?;
            nexus_user::close(existing)
                .map_err(|error| format!("{file}: cannot let go of the old copy: {error:?}"))?;
            let backup = format!("{file}{SAVED}");
            write_file(where_to, &backup, &old)?;
            true
        }
        Err(_) => false,
    };

    write_file(where_to, file, contents)?;

    // And read it back. What is being checked here is not the package -- that
    // was checked before any of this started -- it is the disk, and the only
    // way to check a disk is to ask it for what you just gave it.
    let written = read_named(where_to, file)?;
    if !nexus_pkg::digests_equal(&nexus_pkg::digest(&written), &nexus_pkg::digest(contents)) {
        done.push(Step {
            directory: where_to,
            name: String::from(file),
            saved,
            is_directory: false,
        });
        return Err(format!("{target}: what was written is not what was sent"));
    }

    done.push(Step {
        directory: where_to,
        name: String::from(file),
        saved,
        is_directory: false,
    });
    Ok(())
}

/// Put everything back, and say how many changes were undone.
fn rewind(root: Handle, done: &mut Vec<Step>) -> usize {
    let mut undone = 0;
    while let Some(step) = done.pop() {
        if step.is_directory {
            // Only if it is empty, which after the files above it have gone it
            // will be. A directory that will not go was not made by this.
            if nexus_user::remove(root, &step.name).is_ok() {
                undone += 1;
            }
            continue;
        }

        if step.saved {
            let backup = format!("{}{SAVED}", step.name);
            if let Ok(old) = read_named(step.directory, &backup) {
                if write_file(step.directory, &step.name, &old).is_ok() {
                    nexus_user::remove(step.directory, &backup).ok();
                    undone += 1;
                }
            }
        } else if nexus_user::remove(step.directory, &step.name).is_ok() {
            undone += 1;
        }
    }
    undone
}

/// Throw away the saved copies, now that the install has worked.
fn tidy(done: &[Step]) {
    for step in done {
        if step.saved {
            nexus_user::remove(step.directory, &format!("{}{SAVED}", step.name)).ok();
        }
    }
}

/// Write a file, replacing whatever was there.
fn write_file(directory: Handle, name: &str, contents: &[u8]) -> Result<(), String> {
    // Removed first, because writing over a longer file would leave its tail
    // behind: the filesystem has no truncate, so a shorter file written into a
    // longer one is a file with somebody else's ending.
    //
    // A file that was not there is the ordinary case and not a failure. Any
    // other refusal is: it means the old file is still in the way, and the
    // create below would fail with `Exists` and blame the wrong thing. Which is
    // what it did -- "cannot create: Exists" for a file this had just asked to
    // remove, with no hint that the removal was what went wrong.
    match nexus_user::remove(directory, name) {
        Ok(()) | Err(nexus_user::Error::NotFound) => {}
        Err(error) => return Err(format!("{name}: cannot replace what is there: {error:?}")),
    }
    let file = nexus_user::create(directory, name, Kind::File)
        .map_err(|error| format!("{name}: cannot create: {error:?}"))?;

    let mut written = 0;
    while written < contents.len() {
        match nexus_user::write_at(file, written as u64, &contents[written..]) {
            Ok(0) => {
                nexus_user::close(file).ok();
                return Err(format!("{name}: the write stopped at {written} bytes"));
            }
            Ok(count) => written += count,
            Err(error) => {
                nexus_user::close(file).ok();
                return Err(format!("{name}: cannot write: {error:?}"));
            }
        }
    }
    nexus_user::close(file).ok();
    Ok(())
}

/// Read a whole file by name.
fn read_named(directory: Handle, name: &str) -> Result<Vec<u8>, String> {
    let file = nexus_user::open(directory, name)
        .map_err(|error| format!("{name}: cannot open: {error:?}"))?;
    let contents = read_handle(file);
    nexus_user::close(file).ok();
    contents
}

/// Read a whole file from a handle.
fn read_handle(file: Handle) -> Result<Vec<u8>, String> {
    let size = nexus_user::size(file).map_err(|error| format!("cannot size: {error:?}"))?;
    let mut contents = vec![0u8; size];
    let mut read = 0;
    while read < size {
        match nexus_user::read_at(file, read as u64, &mut contents[read..]) {
            Ok(0) => break,
            Ok(count) => read += count,
            Err(error) => return Err(format!("cannot read: {error:?}")),
        }
    }
    contents.truncate(read);
    Ok(contents)
}

/// Read a whole file at a path that may name a directory first.
pub fn read_whole(root: Handle, path: &str) -> Result<Vec<u8>, String> {
    match path.split_once('/') {
        Some((directory, name)) => {
            let handle = nexus_user::open(root, directory)
                .map_err(|error| format!("{directory}: cannot open: {error:?}"))?;
            let contents = read_named(handle, name);
            nexus_user::close(handle).ok();
            contents
        }
        None => read_named(root, path),
    }
}
