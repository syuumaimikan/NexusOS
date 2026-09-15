//! Local model generation. Output is untrusted text, never a tool grant.
//! Numerical architecture and legacy checkpoint layout follow llama2.c (MIT);
//! see tools/nexus-model/LICENSE.llama2.c. No network or OS capabilities here.
mod llama;
mod tokenizer;

use alloc::vec::Vec;
pub use llama::Llama;
pub use tokenizer::Tokenizer;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidModel,
    InvalidTokenizer,
    InvalidRequest,
    Limit,
    Cancelled,
    Numerical,
}

/// A provider owns immutable model weights. Each session gets private KV state.
/// Implementations must bound a step; cancellation is cooperative between steps.
pub trait ModelProvider {
    type State;
    fn context_len(&self) -> usize;
    fn vocab_size(&self) -> usize;
    fn new_state(&self) -> Self::State;
    fn next(&self, state: &mut Self::State, token: u32) -> Result<u32, Error>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Finish {
    Eos,
    Length,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Prefill,
    Token(u32),
    Finished(Finish),
}

/// One bounded request, greedy decoding, no implicit history or persistence.
/// At most one transformer forward pass per poll, including prompt prefill.
pub struct ModelSession<'a, P: ModelProvider> {
    provider: &'a P,
    state: P::State,
    prompt: Vec<u32>,
    cursor: usize,
    last: u32,
    remaining: usize,
    finished: Option<Finish>,
    failed: Option<Error>,
}
impl<'a, P: ModelProvider> ModelSession<'a, P> {
    pub fn new(provider: &'a P, prompt: Vec<u32>, max_new_tokens: usize) -> Result<Self, Error> {
        if prompt.is_empty()
            || max_new_tokens == 0
            || prompt.iter().any(|&t| t as usize >= provider.vocab_size())
        {
            return Err(Error::InvalidRequest);
        }
        if max_new_tokens > 256
            || prompt
                .len()
                .checked_add(max_new_tokens)
                .is_none_or(|n| n > provider.context_len())
        {
            return Err(Error::Limit);
        }
        Ok(Self {
            provider,
            state: provider.new_state(),
            last: prompt[0],
            prompt,
            cursor: 0,
            remaining: max_new_tokens,
            finished: None,
            failed: None,
        })
    }
    pub fn cancel(&mut self) {
        if self.finished.is_none() {
            self.failed = Some(Error::Cancelled);
        }
    }
    pub fn poll(&mut self) -> Result<Event, Error> {
        if let Some(error) = self.failed {
            return Err(error);
        }
        if let Some(reason) = self.finished {
            return Ok(Event::Finished(reason));
        }
        if self.remaining == 0 {
            self.finished = Some(Finish::Length);
            return Ok(Event::Finished(Finish::Length));
        }
        let next = match self.provider.next(&mut self.state, self.last) {
            Ok(t) if (t as usize) < self.provider.vocab_size() => t,
            Ok(_) => {
                self.failed = Some(Error::Numerical);
                return Err(Error::Numerical);
            }
            Err(e) => {
                self.failed = Some(e);
                return Err(e);
            }
        };
        self.cursor += 1;
        if self.cursor < self.prompt.len() {
            self.last = self.prompt[self.cursor];
            return Ok(Event::Prefill);
        }
        if next == 1 || next == 2 {
            self.finished = Some(Finish::Eos);
            return Ok(Event::Finished(Finish::Eos));
        }
        self.last = next;
        self.remaining -= 1;
        Ok(Event::Token(next))
    }
}

#[cfg(test)]
mod tests;
