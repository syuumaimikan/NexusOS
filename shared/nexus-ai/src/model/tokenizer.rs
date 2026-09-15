use super::Error;
use alloc::{vec, vec::Vec};

/// Bounded llama2.c BPE vocabulary. Text input is UTF-8; token pieces may be
/// partial UTF-8 bytes and consumers must assemble them before rendering.
pub struct Tokenizer {
    vocab: Vec<Vec<u8>>,
    scores: Vec<f32>,
    bytes: [u8; 256],
}
impl Tokenizer {
    pub fn load(data: &[u8], vocab_size: usize) -> Result<Self, Error> {
        if data.len() < 4 || data.len() > 1024 * 1024 || !(259..=4096).contains(&vocab_size) {
            return Err(Error::InvalidTokenizer);
        }
        let max = u32::from_le_bytes(data[..4].try_into().unwrap()) as usize;
        if max == 0 || max > 128 {
            return Err(Error::InvalidTokenizer);
        }
        let mut at = 4;
        let mut vocab = Vec::new();
        let mut scores = Vec::new();
        for _ in 0..vocab_size {
            let header = data.get(at..at + 8).ok_or(Error::InvalidTokenizer)?;
            let score = f32::from_le_bytes(header[..4].try_into().unwrap());
            let len = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
            if !score.is_finite() || len == 0 || len > max {
                return Err(Error::InvalidTokenizer);
            }
            at += 8;
            let piece = data.get(at..at + len).ok_or(Error::InvalidTokenizer)?;
            if vocab.iter().any(|v: &Vec<u8>| v.as_slice() == piece) {
                return Err(Error::InvalidTokenizer);
            }
            vocab.push(piece.to_vec());
            scores.push(score);
            at += len;
        }
        if at != data.len()
            || vocab[0] != b"<unk>"
            || !(vocab[1] == b"<s>" || vocab[1] == b"\n<s>\n")
            || !(vocab[2] == b"</s>" || vocab[2] == b"\n</s>\n")
            || !vocab.iter().any(|v| v == b" ")
        {
            return Err(Error::InvalidTokenizer);
        }
        let mut bytes = [0; 256];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = i as u8;
            let hex = b"0123456789ABCDEF";
            let expected = [b'<', b'0', b'x', hex[i >> 4], hex[i & 15], b'>'];
            if vocab[i + 3].as_slice() != expected {
                return Err(Error::InvalidTokenizer);
            }
        }
        Ok(Self {
            vocab,
            scores,
            bytes,
        })
    }
    pub fn vocab_size(&self) -> usize {
        self.vocab.len()
    }
    fn lookup(&self, piece: &[u8]) -> Option<u32> {
        self.vocab.iter().position(|v| v == piece).map(|i| i as u32)
    }
    pub fn encode(&self, text: &str) -> Result<Vec<u32>, Error> {
        if text.len() > 1024 {
            return Err(Error::Limit);
        }
        let mut tokens = vec![1];
        if !text.is_empty() {
            tokens.push(self.lookup(b" ").ok_or(Error::InvalidTokenizer)?);
        }
        for ch in text.chars() {
            let mut bytes = [0; 4];
            let piece = ch.encode_utf8(&mut bytes).as_bytes();
            if let Some(id) = self.lookup(piece) {
                tokens.push(id);
            } else {
                tokens.extend(piece.iter().map(|&b| b as u32 + 3));
            }
        }
        let mut pair = Vec::with_capacity(256);
        loop {
            let mut best = None;
            let mut score = f32::NEG_INFINITY;
            for i in 1..tokens.len().saturating_sub(1) {
                pair.clear();
                pair.extend_from_slice(&self.vocab[tokens[i] as usize]);
                pair.extend_from_slice(&self.vocab[tokens[i + 1] as usize]);
                if let Some(id) = self.lookup(&pair) {
                    if id >= 3 && self.scores[id as usize] > score {
                        score = self.scores[id as usize];
                        best = Some((i, id));
                    }
                }
            }
            let Some((i, id)) = best else {
                break;
            };
            tokens[i] = id;
            tokens.remove(i + 1);
        }
        Ok(tokens)
    }
    pub fn decode(&self, previous: u32, token: u32) -> Result<&[u8], Error> {
        let mut piece = self
            .vocab
            .get(token as usize)
            .ok_or(Error::InvalidRequest)?
            .as_slice();
        if (3..259).contains(&token) {
            return Ok(&self.bytes[token as usize - 3..token as usize - 2]);
        }
        if previous == 1 && piece.starts_with(b" ") {
            piece = &piece[1..];
        }
        Ok(piece)
    }
}
