//! Turning a password into something safe to write down.
//!
//! A password is never stored. What is stored is the output of a function that
//! is easy to run once and expensive to run a hundred million times, applied to
//! the password and a random-ish salt — so somebody who reads the file learns
//! nothing they can use without spending that cost per guess, per account.
//!
//! # PBKDF2-HMAC-SHA512
//!
//! Chosen because it is built entirely out of the SHA-512 that is already here
//! for Ed25519, and adding a password hash should not mean adding a second
//! primitive to be sure of. It is not the strongest choice available — scrypt
//! and Argon2 also make an attacker spend *memory*, which is what defeats
//! hardware built for the purpose — and this system should move to one of them.
//! What PBKDF2 has is that it can be written correctly in fifty lines on top of
//! a hash that is already tested.
//!
//! # About the iteration count
//!
//! [`ITERATIONS`] is deliberately low, and that is a fact about the machine
//! rather than about what is safe. A modern deployment uses hundreds of
//! thousands; this system runs under dynamic translation on an emulated
//! processor, where each iteration costs perhaps a thousand times what it costs
//! on real hardware, and a first-run setup screen that takes four minutes to
//! accept a password is a setup screen nobody finishes.
//!
//! It is one constant, in one place, and raising it is the whole of what has to
//! change. Saying so here is better than choosing a number that looks
//! respectable and pretending the trade is not being made.

use crate::sha512;

/// Bytes of hash output stored per password.
pub const HASH: usize = 32;
/// Bytes of salt.
pub const SALT: usize = 16;

/// How many times the password is fed through the hash.
///
/// See the note at the top of this file: this number is a compromise with the
/// emulated machine this runs on, not a recommendation.
pub const ITERATIONS: u32 = 4_096;

/// Bytes in a SHA-512 block, which is what HMAC pads its key to.
const BLOCK: usize = 128;

/// HMAC-SHA-512.
///
/// The construction exists because hashing a key and a message together
/// naively lets an attacker *extend* the message: a hash of `key || message`
/// can be continued without knowing the key, because the hash's state after the
/// message is exactly what a continuation needs. Two nested hashes with
/// different padding remove that.
#[must_use]
pub fn hmac(key: &[u8], message: &[u8]) -> [u8; sha512::DIGEST] {
    // A key longer than a block is replaced by its hash. Not truncated: two
    // long keys that shared a prefix would otherwise be the same key.
    let mut padded = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = sha512::digest(key);
        padded[..digest.len()].copy_from_slice(&digest);
    } else {
        padded[..key.len()].copy_from_slice(key);
    }

    let mut inner_key = [0x36u8; BLOCK];
    let mut outer_key = [0x5Cu8; BLOCK];
    for index in 0..BLOCK {
        inner_key[index] ^= padded[index];
        outer_key[index] ^= padded[index];
    }

    let mut inner = sha512::Hasher::new();
    inner.update(&inner_key);
    inner.update(message);
    let inner = inner.finish();

    let mut outer = sha512::Hasher::new();
    outer.update(&outer_key);
    outer.update(&inner);
    outer.finish()
}

/// Derive a key from a password and a salt.
///
/// One block of output, which is all that is wanted: thirty-two bytes of a
/// sixty-four byte hash. Deriving more would mean running the whole thing again
/// per block, and nothing here needs more.
#[must_use]
pub fn derive(password: &[u8], salt: &[u8; SALT], iterations: u32) -> [u8; HASH] {
    // The first block is HMAC over the salt followed by the block number, big
    // endian. The number is what makes each block different, and there is one
    // block here.
    let mut first = [0u8; SALT + 4];
    first[..SALT].copy_from_slice(salt);
    first[SALT..].copy_from_slice(&1u32.to_be_bytes());

    let mut current = hmac(password, &first);
    let mut accumulated = current;

    for _ in 1..iterations.max(1) {
        current = hmac(password, &current);
        // Every round is folded in, not just the last. That is what makes the
        // work unavoidable: an attacker cannot skip to the end, because the end
        // is the exclusive-or of all of it.
        for (into, from) in accumulated.iter_mut().zip(current.iter()) {
            *into ^= *from;
        }
    }

    let mut out = [0u8; HASH];
    out.copy_from_slice(&accumulated[..HASH]);
    out
}

/// Whether a password matches a stored hash.
///
/// The comparison takes the same time whether the first byte is wrong or the
/// last, because one that stopped early would tell an attacker how much of a
/// guess was right — and finding thirty-two bytes one at a time is a thing that
/// finishes.
#[must_use]
pub fn matches(password: &[u8], salt: &[u8; SALT], iterations: u32, stored: &[u8; HASH]) -> bool {
    let computed = derive(password, salt, iterations);
    let mut difference = 0u8;
    for index in 0..HASH {
        difference |= computed[index] ^ stored[index];
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    fn hex(bytes: &[u8]) -> String {
        use core::fmt::Write as _;
        let mut out = String::new();
        for byte in bytes {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    #[test]
    fn rfc_4231_test_case_1() {
        // Key of twenty 0x0b bytes, message "Hi There".
        let key = [0x0bu8; 20];
        assert_eq!(
            hex(&hmac(&key, b"Hi There")),
            "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cde\
             daa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854"
        );
    }

    #[test]
    fn rfc_4231_test_case_2() {
        assert_eq!(
            hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "164b7a7bfcf819e2e395fbe73b56e0a387bd64222e831fd610270cd7ea250554\
             9758bf75c05a994a6d034f65f8f0e6fdcaeab1a34d4a6b4b636e070a38bce737"
        );
    }

    #[test]
    fn a_key_longer_than_a_block_is_hashed_first() {
        // Two long keys sharing a prefix must not be the same key, which is
        // what truncation instead of hashing would make them.
        let mut first = [0xaau8; 200];
        let mut second = [0xaau8; 200];
        second[199] = 0xbb;
        first[0] = 0xaa;
        assert_ne!(hmac(&first, b"message"), hmac(&second, b"message"));
    }

    #[test]
    fn different_passwords_give_different_hashes() {
        let salt = [7u8; SALT];
        assert_ne!(
            derive(b"correct horse", &salt, 16),
            derive(b"correct horst", &salt, 16)
        );
    }

    #[test]
    fn the_salt_changes_the_answer() {
        // The whole reason a salt exists: the same password on two machines
        // must not produce the same stored bytes, or one table of precomputed
        // hashes breaks both.
        assert_ne!(
            derive(b"password", &[1u8; SALT], 16),
            derive(b"password", &[2u8; SALT], 16)
        );
    }

    #[test]
    fn more_iterations_change_the_answer() {
        // Which is what says every round is actually folded in rather than the
        // last one being returned.
        let salt = [3u8; SALT];
        assert_ne!(derive(b"password", &salt, 1), derive(b"password", &salt, 2));
        assert_ne!(derive(b"password", &salt, 2), derive(b"password", &salt, 3));
    }

    #[test]
    fn a_password_matches_its_own_hash_and_nothing_else() {
        let salt = [9u8; SALT];
        let stored = derive(b"open sesame", &salt, 64);
        assert!(matches(b"open sesame", &salt, 64, &stored));
        assert!(!matches(b"open sesamf", &salt, 64, &stored));
        assert!(!matches(b"", &salt, 64, &stored));
        // Right password, wrong salt: the file was copied from another machine.
        assert!(!matches(b"open sesame", &[8u8; SALT], 64, &stored));
    }
}
