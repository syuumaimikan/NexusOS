use super::*;
use crate::{Runtime, Status, SystemSource, Tool, Unavailable};

fn policy() -> Policy {
    Policy {
        scope: Scope {
            session: 1,
            task: 1,
        },
        now_ms: 100,
        max_age_ms: 50,
        destination: Destination::Local,
        allow_private: false,
        byte_budget: MAX_BYTES,
        token_budget: MAX_BYTES + 2,
    }
}
fn entry(text: &str) -> Entry<'_> {
    Entry {
        text,
        origin: Origin::File,
        scope: policy().scope,
        collected_ms: 100,
        priority: 1,
        relevance: 1,
        sensitivity: Sensitivity::Public,
        authorized: true,
        remote_allowed: false,
    }
}

#[test]
fn filters_authority_scope_age_and_private_data_before_tokenization() {
    struct Never;
    impl TokenCounter for Never {
        fn count(&self, _: &str) -> Option<usize> {
            panic!("filtered text reached tokenizer")
        }
    }
    let mut inputs = core::array::from_fn::<_, 9, _>(|_| entry("private content"));
    inputs[0].authorized = false;
    inputs[1].scope.session = 2;
    inputs[2].scope.task = 2;
    inputs[3].collected_ms = 49;
    inputs[4].collected_ms = 101;
    inputs[5].sensitivity = Sensitivity::Private;
    inputs[6].relevance = 0;
    inputs[7].text = "";
    inputs[8].sensitivity = Sensitivity::Secret;
    let context = Context::select(&inputs, &policy(), &Never).unwrap();
    assert_eq!(context.text(), "");
    assert_eq!(
        context.report(),
        Report {
            filtered: 8,
            redacted: 1,
            over_budget: 0
        }
    );
    assert_eq!(context.tokens(), 0);
}

#[test]
fn secret_markers_omit_whole_entry_case_insensitively() {
    for text in [
        "PASSWORD = test-value",
        "Authorization: Bearer example",
        "api_key=demo",
        "-----BEGIN RSA PRIVATE KEY-----",
        "パスワード：例",
        "token: ghp_example",
        "prefix\nrefresh_token: example\nsuffix",
    ] {
        let context = Context::select(&[entry(text)], &policy(), &ByteBpeBudget).unwrap();
        assert!(context.text().is_empty());
        assert_eq!(context.report().redacted, 1);
        assert!(context.bytes.iter().all(|&b| b == 0));
    }
}

#[test]
fn remote_disclosure_and_private_access_are_independent() {
    let mut p = policy();
    p.destination = Destination::Remote;
    let mut item = entry("個人の予定");
    item.sensitivity = Sensitivity::Private;
    let mut inputs = [item];
    for (private, remote, expected) in [
        (false, false, ""),
        (true, false, ""),
        (false, true, ""),
        (true, true, "個人の予定"),
    ] {
        p.allow_private = private;
        inputs[0].remote_allowed = remote;
        let context = Context::select(&inputs, &p, &ByteBpeBudget).unwrap();
        assert_eq!(context.text(), expected);
    }
    inputs[0].sensitivity = Sensitivity::Secret;
    assert!(Context::select(&inputs, &p, &ByteBpeBudget)
        .unwrap()
        .text()
        .is_empty());
}

#[test]
fn stable_ranking_preserves_provenance() {
    let mut entries = [entry("a"), entry("b"), entry("c"), entry("d"), entry("e")];
    entries[1].relevance = 2;
    entries[2].priority = 2;
    entries[3].priority = 2;
    entries[3].collected_ms = 99;
    entries[4].priority = 2;
    let context = Context::select(&entries, &policy(), &ByteBpeBudget).unwrap();
    assert_eq!(context.text(), "c\ne\nd\nb\na");
    for (selected, index) in context.selected().zip([2, 4, 3, 1, 0]) {
        assert_eq!(selected.input_index, index);
        assert_eq!(
            &context.text()[selected.start..selected.end],
            entries[index].text
        );
        assert_eq!(selected.collected_ms, entries[index].collected_ms);
        assert_eq!(selected.origin, Origin::File);
    }
}

#[test]
fn japanese_budget_boundaries_never_split_utf8_or_leave_rejected_bytes() {
    let entries = [entry("日本語🌸"), entry("a"), entry("世界")];
    for bytes in 1..32 {
        for tokens in 1..32 {
            let mut p = policy();
            p.byte_budget = bytes;
            p.token_budget = tokens;
            let context = Context::select(&entries, &p, &ByteBpeBudget).unwrap();
            assert!(context.text().len() <= bytes);
            assert!(context.tokens() <= tokens);
            assert!(context.bytes[context.len..].iter().all(|&b| b == 0));
            for selected in context.selected() {
                assert_eq!(
                    &context.text()[selected.start..selected.end],
                    entries[selected.input_index].text
                );
            }
        }
    }
}

#[test]
fn counts_the_whole_prompt_including_separators_and_special_tokens() {
    let mut p = policy();
    p.token_budget = 5;
    let context =
        Context::select(&[entry("ab"), entry("cd"), entry("e")], &p, &ByteBpeBudget).unwrap();
    assert_eq!(context.text(), "ab");
    assert_eq!(context.tokens(), 4);
    assert_eq!(context.report().over_budget, 2);
}

#[test]
fn oversize_high_priority_entry_does_not_starve_smaller_items() {
    let mut p = policy();
    p.byte_budget = 4;
    let context = Context::select(&[entry("large item"), entry("ok")], &p, &ByteBpeBudget).unwrap();
    assert_eq!(context.text(), "ok");
    assert_eq!(context.report().over_budget, 1);
}

#[test]
fn rejects_unbounded_input_and_invalid_policy() {
    let entries = core::array::from_fn::<_, 17, _>(|_| entry("a"));
    assert!(matches!(
        Context::select(&entries, &policy(), &ByteBpeBudget),
        Err(Error::TooManyEntries)
    ));
    for p in [
        Policy {
            byte_budget: 1025,
            ..policy()
        },
        Policy {
            byte_budget: 0,
            ..policy()
        },
        Policy {
            token_budget: 0,
            ..policy()
        },
        Policy {
            scope: Scope {
                session: 0,
                task: 1,
            },
            ..policy()
        },
    ] {
        assert!(matches!(
            Context::select(&[], &p, &ByteBpeBudget),
            Err(Error::InvalidPolicy)
        ));
    }
    let long = [b'x'; 1025];
    let text = core::str::from_utf8(&long).unwrap();
    let context = Context::select(&[entry(text)], &policy(), &ByteBpeBudget).unwrap();
    assert_eq!(context.report().filtered, 1);
}

#[test]
fn tokenizer_error_is_not_a_partial_success() {
    struct Failing;
    impl TokenCounter for Failing {
        fn count(&self, text: &str) -> Option<usize> {
            if text.contains('\n') {
                None
            } else {
                Some(1)
            }
        }
    }
    assert!(matches!(
        Context::select(&[entry("a"), entry("b")], &policy(), &Failing),
        Err(Error::Tokenization)
    ));
}

#[test]
fn clear_discards_text_metadata_and_counts() {
    let mut context = Context::select(&[entry("日本語")], &policy(), &ByteBpeBudget).unwrap();
    context.clear();
    assert_eq!(context.text(), "");
    assert_eq!(context.tokens(), 0);
    assert_eq!(context.selected().count(), 0);
    assert_eq!(context.report(), Report::default());
    assert!(context.bytes.iter().all(|&b| b == 0));
}

struct Source;
impl SystemSource for Source {
    fn snapshot(&mut self) -> Result<Snapshot, Unavailable> {
        Ok(Snapshot {
            uptime_ms: 100,
            thread_id: 7,
        })
    }
}
#[test]
fn runtime_only_exposes_verified_current_observation() {
    let mut runtime = Runtime::new(1);
    assert!(runtime.context().is_none());
    let mut request = Request {
        session: 1,
        id: 1,
        tool: Tool::SystemInfo,
    };
    assert_eq!(
        runtime.execute(request, 100, &mut Source).status,
        Status::Verified
    );
    let facts = runtime.context().unwrap();
    assert_eq!(facts.scope(), policy().scope);
    assert_eq!(facts.collected_ms(), 100);
    let mut buffer = [0; 96];
    assert_eq!(
        facts.write(&mut buffer).unwrap(),
        "uptime_ms=100\nservice_thread_id=7"
    );
    assert_eq!(facts.write(&mut [0; 3]), Err(Error::Buffer));
    request.id = 2;
    request.tool = Tool::FileWrite;
    assert_eq!(
        runtime.execute(request, 100, &mut Source).status,
        Status::ConfirmRequired
    );
    assert!(runtime.context().is_none());
    request.id = 3;
    request.tool = Tool::SystemInfo;
    runtime.execute(request, 100, &mut Source);
    runtime.cancel();
    assert!(runtime.context().is_none());
}

#[test]
fn failed_or_replayed_observations_clear_previous_context() {
    let request = Request {
        session: 1,
        id: 1,
        tool: Tool::SystemInfo,
    };
    for replay in [false, true] {
        let mut runtime = Runtime::new(1);
        runtime.execute(request, 100, &mut Source);
        let second = Request {
            id: if replay { 1 } else { 2 },
            ..request
        };
        assert_ne!(
            runtime.execute(second, 101, &mut Source).status,
            Status::Verified
        );
        assert!(runtime.context().is_none());
    }
}

#[cfg(feature = "model")]
#[test]
fn selected_context_drives_real_pretrained_model_with_exact_token_budget() {
    extern crate std;
    use crate::model::{Event, Llama, ModelProvider, ModelSession, Tokenizer};
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/models");
    let weights =
        std::fs::read(root.join("stories260K.bin")).expect("run tools/nexus-model/fetch.py");
    let tokenizer = Tokenizer::load(&std::fs::read(root.join("tok512.bin")).unwrap(), 512).unwrap();
    let model = Llama::load(&weights).unwrap();
    let inputs = [entry("Once upon a time"), entry("PASSWORD=never-send")];
    let budget = tokenizer.encode(inputs[0].text).unwrap().len();
    let p = Policy {
        token_budget: budget,
        ..policy()
    };
    let context = Context::select(&inputs, &p, &tokenizer).unwrap();
    assert_eq!(context.text(), inputs[0].text);
    assert_eq!(context.tokens(), budget);
    assert_eq!(context.report().redacted, 1);
    assert!(context.tokens() < model.context_len());
    let mut session =
        ModelSession::new(&model, tokenizer.encode(context.text()).unwrap(), 1).unwrap();
    loop {
        match session.poll().unwrap() {
            Event::Prefill => {}
            Event::Token(id) => {
                assert_eq!(id, 432);
                break;
            }
            Event::Finished(_) => panic!("expected oracle token"),
        }
    }
    let p = Policy {
        token_budget: budget - 1,
        ..policy()
    };
    assert!(Context::select(&inputs, &p, &tokenizer)
        .unwrap()
        .text()
        .is_empty());
}
