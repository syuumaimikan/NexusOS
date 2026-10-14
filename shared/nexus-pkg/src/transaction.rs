//! Bounded package installation, shared by native callers and fault tests.
//!
//! The caller verifies the signing key before calling this module and holds
//! exclusive write access to the destination namespace for the operation.
//! This is rollback after an I/O error, not a power-loss transaction. A second
//! I/O error during recovery is reported; its verified backup is retained.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::{digest, digests_equal, Package};

pub const MAX_FILES: usize = 1024;
pub const MAX_DIRECTORIES: usize = 1024;
pub const MAX_FILE: usize = 1024 * 1024;
const BACKUP: &str = ".OLD";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    Link,
}

/// Paths are relative to one authority root; implementations must not follow
/// links. `create_file` is exclusive and must not replace an existing node.
pub trait Filesystem {
    type File: Copy;
    fn kind(&mut self, path: &str) -> Result<Option<Kind>, String>;
    fn mkdir(&mut self, path: &str) -> Result<(), String>;
    fn open_file(&mut self, path: &str) -> Result<Self::File, String>;
    fn create_file(&mut self, path: &str) -> Result<Self::File, String>;
    fn close(&mut self, file: Self::File);
    fn size(&mut self, file: Self::File) -> Result<usize, String>;
    fn read_at(&mut self, file: Self::File, at: usize, bytes: &mut [u8]) -> Result<usize, String>;
    fn write_at(&mut self, file: Self::File, at: usize, bytes: &[u8]) -> Result<usize, String>;
    /// Removing an absent node succeeds. A nonempty directory must be refused.
    fn remove(&mut self, path: &str) -> Result<(), String>;
}

#[derive(Debug)]
pub enum Failure {
    Refused(String),
    Io {
        reason: String,
        restored: usize,
        recovery_failures: Vec<String>,
    },
}

#[derive(Debug)]
pub struct Report {
    pub files: usize,
    /// Installation succeeded, but these backup files could not be removed.
    pub retained_backups: Vec<String>,
}

enum Step {
    Directory(String),
    File {
        path: String,
        backup: Option<(String, [u8; 32])>,
        changed: bool,
    },
}

/// Check every destination before any filesystem mutation. Duplicate files,
/// file/directory conflicts and backup names cannot form a valid transaction.
pub fn validate(package: &Package<'_>) -> Result<(), String> {
    let paths = destinations(package)?;
    validate_destinations(&paths)
}

fn destinations(package: &Package<'_>) -> Result<Vec<(String, usize)>, String> {
    if package.len() > MAX_FILES {
        return Err(format!("too many files (limit {MAX_FILES})"));
    }
    let mut paths = Vec::new();
    for index in 0..package.len() {
        let entry = package.entry(index).ok_or("missing package entry")?;
        paths.push((
            entry.name().map_err(|e| format!("{e}"))?.to_string(),
            entry.length as usize,
        ));
    }
    Ok(paths)
}

fn validate_destinations(paths: &[(String, usize)]) -> Result<(), String> {
    if paths.len() > MAX_FILES {
        return Err(format!("too many files (limit {MAX_FILES})"));
    }
    let mut names = BTreeSet::new();
    let mut directories = BTreeSet::new();
    for (path, length) in paths {
        crate::validate_path(path).map_err(|e| format!("{path}: {e}"))?;
        if path == ".INSTALL.LOCK" {
            return Err(String::from("the installer lock is reserved"));
        }
        if *length > MAX_FILE {
            return Err(format!("{path}: exceeds the {MAX_FILE}-byte file limit"));
        }
        if path.split('/').any(|part| part.ends_with(BACKUP)) {
            return Err(format!("{path}: .OLD is reserved for recovery"));
        }
        if !names.insert(path.as_str()) {
            return Err(format!("{path}: duplicate destination"));
        }
        for (index, _) in path.match_indices('/') {
            directories.insert(&path[..index]);
            if directories.len() > MAX_DIRECTORIES {
                return Err(format!(
                    "too many parent directories (limit {MAX_DIRECTORIES})"
                ));
            }
        }
    }
    for name in &names {
        for (index, _) in name.match_indices('/') {
            if names.contains(&name[..index]) {
                return Err(format!("{name}: a parent is also a package file"));
            }
        }
    }
    Ok(())
}

pub fn install<F: Filesystem>(fs: &mut F, package: &Package<'_>) -> Result<Report, Failure> {
    install_with_extra(fs, package, &[])
}

/// Add caller-generated bookkeeping to the same rollback transaction. Extra
/// bytes are NOT publisher-signed; the trusted caller supplies them after
/// authenticating the original package. Destinations must not overlap it.
pub fn install_with_extra<F: Filesystem>(
    fs: &mut F,
    package: &Package<'_>,
    extra: &[(&str, &[u8])],
) -> Result<Report, Failure> {
    package
        .verify()
        .map_err(|e| Failure::Refused(format!("{e}")))?;
    let mut paths = destinations(package).map_err(Failure::Refused)?;
    for (path, bytes) in extra {
        paths.push((path.to_string(), bytes.len()));
    }
    validate_destinations(&paths).map_err(Failure::Refused)?;
    let mut files = Vec::new();
    for index in 0..package.len() {
        let entry = package.entry(index).expect("validated table");
        files.push((
            paths[index].0.as_str(),
            package.contents(&entry).expect("validated extent"),
        ));
    }
    files.extend_from_slice(extra);
    let mut steps = Vec::new();
    steps
        .try_reserve_exact(paths.len() + MAX_DIRECTORIES)
        .map_err(|_| Failure::Refused(String::from("not enough transaction memory")))?;
    for (path, bytes) in files {
        if let Err(reason) = place(fs, path, bytes, &mut steps) {
            let (restored, recovery_failures) = rewind(fs, &mut steps);
            return Err(Failure::Io {
                reason,
                restored,
                recovery_failures,
            });
        }
    }
    let mut retained_backups = Vec::new();
    for step in &steps {
        if let Step::File {
            backup: Some((path, _)),
            ..
        } = step
        {
            if fs.remove(path).is_err() {
                retained_backups.push(path.clone());
            }
        }
    }
    Ok(Report {
        files: package.len() + extra.len(),
        retained_backups,
    })
}

fn place<F: Filesystem>(
    fs: &mut F,
    path: &str,
    bytes: &[u8],
    steps: &mut Vec<Step>,
) -> Result<(), String> {
    for (index, _) in path.match_indices('/') {
        let parent = &path[..index];
        match fs.kind(parent)? {
            Some(Kind::Directory) => {}
            Some(_) => return Err(format!("{parent}: parent is not a directory")),
            None => {
                fs.mkdir(parent)?;
                steps.push(Step::Directory(parent.to_string()));
            }
        }
    }
    match fs.kind(path)? {
        Some(Kind::File) => {
            let backup = format!("{path}{BACKUP}");
            if fs.kind(&backup)?.is_some() {
                return Err(format!(
                    "{backup}: existing recovery file was left untouched"
                ));
            }
            let old = read(fs, path)?;
            let original_digest = digest(&old);
            // Only remember ownership once exclusive create succeeds. A create
            // refusal must never authorize removing somebody else's file.
            let file = fs.create_file(&backup)?;
            steps.push(Step::File {
                path: path.to_string(),
                backup: Some((backup.clone(), original_digest)),
                changed: false,
            });
            write_open(fs, file, &old)?;
            drop(old);
            check(fs, &backup, &original_digest)?;
            if let Some(Step::File { changed, .. }) = steps.last_mut() {
                // Journal BEFORE removing, creating, writing or reading back.
                *changed = true;
            }
            fs.remove(path)?;
        }
        Some(_) => return Err(format!("{path}: refusing to replace a directory or link")),
        None => {
            // Exclusive create first; from this point the new file belongs to
            // this transaction, even if its first write fails.
            let file = fs.create_file(path)?;
            steps.push(Step::File {
                path: path.to_string(),
                backup: None,
                changed: true,
            });
            write_open(fs, file, bytes)?;
            return check(fs, path, &digest(bytes));
        }
    }
    let file = fs.create_file(path)?;
    write_open(fs, file, bytes)?;
    check(fs, path, &digest(bytes))
}

/// Exact, bounded reads. A zero/oversized read is an error, not a truncated file.
pub fn read<F: Filesystem>(fs: &mut F, path: &str) -> Result<Vec<u8>, String> {
    read_with_limit(fs, path, MAX_FILE)
}

pub fn read_with_limit<F: Filesystem>(
    fs: &mut F,
    path: &str,
    limit: usize,
) -> Result<Vec<u8>, String> {
    let file = fs.open_file(path)?;
    let result = (|| {
        let size = fs.size(file)?;
        if size > limit {
            return Err(format!("{path}: exceeds the {limit}-byte read limit"));
        }
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size)
            .map_err(|_| format!("{path}: not enough memory"))?;
        bytes.resize(size, 0);
        let mut at = 0;
        while at < size {
            let count = fs.read_at(file, at, &mut bytes[at..])?;
            if count == 0 || count > size - at {
                return Err(format!("{path}: incomplete or invalid read at {at}"));
            }
            at += count;
        }
        Ok(bytes)
    })();
    fs.close(file);
    result
}

fn write_open<F: Filesystem>(fs: &mut F, file: F::File, bytes: &[u8]) -> Result<(), String> {
    let result = (|| {
        let mut at = 0;
        while at < bytes.len() {
            let count = fs.write_at(file, at, &bytes[at..])?;
            if count == 0 || count > bytes.len() - at {
                return Err(format!("incomplete or invalid write at {at}"));
            }
            at += count;
        }
        Ok(())
    })();
    fs.close(file);
    result
}

fn check<F: Filesystem>(fs: &mut F, path: &str, expected: &[u8; 32]) -> Result<(), String> {
    let bytes = read(fs, path)?;
    if digests_equal(&digest(&bytes), expected) {
        Ok(())
    } else {
        Err(format!(
            "{path}: what was read back is not what was written"
        ))
    }
}

fn rewind<F: Filesystem>(fs: &mut F, steps: &mut Vec<Step>) -> (usize, Vec<String>) {
    let mut restored = 0;
    let mut failures = Vec::new();
    while let Some(step) = steps.pop() {
        let result = match step {
            Step::Directory(path) => fs.remove(&path),
            Step::File {
                path, backup: None, ..
            } => fs.remove(&path),
            Step::File {
                path,
                backup: Some((backup, expected)),
                changed,
            } => (|| {
                if changed {
                    let old = read(fs, &backup)?;
                    if !digests_equal(&digest(&old), &expected) {
                        return Err(format!("{backup}: recovery copy is damaged; retained"));
                    }
                    fs.remove(&path)?;
                    let file = fs.create_file(&path)?;
                    write_open(fs, file, &old)?;
                    drop(old);
                    check(fs, &path, &expected)?;
                }
                fs.remove(&backup)
            })(),
        };
        match result {
            Ok(()) => restored += 1,
            Err(reason) => failures.push(reason),
        }
    }
    (restored, failures)
}

#[cfg(test)]
mod tests;
