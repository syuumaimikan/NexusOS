//! Version 1: little endian, exact lengths, reserved bytes must be zero.
use crate::{Request, Response, Snapshot, Status, Tool};

pub const REQUEST_LEN: usize = 24;
pub const RESPONSE_LEN: usize = 40;

fn header<const N: usize>(magic: &[u8; 4], kind: u16, session: u64, id: u64) -> [u8; N] {
    let mut out = [0; N];
    out[..4].copy_from_slice(magic);
    out[4..6].copy_from_slice(&kind.to_le_bytes());
    out[8..16].copy_from_slice(&session.to_le_bytes());
    out[16..24].copy_from_slice(&id.to_le_bytes());
    out
}

fn fields(bytes: &[u8], magic: &[u8; 4], len: usize) -> Option<(u16, u64, u64)> {
    if bytes.len() != len || &bytes[..4] != magic || bytes[6..8] != [0, 0] {
        return None;
    }
    Some((
        u16::from_le_bytes(bytes[4..6].try_into().ok()?),
        u64::from_le_bytes(bytes[8..16].try_into().ok()?),
        u64::from_le_bytes(bytes[16..24].try_into().ok()?),
    ))
}

pub fn encode_request(request: Request) -> [u8; REQUEST_LEN] {
    header(b"NAI1", request.tool as u16, request.session, request.id)
}

pub fn decode_request(bytes: &[u8]) -> Option<Request> {
    let (tool, session, id) = fields(bytes, b"NAI1", REQUEST_LEN)?;
    if session == 0 || id == 0 {
        return None;
    }
    Some(Request {
        session,
        id,
        tool: Tool::from_id(tool)?,
    })
}

pub fn encode_response(response: Response) -> [u8; RESPONSE_LEN] {
    let mut out = header(
        b"NAR1",
        response.status as u16,
        response.session,
        response.id,
    );
    out[24..32].copy_from_slice(&response.snapshot.uptime_ms.to_le_bytes());
    out[32..40].copy_from_slice(&response.snapshot.thread_id.to_le_bytes());
    out
}

/// Zero session/request IDs identify an unattributable malformed-frame reply,
/// not a session or request a client may correlate with outstanding work.
pub fn decode_response(bytes: &[u8]) -> Option<Response> {
    let (status, session, id) = fields(bytes, b"NAR1", RESPONSE_LEN)?;
    let response = Response {
        session,
        id,
        status: Status::from_id(status)?,
        snapshot: Snapshot {
            uptime_ms: u64::from_le_bytes(bytes[24..32].try_into().ok()?),
            thread_id: u64::from_le_bytes(bytes[32..40].try_into().ok()?),
        },
    };
    if response.status != Status::Verified && response.snapshot != Snapshot::default() {
        return None;
    }
    if response.status == Status::Verified
        && (session == 0 || id == 0 || response.snapshot.thread_id == 0)
    {
        return None;
    }
    Some(response)
}
