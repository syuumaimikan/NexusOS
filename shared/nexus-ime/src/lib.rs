//! Typing Japanese on a keyboard that has none.
//!
//! Romaji in, kana out, as the letters arrive. `k` shows nothing yet, `ka`
//! commits か, `kk` commits っ and keeps `k` waiting, `n` waits to see whether
//! the next letter makes it な or leaves it ん.
//!
//! That waiting is the whole of the problem. A converter that worked on a
//! finished word would be easy and useless: what somebody typing wants is to
//! see the kana appear under their fingers, which means every letter has to be
//! answered immediately with either "here is a kana" or "still deciding", and
//! the deciding has to be right about the cases where one letter changes what
//! the previous two meant.
//!
//! # What this is not
//!
//! It does not convert to kanji. That needs a dictionary of readings, a way to
//! rank candidates and a window to choose from them; the dictionary alone is
//! larger than this whole system. What is here is the half that is a function
//! of the letters — and it is the half that has to be right first, because a
//! kanji converter is fed kana.
//!
//! Saying so matters: a machine that offered "Japanese input" and quietly meant
//! kana only would be a machine somebody discovers the limits of half way
//! through a sentence.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::string::{String, ToString as _};

/// How many letters may wait before the oldest is given up on.
///
/// Three is enough for every sequence in the table (`kyo`, `sshi` reaches four
/// through the doubling rule but only three ever wait). A longer run is
/// somebody typing something this cannot convert, and holding it for ever would
/// be a field that swallows their letters.
const MAX_PENDING: usize = 3;

/// Which script letters turn into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Script {
    /// Letters go through unchanged.
    Direct,
    /// ひらがな.
    #[default]
    Hiragana,
    /// カタカナ.
    Katakana,
}

impl Script {
    /// The next one round, which is what a key that toggles does.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Direct => Self::Hiragana,
            Self::Hiragana => Self::Katakana,
            Self::Katakana => Self::Direct,
        }
    }

    /// What to show somebody about which mode they are in.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Direct => "A",
            Self::Hiragana => "あ",
            Self::Katakana => "ア",
        }
    }
}

/// The romaji this understands, longest first.
///
/// Longest first matters: `kya` must be found before `ky` would be, and `shi`
/// before `sh`. The table is walked in order, so its order is its meaning.
///
/// Only hiragana is listed. Katakana is the same table shifted by the distance
/// between the two blocks, which is a fixed ninety-six code points — a second
/// table would be the same data with one more chance to disagree with itself.
const TABLE: &[(&str, &str)] = &[
    // Three letters: the contracted sounds, and the ones spelled with h or s.
    ("kya", "きゃ"),
    ("kyu", "きゅ"),
    ("kyo", "きょ"),
    ("gya", "ぎゃ"),
    ("gyu", "ぎゅ"),
    ("gyo", "ぎょ"),
    ("sha", "しゃ"),
    ("shu", "しゅ"),
    ("sho", "しょ"),
    ("shi", "し"),
    ("sya", "しゃ"),
    ("syu", "しゅ"),
    ("syo", "しょ"),
    ("jya", "じゃ"),
    ("jyu", "じゅ"),
    ("jyo", "じょ"),
    ("cha", "ちゃ"),
    ("chu", "ちゅ"),
    ("cho", "ちょ"),
    ("chi", "ち"),
    ("tya", "ちゃ"),
    ("tyu", "ちゅ"),
    ("tyo", "ちょ"),
    ("tsu", "つ"),
    ("nya", "にゃ"),
    ("nyu", "にゅ"),
    ("nyo", "にょ"),
    ("hya", "ひゃ"),
    ("hyu", "ひゅ"),
    ("hyo", "ひょ"),
    ("bya", "びゃ"),
    ("byu", "びゅ"),
    ("byo", "びょ"),
    ("pya", "ぴゃ"),
    ("pyu", "ぴゅ"),
    ("pyo", "ぴょ"),
    ("mya", "みゃ"),
    ("myu", "みゅ"),
    ("myo", "みょ"),
    ("rya", "りゃ"),
    ("ryu", "りゅ"),
    ("ryo", "りょ"),
    ("dya", "ぢゃ"),
    ("dyu", "ぢゅ"),
    ("dyo", "ぢょ"),
    ("fyu", "ふゅ"),
    ("xtu", "っ"),
    ("ltu", "っ"),
    ("xya", "ゃ"),
    ("xyu", "ゅ"),
    ("xyo", "ょ"),
    // Two letters.
    ("ka", "か"),
    ("ki", "き"),
    ("ku", "く"),
    ("ke", "け"),
    ("ko", "こ"),
    ("ga", "が"),
    ("gi", "ぎ"),
    ("gu", "ぐ"),
    ("ge", "げ"),
    ("go", "ご"),
    ("sa", "さ"),
    ("si", "し"),
    ("su", "す"),
    ("se", "せ"),
    ("so", "そ"),
    ("za", "ざ"),
    ("zi", "じ"),
    ("zu", "ず"),
    ("ze", "ぜ"),
    ("zo", "ぞ"),
    ("ja", "じゃ"),
    ("ji", "じ"),
    ("ju", "じゅ"),
    ("je", "じぇ"),
    ("jo", "じょ"),
    ("ta", "た"),
    ("ti", "ち"),
    ("tu", "つ"),
    ("te", "て"),
    ("to", "と"),
    ("da", "だ"),
    ("di", "ぢ"),
    ("du", "づ"),
    ("de", "で"),
    ("do", "ど"),
    ("na", "な"),
    ("ni", "に"),
    ("nu", "ぬ"),
    ("ne", "ね"),
    ("no", "の"),
    ("ha", "は"),
    ("hi", "ひ"),
    ("hu", "ふ"),
    ("he", "へ"),
    ("ho", "ほ"),
    ("ba", "ば"),
    ("bi", "び"),
    ("bu", "ぶ"),
    ("be", "べ"),
    ("bo", "ぼ"),
    ("pa", "ぱ"),
    ("pi", "ぴ"),
    ("pu", "ぷ"),
    ("pe", "ぺ"),
    ("po", "ぽ"),
    ("fa", "ふぁ"),
    ("fi", "ふぃ"),
    ("fu", "ふ"),
    ("fe", "ふぇ"),
    ("fo", "ふぉ"),
    ("ma", "ま"),
    ("mi", "み"),
    ("mu", "む"),
    ("me", "め"),
    ("mo", "も"),
    ("ya", "や"),
    ("yu", "ゆ"),
    ("yo", "よ"),
    ("ra", "ら"),
    ("ri", "り"),
    ("ru", "る"),
    ("re", "れ"),
    ("ro", "ろ"),
    ("wa", "わ"),
    ("wi", "うぃ"),
    ("we", "うぇ"),
    ("wo", "を"),
    ("vu", "ゔ"),
    ("xa", "ぁ"),
    ("xi", "ぃ"),
    ("xu", "ぅ"),
    ("xe", "ぇ"),
    ("xo", "ぉ"),
    // One letter.
    ("a", "あ"),
    ("i", "い"),
    ("u", "う"),
    ("e", "え"),
    ("o", "お"),
    ("-", "ー"),
    (",", "、"),
    (".", "。"),
    ("/", "・"),
    ("[", "「"),
    ("]", "」"),
];

/// Where hiragana starts, and how far katakana is from it.
const HIRAGANA_FIRST: u32 = 0x3041;
const HIRAGANA_LAST: u32 = 0x3096;
const TO_KATAKANA: u32 = 0x60;

/// Turn hiragana into katakana, leaving everything else alone.
#[must_use]
pub fn to_katakana(text: &str) -> String {
    text.chars()
        .map(|character| {
            let value = character as u32;
            if (HIRAGANA_FIRST..=HIRAGANA_LAST).contains(&value) {
                char::from_u32(value + TO_KATAKANA).unwrap_or(character)
            } else {
                character
            }
        })
        .collect()
}

/// What a keystroke produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Output {
    /// Text that is finished and should go into the line.
    pub committed: String,
    /// Letters still being decided, to be shown under the caret.
    pub pending: String,
}

impl Output {
    /// Whether nothing at all came of it.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.committed.is_empty() && self.pending.is_empty()
    }
}

/// A conversion in progress.
#[derive(Debug, Clone, Default)]
pub struct Ime {
    script: Script,
    pending: String,
}

impl Ime {
    /// One that types letters through unchanged.
    #[must_use]
    pub fn new() -> Self {
        Self {
            script: Script::Direct,
            pending: String::new(),
        }
    }

    /// Which script it is converting to.
    #[must_use]
    pub const fn script(&self) -> Script {
        self.script
    }

    /// Change it, committing whatever was waiting.
    ///
    /// Committed rather than dropped: the letters were typed, and a mode change
    /// that ate them would be a mode change that loses work.
    pub fn set_script(&mut self, script: Script) -> String {
        let left = core::mem::take(&mut self.pending);
        self.script = script;
        left
    }

    /// Move to the next script round.
    pub fn cycle(&mut self) -> String {
        self.set_script(self.script.next())
    }

    /// What is still being decided.
    #[must_use]
    pub fn pending(&self) -> &str {
        &self.pending
    }

    /// Give up whatever is waiting, and say what it was.
    pub fn clear(&mut self) -> String {
        core::mem::take(&mut self.pending)
    }

    /// Take a letter back.
    ///
    /// Returns whether it took one from what was waiting. When nothing is
    /// waiting the caller should take one from the line instead, which is the
    /// behaviour every input method has: backspace un-types the romaji first.
    pub fn backspace(&mut self) -> bool {
        self.pending.pop().is_some()
    }

    /// Type one character.
    pub fn push(&mut self, character: char) -> Output {
        if self.script == Script::Direct {
            return Output {
                committed: character.to_string(),
                pending: String::new(),
            };
        }

        // Anything that is not a letter the table could use ends whatever was
        // waiting and goes through as itself. A space after `ka` should commit
        // か and then a space, not disappear.
        if !is_romaji(character) {
            let mut committed = core::mem::take(&mut self.pending);
            committed.push(character);
            return Output {
                committed: self.script_of(&committed),
                pending: String::new(),
            };
        }

        self.pending.push(character.to_ascii_lowercase());
        let mut committed = String::new();

        // Two things, alternating until neither can do anything: take the
        // longest kana at the front, and give up on a letter that can never
        // become one.
        //
        // Alternating rather than one after the other, because giving up on a
        // letter changes what is at the front: `b.` converts nothing, and after
        // the `b` goes through as itself the `.` is a full stop the table knows.
        // A loop that did the two in sequence would leave it waiting for ever.
        loop {
            if let Some((text, eaten)) = self.take_front() {
                committed.push_str(&text);
                self.pending.drain(..eaten);
                continue;
            }
            if self.pending.is_empty() {
                break;
            }
            if self.pending.len() > MAX_PENDING || !self.could_become_something() {
                let head = self.pending.remove(0);
                committed.push(head);
                continue;
            }
            break;
        }

        Output {
            committed: self.script_of(&committed),
            pending: self.pending.clone(),
        }
    }

    /// Katakana if that is the mode, and hiragana otherwise.
    ///
    /// Applied at the end rather than in the table, because the sokuon and the
    /// `n` rule produce hiragana too and converting once covers all three.
    fn script_of(&self, text: &str) -> String {
        match self.script {
            Script::Katakana => to_katakana(text),
            _ => text.to_string(),
        }
    }

    /// The longest kana at the front of what is waiting, and how many letters
    /// it used.
    fn take_front(&self) -> Option<(String, usize)> {
        let pending = self.pending.as_str();

        // A doubled consonant is っ and the second letter stays: `kka` is っか,
        // and the `k` that is left is the start of `ka`.
        let bytes = pending.as_bytes();
        if bytes.len() >= 2 && bytes[0] == bytes[1] && is_consonant(bytes[0] as char) {
            // `nn` is ん and not っん, which is why the table has it and why it
            // is looked up before this rule applies.
            if bytes[0] != b'n' {
                return Some((String::from("っ"), 1));
            }
        }

        // `n` before a consonant that is not `y` is ん, and it consumes *one*
        // letter. That includes a second `n`, which is the case that decides
        // the rule: `minna` is みんな and `annai` is あんない, and both need the
        // second `n` to survive and start the next kana. A `nn` that consumed
        // both would give みんあ, which is why there is no `nn` in the table.
        //
        // The price is that `nn` alone finishes as んん rather than ん. That is
        // the honest trade: the words are common and typing `nn` and stopping
        // is not.
        if bytes.len() >= 2 && bytes[0] == b'n' {
            let next = bytes[1] as char;
            if is_consonant(next) && next != 'y' {
                return Some((String::from("ん"), 1));
            }
        }

        // And the table, longest first.
        for (romaji, kana) in TABLE {
            if pending.starts_with(romaji) {
                return Some(((*kana).to_string(), romaji.len()));
            }
        }
        None
    }

    /// Whether what is waiting could still become a kana.
    ///
    /// True when some entry in the table starts with it. That is what makes `k`
    /// wait and `q` not.
    fn could_become_something(&self) -> bool {
        if self.pending.is_empty() {
            return true;
        }
        let pending = self.pending.as_str();
        // A doubled consonant and a trailing `n` are rules rather than entries,
        // so they are checked here too.
        if pending.len() == 1 {
            let only = pending.as_bytes()[0] as char;
            if is_consonant(only) {
                return true;
            }
        }
        TABLE.iter().any(|(romaji, _)| romaji.starts_with(pending))
    }

    /// Finish: commit whatever is waiting as best it can.
    ///
    /// A lone `n` becomes ん, which is what somebody typing `hon` and pressing
    /// space means. Anything else goes through as the letters it is.
    pub fn finish(&mut self) -> String {
        let pending = core::mem::take(&mut self.pending);
        if pending == "n" {
            return self.script_of("ん");
        }
        self.script_of(&pending)
    }
}

/// Whether a character is one the table could use.
fn is_romaji(character: char) -> bool {
    character.is_ascii_alphabetic() || matches!(character, '-' | ',' | '.' | '/' | '[' | ']')
}

/// Whether it is a consonant, for the doubling and `n` rules.
fn is_consonant(character: char) -> bool {
    character.is_ascii_alphabetic() && !matches!(character, 'a' | 'i' | 'u' | 'e' | 'o')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Type a whole word and return what ended up in the line.
    fn typed(script: Script, text: &str) -> String {
        let mut ime = Ime::new();
        ime.set_script(script);
        let mut line = String::new();
        for character in text.chars() {
            line.push_str(&ime.push(character).committed);
        }
        line.push_str(&ime.finish());
        line
    }

    #[test]
    fn direct_typing_is_unchanged() {
        assert_eq!(typed(Script::Direct, "hello"), "hello");
    }

    #[test]
    fn the_five_vowels() {
        assert_eq!(typed(Script::Hiragana, "aiueo"), "あいうえお");
    }

    #[test]
    fn a_consonant_waits_for_its_vowel() {
        let mut ime = Ime::new();
        ime.set_script(Script::Hiragana);
        let first = ime.push('k');
        assert_eq!(first.committed, "");
        assert_eq!(first.pending, "k");
        let second = ime.push('a');
        assert_eq!(second.committed, "か");
        assert_eq!(second.pending, "");
    }

    #[test]
    fn ordinary_words() {
        assert_eq!(typed(Script::Hiragana, "nihongo"), "にほんご");
        assert_eq!(typed(Script::Hiragana, "konnichiha"), "こんにちは");
        assert_eq!(typed(Script::Hiragana, "arigatou"), "ありがとう");
        assert_eq!(typed(Script::Hiragana, "sakura"), "さくら");
    }

    #[test]
    fn contracted_sounds_are_found_before_their_prefixes() {
        assert_eq!(typed(Script::Hiragana, "kyou"), "きょう");
        assert_eq!(typed(Script::Hiragana, "sha"), "しゃ");
        assert_eq!(typed(Script::Hiragana, "chotto"), "ちょっと");
        assert_eq!(typed(Script::Hiragana, "ryokou"), "りょこう");
    }

    #[test]
    fn a_doubled_consonant_is_a_small_tsu() {
        assert_eq!(typed(Script::Hiragana, "kitte"), "きって");
        assert_eq!(typed(Script::Hiragana, "gakkou"), "がっこう");
        assert_eq!(typed(Script::Hiragana, "issho"), "いっしょ");
    }

    #[test]
    fn n_waits_to_find_out_what_it_is() {
        // Before a vowel it is な; before a consonant it is ん; doubled it is ん.
        assert_eq!(typed(Script::Hiragana, "na"), "な");
        assert_eq!(typed(Script::Hiragana, "nda"), "んだ");
        assert_eq!(typed(Script::Hiragana, "nna"), "んな");
        assert_eq!(typed(Script::Hiragana, "minna"), "みんな");
        assert_eq!(typed(Script::Hiragana, "annai"), "あんない");
        assert_eq!(typed(Script::Hiragana, "nya"), "にゃ");
        // And on its own at the end.
        assert_eq!(typed(Script::Hiragana, "hon"), "ほん");
    }

    #[test]
    fn katakana_is_the_same_table_shifted() {
        assert_eq!(typed(Script::Katakana, "konpyu-ta"), "コンピュータ");
        assert_eq!(typed(Script::Katakana, "aiueo"), "アイウエオ");
        assert_eq!(to_katakana("ひらがな"), "ヒラガナ");
        // Everything else is left exactly as it was.
        assert_eq!(to_katakana("abc 123 漢字"), "abc 123 漢字");
    }

    #[test]
    fn punctuation_becomes_japanese_punctuation() {
        assert_eq!(typed(Script::Hiragana, "a,b."), "あ、b。");
    }

    #[test]
    fn a_space_ends_what_was_waiting() {
        let mut ime = Ime::new();
        ime.set_script(Script::Hiragana);
        ime.push('k');
        let output = ime.push(' ');
        // The `k` was typed, so it is not thrown away.
        assert_eq!(output.committed, "k ");
        assert_eq!(output.pending, "");
    }

    #[test]
    fn letters_that_can_never_become_kana_go_through() {
        // `q` starts nothing in the table, so it is given up on at once rather
        // than sitting at the front and blocking everything after it.
        assert_eq!(typed(Script::Hiragana, "qka"), "qか");
        assert_eq!(typed(Script::Hiragana, "xyz"), "xyz");
    }

    #[test]
    fn backspace_takes_the_romaji_first() {
        let mut ime = Ime::new();
        ime.set_script(Script::Hiragana);
        ime.push('k');
        assert!(ime.backspace());
        assert_eq!(ime.pending(), "");
        // With nothing waiting it says so, and the caller takes from the line.
        assert!(!ime.backspace());
    }

    #[test]
    fn changing_mode_keeps_what_was_typed() {
        let mut ime = Ime::new();
        ime.set_script(Script::Hiragana);
        ime.push('k');
        let left = ime.set_script(Script::Direct);
        assert_eq!(left, "k");
        assert_eq!(ime.pending(), "");
    }

    #[test]
    fn the_mode_goes_round() {
        let mut ime = Ime::new();
        assert_eq!(ime.script(), Script::Direct);
        ime.cycle();
        assert_eq!(ime.script(), Script::Hiragana);
        ime.cycle();
        assert_eq!(ime.script(), Script::Katakana);
        ime.cycle();
        assert_eq!(ime.script(), Script::Direct);
    }

    #[test]
    fn nothing_typed_at_it_makes_it_loop_or_panic() {
        // Every printable ASCII character, in every mode, one after another.
        for script in [Script::Direct, Script::Hiragana, Script::Katakana] {
            let mut ime = Ime::new();
            ime.set_script(script);
            for byte in 0x20u8..0x7F {
                let _ = ime.push(byte as char);
            }
            let _ = ime.finish();
        }
    }

    #[test]
    fn a_long_run_of_consonants_does_not_pile_up() {
        let mut ime = Ime::new();
        ime.set_script(Script::Hiragana);
        for _ in 0..50 {
            ime.push('k');
        }
        // Whatever it decided, it is not holding fifty letters.
        assert!(ime.pending().len() <= MAX_PENDING);
    }
}
