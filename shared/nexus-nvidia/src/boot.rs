//! Byte-wise initial-boot ABIs from pinned NVIDIA gspifpub.h, WPR metadata,
//! kern_fsp_cot_payload.h and msgq_priv.h. These encodings do not start a GPU.
use crate::Error;
pub const WPR_SIZE: usize = 256;
pub const FMC_SIZE: usize = 256;
pub const COT_SIZE: usize = 860;
pub const RM_ARGS_SIZE: usize = 184;
pub const PAGE: usize = 4096;
pub const QUEUE_SIZE: usize = 0x40000;
pub const SHARED_SIZE: usize = PAGE + 2 * QUEUE_SIZE;
pub const PTE_COUNT: usize = SHARED_SIZE / PAGE;
pub const CMD_OFFSET: usize = PAGE;
pub const STATUS_OFFSET: usize = PAGE + QUEUE_SIZE;
pub const FRTS_SIZE: usize = 1024 * 1024;
pub const VGA_SIZE: u64 = 128 * 1024;
/// GB206 release firmware: PMU backing + surfaces + misc, rounded to 128 KiB.
pub const GB206_PMU_RESERVED: u32 = 0x19a0000;
pub const LOG_SIZES: [usize; 7] = [65536, 65536, 65536, 65536, 65536, 4096, 4096];
pub const LOG_NAMES: [&[u8]; 7] = [
    b"LOGINIT", b"LOGINTR", b"LOGRM", b"LOGMNOC", b"LOGDBG", b"LOGROOT", b"LOGRMON",
];

fn put16(bytes: &mut [u8], at: usize, value: u16) {
    bytes[at..at + 2].copy_from_slice(&value.to_le_bytes());
}
fn put32(bytes: &mut [u8], at: usize, value: u32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}
fn put64(bytes: &mut [u8], at: usize, value: u64) {
    bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
}
#[derive(Debug, Clone, Copy)]
pub struct Buffer {
    pub address: u64,
    pub size: u64,
}
impl Buffer {
    pub fn check(self) -> Result<(), Error> {
        if self.address == 0
            || self.address % 4096 != 0
            || self.size == 0
            || self.address.checked_add(self.size).ok_or(Error::Bounds)? > 1 << 52
        {
            return Err(Error::Bounds);
        }
        Ok(())
    }
}
pub struct Firmware {
    pub rm: Buffer,
    pub bootloader: Buffer,
    pub signature: Buffer,
    pub code_offset: u64,
    pub data_offset: u64,
    pub manifest_offset: u64,
}
pub struct Sizing {
    pub heap: u64,
    pub pmu_reserved: u32,
}
impl Sizing {
    pub fn check(&self) -> Result<(), Error> {
        if !(88 * 1024 * 1024..=280 * 1024 * 1024).contains(&self.heap)
            || self.heap % (1024 * 1024) != 0
            || self.pmu_reserved % (128 * 1024) != 0
        {
            return Err(Error::Bounds);
        }
        Ok(())
    }
}

/// Hopper/Blackwell initial request. ACR/FMC fills the FB offsets and verified
/// state; host code must not invent these or set the authenticated marker.
pub fn wpr(firmware: &Firmware, sizing: &Sizing) -> Result<[u8; WPR_SIZE], Error> {
    firmware.rm.check()?;
    firmware.bootloader.check()?;
    firmware.signature.check()?;
    sizing.check()?;
    if firmware.signature.size != 4096 {
        return Err(Error::Bounds);
    }
    for offset in [
        firmware.code_offset,
        firmware.data_offset,
        firmware.manifest_offset,
    ] {
        if offset % 256 != 0 || offset >= firmware.bootloader.size {
            return Err(Error::Bounds);
        }
    }
    let mut bytes = [0u8; WPR_SIZE];
    put64(&mut bytes, 0, 0xdc3aae21371a60b3);
    put64(&mut bytes, 8, 1);
    for (at, value) in [
        (16, firmware.rm.address),
        (24, firmware.rm.size),
        (32, firmware.bootloader.address),
        (40, firmware.bootloader.size),
        (48, firmware.code_offset),
        (56, firmware.data_offset),
        (64, firmware.manifest_offset),
        (72, firmware.signature.address),
        (80, firmware.signature.size),
        (104, 3 * 1024 * 1024),
        (128, sizing.heap),
        (160, FRTS_SIZE as u64),
        (192, VGA_SIZE),
    ] {
        put64(&mut bytes, at, value);
    }
    put32(&mut bytes, 244, sizing.pmu_reserved);
    Ok(bytes)
}

/// FMC -> GSP-RM initial boot. Platform flags require hardware qualification;
/// this routine accepts the caller's policy rather than fabricating it.
pub fn fmc(meta: Buffer, libos: Buffer, icu: bool, scrub: bool) -> Result<[u8; FMC_SIZE], Error> {
    meta.check()?;
    libos.check()?;
    if meta.size != WPR_SIZE as u64 || libos.size != PAGE as u64 {
        return Err(Error::Bounds);
    }
    let mut bytes = [0u8; FMC_SIZE];
    put32(&mut bytes, 0, 0x20434d46);
    put16(&mut bytes, 4, FMC_SIZE as u16);
    put32(&mut bytes, 64, 1);
    put32(&mut bytes, 68, WPR_SIZE as u32);
    put64(&mut bytes, 72, meta.address);
    bytes[92] = 1;
    bytes[94] = icu as u8;
    bytes[95] = scrub as u8;
    put32(&mut bytes, 128, 1);
    put64(&mut bytes, 136, libos.address);
    Ok(bytes)
}

/// Packed version-2 chain-of-trust request. Keep opaque production ECC bytes
/// unchanged, zero-pad the fixed-width key/signature slots, and send no data.
pub fn cot(
    fmc_image: Buffer,
    frts: Buffer,
    args: Buffer,
    sizing: &Sizing,
    hash: &[u8; 48],
    key: &[u8; 97],
    signature: &[u8; 96],
) -> Result<[u8; COT_SIZE], Error> {
    fmc_image.check()?;
    frts.check()?;
    args.check()?;
    sizing.check()?;
    if frts.size != FRTS_SIZE as u64 || args.size != FMC_SIZE as u64 || key[0] != 4 {
        return Err(Error::Bounds);
    }
    let end_offset = VGA_SIZE
        .checked_add(sizing.pmu_reserved as u64)
        .ok_or(Error::Bounds)?;
    let end_offset = end_offset.checked_add(VGA_SIZE - 1).ok_or(Error::Bounds)? & !(VGA_SIZE - 1);
    let mut bytes = [0u8; COT_SIZE];
    put16(&mut bytes, 0, 2);
    put16(&mut bytes, 2, COT_SIZE as u16);
    put64(&mut bytes, 4, fmc_image.address);
    put64(&mut bytes, 12, frts.address);
    put32(&mut bytes, 20, FRTS_SIZE as u32);
    put64(&mut bytes, 24, end_offset);
    put32(&mut bytes, 32, FRTS_SIZE as u32);
    bytes[36..84].copy_from_slice(hash);
    bytes[84..181].copy_from_slice(key);
    bytes[468..564].copy_from_slice(signature);
    put64(&mut bytes, 852, args.address);
    Ok(bytes)
}

pub fn queue_header() -> [u8; 32] {
    let mut bytes = [0u8; 32];
    put16(&mut bytes, 0, 2);
    put16(&mut bytes, 2, 0);
    put32(&mut bytes, 4, QUEUE_SIZE as u32);
    put32(&mut bytes, 8, PAGE as u32);
    put32(&mut bytes, 12, (QUEUE_SIZE / PAGE - 1) as u32);
    put32(&mut bytes, 16, PAGE as u32);
    bytes
}
pub fn rm_args(shared: Buffer, ucodes: Buffer) -> Result<[u8; RM_ARGS_SIZE], Error> {
    shared.check()?;
    ucodes.check()?;
    if shared.size != SHARED_SIZE as u64 {
        return Err(Error::Bounds);
    }
    let mut bytes = [0u8; RM_ARGS_SIZE];
    put32(&mut bytes, 0, 0x20505347);
    put16(&mut bytes, 4, RM_ARGS_SIZE as u16);
    put64(&mut bytes, 8, 2); // normal boot with DMEM stack, paging disabled.
    put64(&mut bytes, 24, shared.address);
    put64(&mut bytes, 32, CMD_OFFSET as u64);
    put64(&mut bytes, 40, STATUS_OFFSET as u64);
    for (at, value) in [
        (48, 24),
        (52, PAGE as u32),
        (56, 16 * PAGE as u32),
        (60, 4),
        (64, 12),
        (68, PTE_COUNT as u32),
    ] {
        put32(&mut bytes, at, value);
    }
    put64(&mut bytes, 152, ucodes.address);
    put64(&mut bytes, 160, ucodes.size);
    Ok(bytes)
}

/// Libos regions have 32-byte natural C stride. LOGINIT precedes RMARGS for
/// early logging; the remote owns log bytes and initializes its own RX header.
pub fn libos(logs: &[Buffer; 7], args: Buffer) -> Result<[u8; PAGE], Error> {
    args.check()?;
    if args.size != PAGE as u64 {
        return Err(Error::Bounds);
    }
    let mut bytes = [0u8; PAGE];
    for (index, (name, buffer)) in LOG_NAMES
        .iter()
        .copied()
        .zip(logs.iter().copied())
        .chain(core::iter::once((b"RMARGS".as_slice(), args)))
        .enumerate()
    {
        buffer.check()?;
        if index < 7 && buffer.size != LOG_SIZES[index] as u64 {
            return Err(Error::Bounds);
        }
        let at = index * 32;
        let id = name.iter().fold(0u64, |id, &byte| (id << 8) | byte as u64);
        put64(&mut bytes, at, id);
        put64(&mut bytes, at + 8, buffer.address);
        put64(&mut bytes, at + 16, buffer.size);
        bytes[at + 24] = 1;
        bytes[at + 25] = 1;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn buffer(address: u64, size: usize) -> Buffer {
        Buffer {
            address,
            size: size as u64,
        }
    }
    fn u64_at(bytes: &[u8], at: usize) -> u64 {
        u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
    }
    fn sizing() -> Sizing {
        Sizing {
            heap: 137 << 20,
            pmu_reserved: GB206_PMU_RESERVED,
        }
    }
    #[test]
    fn sdk_boot_layout_preserves_authentication_ownership() {
        let firmware = Firmware {
            rm: buffer(0x1_0000_0000, 26992640),
            bootloader: buffer(0x1_1000_0000, 290816),
            signature: buffer(0x1_2000_0000, 4096),
            code_offset: 61952,
            data_offset: 2560,
            manifest_offset: 0,
        };
        let meta = wpr(&firmware, &sizing()).unwrap();
        assert_eq!(
            &meta[..16],
            &[
                0xb3, 0x60, 0x1a, 0x37, 0x21, 0xae, 0x3a, 0xdc, 1, 0, 0, 0, 0, 0, 0, 0
            ]
        );
        assert_eq!(u64_at(&meta, 16), firmware.rm.address);
        // Host has no authenticated FB placement. All firmware-owned values,
        // including the final verified word, must still be zero.
        for (start, end) in [
            (88, 104),
            (112, 128),
            (136, 160),
            (168, 192),
            (200, 244),
            (248, 256),
        ] {
            assert!(meta[start..end].iter().all(|&byte| byte == 0));
        }
        let mut key = [0x81; 97];
        key[0] = 4;
        let cot = cot(
            buffer(0x1_3000_0000, 287304),
            buffer(0x1_4000_0000, FRTS_SIZE),
            buffer(0x1_5000_0000, FMC_SIZE),
            &sizing(),
            &[0x52; 48],
            &key,
            &[0xe3; 96],
        )
        .unwrap();
        assert_eq!(&cot[..4], &[2, 0, 0x5c, 3]); // SDK packed size 860, not 864.
        assert_eq!(u64_at(&cot, 24), 0x19c0000);
        assert_eq!(&cot[84..181], &key);
        assert!(cot[181..468].iter().all(|&byte| byte == 0));
        assert_eq!(&cot[468..564], &[0xe3; 96]);
        assert!(cot[564..852].iter().all(|&byte| byte == 0));
        assert_eq!(u64_at(&cot, 852), 0x1_5000_0000);
    }
    #[test]
    fn sdk_queue_and_region_conventions() {
        assert_eq!(
            queue_header(),
            [
                2, 0, 0, 0, 0, 0, 4, 0, 0, 16, 0, 0, 63, 0, 0, 0, 0, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0, 0
            ]
        );
        let rm = rm_args(
            buffer(0x1_0000_0000, SHARED_SIZE),
            buffer(0x1_2000_0000, 58215936),
        )
        .unwrap();
        assert_eq!(&rm[..8], b"GSP \xb8\x00\x00\x00");
        assert_eq!(u64_at(&rm, 40), 0x41000);
        assert_eq!(&rm[68..72], &[129, 0, 0, 0]);
        let logs = core::array::from_fn(|index| {
            buffer(0x1_3000_0000 + (index as u64) * 0x10000, LOG_SIZES[index])
        });
        let regions = libos(&logs, buffer(0x1_4000_0000, PAGE)).unwrap();
        assert_eq!(&regions[..8], b"TINIGOL\0"); // SDK big-endian numeric ID, LE storage.
        assert_eq!(&regions[7 * 32..7 * 32 + 8], b"SGRAMR\0\0");
        for entry in regions[..8 * 32].chunks_exact(32) {
            assert_eq!(&entry[24..32], &[1, 1, 0, 0, 0, 0, 0, 0]);
        }
        assert!(regions[256..].iter().all(|&byte| byte == 0));
    }
    #[test]
    fn invalid_boot_storage_is_refused() {
        for bad in [
            buffer(0, PAGE),
            buffer(4097, PAGE),
            buffer(4096, 0),
            buffer(1 << 52, PAGE),
            buffer(u64::MAX - 4095, PAGE),
        ] {
            assert!(bad.check().is_err());
        }
        assert!(
            Sizing {
                heap: 137 << 20,
                pmu_reserved: 4096
            }
            .check()
            .is_err()
        );
        assert!(rm_args(buffer(4096, SHARED_SIZE - 1), buffer(8192, PAGE)).is_err());
        let mut logs = core::array::from_fn(|index| {
            buffer(0x100000 + (index as u64) * 0x10000, LOG_SIZES[index])
        });
        logs[6].size = 65536;
        assert!(libos(&logs, buffer(0x200000, PAGE)).is_err());
    }
}
