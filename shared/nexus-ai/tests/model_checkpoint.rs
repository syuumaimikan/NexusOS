//! Requires `python tools/nexus-model/fetch.py`. Expected IDs come from the
//! independently compiled pinned llama2.c, see tools/nexus-model/reference.c.
#![cfg(feature = "model")]
use nexus_ai_core::model::{Event, Llama, ModelProvider, ModelSession, Tokenizer};
fn assets() -> (Vec<u8>, Vec<u8>) {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/models");
    (
        std::fs::read(root.join("stories260K.bin")).expect("run tools/nexus-model/fetch.py"),
        std::fs::read(root.join("tok512.bin")).unwrap(),
    )
}
#[test]
fn pretrained_output_matches_independent_c_oracle() {
    let (w, t) = assets();
    let model = Llama::load(&w).unwrap();
    let tok = Tokenizer::load(&t, model.vocab_size()).unwrap();
    for (prompt, expected) in [
        (
            "Once upon a time",
            &[
                432, 383, 286, 261, 376, 298, 315, 421, 395, 317, 426, 338, 401, 396, 267, 337,
                410, 408, 419, 292, 411, 322, 265, 282, 295, 433, 426, 385, 328, 432, 358, 394,
            ][..],
        ),
        (
            "The little cat",
            &[
                269, 261, 400, 428, 382, 276, 337, 299, 322, 265, 282, 295, 433, 426, 342, 397,
                355, 267, 337, 335, 265, 315, 267, 422, 419, 269, 352, 379, 261, 420, 277, 264,
            ][..],
        ),
    ] {
        let mut session = ModelSession::new(&model, tok.encode(prompt).unwrap(), 32).unwrap();
        let mut got = Vec::new();
        loop {
            match session.poll().unwrap() {
                Event::Prefill => {}
                Event::Token(t) => got.push(t),
                Event::Finished(_) => break,
            }
        }
        assert_eq!(got, expected);
    }
}
#[test]
fn japanese_and_emoji_round_trip_without_loss() {
    let (_, t) = assets();
    let tok = Tokenizer::load(&t, 512).unwrap();
    for text in [
        "こんにちは、世界🌸",
        "日本語\nUTF-8",
        "",
        "\0",
        "<s> literal",
    ] {
        let ids = tok.encode(text).unwrap();
        let mut out = Vec::new();
        let mut previous = 1;
        for &id in &ids[1..] {
            out.extend_from_slice(tok.decode(previous, id).unwrap());
            previous = id;
        }
        assert_eq!(String::from_utf8(out).unwrap(), text);
    }
}
#[test]
fn real_checkpoint_rejects_corruption_and_truncation() {
    let (w, t) = assets();
    for len in [0, 27, 28, 29, w.len() - 1] {
        assert!(Llama::load(&w[..len]).is_err());
    }
    let mut bad = w.clone();
    bad.extend_from_slice(&[0; 4]);
    assert!(Llama::load(&bad).is_err());
    let mut bad = w;
    bad[28..32].copy_from_slice(&f32::NAN.to_le_bytes());
    assert!(Llama::load(&bad).is_err());
    for len in [0, 3, 4, 5, t.len() - 1] {
        assert!(Tokenizer::load(&t[..len], 512).is_err());
    }
    let mut bad = t;
    bad.extend_from_slice(&[0]);
    assert!(Tokenizer::load(&bad, 512).is_err());
}
#[test]
fn interleaved_sessions_do_not_share_kv_cache() {
    let (w, t) = assets();
    let model = Llama::load(&w).unwrap();
    let tok = Tokenizer::load(&t, 512).unwrap();
    let prompt = tok.encode("Once upon a time").unwrap();
    let mut a = ModelSession::new(&model, prompt.clone(), 16).unwrap();
    let mut b = ModelSession::new(&model, prompt, 16).unwrap();
    let mut noise = ModelSession::new(&model, tok.encode("different input").unwrap(), 16).unwrap();
    loop {
        let left = a.poll().unwrap();
        noise.poll().unwrap();
        assert_eq!(left, b.poll().unwrap());
        if matches!(left, Event::Finished(_)) {
            break;
        }
    }
}

#[test]
fn finite_but_overflowing_weights_report_numerical_failure() {
    let (mut w, _) = assets();
    for v in w[28..].as_chunks_mut::<4>().0.iter_mut() {
        *v = f32::MAX.to_le_bytes();
    }
    let model = Llama::load(&w).unwrap();
    let mut session = ModelSession::new(&model, vec![1], 1).unwrap();
    assert_eq!(session.poll(), Err(nexus_ai_core::model::Error::Numerical));
    assert_eq!(session.poll(), Err(nexus_ai_core::model::Error::Numerical));
}
