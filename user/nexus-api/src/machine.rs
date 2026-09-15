//! What the machine is doing.
//!
//! One request, one reply, and the reply's shape lives in `shared/nexus-machine`
//! so that the kernel writing it and the program reading it cannot drift apart
//! without a test noticing. All this adds is the round trip.

use nexus_user::Handle;

pub use nexus_machine::{Snapshot, Trouble};

/// Ask the machine service for a snapshot.
///
/// The kernel reuses one for a tenth of a second, so a window may ask whenever
/// it likes: a caller that polls tightly gets the same answer with the same
/// `taken_at` rather than an error, and the kernel does not walk its own tables
/// in a loop.
///
/// # Errors
///
/// [`Trouble`] as the service reports it -- an unknown request, a version it
/// does not speak -- and [`Trouble::NotAReply`] when the channel itself fails,
/// which is what a caller handed something that is not the machine service
/// gets: nothing came back that could be read as an answer.
pub fn snapshot(service: Handle) -> Result<Snapshot, Trouble> {
    let request = nexus_machine::request();
    if nexus_user::send(service, &request, &[]).is_err() {
        return Err(Trouble::NotAReply);
    }

    let mut reply = [0u8; nexus_user::MAX_MESSAGE];
    let mut none = [Handle(0); 1];
    let Ok(received) = nexus_user::receive(service, &mut reply, &mut none) else {
        return Err(Trouble::NotAReply);
    };
    Snapshot::of(&reply[..received.bytes])
}
