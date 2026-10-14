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
