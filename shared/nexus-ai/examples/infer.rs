use nexus_ai_core::model::{Event, Llama, ModelProvider, ModelSession, Tokenizer};
fn main() {
    let args: Vec<_> = std::env::args().collect();
    assert!(args.len() >= 4, "infer WEIGHTS TOKENIZER PROMPT [TOKENS]");
    let model = Llama::load(&std::fs::read(&args[1]).unwrap()).expect("invalid weights");
    let tokenizer = Tokenizer::load(&std::fs::read(&args[2]).unwrap(), model.vocab_size())
        .expect("invalid tokenizer");
    let prompt = tokenizer.encode(&args[3]).unwrap();
    eprintln!("prompt={prompt:?}");
    let mut previous = *prompt.last().unwrap();
    let mut session = ModelSession::new(
        &model,
        prompt,
        args.get(4).map(|s| s.parse().unwrap()).unwrap_or(32),
    )
    .unwrap();
    let mut output = Vec::new();
    let mut tokens = Vec::new();
    loop {
        match session.poll().unwrap() {
            Event::Prefill => {}
            Event::Token(id) => {
                tokens.push(id);
                output.extend_from_slice(tokenizer.decode(previous, id).unwrap());
                previous = id;
            }
            Event::Finished(reason) => {
                eprintln!("finish={reason:?}");
                break;
            }
        }
    }
    eprintln!("tokens={tokens:?}");
    println!(
        "{}",
        String::from_utf8(output).expect("model emitted invalid UTF-8")
    );
}
