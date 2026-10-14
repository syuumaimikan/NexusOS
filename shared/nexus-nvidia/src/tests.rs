extern crate std;
use super::*;
use std::{vec, vec::Vec};

fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn boot() -> Vec<u8> {
    let mut bytes = vec![0u8; 116 + 1024];
    for (at, value) in [
        (0, 0x10de),
        (4, 1),
        (8, 1280),
        (12, 24),
        (16, 116),
        (20, 1024),
        (24, 6),
        (24 + 8 * 4, 0),
        (24 + 9 * 4, 256),
        (24 + 10 * 4, 256),
        (24 + 11 * 4, 256),
        (24 + 12 * 4, 512),
        (24 + 13 * 4, 512),
        (24 + 14 * 4, 1),
    ] {
        put32(&mut bytes, at, value);
    }
    bytes
}
fn fmc() -> Vec<u8> {
    let names = b"\0.shstrtab\0hash\0signature\0publickey\0image\0";
    let mut bytes = vec![0u8; 492 + 48 + 96 + 100 + 16];
    bytes[..8].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    put32(&mut bytes, 20, 1);
    put64(&mut bytes, 40, 64);
    for (at, value) in [(52, 64), (58, 64), (60, 6), (62, 1)] {
        put16(&mut bytes, at, value);
    }
    put32(&mut bytes, 128, 1);
    put32(&mut bytes, 132, 3);
    put64(&mut bytes, 152, 448);
    put64(&mut bytes, 160, names.len() as u64);
    bytes[448..448 + names.len()].copy_from_slice(names);
    for (index, (name_at, offset, size)) in
        [(11, 492, 48), (16, 540, 96), (26, 636, 97), (36, 736, 16)]
            .into_iter()
            .enumerate()
    {
        let at = 64 + (index + 2) * 64;
        put32(&mut bytes, at, name_at);
        put32(&mut bytes, at + 4, 1);
        put64(&mut bytes, at + 8, 0xfff00102);
        put64(&mut bytes, at + 24, offset);
        put64(&mut bytes, at + 32, size);
        put64(&mut bytes, at + 48, 4);
        if index == 2 {
            bytes[offset as usize] = 4;
        }
        let crc = crc32(&bytes[offset as usize..(offset + size) as usize]);
        put32(&mut bytes, at + 44, crc);
    }
    bytes
}

#[test]
fn bootstrap_manifest_code_and_data_are_checked_before_use() {
    let bytes = boot();
    let mut different = bytes.clone();
    put32(&mut different, 24 + 4, 256);
    let parsed = Bootloader::parse(&different).unwrap();
    assert_eq!(parsed.boot_manifest_offset, 256);
    assert_eq!(parsed.manifest.offset, 0);
    let parsed = Bootloader::parse(&bytes).unwrap();
    assert_eq!(parsed.manifest.bytes(parsed.image).unwrap().len(), 256);
    assert_eq!(parsed.code.offset, 512);
    assert_eq!(parsed.data.size, 256);
    for length in 0..bytes.len() {
        assert!(Bootloader::parse(&bytes[..length]).is_err());
    }
    for (at, value, error) in [
        (24, 5, Error::Version),
        (24 + 12 * 4, u32::MAX, Error::Bounds),
        (24 + 12 * 4, 256, Error::Overlap),
        (24 + 12 * 4, 513, Error::Bounds),
        (24 + 14 * 4, 0, Error::Header),
    ] {
        let mut changed = bytes.clone();
        put32(&mut changed, at, value);
        assert_eq!(Bootloader::parse(&changed).err(), Some(error));
    }
}

#[test]
fn fmc_preserves_opaque_crypto_material_and_rejects_damage() {
    let bytes = fmc();
    let parsed = Fmc::parse(&bytes).unwrap();
    assert_eq!(
        (
            parsed.hash.len(),
            parsed.signature.len(),
            parsed.public_key.len(),
            parsed.image.len()
        ),
        (48, 96, 97, 16)
    );
    for length in 0..bytes.len() {
        assert!(Fmc::parse(&bytes[..length]).is_err());
    }
    let mut changed = bytes.clone();
    changed[540] ^= 1;
    assert_eq!(Fmc::parse(&changed).err(), Some(Error::Checksum));
    let mut changed = bytes.clone();
    put64(&mut changed, 64 + 5 * 64 + 24, u64::MAX);
    assert_eq!(Fmc::parse(&changed).err(), Some(Error::Bounds));
    let mut changed = bytes.clone();
    put64(&mut changed, 64 + 5 * 64 + 24, 492);
    assert_eq!(Fmc::parse(&changed).err(), Some(Error::Overlap));
    let mut changed = bytes.clone();
    put32(&mut changed, 64 + 5 * 64, 10);
    assert_eq!(Fmc::parse(&changed).err(), Some(Error::Section));
}

#[test]
fn locally_repaired_crc_is_not_a_pinned_firmware_release() {
    let mut fmc = fmc();
    fmc[736] = 12;
    let crc = crc32(&fmc[736..752]);
    put32(&mut fmc, 64 + 5 * 64 + 44, crc);
    assert!(Fmc::parse(&fmc).is_ok());
    assert_eq!(
        verify_pinned(&boot(), &fmc).err(),
        Some(Error::ReleaseMismatch)
    );
    assert_eq!(crc32(b"123456789"), 0xcbf43926);
}

#[test]
fn fmc_is_a_firmware_container_and_never_an_executable() {
    let mut bytes = fmc();
    put16(&mut bytes, 18, 243);
    assert_eq!(Fmc::parse(&bytes).err(), Some(Error::Header));
    let mut bytes = fmc();
    put16(&mut bytes, 60, u16::MAX);
    assert_eq!(Fmc::parse(&bytes).err(), Some(Error::Header));
    let mut bytes = fmc();
    put64(&mut bytes, 128 + 24, u64::MAX);
    assert_eq!(Fmc::parse(&bytes).err(), Some(Error::Bounds));
    assert_eq!(
        Span {
            offset: usize::MAX,
            size: 2
        }
        .bytes(&[]),
        None
    );
}

fn gsp_container() -> Vec<u8> {
    let names = b"\0.fwimage\0.fwsignature_gb20x\0.fwversion\0.note.gnu.build-id\0.shstrtab\0";
    let strings_at = 8302;
    let table = (strings_at + names.len() + 7) & !7;
    let mut bytes = vec![0u8; table + 6 * 64];
    bytes[..8].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    put16(&mut bytes, 16, 1);
    put16(&mut bytes, 18, 243);
    put32(&mut bytes, 20, 1);
    put64(&mut bytes, 40, table as u64);
    for (at, value) in [(52, 64), (58, 64), (60, 6), (62, 5)] {
        put16(&mut bytes, at, value);
    }
    for (index, (offset, size, kind)) in [
        (64, 4096, 1),
        (4160, 4096, 1),
        (8256, 10, 1),
        (8266, 36, 7),
        (strings_at as u64, names.len() as u64, 3),
    ]
    .into_iter()
    .enumerate()
    {
        let at = table + (index + 1) * 64;
        let label: &[u8] = match index {
            0 => b".fwimage",
            1 => b".fwsignature_gb20x",
            2 => b".fwversion",
            3 => b".note.gnu.build-id",
            _ => b".shstrtab",
        };
        let position = names
            .windows(label.len())
            .position(|window| window == label)
            .unwrap();
        put32(&mut bytes, at, position as u32);
        put32(&mut bytes, at + 4, kind);
        put64(&mut bytes, at + 24, offset);
        put64(&mut bytes, at + 32, size);
    }
    bytes[strings_at..strings_at + names.len()].copy_from_slice(names);
    bytes[8256..8266].copy_from_slice(b"615.78.08\0");
    bytes
}

#[test]
fn gsp_metadata_is_bounded_and_selects_only_blackwell_signature() {
    let bytes = gsp_container();
    let parsed = gsp::Gsp::parse(&bytes[..]).unwrap();
    assert_eq!(
        (
            parsed.image.offset,
            parsed.image.size,
            parsed.signature.size
        ),
        (64, 4096, 4096)
    );
    for length in 0..bytes.len() {
        assert!(gsp::Gsp::parse(&bytes[..length]).is_err());
    }
    let table = wide(&bytes, 40).unwrap();
    for (at, value) in [
        (40, u64::MAX),
        (table + 64 + 24, u64::MAX),
        (table + 128 + 24, 64),
    ] {
        let mut changed = bytes.clone();
        put64(&mut changed, at, value);
        assert!(gsp::Gsp::parse(&changed[..]).is_err());
    }
    let mut changed = bytes.clone();
    changed[8256] = b'5';
    assert_eq!(gsp::Gsp::parse(&changed[..]).err(), Some(Error::Version));
    let mut changed = bytes.clone();
    put32(&mut changed, table + 128, 1);
    assert_eq!(gsp::Gsp::parse(&changed[..]).err(), Some(Error::Section));
    assert_eq!(gsp::verify(&bytes[..], false), Err(Error::ReleaseMismatch));
}

#[test]
fn radix_walk_recovers_noncontiguous_pages_across_leaf_boundary() {
    use radix::{PAGE, Plan};
    let plan = Plan::new(512 * PAGE + 13).unwrap();
    assert_eq!((plan.data_pages(), plan.table_pages()), (513, 4));
    let tables = [0x2000, 0x8000, 0xc000, 0x11000];
    let data: Vec<u64> = (0..513)
        .map(|index| 0x100000 + index * 3 * PAGE as u64)
        .collect();
    let mut bytes = vec![[0u8; PAGE]; 4];
    plan.write(&tables, &data, |index, page| {
        bytes[index] = *page;
        Ok(())
    })
    .unwrap();
    let entry = |page: usize, index: usize| {
        u64::from_le_bytes(bytes[page][index * 8..index * 8 + 8].try_into().unwrap())
    };
    // Independently walk root -> middle -> leaf -> data, rather than compare
    // the builder with another call to the same builder.
    for (index, &expected) in data.iter().enumerate() {
        let middle_address = entry(0, index / (512 * 512));
        let middle = tables
            .iter()
            .position(|&address| address == middle_address)
            .unwrap();
        let leaf_address = entry(middle, (index / 512) % 512);
        let leaf = tables
            .iter()
            .position(|&address| address == leaf_address)
            .unwrap();
        assert_eq!(entry(leaf, index % 512), expected);
    }
    assert!(bytes[0][8..].iter().all(|&b| b == 0));
    assert!(bytes[1][16..].iter().all(|&b| b == 0));
    assert!(bytes[3][8..].iter().all(|&b| b == 0));
}

#[test]
fn radix_rejects_bad_geometry_addresses_and_sink_failures() {
    use radix::{MAX_DATA, PAGE, Plan};
    for size in [0, MAX_DATA + 1, usize::MAX] {
        assert!(Plan::new(size).is_err());
    }
    assert_eq!(Plan::new(MAX_DATA).unwrap().data_pages(), MAX_DATA / PAGE);
    let plan = Plan::new(1).unwrap();
    let mut called = false;
    for tables in [
        [0, 8192, 12288],
        [1, 8192, 12288],
        [u64::MAX - 4095, 8192, 12288],
    ] {
        assert!(
            plan.write(&tables, &[16384], |_, _| {
                called = true;
                Ok(())
            })
            .is_err()
        );
    }
    assert!(!called);
    assert!(
        plan.write(&[4096, 8192, 12288], &[], |_, _| Ok(()))
            .is_err()
    );
    assert_eq!(
        plan.write(&[4096, 8192, 12288], &[16384], |_, _| Err(Error::Truncated)),
        Err(Error::Truncated)
    );
}

fn ivrs_fixture(entries: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0u8; 48 + 24 + entries.len()];
    bytes[..4].copy_from_slice(b"IVRS");
    let length = bytes.len() as u32;
    put32(&mut bytes, 4, length);
    bytes[8] = 2;
    bytes[48] = 0x10;
    put16(&mut bytes, 50, (24 + entries.len()) as u16);
    put16(&mut bytes, 52, 0x30);
    put16(&mut bytes, 54, 0x40);
    put64(&mut bytes, 56, 0xfed80000);
    bytes[72..].copy_from_slice(entries);
    bytes[9] = 0u8.wrapping_sub(bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b)));
    bytes
}
#[test]
fn amdvi_firmware_scopes_ranges_aliases_and_checksum_are_checked() {
    let bytes = ivrs_fixture(&[3, 0, 1, 0, 4, 0xff, 1, 0, 0x42, 0, 2, 0, 0, 0, 3, 0]);
    let table = amdvi::Ivrs::parse(&bytes).unwrap();
    let unit = table.units().next().unwrap();
    assert_eq!(unit.requester(0x108), Ok(Some(0x108)));
    assert_eq!(unit.requester(0x200), Ok(Some(0x300)));
    assert_eq!(unit.requester(0x201), Ok(None));
    for length in 0..bytes.len() {
        assert!(amdvi::Ivrs::parse(&bytes[..length]).is_err());
    }
    let mut bad = bytes.clone();
    bad[49] ^= 1;
    assert!(amdvi::Ivrs::parse(&bad).is_err());
    for entries in [
        &[3, 0, 1, 0][..],
        &[4, 0, 1, 0][..],
        &[0x42, 0, 1, 0][..],
        &[0xf0, 0, 1, 0][..],
    ] {
        assert!(amdvi::Ivrs::parse(&ivrs_fixture(entries)).is_err());
    }
}
#[test]
fn amdvi_paging_permissions_widths_and_commands_match_protocol() {
    assert_eq!(
        amdvi::device_entry(0x1000, 7).unwrap(),
        [amdvi::RW | 0x1603, 7, 0, 0]
    );
    assert_eq!(
        amdvi::pointer(0x2000, 0, amdvi::READ),
        Ok(amdvi::READ | 0x2001)
    );
    for address in [0, 1, 0x1001, 1u64 << 52, u64::MAX] {
        assert!(amdvi::pointer(address, 0, amdvi::RW).is_err());
    }
    assert!(amdvi::pointer(0x1000, 4, amdvi::RW).is_err());
    assert!(amdvi::pointer(0x1000, 0, 0).is_err());
    assert!(amdvi::pointer(0x1000, 0, 1).is_err());
    assert_eq!(amdvi::invalidate_device(0x1234), [0x2000000000001234, 0]);
    assert_eq!(
        amdvi::invalidate_domain(7),
        [0x3000000700000000, 0x7ffffffffffff001]
    );
    assert_eq!(
        amdvi::completion(0x1008, 0x55),
        Ok([0x1000000000001009, 0x55])
    );
    assert!(amdvi::completion(0x1001, 0).is_err());
}
