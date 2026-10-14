//! Three levels of 4 KiB pages containing 512 little-endian addresses each.
//! Layout reference: pinned NVIDIA kgspCreateRadix3 and libos_init_args.h.
//! Addresses must belong to the caller's mapping; this creates no IOMMU mapping.
use crate::Error;
pub const PAGE: usize = 4096;
pub const ENTRIES: usize = PAGE / 8;
pub const MAX_DATA: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub struct Plan {
    data_pages: usize,
    middle_pages: usize,
    table_pages: usize,
}
impl Plan {
    pub fn new(size: usize) -> Result<Self, Error> {
        if size == 0 || size > MAX_DATA {
            return Err(Error::Bounds);
        }
        let data_pages = size.div_ceil(PAGE);
        let leaf_pages = data_pages.div_ceil(ENTRIES);
        let middle_pages = leaf_pages.div_ceil(ENTRIES);
        Ok(Self {
            data_pages,
            middle_pages,
            table_pages: 1 + middle_pages + leaf_pages,
        })
    }

    /// Validate all supplied addresses before writing any table bytes.
    /// The caller must supply distinct, exclusively owned table/data pages.
    pub fn write(
        &self,
        tables: &[u64],
        data: &[u64],
        mut write_page: impl FnMut(usize, &[u8; PAGE]) -> Result<(), Error>,
    ) -> Result<(), Error> {
        if tables.len() != self.table_pages || data.len() != self.data_pages {
            return Err(Error::Bounds);
        }
        // Sorted scratch is unnecessary: bounded O(n²) would be too slow for
        // real firmware. Ownership/uniqueness is checked by the caller; here
        // ensure every address is aligned, nonzero and a complete u64 page.
        if tables.iter().chain(data).any(|&address| {
            address == 0 || address % PAGE as u64 != 0 || address.checked_add(PAGE as u64).is_none()
        }) {
            return Err(Error::Bounds);
        }
        for index in 0..self.table_pages {
            let mut page = [0u8; PAGE];
            let next = if index == 0 {
                &tables[1..1 + self.middle_pages]
            } else if index <= self.middle_pages {
                let first = 1 + self.middle_pages + (index - 1) * ENTRIES;
                &tables[first..(first + ENTRIES).min(tables.len())]
            } else {
                let first = (index - 1 - self.middle_pages) * ENTRIES;
                &data[first..(first + ENTRIES).min(data.len())]
            };
            for (entry, address) in next.iter().enumerate() {
                page[entry * 8..entry * 8 + 8].copy_from_slice(&address.to_le_bytes());
            }
            write_page(index, &page)?;
        }
        Ok(())
    }
    pub fn data_pages(&self) -> usize {
        self.data_pages
    }
    pub fn table_pages(&self) -> usize {
        self.table_pages
    }
}
