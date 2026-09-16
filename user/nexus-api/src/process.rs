//! Starting a program, and what to do with it once it is running.
//!
//! # The wire format this hides
//!
//! A spawn request is the path, a zero byte, and the arguments, with handles
//! attached. The reply is a refusal as text, or two handles in a fixed order:
//! a channel to the new program and its process. Every one of those details is
//! a thing five programs had a copy of.
//!
//! The zero byte is the interesting one. It is not a separator so much as a
//! declaration: a request *with* one says "this caller uses arguments", and the
//! kernel then sends the arguments message even when they are empty. Without
//! it, nothing is sent — which is right for a program whose first message
//! should be a reply, and fatal for one that reads its arguments first and
//! would otherwise wait for ever. [`Spawn`] always writes the byte, because
//! everything it starts is the second kind.

use alloc::string::String;
use alloc::vec::Vec;

use nexus_user::{Ending, Error, Handle};

/// How a program is started.
///
/// ```ignore
/// let child = Spawn::new("BIN/LS.ELF")
///     .arguments("PKG")
///     .output(theirs)
///     .input(nothing)
///     .lend(directory)
///     .start(spawner)?;
/// ```
///
/// Nothing is sent until [`start`](Self::start), so a builder that is put
/// together and dropped has taken nothing from anybody. After `start` the
/// handles given to it belong to the new program: sending a handle gives it up,
/// which is why a caller that means to keep one duplicates it first.
pub struct Spawn {
    program: String,
    arguments: String,
    lent: Vec<Handle>,
}

impl Spawn {
    /// A program by name, as the spawn service will look for it.
    #[must_use]
    pub fn new(program: &str) -> Self {
        Self {
            program: String::from(program),
            arguments: String::new(),
            lent: Vec::new(),
        }
    }

    /// What to tell it, as its first message.
    #[must_use]
    pub fn arguments(mut self, arguments: &str) -> Self {
        self.arguments = String::from(arguments);
        self
    }

    /// Somewhere for it to write: handle two in the new program.
    ///
    /// Must be set before [`input`](Self::input), because the two are numbered
    /// by the order they are attached and nothing in the message says which is
    /// which.
    #[must_use]
    pub fn output(mut self, handle: Handle) -> Self {
        self.lent.push(handle);
        self
    }

    /// Somewhere for it to read: handle three.
    #[must_use]
    pub fn input(mut self, handle: Handle) -> Self {
        self.lent.push(handle);
        self
    }

    /// Anything else, from handle four on, by arrangement with the program.
    #[must_use]
    pub fn lend(mut self, handle: Handle) -> Self {
        self.lent.push(handle);
        self
    }

    /// Ask `spawner` to start it.
    ///
    /// # Errors
    ///
    /// [`Trouble::Refused`] carries what the service said, which is written for
    /// somebody to read rather than for a program to match on: a path that is
    /// not there, a file that is not an executable, no memory. Everything else
    /// is the channel to the service failing, which means the caller was handed
    /// something that is not a spawn service or the service has stopped.
    ///
    /// The handles attached are given away whether this succeeds or fails. A
    /// refusal that left them with the caller and a success that did not would
    /// be two different rules to remember.
    pub fn start(self, spawner: Handle) -> Result<Child, Trouble> {
        let mut request = Vec::with_capacity(self.program.len() + 1 + self.arguments.len());
        request.extend_from_slice(self.program.as_bytes());
        // Always, even with no arguments. See the note at the top of the file.
        request.push(0);
        request.extend_from_slice(self.arguments.as_bytes());

        nexus_user::send(spawner, &request, &self.lent).map_err(Trouble::Channel)?;

        let mut reply = [0u8; nexus_user::MAX_MESSAGE];
        let mut handles = [Handle(0); 2];
        let received =
            nexus_user::receive(spawner, &mut reply, &mut handles).map_err(Trouble::Channel)?;

        // Two handles is the whole of "it started". A refusal carries none, and
        // the text says why.
        if received.handles != 2 {
            let said = core::str::from_utf8(&reply[..received.bytes]).unwrap_or("<not text>");
            return Err(Trouble::Refused(String::from(said)));
        }
        Ok(Child {
            program: self.program,
            channel: handles[0],
            process: handles[1],
        })
    }
}

/// Why a program did not start.
#[derive(Debug)]
pub enum Trouble {
    /// The spawn service refused, and said this.
    Refused(String),
    /// The channel to the spawn service failed.
    Channel(Error),
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Refused(said) => f.write_str(said),
            Self::Channel(error) => write!(f, "the spawn service: {error}"),
        }
    }
}

/// A program that is running.
///
/// Holding one holds two things: a channel to talk to it, and the authority to
/// wait for it and to stop it. Dropping it without [`wait`](Self::wait) leaves
/// the program running and closes both, which is a real thing to want -- but it
/// is also how a program gets left behind with nobody listening, so the two
/// endings are separate methods rather than one and a flag.
pub struct Child {
    program: String,
    channel: Handle,
    process: Handle,
}

impl Child {
    /// What it was started as.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The channel to it, for anything beyond its standard output.
    #[must_use]
    pub const fn channel(&self) -> Handle {
        self.channel
    }

    /// Wait for it to end, and say how.
    ///
    /// Closes both handles: after this there is nothing left to hold.
    ///
    /// # Errors
    ///
    /// When the process handle fails, which means the kernel lost track of it --
    /// not that the program failed, which is [`Ending::Exited`] with a status.
    pub fn wait(self) -> Result<Ending, Error> {
        let ending = nexus_user::wait(self.process);
        nexus_user::close(self.channel).ok();
        nexus_user::close(self.process).ok();
        ending
    }

    /// Stop it, and wait for it to have stopped.
    ///
    /// # Errors
    ///
    /// As [`wait`](Self::wait).
    pub fn kill(self) -> Result<Ending, Error> {
        nexus_user::kill(self.process).ok();
        self.wait()
    }
}

/// Read everything written to `mine` until the far end closes.
///
/// For whoever holds the other half of a child's standard output. Read *while*
/// the program runs and not afterwards: a channel holds a bounded number of
/// messages, so a program that writes more than that into a queue nobody is
/// draining stops until somebody reads -- and if the reader is waiting for the
/// program to exit first, neither of them moves again.
///
/// `each` is given the text as it arrives, in whatever pieces it arrives in.
/// Splitting into lines is the caller's, because a message boundary is not a
/// line boundary and nothing promises it will be.
///
/// Returns how many bytes there were.
pub fn drain(mine: Handle, mut each: impl FnMut(&str)) -> usize {
    let mut buffer = [0u8; nexus_user::MAX_MESSAGE];
    let mut none = [Handle(0); 1];
    let mut read = 0usize;
    while let Ok(got) = nexus_user::receive(mine, &mut buffer, &mut none) {
        read += got.bytes;
        each(&alloc::string::String::from_utf8_lossy(
            &buffer[..got.bytes],
        ));
    }
    nexus_user::close(mine).ok();
    read
}

/// A channel with its writing end already closed.
///
/// For a program with nothing to read. It still gets a handle, so the numbering
/// is the same for every program rather than depending on how it was started;
/// this end is dropped at once, which is what makes the program's first read
/// report the end of its input rather than blocking for ever.
///
/// # Errors
///
/// When there is no room for another channel.
pub fn nothing_to_read() -> Result<Handle, Error> {
    let (ours, theirs) = nexus_user::channel()?;
    nexus_user::close(ours).ok();
    Ok(theirs)
}
