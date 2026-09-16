#![no_std]
//! Reading the archive formats software written for Linux arrives in.
//!
//! Three of them, and between them they cover what a browser downloads when
//! somebody goes to fetch a program: `tar`, `ar`, and the Debian package that
//! is one inside the other.
//!
//! # What this is, and what it is not
//!
//! This **unpacks** software. It does not run it, and unpacking is by a wide
//! margin the smaller half: a Debian package is an archive of an archive and
//! the format is a week of work, while the program inside it expects a C
//! library, a dynamic loader, a window system and a driver stack, each of which
//! is larger than this operating system. That distinction is the first thing in
//! this file because "it installed" and "it runs" are different sentences, and
//! a system that blurred them would be lying about the second.
//!
//! What it makes possible is real all the same. A file that arrived over the
//! network can be opened, checked, listed and written out as the tree of files
//! it describes -- with the names inside it treated as hostile, which is the
//! part that has to be right.

extern crate alloc;

#[cfg(test)]
mod fixtures;
#[cfg(test)]
mod tests;

use alloc::string::String;
use alloc::vec::Vec;

/// Why an archive could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// Not the format it was opened as.
    NotThisFormat,
    /// It ends in the middle of something.
    Truncated,
    /// A header says something impossible.
    Corrupt,
    /// A name that would write outside the directory being unpacked into.
    UnsafeName,
    /// Compressed with something this does not read.
    Compressed(nexus_inflate::Trouble),
}

/// What a member of an archive is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    /// A link of either sort. Recorded and not followed: this system has no
    /// links, and inventing one by copying the target turns one file into two
    /// that then disagree.
    Link,
}

/// One thing inside an archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its path, with leading slashes and `./` already removed.
    pub name: String,
    pub kind: Kind,
    /// Where its bytes are, and how many. A range rather than a copy, so that
    /// listing a large archive costs the headers and not the contents.
    pub at: usize,
    pub length: usize,
    /// The permission bits, for what they are worth here.
    pub mode: u32,
}

impl Entry {
    /// Its bytes, out of the archive it came from.
    ///
    /// # Errors
    ///
    /// [`Trouble::Truncated`] if the archive is shorter than its header said.
    pub fn bytes<'a>(&self, archive: &'a [u8]) -> Result<&'a [u8], Trouble> {
        archive
            .get(self.at..self.at + self.length)
            .ok_or(Trouble::Truncated)
    }
}

/// A path out of an archive, made safe to join onto a directory.
///
/// This is the part of unpacking that has to be right, which is why it is a
/// function with its own tests rather than a line inside a loop.
///
/// An archive comes from somewhere else and the names in it were chosen by
/// whoever made it. A name climbing out of the directory with `..` is the
/// oldest attack there is against a program that unpacks things, and it is
/// still shipped in real archives by accident as often as on purpose. An
/// absolute path is the same problem wearing a different hat.
///
/// So leading slashes go, `.` components go, and **any** `..` component makes
/// the whole name refused rather than resolved. Refusing is deliberate:
/// resolving `a/../b` to `b` is arithmetically correct and means a name
/// containing `..` sometimes works, which is a rule nobody can hold in their
/// head while reading the loop that uses it.
///
/// # Errors
///
/// [`Trouble::UnsafeName`] for a name with a `..` component, and for one that
/// is empty once the harmless parts are gone.
pub fn safe_name(raw: &str) -> Result<String, Trouble> {
    let mut out = String::new();
    for part in raw.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." {
            return Err(Trouble::UnsafeName);
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(part);
    }
    if out.is_empty() {
        return Err(Trouble::UnsafeName);
    }
    Ok(out)
}

// -- tar ---------------------------------------------------------------------

/// Bytes in a tar block, which is also the size of a header.
const BLOCK: usize = 512;

/// Read a tar archive's headers.
///
/// A member whose name is unsafe makes the **whole archive** refused rather
/// than being skipped. An archive containing one was made deliberately, and
/// unpacking the rest of it would be doing most of what was asked by something
/// that should not have been trusted at all.
///
/// # Errors
///
/// [`Trouble::Truncated`] for an archive ending inside a member,
/// [`Trouble::Corrupt`] for a header whose checksum or size does not parse, and
/// [`Trouble::UnsafeName`] as above.
pub fn tar(archive: &[u8]) -> Result<Vec<Entry>, Trouble> {
    let mut entries = Vec::new();
    let mut at = 0usize;
    // A long name belonging to the member after this one. GNU tar stores a name
    // longer than a hundred bytes as a member of its own, immediately before
    // the member it names.
    let mut long_name: Option<String> = None;
    // Whether the end-of-archive marker was reached. A tar ends with zero
    // blocks; running out of bytes instead means the file was cut short, and
    // without this that is indistinguishable from a tidy ending -- which is how
    // a half-finished download reads as a small but valid archive.
    let mut ended = false;

    while at + BLOCK <= archive.len() {
        let header = &archive[at..at + BLOCK];

        // Two zero blocks end the archive, and one is enough to stop on: there
        // is nothing after it but padding.
        if header.iter().all(|&byte| byte == 0) {
            ended = true;
            break;
        }
        if !checksum_matches(header) {
            return Err(Trouble::Corrupt);
        }

        let length = octal(&header[124..136]).ok_or(Trouble::Corrupt)? as usize;
        let body = at + BLOCK;
        if body + length > archive.len() {
            return Err(Trouble::Truncated);
        }

        let flag = header[156];
        // GNU long name: this member's body is the next member's name.
        if flag == b'L' {
            let raw = core::str::from_utf8(&archive[body..body + length])
                .map_err(|_| Trouble::Corrupt)?;
            long_name = Some(String::from(raw.trim_end_matches('\0')));
            at = body + round_up(length);
            continue;
        }

        // POSIX's way of saying the same thing, and the one every tar written
        // this decade uses: a member of key-value records, of which `path` is
        // the one that matters here. Without this an extended header reads as
        // an ordinary file, and an archive of one file appears to hold two.
        //
        // A *global* header sets defaults for the whole archive and is skipped
        // rather than applied: the only field it could usefully carry here is a
        // path, and one that applied to every member would name them all the
        // same thing.
        if flag == b'x' || flag == b'g' {
            if flag == b'x' {
                if let Some(path) = pax_path(&archive[body..body + length]) {
                    long_name = Some(path);
                }
            }
            at = body + round_up(length);
            continue;
        }

        let name = match long_name.take() {
            Some(name) => name,
            None => {
                // ustar splits a long name across `prefix` and `name`.
                let prefix = trimmed(&header[345..500]).map_err(|_| Trouble::Corrupt)?;
                let short = trimmed(&header[0..100]).map_err(|_| Trouble::Corrupt)?;
                if prefix.is_empty() {
                    String::from(short)
                } else {
                    let mut joined = String::from(prefix);
                    joined.push('/');
                    joined.push_str(short);
                    joined
                }
            }
        };

        let kind = match flag {
            b'5' => Kind::Directory,
            b'1' | b'2' => Kind::Link,
            // A zero byte and an ASCII zero are both an ordinary file; the
            // first is what very old archives use and it is still produced.
            _ => Kind::File,
        };

        entries.push(Entry {
            name: safe_name(&name)?,
            kind,
            at: body,
            length: if kind == Kind::File { length } else { 0 },
            mode: octal(&header[100..108]).unwrap_or(0o644) as u32,
        });

        at = body + round_up(length);
    }

    if !ended {
        return Err(Trouble::Truncated);
    }
    Ok(entries)
}

/// The `path` record out of a POSIX extended header, if it has one.
///
/// The format is a sequence of `length key=value`, each ended by a newline,
/// where the length counts its own digits and that newline. It is parsed
/// rather than searched, because a value may contain anything at all --
/// including the text `path=`.
fn pax_path(body: &[u8]) -> Option<String> {
    let mut at = 0usize;
    while at < body.len() {
        let space = body[at..].iter().position(|&byte| byte == b' ')? + at;
        let length: usize = core::str::from_utf8(&body[at..space]).ok()?.parse().ok()?;
        if length == 0 || at + length > body.len() {
            return None;
        }
        let record = &body[space + 1..at + length];
        let text = core::str::from_utf8(record).ok()?;
        if let Some(value) = text.strip_prefix("path=") {
            return Some(String::from(value.trim_end_matches('\n')));
        }
        at += length;
    }
    None
}

/// Whether a tar header agrees with its own checksum.
///
/// The only integrity check the format has. Computed with the checksum field
/// itself read as spaces, which is the convention and is exactly the detail
/// that makes a reader written from memory reject every real archive.
fn checksum_matches(header: &[u8]) -> bool {
    let Some(stated) = octal(&header[148..156]) else {
        return false;
    };
    let sum: u64 = header
        .iter()
        .enumerate()
        .map(|(index, &byte)| {
            if (148..156).contains(&index) {
                u64::from(b' ')
            } else {
                u64::from(byte)
            }
        })
        .sum();
    sum == stated
}

/// A tar number: octal digits, ended by a space or a zero byte.
fn octal(field: &[u8]) -> Option<u64> {
    let mut value = 0u64;
    let mut any = false;
    for &byte in field {
        match byte {
            b'0'..=b'7' => {
                value = value.checked_mul(8)?.checked_add(u64::from(byte - b'0'))?;
                any = true;
            }
            b' ' | 0 => {
                if any {
                    break;
                }
            }
            _ => return None,
        }
    }
    any.then_some(value)
}

/// A NUL-padded field as text.
fn trimmed(field: &[u8]) -> Result<&str, core::str::Utf8Error> {
    let end = field
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(field.len());
    core::str::from_utf8(&field[..end])
}

/// Up to the next whole block, which is how tar pads a member's body.
const fn round_up(length: usize) -> usize {
    length.div_ceil(BLOCK) * BLOCK
}

// -- ar ----------------------------------------------------------------------

/// Read an `ar` archive's headers.
///
/// What a `.deb` is, and the simplest archive format still in use: eight magic
/// bytes, then for each member a sixty-byte header of space-padded ASCII
/// followed by its bytes, padded to an even length.
///
/// # Errors
///
/// [`Trouble::NotThisFormat`] without the magic, [`Trouble::Truncated`] for a
/// member running off the end, [`Trouble::Corrupt`] for a header that does not
/// parse.
pub fn ar(archive: &[u8]) -> Result<Vec<Entry>, Trouble> {
    const MAGIC: &[u8] = b"!<arch>\n";
    const HEADER: usize = 60;

    if archive.len() < MAGIC.len() || &archive[..MAGIC.len()] != MAGIC {
        return Err(Trouble::NotThisFormat);
    }

    let mut entries = Vec::new();
    let mut at = MAGIC.len();
    while at + HEADER <= archive.len() {
        let header = &archive[at..at + HEADER];
        // Every header ends with these two bytes. Checking them is what stops a
        // corrupt length from walking this loop through the file's contents.
        if header[58] != 0x60 || header[59] != b'\n' {
            return Err(Trouble::Corrupt);
        }
        let name = core::str::from_utf8(&header[0..16])
            .map_err(|_| Trouble::Corrupt)?
            .trim_end()
            // A name is ended with a slash so that trailing spaces could be
            // part of it. Debian's writer does this.
            .trim_end_matches('/');
        let length: usize = core::str::from_utf8(&header[48..58])
            .map_err(|_| Trouble::Corrupt)?
            .trim()
            .parse()
            .map_err(|_| Trouble::Corrupt)?;

        let body = at + HEADER;
        if body + length > archive.len() {
            return Err(Trouble::Truncated);
        }
        entries.push(Entry {
            name: String::from(name),
            kind: Kind::File,
            at: body,
            length,
            mode: 0o644,
        });
        // Members start at an even offset.
        at = body + length + (length & 1);
    }
    Ok(entries)
}

// -- Debian packages ---------------------------------------------------------

/// What was inside a `.deb`.
pub struct Package {
    /// The control file's text, as it was written.
    pub control: String,
    /// The files the package installs, already decompressed, with every name
    /// checked.
    pub files: Vec<(String, Kind, Vec<u8>)>,
}

impl Package {
    /// One field out of the control file.
    ///
    /// The format is `Name: value`, with continuation lines indented. Only the
    /// first line of a value is returned, which is all that any field this
    /// system reads has.
    #[must_use]
    pub fn field(&self, name: &str) -> Option<&str> {
        self.control.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key.eq_ignore_ascii_case(name)).then(|| value.trim())
        })
    }
}

/// Read a Debian package.
///
/// An `ar` archive of three members: a version stamp, a compressed `control`
/// tar carrying the package's description, and a compressed `data` tar carrying
/// the files it installs.
///
/// Only gzip is read. Packages built in the last few years are usually `xz` or
/// `zstd`, and each of those is a whole decompressor in its own right -- so a
/// refusal that **names the compression** is a great deal more use than a wrong
/// answer, and that is what this gives.
///
/// # Errors
///
/// [`Trouble::NotThisFormat`] if it is not an `ar` archive, has no `data`
/// member, or is compressed with something not read here;
/// [`Trouble::Compressed`] if decompression failed; and whatever [`tar`] says
/// about the contents.
pub fn deb(archive: &[u8], most: usize) -> Result<Package, Trouble> {
    let members = ar(archive)?;

    let mut control = String::new();
    let mut files = Vec::new();
    let mut saw_data = false;

    for member in &members {
        let is_control = member.name.starts_with("control.tar");
        let is_data = member.name.starts_with("data.tar");
        if !is_control && !is_data {
            continue;
        }

        let raw = member.bytes(archive)?;
        let inner = if member.name.ends_with(".gz") {
            nexus_inflate::gzip(raw, most).map_err(Trouble::Compressed)?
        } else if member.name.ends_with(".tar") {
            raw.to_vec()
        } else {
            return Err(Trouble::NotThisFormat);
        };

        for entry in tar(&inner)? {
            if is_control {
                if entry.name == "control" {
                    control = String::from(
                        core::str::from_utf8(entry.bytes(&inner)?).map_err(|_| Trouble::Corrupt)?,
                    );
                }
            } else {
                saw_data = true;
                files.push((
                    entry.name.clone(),
                    entry.kind,
                    entry.bytes(&inner)?.to_vec(),
                ));
            }
        }
    }

    if !saw_data {
        return Err(Trouble::NotThisFormat);
    }
    Ok(Package { control, files })
}

/// What compression a `.deb`'s data member uses, whether or not it can be read.
///
/// So that a refusal can say which one. "This package is compressed with xz,
/// which this system does not read" is something somebody can act on; "could
/// not install" is not.
#[must_use]
pub fn compression(archive: &[u8]) -> Option<&'static str> {
    let members = ar(archive).ok()?;
    let data = members.iter().find(|m| m.name.starts_with("data.tar"))?;
    Some(match () {
        () if data.name.ends_with(".gz") => "gzip",
        () if data.name.ends_with(".xz") => "xz",
        () if data.name.ends_with(".zst") => "zstd",
        () if data.name.ends_with(".bz2") => "bzip2",
        () if data.name.ends_with(".tar") => "none",
        () => "something unrecognised",
    })
}
