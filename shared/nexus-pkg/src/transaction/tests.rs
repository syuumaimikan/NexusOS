use super::*;
use alloc::collections::BTreeMap;
use alloc::vec;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Node {
    kind: Kind,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct Memory {
    nodes: BTreeMap<String, Node>,
    handles: BTreeMap<usize, String>,
    next: usize,
    operations: usize,
    fail_at: Option<usize>,
    corrupt_read: Option<String>,
    short_read: Option<String>,
    oversize_write: Option<String>,
    persistent_write_failure: Option<String>,
}

impl Memory {
    fn seeded() -> Self {
        let mut fs = Self::default();
        fs.nodes.insert(
            "apps".into(),
            Node {
                kind: Kind::Directory,
                bytes: vec![],
            },
        );
        fs.nodes.insert(
            "apps/old".into(),
            Node {
                kind: Kind::File,
                bytes: b"original longer than the replacement".to_vec(),
            },
        );
        fs.nodes.insert(
            "outside".into(),
            Node {
                kind: Kind::File,
                bytes: b"unrelated data".to_vec(),
            },
        );
        fs
    }
    fn tick(&mut self) -> Result<(), String> {
        self.operations += 1;
        if self.fail_at == Some(self.operations) {
            Err(format!("injected I/O failure {}", self.operations))
        } else {
            Ok(())
        }
    }
    fn handle(&mut self, path: &str) -> usize {
        self.next += 1;
        self.handles.insert(self.next, path.into());
        self.next
    }
    fn parent_exists(&self, path: &str) -> bool {
        path.rsplit_once('/').map_or(true, |(p, _)| {
            self.nodes.get(p).is_some_and(|n| n.kind == Kind::Directory)
        })
    }
}

impl Filesystem for Memory {
    type File = usize;
    fn kind(&mut self, path: &str) -> Result<Option<Kind>, String> {
        self.tick()?;
        Ok(self.nodes.get(path).map(|n| n.kind))
    }
    fn mkdir(&mut self, path: &str) -> Result<(), String> {
        self.tick()?;
        if self.nodes.contains_key(path) || !self.parent_exists(path) {
            return Err("mkdir refused".into());
        }
        self.nodes.insert(
            path.into(),
            Node {
                kind: Kind::Directory,
                bytes: vec![],
            },
        );
        Ok(())
    }
    fn open_file(&mut self, path: &str) -> Result<usize, String> {
        self.tick()?;
        if !self.nodes.get(path).is_some_and(|n| n.kind == Kind::File) {
            return Err("not a file".into());
        }
        Ok(self.handle(path))
    }
    fn create_file(&mut self, path: &str) -> Result<usize, String> {
        self.tick()?;
        if self.nodes.contains_key(path) || !self.parent_exists(path) {
            return Err("exclusive create refused".into());
        }
        self.nodes.insert(
            path.into(),
            Node {
                kind: Kind::File,
                bytes: vec![],
            },
        );
        Ok(self.handle(path))
    }
    fn close(&mut self, file: usize) {
        assert!(self.handles.remove(&file).is_some());
    }
    fn size(&mut self, file: usize) -> Result<usize, String> {
        self.tick()?;
        Ok(self.nodes[&self.handles[&file]].bytes.len())
    }
    fn read_at(&mut self, file: usize, at: usize, bytes: &mut [u8]) -> Result<usize, String> {
        self.tick()?;
        let path = &self.handles[&file];
        if self.short_read.as_ref() == Some(path) {
            self.short_read = None;
            return Ok(0);
        }
        let source = &self.nodes[path].bytes;
        let count = bytes.len().min(source.len() - at).min(3);
        bytes[..count].copy_from_slice(&source[at..at + count]);
        if self.corrupt_read.as_ref() == Some(path) && count > 0 {
            self.corrupt_read = None;
            bytes[0] ^= 1;
        }
        Ok(count)
    }
    fn write_at(&mut self, file: usize, at: usize, bytes: &[u8]) -> Result<usize, String> {
        self.tick()?;
        let path = &self.handles[&file];
        if self.persistent_write_failure.as_ref() == Some(path) {
            return Err("persistent write failure".into());
        }
        if self.oversize_write.as_ref() == Some(path) {
            self.oversize_write = None;
            return Ok(bytes.len() + 1);
        }
        let count = bytes.len().min(3);
        let dest = &mut self.nodes.get_mut(path).unwrap().bytes;
        dest.resize(at + count, 0);
        dest[at..at + count].copy_from_slice(&bytes[..count]);
        Ok(count)
    }
    fn remove(&mut self, path: &str) -> Result<(), String> {
        self.tick()?;
        if self
            .nodes
            .keys()
            .any(|p| p.starts_with(&format!("{path}/")))
        {
            return Err(format!("{path}: not empty"));
        }
        assert!(
            !self.handles.values().any(|p| p == path),
            "must close before removing"
        );
        self.nodes.remove(path);
        Ok(())
    }
}

fn fixture() -> Vec<u8> {
    crate::build(
        "example",
        "1.0",
        &[
            ("apps/old", b"new"),
            ("apps/linux/bin/hello", b"executable"),
            ("apps/linux/share/data", b"data"),
        ],
    )
    .unwrap()
}

#[test]
fn nested_install_and_shorter_replacement_close_all_handles() {
    let bytes = fixture();
    let mut fs = Memory::seeded();
    let report = install(&mut fs, &Package::open(&bytes).unwrap()).unwrap();
    assert_eq!(report.files, 3);
    assert!(report.retained_backups.is_empty());
    assert_eq!(fs.nodes["apps/old"].bytes, b"new");
    assert_eq!(fs.nodes["apps/linux/bin/hello"].bytes, b"executable");
    assert!(!fs.nodes.contains_key("apps/old.OLD"));
    assert_eq!(fs.nodes["outside"].bytes, b"unrelated data");
    assert!(fs.handles.is_empty());
}

#[test]
fn failure_at_every_io_boundary_restores_original_namespace() {
    let bytes = fixture();
    let package = Package::open(&bytes).unwrap();
    let mut normal = Memory::seeded();
    install(&mut normal, &package).unwrap();
    // Includes partial backup writes, partial replacement writes, creates,
    // every readback, and directory creation/removal. The last cleanup failure
    // leaves a reported backup after a successful install.
    for boundary in 1..=normal.operations {
        let mut fs = Memory::seeded();
        let initial = fs.nodes.clone();
        fs.fail_at = Some(boundary);
        match install(&mut fs, &package) {
            Err(Failure::Io {
                recovery_failures, ..
            }) => {
                assert!(
                    recovery_failures.is_empty(),
                    "boundary {boundary}: {recovery_failures:?}"
                );
                assert_eq!(fs.nodes, initial, "boundary {boundary}");
            }
            Ok(report) => {
                assert_eq!(fs.nodes["apps/old"].bytes, b"new");
                assert_eq!(report.retained_backups, vec![String::from("apps/old.OLD")]);
            }
            other => panic!("boundary {boundary}: {other:?}"),
        }
        assert!(
            fs.handles.is_empty(),
            "leaked handle at boundary {boundary}"
        );
    }
}

#[test]
fn corrupt_backup_is_detected_before_replacement() {
    let mut fs = Memory::seeded();
    let initial = fs.nodes.clone();
    fs.corrupt_read = Some("apps/old.OLD".into());
    let bytes = fixture();
    assert!(matches!(
        install(&mut fs, &Package::open(&bytes).unwrap()),
        Err(Failure::Io { .. })
    ));
    assert_eq!(fs.nodes, initial);
    assert!(fs.handles.is_empty());
}

#[test]
fn short_or_corrupt_readback_and_invalid_write_roll_back() {
    for mode in 0..3 {
        let mut fs = Memory::seeded();
        let initial = fs.nodes.clone();
        let target = String::from("apps/linux/bin/hello");
        match mode {
            0 => fs.short_read = Some(target),
            1 => fs.corrupt_read = Some(target),
            _ => fs.oversize_write = Some(target),
        }
        let bytes = fixture();
        assert!(matches!(
            install(&mut fs, &Package::open(&bytes).unwrap()),
            Err(Failure::Io { .. })
        ));
        assert_eq!(fs.nodes, initial);
        assert!(fs.handles.is_empty());
    }
}

#[test]
fn recovery_write_failure_is_reported_and_backup_retained() {
    let mut fs = Memory::seeded();
    fs.persistent_write_failure = Some("apps/old".into());
    let bytes = fixture();
    match install(&mut fs, &Package::open(&bytes).unwrap()) {
        Err(Failure::Io {
            recovery_failures, ..
        }) => assert!(!recovery_failures.is_empty()),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        fs.nodes["apps/old.OLD"].bytes,
        b"original longer than the replacement"
    );
    assert!(fs.handles.is_empty());
}

#[test]
fn existing_backup_directories_and_links_are_never_replaced() {
    for (path, kind) in [
        ("apps/old.OLD", Kind::File),
        ("apps/old", Kind::Directory),
        ("apps/old", Kind::Link),
    ] {
        let mut fs = Memory::seeded();
        fs.nodes.insert(
            path.into(),
            Node {
                kind,
                bytes: b"protected".to_vec(),
            },
        );
        let initial = fs.nodes.clone();
        let bytes = fixture();
        assert!(matches!(
            install(&mut fs, &Package::open(&bytes).unwrap()),
            Err(Failure::Io { .. })
        ));
        assert_eq!(fs.nodes, initial);
    }
}

#[test]
fn invalid_destinations_refused_before_any_filesystem_call() {
    for paths in [
        vec!["a", "a"],
        vec!["a", "a/b"],
        vec!["z", "z.OLD"],
        vec!["a.OLD/b"],
        vec![".INSTALL.LOCK"],
    ] {
        let files: Vec<(&str, &[u8])> = paths.into_iter().map(|p| (p, b"x".as_slice())).collect();
        let bytes = crate::build("bad", "1", &files).unwrap();
        let mut fs = Memory::seeded();
        assert!(matches!(
            install(&mut fs, &Package::open(&bytes).unwrap()),
            Err(Failure::Refused(_))
        ));
        assert_eq!(fs.operations, 0);
    }
}

#[test]
fn directory_fanout_is_bounded_before_mutation() {
    let files: Vec<(String, Vec<u8>)> = (0..MAX_FILES)
        .map(|i| (format!("root/{i}/deep/file"), vec![1]))
        .collect();
    let borrowed: Vec<(&str, &[u8])> = files
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let bytes = crate::build("directories", "1", &borrowed).unwrap();
    let mut fs = Memory::seeded();
    assert!(matches!(
        install(&mut fs, &Package::open(&bytes).unwrap()),
        Err(Failure::Refused(_))
    ));
    assert_eq!(fs.operations, 0);
}

#[test]
fn oversized_file_refused_without_allocation_or_mutation_by_installer() {
    let bytes = crate::build("big", "1", &[("big", &vec![0; MAX_FILE + 1])]).unwrap();
    let mut fs = Memory::seeded();
    assert!(matches!(
        install(&mut fs, &Package::open(&bytes).unwrap()),
        Err(Failure::Refused(_))
    ));
    assert_eq!(fs.operations, 0);
}

#[test]
fn exact_read_closes_handle_after_size_failure_and_limit_refusal() {
    let mut fs = Memory::seeded();
    assert!(read_with_limit(&mut fs, "apps/old", 2).is_err());
    assert!(fs.handles.is_empty());
    fs.fail_at = Some(fs.operations + 2);
    assert!(read(&mut fs, "apps/old").is_err());
    assert!(fs.handles.is_empty());
}

#[test]
fn bookkeeping_and_payload_roll_back_together_at_every_boundary() {
    let bytes = fixture();
    let package = Package::open(&bytes).unwrap();
    let extra = [("system/installed.txt", b"example = 1.0.0\n".as_slice())];
    let mut normal = Memory::seeded();
    normal.nodes.insert(
        "system".into(),
        Node {
            kind: Kind::Directory,
            bytes: vec![],
        },
    );
    normal.nodes.insert(
        "system/installed.txt".into(),
        Node {
            kind: Kind::File,
            bytes: b"old = 2.0.0\n".to_vec(),
        },
    );
    let initial = normal.nodes.clone();
    install_with_extra(&mut normal, &package, &extra).unwrap();
    for boundary in 1..=normal.operations {
        let mut fs = Memory::default();
        fs.nodes = initial.clone();
        fs.fail_at = Some(boundary);
        match install_with_extra(&mut fs, &package, &extra) {
            Err(Failure::Io {
                recovery_failures, ..
            }) => {
                assert!(
                    recovery_failures.is_empty(),
                    "{boundary}: {recovery_failures:?}"
                );
                assert_eq!(fs.nodes, initial, "{boundary}");
            }
            Ok(report) => {
                assert_eq!(fs.nodes["system/installed.txt"].bytes, extra[0].1);
                assert!(!report.retained_backups.is_empty());
            }
            other => panic!("{other:?}"),
        }
        assert!(fs.handles.is_empty());
    }
    let mut fs = Memory::seeded();
    assert!(matches!(
        install_with_extra(&mut fs, &package, &[("apps/old", b"conflicting record")]),
        Err(Failure::Refused(_))
    ));
    assert_eq!(fs.operations, 0);
}
