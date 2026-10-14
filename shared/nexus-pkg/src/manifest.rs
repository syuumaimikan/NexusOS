//! Optional signed application metadata, stored as `META/<package>.app`.
//! Metadata describes the intended ABI; it does not promise runtime support.
use crate::{validate_path, Error, Package};
use alloc::format;
use alloc::string::String;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Nexus,
    Linux,
    Windows,
}
impl Platform {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Nexus => "nexus",
            Self::Linux => "linux",
            Self::Windows => "windows",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Architecture {
    X86_64,
    X86,
}
impl Architecture {
    pub const fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::X86 => "x86",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Application<'a> {
    pub platform: Platform,
    pub architecture: Architecture,
    pub entry: &'a str,
}
pub fn path(name: &str) -> Result<String, Error> {
    if name.is_empty()
        || name.len() >= crate::MAX_NAME
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
    {
        return Err(Error::BadName);
    }
    Ok(format!("META/{name}.app"))
}
pub fn parse(text: &str) -> Result<Application<'_>, Error> {
    if text.len() > 4096 || text.contains('\0') {
        return Err(Error::Malformed);
    }
    let (mut format, mut platform, mut arch, mut entry) = (None, None, None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (key, value) = line.split_once('=').ok_or(Error::Malformed)?;
        let slot = match key.trim() {
            "format" => &mut format,
            "platform" => &mut platform,
            "arch" => &mut arch,
            "entry" => &mut entry,
            _ => return Err(Error::Malformed),
        };
        if slot.replace(value.trim()).is_some() {
            return Err(Error::Malformed);
        }
    }
    if format != Some("1") {
        return Err(Error::Malformed);
    }
    let platform = match platform {
        Some("nexus") => Platform::Nexus,
        Some("linux") => Platform::Linux,
        Some("windows") => Platform::Windows,
        _ => return Err(Error::Malformed),
    };
    let architecture = match arch {
        Some("x86_64") => Architecture::X86_64,
        Some("x86") => Architecture::X86,
        _ => return Err(Error::Malformed),
    };
    let entry = entry.ok_or(Error::Malformed)?;
    validate_path(entry)?;
    Ok(Application {
        platform,
        architecture,
        entry,
    })
}
impl<'a> Package<'a> {
    /// Read optional application metadata. The entrypoint must be a payload
    /// file of this same package. Authenticate the package before using it.
    pub fn application(&self) -> Result<Option<Application<'a>>, Error> {
        // Legacy packages can have display names that are not application IDs.
        // They remain installable without the optional metadata extension.
        let Ok(metadata) = path(self.name()?) else {
            return Ok(None);
        };
        let mut found = None;
        for index in 0..self.len() {
            let entry = self.entry(index).ok_or(Error::Malformed)?;
            if entry.name()? == metadata {
                if found.is_some() {
                    return Err(Error::Malformed);
                }
                let text = core::str::from_utf8(self.contents(&entry).ok_or(Error::Malformed)?)
                    .map_err(|_| Error::Malformed)?;
                found = Some(parse(text)?);
            }
        }
        if let Some(app) = found {
            if app.entry == metadata
                || !(0..self.len())
                    .any(|i| self.entry(i).is_some_and(|e| e.name() == Ok(app.entry)))
            {
                return Err(Error::Malformed);
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_platforms_round_trip_and_require_the_packaged_entry() {
        for platform in ["nexus", "linux", "windows"] {
            let manifest =
                format!("format=1\nplatform={platform}\narch=x86_64\nentry=APPS/demo/bin/app\n");
            let bytes = crate::build(
                "demo",
                "1.0.0",
                &[
                    ("META/demo.app", manifest.as_bytes()),
                    ("APPS/demo/bin/app", b"binary"),
                ],
            )
            .unwrap();
            let app = Package::open(&bytes)
                .unwrap()
                .application()
                .unwrap()
                .unwrap();
            assert_eq!(app.platform.name(), platform);
            assert_eq!(app.entry, "APPS/demo/bin/app");
            let bytes =
                crate::build("demo", "1", &[("META/demo.app", manifest.as_bytes())]).unwrap();
            assert_eq!(
                Package::open(&bytes).unwrap().application(),
                Err(Error::Malformed)
            );
        }
    }
    #[test]
    fn ambiguous_metadata_and_escaping_paths_are_refused() {
        let base = "format=1\nplatform=linux\narch=x86_64\nentry=bin/app\n";
        for extra in [
            "platform=windows",
            "unknown=1",
            "entry=../outside",
            "arch=arm64",
        ] {
            assert!(parse(&format!("{base}{extra}")).is_err());
        }
        for path in [
            "../outside",
            "a//b",
            "a/./b",
            "a\\b",
            "C:/x",
            "a\0b",
            "/bin/app",
        ] {
            assert!(parse(&format!("format=1\nplatform=linux\narch=x86\nentry={path}")).is_err());
        }
        assert!(path("../demo").is_err());
    }
}
