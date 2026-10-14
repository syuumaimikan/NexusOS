//! Native adapter for signed, bounded package transactions.
//! No path can escape the directory capability lent by the caller.
#![no_std]
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use nexus_pkg::{transaction as tx, Package};
use nexus_user::{Error, Handle, Kind};
use tx::Filesystem;

include!(concat!(env!("OUT_DIR"), "/trusted_key.rs"));

pub const MAX_PACKAGE: usize = 2 * 1024 * 1024;

pub enum Trouble {
    Refused(String),
    Broken(String),
}

/// Verify the trusted signature before any destination is modified. Callers
/// must serialize installations and other writes to package destinations.
pub fn install(root: Handle, path: &str) -> Result<String, Trouble> {
    let bytes = read_whole(root, path).map_err(Trouble::Broken)?;
    let package = Package::open(&bytes).map_err(|e| Trouble::Refused(format!("{e}")))?;
    package
        .verify_signed_by(&TRUSTED_KEY)
        .map_err(|e| Trouble::Refused(format!("{e}")))?;
    package
        .application()
        .map_err(|e| Trouble::Refused(format!("application metadata: {e}")))?;
    tx::validate(&package).map_err(Trouble::Refused)?;
    let _lock = InstallLock::acquire(root).map_err(Trouble::Broken)?;
    install_checked(root, &package, &[])
}

/// Install the payload and shared Store/updater version record together.
pub fn install_recorded(root: Handle, source: Handle, path: &str) -> Result<String, Trouble> {
    let bytes = read_whole(source, path).map_err(Trouble::Broken)?;
    install_recorded_bytes(root, &bytes)
}

pub fn install_recorded_bytes(root: Handle, bytes: &[u8]) -> Result<String, Trouble> {
    let package = Package::open(bytes).map_err(|e| Trouble::Refused(format!("{e}")))?;
    package
        .verify_signed_by(&TRUSTED_KEY)
        .map_err(|e| Trouble::Refused(format!("{e}")))?;
    package
        .application()
        .map_err(|e| Trouble::Refused(format!("application metadata: {e}")))?;
    let name = package
        .name()
        .map_err(|e| Trouble::Refused(format!("{e}")))?;
    nexus_pkg::manifest::path(name)
        .map_err(|_| Trouble::Refused(String::from("package name cannot be recorded")))?;
    let version = package
        .release()
        .ok()
        .and_then(nexus_update::Version::parse)
        .ok_or_else(|| {
            Trouble::Refused(String::from(
                "a recorded install requires major.minor.patch",
            ))
        })?;
    tx::validate(&package).map_err(Trouble::Refused)?;
    let _lock = InstallLock::acquire(root).map_err(Trouble::Broken)?;
    let mut record = installed(root).map_err(Trouble::Broken)?;
    record.set(name, version);
    let text = record.to_text();
    if text.len() > 32 * 1024 {
        return Err(Trouble::Refused(String::from(
            "installed package record exceeds 32 KiB",
        )));
    }
    install_checked(root, &package, &[("system/installed.txt", text.as_bytes())])
}

pub fn installed(root: Handle) -> Result<nexus_update::Installed, String> {
    let mut fs = Native { root };
    match fs.kind("system")? {
        None => return Ok(nexus_update::Installed::new()),
        Some(tx::Kind::Directory) => {}
        _ => return Err(String::from("system: not a directory")),
    }
    match fs.kind("system/installed.txt")? {
        None => Ok(nexus_update::Installed::new()),
        Some(tx::Kind::File) => {
            let bytes = tx::read_with_limit(&mut fs, "system/installed.txt", 32 * 1024)?;
            let text = core::str::from_utf8(&bytes)
                .map_err(|_| String::from("installed.txt is not UTF-8"))?;
            Ok(nexus_update::Installed::parse(text))
        }
        _ => Err(String::from("installed.txt: not a regular file")),
    }
}

fn install_checked(
    root: Handle,
    package: &Package<'_>,
    extra: &[(&str, &[u8])],
) -> Result<String, Trouble> {
    let name = package
        .name()
        .map_err(|e| Trouble::Refused(format!("{e}")))?;
    let release = package
        .release()
        .map_err(|e| Trouble::Refused(format!("{e}")))?;
    nexus_user::log(&format!(
        "install: {name} {release}, {} files, signed by the key this machine trusts",
        package.len()
    ))
    .ok();
    match tx::install_with_extra(&mut Native { root }, package, extra) {
        Ok(report) => {
            let mut text = format!("install: wrote {} files for {name} {release}", report.files);
            for path in report.retained_backups {
                text.push_str(&format!("; recovery backup retained: {path}"));
            }
            Ok(text)
        }
        Err(tx::Failure::Refused(why)) => Err(Trouble::Refused(why)),
        Err(tx::Failure::Io {
            reason,
            restored,
            recovery_failures,
        }) => {
            let mut text = format!("{reason}; rolled back {restored} changes");
            for why in recovery_failures {
                text.push_str(&format!("; recovery FAILED: {why}"));
            }
            Err(Trouble::Broken(text))
        }
    }
}

/// A whole package or metadata file, with exact reads and a memory bound.
pub fn read_whole(root: Handle, path: &str) -> Result<Vec<u8>, String> {
    tx::read_with_limit(&mut Native { root }, path, MAX_PACKAGE)
}

struct Native {
    root: Handle,
}

/// Exclusive create serializes Store, updater and CLI calls on the same root.
/// A stale lock after a crash is retained for explicit recovery, never stolen.
struct InstallLock {
    root: Handle,
    file: Handle,
}
impl InstallLock {
    fn acquire(root: Handle) -> Result<Self, String> {
        let file = nexus_user::create(root, ".INSTALL.LOCK", Kind::File).map_err(|e| {
            format!(
                "cannot acquire installation lock: {e}; another install or recovery may be pending"
            )
        })?;
        Ok(Self { root, file })
    }
}
impl Drop for InstallLock {
    fn drop(&mut self) {
        nexus_user::close(self.file).ok();
        if let Err(e) = nexus_user::remove(self.root, ".INSTALL.LOCK") {
            nexus_user::log(&format!("install: recovery lock retained: {e}")).ok();
        }
    }
}

/// Every opened intermediate directory is closed, including error paths. The
/// authority root belongs to the caller and must never be closed by this guard.
struct Held {
    handle: Handle,
    owned: bool,
}
impl Drop for Held {
    fn drop(&mut self) {
        if self.owned {
            nexus_user::close(self.handle).ok();
        }
    }
}

impl Native {
    fn directory(&self, path: &str) -> Result<Held, String> {
        let mut at = Held {
            handle: self.root,
            owned: false,
        };
        if path.is_empty() {
            return Ok(at);
        }
        for part in path.split('/') {
            if part.is_empty()
                || part == "."
                || part == ".."
                || part
                    .chars()
                    .any(|c| c.is_control() || c == '\\' || c == ':')
            {
                return Err(format!("{path}: invalid relative path"));
            }
            let handle = nexus_user::open(at.handle, part).map_err(|e| format!("{path}: {e}"))?;
            at = Held {
                handle,
                owned: true,
            };
        }
        Ok(at)
    }
    fn parent<'a>(&self, path: &'a str) -> Result<(Held, &'a str), String> {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        if name.is_empty()
            || name == "."
            || name == ".."
            || name
                .chars()
                .any(|c| c.is_control() || c == '\\' || c == ':')
        {
            return Err(format!("{path}: invalid file name"));
        }
        Ok((self.directory(parent)?, name))
    }
}

impl Filesystem for Native {
    type File = Handle;
    fn kind(&mut self, path: &str) -> Result<Option<tx::Kind>, String> {
        let (parent, name) = self.parent(path)?;
        let mut packed = Vec::new();
        let mut capacity = 1024;
        loop {
            packed
                .try_reserve_exact(capacity - packed.len())
                .map_err(|_| format!("{path}: not enough directory memory"))?;
            packed.resize(capacity, 0);
            match nexus_user::list(parent.handle, &mut packed) {
                Ok(length) if length <= packed.len() => {
                    return Ok(nexus_user::entries(&packed[..length])
                        .find(|e| e.name == name)
                        .map(|e| match e.kind {
                            Kind::File => tx::Kind::File,
                            Kind::Directory => tx::Kind::Directory,
                            Kind::Symlink => tx::Kind::Link,
                        }));
                }
                Err(Error::TooBig) if capacity < 64 * 1024 => capacity *= 2,
                Err(e) => return Err(format!("{path}: cannot inspect directory: {e}")),
                _ => return Err(format!("{path}: invalid directory response")),
            }
        }
    }
    fn mkdir(&mut self, path: &str) -> Result<(), String> {
        let (parent, name) = self.parent(path)?;
        let handle = nexus_user::create(parent.handle, name, Kind::Directory)
            .map_err(|e| format!("{path}: cannot create directory: {e}"))?;
        nexus_user::close(handle).ok();
        Ok(())
    }
    fn open_file(&mut self, path: &str) -> Result<Handle, String> {
        if self.kind(path)? != Some(tx::Kind::File) {
            return Err(format!("{path}: not a regular file"));
        }
        let (parent, name) = self.parent(path)?;
        nexus_user::open(parent.handle, name).map_err(|e| format!("{path}: cannot open: {e}"))
    }
    fn create_file(&mut self, path: &str) -> Result<Handle, String> {
        let (parent, name) = self.parent(path)?;
        nexus_user::create(parent.handle, name, Kind::File)
            .map_err(|e| format!("{path}: cannot create: {e}"))
    }
    fn close(&mut self, file: Handle) {
        nexus_user::close(file).ok();
    }
    fn size(&mut self, file: Handle) -> Result<usize, String> {
        nexus_user::size(file).map_err(|e| format!("cannot size file: {e}"))
    }
    fn read_at(&mut self, file: Handle, at: usize, bytes: &mut [u8]) -> Result<usize, String> {
        nexus_user::read_at(file, at as u64, bytes).map_err(|e| format!("cannot read file: {e}"))
    }
    fn write_at(&mut self, file: Handle, at: usize, bytes: &[u8]) -> Result<usize, String> {
        nexus_user::write_at(file, at as u64, bytes).map_err(|e| format!("cannot write file: {e}"))
    }
    fn remove(&mut self, path: &str) -> Result<(), String> {
        let (parent, name) = self.parent(path)?;
        match nexus_user::remove(parent.handle, name) {
            Ok(()) | Err(Error::NotFound) => Ok(()),
            Err(e) => Err(format!("{path}: cannot remove: {e}")),
        }
    }
}
