use super::*;
use alloc::vec;
struct Provider;
impl ModelProvider for Provider {
    type State = usize;
    fn context_len(&self) -> usize {
        8
    }
    fn vocab_size(&self) -> usize {
        512
    }
    fn new_state(&self) -> usize {
        0
    }
    fn next(&self, state: &mut usize, _: u32) -> Result<u32, Error> {
        *state += 1;
        Ok(259)
    }
}
#[test]
fn session_prefill_length_and_cancellation() {
    let mut s = ModelSession::new(&Provider, vec![1, 4], 2).unwrap();
    assert_eq!(s.poll(), Ok(Event::Prefill));
    assert_eq!(s.poll(), Ok(Event::Token(259)));
    assert_eq!(s.poll(), Ok(Event::Token(259)));
    assert_eq!(s.poll(), Ok(Event::Finished(Finish::Length)));
    assert_eq!(s.poll(), Ok(Event::Finished(Finish::Length)));
    let mut s = ModelSession::new(&Provider, vec![1], 2).unwrap();
    s.cancel();
    assert_eq!(s.poll(), Err(Error::Cancelled));
    assert_eq!(s.poll(), Err(Error::Cancelled));
}
#[test]
fn session_rejects_excess_context_and_bad_ids() {
    assert!(matches!(
        ModelSession::new(&Provider, vec![], 2),
        Err(Error::InvalidRequest)
    ));
    assert!(matches!(
        ModelSession::new(&Provider, vec![512], 2),
        Err(Error::InvalidRequest)
    ));
    assert!(matches!(
        ModelSession::new(&Provider, vec![1], 8),
        Err(Error::Limit)
    ));
    assert!(matches!(
        ModelSession::new(&Provider, vec![1], 0),
        Err(Error::InvalidRequest)
    ));
}
#[test]
fn malformed_model_headers_never_panic() {
    for len in 0..100 {
        assert!(Llama::load(&vec![0; len]).is_err());
    }
    for i in 0..7 {
        for value in [i32::MIN, -1, 0, 1, i32::MAX] {
            let mut data = vec![0; 28];
            let mut dims = [64i32, 172, 5, 8, 4, 512, 512];
            dims[i] = value;
            for (slot, v) in data.as_chunks_mut::<4>().0.iter_mut().zip(dims) {
                slot.copy_from_slice(&v.to_le_bytes());
            }
            assert!(Llama::load(&data).is_err());
        }
    }
}
#[test]
fn malformed_tokenizers_reject() {
    for n in 0..64 {
        assert!(Tokenizer::load(&vec![255; n], 512).is_err());
    }
    assert!(Tokenizer::load(&[0; 4], usize::MAX).is_err());
}

#[test]
fn eos_errors_and_invalid_provider_tokens_are_terminal() {
    use core::cell::Cell;
    struct Ending {
        calls: Cell<u32>,
        result: Result<u32, Error>,
    }
    impl ModelProvider for Ending {
        type State = ();
        fn context_len(&self) -> usize {
            8
        }
        fn vocab_size(&self) -> usize {
            512
        }
        fn new_state(&self) {}
        fn next(&self, _: &mut (), _: u32) -> Result<u32, Error> {
            self.calls.set(self.calls.get() + 1);
            self.result
        }
    }
    for result in [Ok(1), Ok(2), Ok(512), Err(Error::Numerical)] {
        let p = Ending {
            calls: Cell::new(0),
            result,
        };
        let mut s = ModelSession::new(&p, vec![1], 2).unwrap();
        let expected = if matches!(result, Ok(1 | 2)) {
            Ok(Event::Finished(Finish::Eos))
        } else {
            Err(Error::Numerical)
        };
        assert_eq!(s.poll(), expected);
        assert_eq!(s.poll(), expected);
        assert_eq!(p.calls.get(), 1);
    }
}
