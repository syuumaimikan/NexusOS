//! GB206's GH100 EMEM protocol, from the pinned kern_fsp_gh100.c HAL and
//! mctp_format.h. This module owns no PCI device or DMA allocation. Hardware
//! activation must supply exclusive FSP ownership and keep all DMA alive.
pub const CHANNEL_SIZE: usize = 1024;
pub const COT_PACKET_SIZE: usize = crate::boot::COT_SIZE + 8;
pub const COT_TYPE: u32 = 0x14;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Register {
    CommandHead,
    CommandTail,
    MessageHead,
    MessageTail,
    Control,
    Data,
}
impl Register {
    /// Published BAR0-relative GH100 register map selected by GB206's HAL.
    pub const fn bar0_offset(self) -> usize {
        match self {
            Self::CommandHead => 0x8f2c00,
            Self::CommandTail => 0x8f2c04,
            Self::MessageHead => 0x8f2c80,
            Self::MessageTail => 0x8f2c84,
            Self::Control => 0x8f2ac0,
            Self::Data => 0x8f2ac4,
        }
    }
}
/// An exclusively owned, initialized FSP RM channel.
///
/// # Safety
/// Implementations must not
/// alias another transport. Hardware implementations must have established
/// PCI ownership, secure-boot readiness and pinned device DMA mappings; merely
/// discovering a BAR is insufficient. MMIO ordering must preserve call order.
pub unsafe trait Registers {
    type Error;
    fn read(&mut self, register: Register) -> Result<u32, Self::Error>;
    fn write(&mut self, register: Register, value: u32) -> Result<(), Self::Error>;
}
#[derive(Debug, PartialEq, Eq)]
pub enum Error<E> {
    Io(E),
    Bounds,
    Busy,
    Cursor,
    Header,
    Remote(u32),
}

pub fn cot_packet(payload: &[u8]) -> Result<[u8; COT_PACKET_SIZE], crate::Error> {
    if payload.len() != crate::boot::COT_SIZE || payload[..4] != [2, 0, 0x5c, 3] {
        return Err(crate::Error::Header);
    }
    let mut packet = [0u8; COT_PACKET_SIZE];
    packet[..8].copy_from_slice(&[1, 0, 0, 0xc0, 0x7e, 0xde, 0x10, 0x14]);
    packet[8..].copy_from_slice(payload);
    Ok(packet)
}
fn read<R: Registers>(io: &mut R, register: Register) -> Result<u32, Error<R::Error>> {
    io.read(register).map_err(Error::Io)
}
fn write<R: Registers>(io: &mut R, register: Register, value: u32) -> Result<(), Error<R::Error>> {
    io.write(register, value).map_err(Error::Io)
}
fn pointer<E>(value: u32) -> Result<u32, Error<E>> {
    if value % 4 != 0 || value >= CHANNEL_SIZE as u32 {
        return Err(Error::Bounds);
    }
    Ok(value)
}
fn cursor(value: u32) -> u32 {
    (value & 0xfffc) >> 2
}
/// One readiness sample, without unbounded polling or acknowledgement writes.
/// Callers use an OS deadline before retrying Busy; no successful response is
/// inferred from an empty channel.
pub fn ready<R: Registers>(io: &mut R) -> Result<bool, Error<R::Error>> {
    let head = pointer(read(io, Register::CommandHead)?)?;
    let tail = pointer(read(io, Register::CommandTail)?)?;
    let msg_head = pointer(read(io, Register::MessageHead)?)?;
    let msg_tail = pointer(read(io, Register::MessageTail)?)?;
    Ok(head == tail && msg_head == msg_tail)
}
/// Populate EMEM, check autoincrement, then publish TAIL before HEAD. HEAD is
/// the interrupt trigger; no trigger is written on a failed data/cursor check.
pub fn send<R: Registers>(io: &mut R, packet: &[u8]) -> Result<(), Error<R::Error>> {
    if packet.len() < 8 || packet.len() > CHANNEL_SIZE {
        return Err(Error::Bounds);
    }
    if !ready(io)? {
        return Err(Error::Busy);
    }
    write(io, Register::Control, 1 << 24)?;
    for chunk in packet.chunks(4) {
        let mut word = [0u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        write(io, Register::Data, u32::from_le_bytes(word))?;
    }
    let words = packet.len().div_ceil(4) as u32;
    if cursor(read(io, Register::Control)?) != words {
        return Err(Error::Cursor);
    }
    write(io, Register::CommandTail, words * 4 - 4)?;
    write(io, Register::CommandHead, 0)
}
/// Read a complete channel packet; leave absent/invalid packets untouched.
/// The GB206 HAL starts each packet at EMEM offset zero. A nonzero start is
/// refused rather than silently reading the wrong memory. Acknowledgement is
/// deferred until every data word and the auto-increment cursor were read.
pub fn receive<R: Registers>(
    io: &mut R,
    output: &mut [u8; CHANNEL_SIZE],
) -> Result<Option<usize>, Error<R::Error>> {
    let head = pointer(read(io, Register::MessageHead)?)?;
    let tail = pointer(read(io, Register::MessageTail)?)?;
    if head == tail {
        return Ok(None);
    }
    if head != 0 || tail < head {
        return Err(Error::Bounds);
    }
    let length = (tail - head + 4) as usize;
    if length < 8 || length > CHANNEL_SIZE {
        return Err(Error::Bounds);
    }
    write(io, Register::Control, 1 << 25)?;
    output.fill(0);
    for chunk in output[..length].chunks_exact_mut(4) {
        chunk.copy_from_slice(&read(io, Register::Data)?.to_le_bytes());
    }
    if cursor(read(io, Register::Control)?) != (length / 4) as u32 {
        return Err(Error::Cursor);
    }
    write(io, Register::MessageTail, head)?;
    write(io, Register::MessageHead, head)?;
    Ok(Some(length))
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Response {
    pub task: u32,
    pub command: u32,
    pub error: u32,
}
impl Response {
    /// COT acknowledgement is a single packet: MCTP 1.0, zero tag/sequence,
    /// NVIDIA PCI vendor type, FSP_RESPONSE, then three packed u32 fields.
    /// A valid FSP success is not proof that GSP-RM initialized or GPU rendered.
    pub fn parse(packet: &[u8]) -> Result<Self, Error<()>> {
        if packet.len() < 20 || packet.len() > CHANNEL_SIZE || packet.len() % 4 != 0 {
            return Err(Error::Bounds);
        }
        let word = |at: usize| u32::from_le_bytes(packet[at..at + 4].try_into().unwrap());
        if word(0) & 0xff00ffff != 0xc0000001 || word(4) != 0x1510de7e {
            return Err(Error::Header);
        }
        Ok(Self {
            task: word(8),
            command: word(12),
            error: word(16),
        })
    }
    pub fn check(self, command: u32) -> Result<Self, Error<()>> {
        if self.command != command {
            return Err(Error::Header);
        }
        if self.error != 0 {
            return Err(Error::Remote(self.error));
        }
        Ok(self)
    }
}

#[cfg(test)]
mod tests;
