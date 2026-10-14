//! Checked PCI memory BAR geometry used by the native NVIDIA owner. Inputs are
//! the saved BAR and sizing readback, obtained with decoding and DMA disabled.
use crate::Error;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryWindow {
    pub base: u64,
    pub size: u64,
    pub sixty_four: bool,
    pub prefetch: bool,
}
impl MemoryWindow {
    pub fn decode(low: u32, high: u32, mask_low: u32, mask_high: u32) -> Result<Self, Error> {
        let kind = (low >> 1) & 3;
        if low & 1 != 0 || !matches!(kind, 0 | 2) || mask_low & 15 != low & 15 {
            return Err(Error::Header);
        }
        let sixty_four = kind == 2;
        let base = (low & !15) as u64 | if sixty_four { (high as u64) << 32 } else { 0 };
        let mask = (mask_low & !15) as u64
            | if sixty_four {
                (mask_high as u64) << 32
            } else {
                0xffff_ffff_0000_0000
            };
        let size = (!mask).checked_add(1).ok_or(Error::Bounds)?;
        if base == 0
            || size < 16
            || !size.is_power_of_two()
            || base % size != 0
            || base.checked_add(size).ok_or(Error::Bounds)? > 1 << 52
        {
            return Err(Error::Bounds);
        }
        Ok(Self {
            base,
            size,
            sixty_four,
            prefetch: low & 8 != 0,
        })
    }
    pub fn contains(self, offset: u64, length: u64) -> Option<u64> {
        if length == 0 || offset.checked_add(length)? > self.size {
            return None;
        }
        self.base.checked_add(offset)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sizing_and_aperture_bounds_preserve_full_64_bit_addresses() {
        let edu = MemoryWindow::decode(0xfea00000, 0, 0xfff00000, 0).unwrap();
        assert_eq!(edu.size, 1 << 20);
        let card = MemoryWindow::decode(0x80000004, 0x10, 0xfe000004, u32::MAX).unwrap();
        assert_eq!(card.base, 0x10_8000_0000);
        assert_eq!(card.size, 32 << 20);
        assert_eq!(card.contains(0x8f2000, 4096), Some(0x10_808f_2000));
        assert_eq!(
            card.contains(card.size - 4, 4),
            Some(card.base + card.size - 4)
        );
        for (offset, length) in [(card.size, 1), (card.size - 4, 8), (0, 0), (u64::MAX, 4096)] {
            assert_eq!(card.contains(offset, length), None);
        }
        assert_eq!(edu.contains(0x8f2000, 4096), None);
    }
    #[test]
    fn malformed_or_unassigned_bar_is_refused() {
        for (low, high, mask_low, mask_high) in [
            (0, 0, 0, 0),
            (0xfea00001, 0, 0xfff00001, 0),
            (0xfea00002, 0, 0xfff00002, 0),
            (0xfea00000, 0, 0xfff00008, 0),
            (0xfea01000, 0, 0xfff00000, 0),
            (0xfea00000, 0, 0xfffa0000, 0),
            (4, 0x10, 4, 0),
            (4, 0x100000, 0xfe000004, u32::MAX),
        ] {
            assert!(MemoryWindow::decode(low, high, mask_low, mask_high).is_err());
        }
    }
}
