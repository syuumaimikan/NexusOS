//! CPU protocol tests with a register trace, never a simulated GPU acceptance.
extern crate std;
use super::*;
use std::vec::Vec;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Event {
    Read(Register),
    Write(Register, u32),
}
struct Trace {
    queue: [u32; 4],
    control: u32,
    ram: [u32; 256],
    events: Vec<Event>,
    broken_cursor: bool,
    refuse_data: bool,
}
impl Trace {
    fn new() -> Self {
        Self {
            queue: [0; 4],
            control: 0,
            ram: [0; 256],
            events: Vec::new(),
            broken_cursor: false,
            refuse_data: false,
        }
    }
}
// SAFETY: this trace owns only CPU arrays, with no device or DMA resources.
unsafe impl Registers for Trace {
    type Error = ();
    fn read(&mut self, register: Register) -> Result<u32, ()> {
        self.events.push(Event::Read(register));
        Ok(match register {
            Register::CommandHead => self.queue[0],
            Register::CommandTail => self.queue[1],
            Register::MessageHead => self.queue[2],
            Register::MessageTail => self.queue[3],
            Register::Control => {
                if self.broken_cursor {
                    self.control & !0xffff
                } else {
                    self.control
                }
            }
            Register::Data => {
                if self.refuse_data {
                    return Err(());
                }
                let index = cursor(self.control) as usize;
                let value = self.ram[index];
                self.control += 4;
                value
            }
        })
    }
    fn write(&mut self, register: Register, value: u32) -> Result<(), ()> {
        self.events.push(Event::Write(register, value));
        match register {
            Register::CommandHead => self.queue[0] = value,
            Register::CommandTail => self.queue[1] = value,
            Register::MessageHead => self.queue[2] = value,
            Register::MessageTail => self.queue[3] = value,
            Register::Control => self.control = value,
            Register::Data => {
                if self.refuse_data {
                    return Err(());
                }
                let index = cursor(self.control) as usize;
                self.ram[index] = value;
                self.control += 4;
            }
        }
        Ok(())
    }
}
#[test]
fn initial_cot_wire_format_and_publish_order() {
    let mut payload = [0x53; crate::boot::COT_SIZE];
    payload[..4].copy_from_slice(&[2, 0, 0x5c, 3]);
    let packet = cot_packet(&payload).unwrap();
    assert_eq!(&packet[..8], &[1, 0, 0, 0xc0, 0x7e, 0xde, 0x10, 0x14]);
    assert_eq!(&packet[8..], &payload);
    let mut io = Trace::new();
    send(&mut io, &packet).unwrap();
    assert_eq!(io.queue, [0, 864, 0, 0]);
    assert_eq!(
        &io.events[io.events.len() - 2..],
        &[
            Event::Write(Register::CommandTail, 864),
            Event::Write(Register::CommandHead, 0)
        ]
    );
    let writes: Vec<_> = io
        .events
        .iter()
        .filter_map(|event| {
            if let Event::Write(Register::Data, value) = event {
                Some(*value)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(writes.len(), 217);
    for (word, chunk) in writes.iter().zip(packet.chunks_exact(4)) {
        assert_eq!(word.to_le_bytes(), chunk);
    }
    // Last partial EMEM word must not reveal bytes after the input slice.
    let mut io = Trace::new();
    send(&mut io, &packet[..11]).unwrap();
    assert_eq!(io.ram[2], 0x005c0002);
    assert_eq!(io.queue[1], 8);
}
#[test]
fn no_doorbell_for_busy_invalid_or_incomplete_emem() {
    let mut payload = [0; crate::boot::COT_SIZE];
    payload[..4].copy_from_slice(&[2, 0, 0x5c, 3]);
    let packet = cot_packet(&payload).unwrap();
    for queue in [[0, 4, 0, 0], [0, 0, 0, 4]] {
        let mut io = Trace::new();
        io.queue = queue;
        assert_eq!(send(&mut io, &packet), Err(Error::Busy));
        assert!(
            io.events
                .iter()
                .all(|event| matches!(event, Event::Read(_)))
        );
    }
    for bad in [1, 1024, u32::MAX] {
        let mut io = Trace::new();
        io.queue[1] = bad;
        assert_eq!(send(&mut io, &packet), Err(Error::Bounds));
        assert!(
            io.events
                .iter()
                .all(|event| matches!(event, Event::Read(_)))
        );
    }
    let mut io = Trace::new();
    io.broken_cursor = true;
    assert_eq!(send(&mut io, &packet), Err(Error::Cursor));
    assert!(!io.events.iter().any(|event| matches!(
        event,
        Event::Write(Register::CommandHead | Register::CommandTail, _)
    )));
    let mut io = Trace::new();
    io.refuse_data = true;
    assert_eq!(send(&mut io, &packet), Err(Error::Io(())));
    assert!(!io.events.iter().any(|event| matches!(
        event,
        Event::Write(Register::CommandHead | Register::CommandTail, _)
    )));
}
const ACK: [u8; 20] = [
    1, 0, 0, 0xc0, 0x7e, 0xde, 0x10, 0x15, 3, 0, 0, 0, 0x14, 0, 0, 0, 0, 0, 0, 0,
];
#[test]
fn acknowledge_only_a_complete_read_and_validate_response() {
    let mut io = Trace::new();
    io.queue[3] = 16;
    for (slot, chunk) in io.ram.iter_mut().zip(ACK.chunks_exact(4)) {
        *slot = u32::from_le_bytes(chunk.try_into().unwrap());
    }
    let mut packet = [0xa5; CHANNEL_SIZE];
    assert_eq!(receive(&mut io, &mut packet).unwrap(), Some(20));
    assert_eq!(&packet[..20], &ACK);
    assert!(packet[20..].iter().all(|&byte| byte == 0));
    assert_eq!(
        &io.events[io.events.len() - 2..],
        &[
            Event::Write(Register::MessageTail, 0),
            Event::Write(Register::MessageHead, 0)
        ]
    );
    assert_eq!(
        Response::parse(&packet[..20])
            .unwrap()
            .check(COT_TYPE)
            .unwrap(),
        Response {
            task: 3,
            command: 0x14,
            error: 0
        }
    );
    assert_eq!(receive(&mut io, &mut packet).unwrap(), None);
    let mut io = Trace::new();
    io.queue[3] = 16;
    io.broken_cursor = true;
    assert_eq!(receive(&mut io, &mut packet), Err(Error::Cursor));
    assert!(!io.events.iter().any(|event| matches!(
        event,
        Event::Write(Register::MessageHead | Register::MessageTail, _)
    )));
    let mut io = Trace::new();
    io.queue[3] = 16;
    io.refuse_data = true;
    assert_eq!(receive(&mut io, &mut packet), Err(Error::Io(())));
    assert!(!io.events.iter().any(|event| matches!(
        event,
        Event::Write(Register::MessageHead | Register::MessageTail, _)
    )));
    for queue in [[0, 0, 4, 16], [0, 0, 0, 1024], [0, 0, 0, 17]] {
        let mut io = Trace::new();
        io.queue = queue;
        assert_eq!(receive(&mut io, &mut packet), Err(Error::Bounds));
        assert!(
            io.events
                .iter()
                .all(|event| matches!(event, Event::Read(_)))
        );
    }
}
#[test]
fn malformed_or_error_ack_never_becomes_success() {
    for length in 0..20 {
        assert!(Response::parse(&ACK[..length]).is_err());
    }
    for (index, value) in [
        (0, 0),
        (0, 0x11),
        (1, 1),
        (3, 0xc1),
        (3, 0xd0),
        (3, 0x80),
        (4, 0x7f),
        (4, 0xfe),
        (5, 0),
        (7, 0x14),
    ] {
        let mut packet = ACK;
        packet[index] = value;
        assert!(Response::parse(&packet).is_err());
    }
    let mut packet = ACK;
    packet[12] = 0x21;
    assert_eq!(
        Response::parse(&packet).unwrap().check(COT_TYPE),
        Err(Error::Header)
    );
    packet = ACK;
    packet[16] = 0xa1;
    assert_eq!(
        Response::parse(&packet).unwrap().check(COT_TYPE),
        Err(Error::Remote(0xa1))
    );
    let mut bad = [0; crate::boot::COT_SIZE];
    assert!(cot_packet(&bad).is_err());
    bad[..4].copy_from_slice(&[2, 0, 0x5c, 3]);
    assert!(cot_packet(&bad[..859]).is_err());
}
