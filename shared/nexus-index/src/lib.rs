//! Turning text into something a machine can compare.
//!
//! # What this is, and what it is not
//!
//! It is a *hashed character n-gram embedding*: every three consecutive
//! characters of a document are hashed into one of a fixed number of buckets,
//! the buckets are counted, and two documents are compared by the cosine of the
//! angle between their count vectors. That is a real and old technique — the
//! hashing trick, over character n-grams — and it does what it says: documents
//! that share sequences of characters come out close together.
//!
//! It is **not** a language model, and nothing here has been trained on
//! anything. There is no learned weight in this file. Saying so plainly matters
//! more than it would elsewhere, because "embedding" is a word that invites the
//! reader to assume a neural network, and a system that let that assumption
//! stand would be claiming something it has not built.
//!
//! What it is good for is finding a file again: a query and a document that
//! share phrases rank highly, and one that shares nothing does not.
//!
//! # Why character n-grams
//!
//! Because splitting on spaces does not work for half the languages this system
//! is written in. `画面には触れていません` has no spaces in it and is not one
//! word; a word tokeniser would treat the whole sentence as a single token and
//! find it similar to nothing. Three characters at a time works the same way in
//! both languages, which is the only reason to prefer it.
//!
//! # Why there is not a single floating-point number here
//!
//! Cosine similarity is a ratio of a dot product to a product of lengths, and
//! lengths are square roots. Both are avoidable: two similarities can be
//! *compared* by cross-multiplying, and squaring both sides removes the roots.
//! What is left is exact integer arithmetic, which gives the same ranking on
//! every machine and needs no floating-point unit — which matters in a system
//! whose kernel does not enable one.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::vec::Vec;

/// How many buckets a document is hashed into.
///
/// Two hundred and fifty-six. Fewer would make collisions common enough to
/// blur documents together; many more would make a vector larger than most of
/// the files this indexes. It is a fixed size on purpose: every vector is the
/// same shape, so comparing two of them is a walk down two arrays.
pub const DIMENSIONS: usize = 256;

/// How many characters make an n-gram.
const GRAM: usize = 3;

/// A document, as counts.
#[derive(Clone)]
pub struct Vector {
    counts: [u32; DIMENSIONS],
    /// The sum of the squares, kept because every comparison needs it and it
    /// does not change once the vector is built.
    norm_squared: u64,
}

impl Default for Vector {
    fn default() -> Self {
        Self::new()
    }
}

impl Vector {
    /// An empty one.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            counts: [0; DIMENSIONS],
            norm_squared: 0,
        }
    }

    /// Whether anything was put in it.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.norm_squared == 0
    }

    /// The sum of the squares of the counts.
    #[must_use]
    pub const fn norm_squared(&self) -> u64 {
        self.norm_squared
    }

    /// How much two documents have in common, before it is turned into an
    /// angle.
    #[must_use]
    pub fn dot(&self, other: &Self) -> u64 {
        let mut total = 0u64;
        for (mine, theirs) in self.counts.iter().zip(other.counts.iter()) {
            total += u64::from(*mine) * u64::from(*theirs);
        }
        total
    }

    /// What one bucket holds, for whoever is checking the arithmetic.
    #[must_use]
    pub fn bucket(&self, index: usize) -> u32 {
        self.counts.get(index).copied().unwrap_or(0)
    }
}

/// Turn text into a vector.
///
/// Lowercased first, so that a query typed in a hurry matches a document that
/// was not. Only ASCII is lowered: there is no case in Japanese, and applying
/// full Unicode case folding would mean carrying a table of it.
#[must_use]
pub fn embed(text: &str) -> Vector {
    let mut vector = Vector::new();

    // The characters, not the bytes. A three-*byte* window would cut a Japanese
    // character into pieces and hash the pieces, which would make two documents
    // similar because their characters happened to overlap the same way.
    let characters: Vec<char> = text
        .chars()
        .map(|character| character.to_ascii_lowercase())
        .collect();

    if characters.len() < GRAM {
        // Too short for a single n-gram. Hashed whole rather than dropped, so a
        // two-character query still matches something.
        if !characters.is_empty() {
            let bucket = hash(&characters) as usize % DIMENSIONS;
            vector.counts[bucket] += 1;
        }
    } else {
        for window in characters.windows(GRAM) {
            let bucket = hash(window) as usize % DIMENSIONS;
            vector.counts[bucket] += 1;
        }
    }

    vector.norm_squared = vector
        .counts
        .iter()
        .map(|count| u64::from(*count) * u64::from(*count))
        .sum();
    vector
}

/// FNV-1a over the characters, which is enough to spread n-grams evenly.
///
/// Not a cryptographic hash and not required to be: nothing here is trying to
/// stop somebody constructing a document that collides. What it has to be is
/// *the same everywhere*, so that an index built on one machine means the same
/// thing on another.
fn hash(characters: &[char]) -> u32 {
    let mut value: u32 = 0x811C_9DC5;
    for character in characters {
        for byte in (*character as u32).to_le_bytes() {
            value ^= u32::from(byte);
            value = value.wrapping_mul(0x0100_0193);
        }
    }
    value
}

/// Whether the first document is a better match than the second.
///
/// Cosine similarity is `dot / (|q| · |d|)`. Comparing two of them against the
/// same query, `|q|` is common and cancels, so the question is whether
/// `dot₁/|d₁| > dot₂/|d₂|` — and cross-multiplying and squaring turns that into
/// `dot₁²·|d₂|² > dot₂²·|d₁|²`, which has no division and no square root in it.
///
/// The squares are done in a hundred and twenty-eight bits because they are the
/// one place this could overflow: a large document's norm can reach the
/// millions, and its square the trillions.
#[must_use]
pub fn better(first: (u64, u64), second: (u64, u64)) -> bool {
    let (first_dot, first_norm) = first;
    let (second_dot, second_norm) = second;
    let left = u128::from(first_dot) * u128::from(first_dot) * u128::from(second_norm);
    let right = u128::from(second_dot) * u128::from(second_dot) * u128::from(first_norm);
    left > right
}

/// One thing that has been indexed.
pub struct Entry<T> {
    pub what: T,
    pub vector: Vector,
}

/// An index of documents, searched by comparing against every one of them.
///
/// Every one, because there is no structure here that would let it be fewer:
/// approximate nearest-neighbour search over a few hundred files would be more
/// code than it saves. What this is, it is exactly — and when the number of
/// documents makes that untrue, the thing to add is the structure, not a
/// cleverer comparison.
pub struct Index<T> {
    entries: Vec<Entry<T>>,
}

impl<T> Default for Index<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Index<T> {
    /// An empty index.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Add a document.
    pub fn add(&mut self, what: T, text: &str) {
        self.entries.push(Entry {
            what,
            vector: embed(text),
        });
    }

    /// How many documents are in it.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether it holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The closest document to a query, and how strongly it matched.
    ///
    /// The strength is returned as the dot product and the document's norm,
    /// which is what a caller needs to compare it against anything else. A
    /// single number would have to be a cosine, and a cosine is a ratio this
    /// deliberately never computes.
    #[must_use]
    pub fn best(&self, query: &str) -> Option<(&T, u64, u64)> {
        let wanted = embed(query);
        if wanted.is_empty() {
            return None;
        }

        let mut best: Option<(&T, u64, u64)> = None;
        for entry in &self.entries {
            if entry.vector.is_empty() {
                continue;
            }
            let dot = wanted.dot(&entry.vector);
            if dot == 0 {
                continue;
            }
            let candidate = (dot, entry.vector.norm_squared());
            match best {
                Some((_, best_dot, best_norm)) if !better(candidate, (best_dot, best_norm)) => {}
                _ => best = Some((&entry.what, dot, entry.vector.norm_squared())),
            }
        }
        best
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    #[test]
    fn a_document_matches_itself_best() {
        let mut index: Index<String> = Index::new();
        index.add(
            String::from("first"),
            "the quick brown fox jumps over the lazy dog",
        );
        index.add(String::from("second"), "a completely different sentence");
        index.add(String::from("third"), "something else again entirely");

        let (found, _, _) = index
            .best("the quick brown fox jumps over the lazy dog")
            .expect("a query this close must match something");
        assert_eq!(found, "first");
    }

    #[test]
    fn a_phrase_finds_the_document_that_contains_it() {
        let mut index: Index<String> = Index::new();
        index.add(
            String::from("packages"),
            "a package is verified before it is installed and rolled back if it fails",
        );
        index.add(
            String::from("network"),
            "the card gets an address by asking a server on the segment",
        );
        index.add(
            String::from("display"),
            "the compositor repaints only what changed on the screen",
        );

        let (found, _, _) = index
            .best("what happens when an install fails")
            .expect("matches");
        assert_eq!(found, "packages");

        let (found, _, _) = index.best("how does it get an address").expect("matches");
        assert_eq!(found, "network");
    }

    #[test]
    fn japanese_works_the_same_way() {
        // The case that decides the shape of this: no spaces, so a word
        // tokeniser would see one enormous token and match nothing.
        let mut index: Index<String> = Index::new();
        index.add(
            String::from("screen"),
            "画面には触れていません。プログラムが与えられたメモリに描きます。",
        );
        index.add(
            String::from("network"),
            "ネットワークカードがアドレスを受け取りました。",
        );

        let (found, _, _) = index.best("画面に描く").expect("matches");
        assert_eq!(found, "screen");

        let (found, _, _) = index.best("アドレス").expect("matches");
        assert_eq!(found, "network");
    }

    #[test]
    fn something_with_nothing_in_common_matches_nothing() {
        let mut index: Index<String> = Index::new();
        index.add(String::from("only"), "aaaaaaaaaa");
        // No three-character sequence in common, so every dot product is zero
        // and there is no match rather than a bad one.
        assert!(index.best("zzzzzzzzzz").is_none());
    }

    #[test]
    fn an_empty_query_matches_nothing() {
        let mut index: Index<String> = Index::new();
        index.add(String::from("only"), "some text");
        assert!(index.best("").is_none());
    }

    #[test]
    fn case_does_not_matter_for_ascii() {
        assert_eq!(
            embed("Hello There").counts,
            embed("hello there").counts,
            "a query typed in a hurry has to match a document that was not"
        );
    }

    #[test]
    fn comparing_similarities_needs_no_division() {
        // A short document matching a query strongly beats a long one matching
        // it weakly, which is the whole reason the norm is in the comparison: a
        // dot product on its own rewards length.
        //
        // dot 10 into a document of norm 100, against dot 12 into one of norm
        // 400: 10/10 against 12/20.
        assert!(better((10, 100), (12, 400)));
        assert!(!better((12, 400), (10, 100)));
        // Equal cosines are not better than each other, in either direction.
        assert!(!better((10, 100), (20, 400)));
        assert!(!better((20, 400), (10, 100)));
    }

    #[test]
    fn a_long_document_does_not_win_by_being_long() {
        let mut index: Index<String> = Index::new();
        index.add(String::from("short"), "damage tracking");
        // The same phrase, buried in a great deal of unrelated text. Without
        // the norm in the comparison this would win every query, because it
        // shares more n-grams with everything.
        let mut long = String::from("damage tracking ");
        for _ in 0..50 {
            long.push_str("unrelated filler about something else entirely ");
        }
        index.add(String::from("long"), &long);

        let (found, _, _) = index.best("damage tracking").expect("matches");
        assert_eq!(found, "short");
    }

    #[test]
    fn a_document_shorter_than_one_gram_is_still_indexed() {
        let mut index: Index<String> = Index::new();
        index.add(String::from("tiny"), "ab");
        let (found, _, _) = index
            .best("ab")
            .expect("two characters is still a document");
        assert_eq!(found, "tiny");
    }

    #[test]
    fn the_hash_does_not_depend_on_where_it_runs() {
        // A fixed answer, written down. If this ever changes, every index built
        // by an older version means something different -- which is the thing
        // that must not happen silently.
        assert_eq!(hash(&['a', 'b', 'c']), 4_070_381_781);
        assert_eq!(hash(&['画', '面', 'に']), hash(&['画', '面', 'に']));
        assert_ne!(hash(&['a', 'b', 'c']), hash(&['a', 'c', 'b']));
    }
}
