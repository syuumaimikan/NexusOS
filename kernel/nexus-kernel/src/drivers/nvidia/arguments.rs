//! Owned, unpublished GB206 initial boot buffers. No hardware authentication,
//! MMIO submission or device execution occurs here. Drop is safe only while
//! these allocations remain CPU-owned; a future activation must transfer them.
use super::bootstrap::{DmaImage, Error};
use alloc::vec::Vec;
use nexus_nvidia::{
    amdvi,
    boot::{self, Buffer, Firmware, Sizing},
};

pub(super) struct Prepared {
    bootstrap: super::bootstrap::Prepared,
    frts: DmaImage,
    shared: DmaImage,
    meta: DmaImage,
    fmc_args: DmaImage,
    rm_args: DmaImage,
    libos: DmaImage,
    cot: DmaImage,
    cot_packet: [u8; nexus_nvidia::fsp::COT_PACKET_SIZE],
    logs: Vec<DmaImage>,
}
impl Prepared {
    pub(super) fn new(
        bootstrap: super::bootstrap::Prepared,
        firmware: Firmware,
        ucodes: Buffer,
    ) -> Result<Self, Error> {
        Self::new_with(bootstrap, firmware, ucodes, DmaImage::zero)
    }
    fn new_with(
        bootstrap: super::bootstrap::Prepared,
        firmware: Firmware,
        ucodes: Buffer,
        mut allocate: impl FnMut(usize) -> Result<DmaImage, Error>,
    ) -> Result<Self, Error> {
        // Production GB206 board sizing. This is a boot request, not a VRAM
        // observation; authenticated FMC still determines the WPR positions.
        let sizing = Sizing {
            heap: 137 * 1024 * 1024,
            pmu_reserved: boot::GB206_PMU_RESERVED,
        };
        let mut output = Self {
            bootstrap,
            frts: allocate(boot::FRTS_SIZE)?,
            shared: allocate(boot::SHARED_SIZE)?,
            meta: allocate(boot::WPR_SIZE)?,
            fmc_args: allocate(boot::FMC_SIZE)?,
            rm_args: allocate(boot::PAGE)?,
            libos: allocate(boot::PAGE)?,
            cot: allocate(boot::COT_SIZE)?,
            cot_packet: [0; nexus_nvidia::fsp::COT_PACKET_SIZE],
            logs: Vec::new(),
        };
        output
            .logs
            .try_reserve_exact(7)
            .map_err(|_| Error::Memory)?;
        for size in boot::LOG_SIZES {
            let mut log = allocate(size)?;
            let address = log.buffer().address;
            log.bytes_mut()[8..16].copy_from_slice(&address.to_le_bytes());
            output.logs.push(log);
        }
        // Shared PTE array includes itself, followed by both 256 KiB queues.
        let shared_address = output.shared.buffer().address;
        for index in 0..boot::PTE_COUNT {
            output.shared.bytes_mut()[index * 8..index * 8 + 8]
                .copy_from_slice(&(shared_address + (index * boot::PAGE) as u64).to_le_bytes());
        }
        output.shared.bytes_mut()[boot::CMD_OFFSET..boot::CMD_OFFSET + 32]
            .copy_from_slice(&boot::queue_header());
        // Remote GSP owns its status header; leave it and all entries zero.
        output
            .meta
            .bytes_mut()
            .copy_from_slice(&boot::wpr(&firmware, &sizing).map_err(Error::Image)?);
        output.fmc_args.bytes_mut().copy_from_slice(
            &boot::fmc(output.meta.buffer(), output.libos.buffer(), false, false)
                .map_err(Error::Image)?,
        );
        output.rm_args.bytes_mut()[..boot::RM_ARGS_SIZE]
            .copy_from_slice(&boot::rm_args(output.shared.buffer(), ucodes).map_err(Error::Image)?);
        let mut logs = [Buffer {
            address: 0,
            size: 0,
        }; 7];
        for (slot, log) in logs.iter_mut().zip(&output.logs) {
            *slot = log.buffer();
        }
        output
            .libos
            .bytes_mut()
            .copy_from_slice(&boot::libos(&logs, output.rm_args.buffer()).map_err(Error::Image)?);
        output.cot.bytes_mut().copy_from_slice(
            &boot::cot(
                output.bootstrap.fmc.buffer(),
                output.frts.buffer(),
                output.fmc_args.buffer(),
                &sizing,
                &output.bootstrap.fmc_hash,
                &output.bootstrap.fmc_public_key,
                &output.bootstrap.fmc_signature,
            )
            .map_err(Error::Image)?,
        );
        output.cot_packet =
            nexus_nvidia::fsp::cot_packet(output.cot.bytes()).map_err(Error::Image)?;
        output.validate()?;
        Ok(output)
    }
    fn validate(&self) -> Result<(), Error> {
        let buffers = [
            &self.frts,
            &self.shared,
            &self.meta,
            &self.fmc_args,
            &self.rm_args,
            &self.libos,
            &self.cot,
        ];
        if buffers
            .into_iter()
            .chain(self.logs.iter())
            .any(|image| !image.padding_is_zero())
        {
            return Err(Error::Memory);
        }
        if self.meta.bytes()[248..256] != [0; 8]
            || self.cot_packet[..8] != [1, 0, 0, 0xc0, 0x7e, 0xde, 0x10, 0x14]
            || self.cot_packet[8..] != *self.cot.bytes()
            || self.shared.bytes()[boot::PTE_COUNT * 8..boot::CMD_OFFSET]
                .iter()
                .any(|&byte| byte != 0)
            || self.shared.bytes()[boot::STATUS_OFFSET..]
                .iter()
                .any(|&byte| byte != 0)
            || self.shared.bytes()[boot::CMD_OFFSET + 32..boot::STATUS_OFFSET]
                .iter()
                .any(|&byte| byte != 0)
            || self.frts.bytes().iter().any(|&byte| byte != 0)
            || self.rm_args.bytes()[boot::RM_ARGS_SIZE..]
                .iter()
                .any(|&byte| byte != 0)
            || self.libos.bytes()[8 * 32..].iter().any(|&byte| byte != 0)
            || self.cot.bytes()[36..84] != self.bootstrap.fmc_hash
            || self.cot.bytes()[84..181] != self.bootstrap.fmc_public_key
            || self.cot.bytes()[468..564] != self.bootstrap.fmc_signature
        {
            return Err(Error::Memory);
        }
        let shared = self.shared.buffer().address;
        for (index, pte) in self.shared.bytes()[..boot::PTE_COUNT * 8]
            .chunks_exact(8)
            .enumerate()
        {
            if u64::from_le_bytes(pte.try_into().map_err(|_| Error::Memory)?)
                != shared + (index * boot::PAGE) as u64
            {
                return Err(Error::Memory);
            }
        }
        for log in &self.logs {
            if log.bytes()[..8] != [0; 8]
                || log.bytes()[8..16] != log.buffer().address.to_le_bytes()
                || log.bytes()[16..].iter().any(|&byte| byte != 0)
            {
                return Err(Error::Memory);
            }
        }
        Ok(())
    }
    pub(super) fn map_into(&self, domain: &mut crate::drivers::amdvi::Domain) -> Result<(), Error> {
        for image in [
            &self.bootstrap.boot,
            &self.bootstrap.fmc,
            &self.fmc_args,
            &self.rm_args,
            &self.libos,
            &self.cot,
        ] {
            image.map_into(domain, amdvi::READ)?;
        }
        for image in [&self.frts, &self.shared, &self.meta]
            .into_iter()
            .chain(self.logs.iter())
        {
            image.map_into(domain, amdvi::RW)?;
        }
        Ok(())
    }
    pub(super) fn report(&self) {
        self.bootstrap.report();
        crate::kprintln!(
            "[nv  ] GB206 boot ABI ready: WPR={:#x}, FMC args={:#x}, packed COT={} bytes; verified=0",
            self.meta.buffer().address,
            self.fmc_args.buffer().address,
            self.cot.bytes().len()
        );
        crate::kprintln!(
            "[nv  ] FSP COT single-packet={} bytes staged; no EMEM writes or FSP response observed",
            self.cot_packet.len()
        );
        crate::kprintln!(
            "[nv  ] RPC shared={} bytes/{} PTEs, seven Libos logs, FRTS={} bytes; remote status remains zero, no GPU submission",
            self.shared.bytes().len(),
            boot::PTE_COUNT,
            self.frts.bytes().len()
        );
    }
    #[cfg(feature = "nvidia-gsp-test")]
    pub(super) fn allocation_refusal(firmware: Firmware, ucodes: Buffer) -> Result<(), Error> {
        let before = crate::memory::stats().ok_or(Error::Memory)?.free_frames;
        let bootstrap = super::bootstrap::prepare()?;
        let mut left = 9;
        let result = Self::new_with(bootstrap, firmware, ucodes, |size| {
            if left == 0 {
                return Err(Error::Memory);
            }
            left -= 1;
            DmaImage::zero(size)
        });
        // Failed construction reclaims both bootstrap and argument buffers.
        if !matches!(result, Err(Error::Memory))
            || crate::memory::stats().ok_or(Error::Memory)?.free_frames != before
        {
            return Err(Error::Memory);
        }
        Ok(())
    }
}
