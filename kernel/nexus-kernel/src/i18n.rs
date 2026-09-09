//! Localisation, which lives in a crate of its own.
//!
//! It moved out when programs needed it. The kernel translates its panel and
//! the desktop translates its dock, and a system with two sets of strings would
//! be a system where switching the language changes half the screen.
//!
//! The crate is where the build script that reads `locales/*.txt` lives, next
//! to the one that rasterises the glyphs those strings need -- neither of which
//! was ever the kernel's business.

pub use nexus_i18n::*;
