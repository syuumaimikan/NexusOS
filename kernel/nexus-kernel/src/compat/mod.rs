//! Running programs that were not built for this system.
//!
//! Every layer here sits *above* the Nexus system-call interface and turns a
//! foreign interface into it. Nothing below knows they exist, and nothing here
//! may reach past the interface a Nexus program has: the moment a foreign call
//! needs something Nexus does not offer, the answer is to add it to Nexus --
//! for everybody -- and then translate.
//!
//! That rule is the difference between a compatibility layer and a fork. A
//! system that grew a Linux-shaped hole in its kernel to make Linux programs
//! work would be a system whose own interface is now the second-class one.

pub mod linux;
pub mod linux_files;
