use super::*;

struct Source {
    calls: u8,
    time: u64,
    thread: u64,
    fail: bool,
}
impl SystemSource for Source {
    fn snapshot(&mut self) -> Result<Snapshot, Unavailable> {
        self.calls += 1;
        if self.fail {
            return Err(Unavailable);
        }
        Ok(Snapshot {
            uptime_ms: self.time,
            thread_id: self.thread,
        })
    }
}
fn source() -> Source {
    Source {
        calls: 0,
        time: 100,
        thread: 7,
        fail: false,
    }
}

#[test]
fn direct_api_rejects_zero_session() {
    let mut s = source();
    let response = Runtime::new(0).execute(
        Request {
            session: 0,
            id: 1,
            tool: Tool::SystemInfo,
        },
        100,
        &mut s,
    );
    assert_eq!(response.status, Status::Invalid);
    assert_eq!(s.calls, 0);
}
fn request(tool: Tool) -> Request {
    Request {
        session: 1,
        id: 1,
        tool,
    }
}

#[test]
fn real_observations_required_for_completion() {
    let mut runtime = Runtime::new(1);
    let mut s = source();
    let response = runtime.execute(request(Tool::SystemInfo), 100, &mut s);
    assert_eq!(response.status, Status::Verified);
    assert_eq!(response.snapshot.thread_id, 7);
    assert_eq!(s.calls, 2);
    assert_eq!(runtime.state(), State::Complete);
    assert_eq!(runtime.audit().unwrap().attempts, 1);
}
#[test]
fn every_disallowed_tool_never_calls_backend() {
    for (tool, status) in [
        (Tool::FileRead, Status::Unsupported),
        (Tool::FileWrite, Status::ConfirmRequired),
        (Tool::FileRemove, Status::ConfirmRequired),
        (Tool::TerminalExecute, Status::ConfirmRequired),
        (Tool::ProcessStop, Status::ConfirmRequired),
        (Tool::SettingsWrite, Status::Privileged),
        (Tool::NetworkConnect, Status::ConfirmRequired),
        (Tool::KernelMemory, Status::Blocked),
    ] {
        let mut s = source();
        let mut runtime = Runtime::new(1);
        let response = runtime.execute(request(tool), 100, &mut s);
        assert_eq!(response.status, status);
        assert_eq!(response.snapshot, Snapshot::default());
        assert_eq!(s.calls, 0);
    }
}
#[test]
fn replay_and_foreign_session_do_not_execute() {
    let mut s = source();
    let mut runtime = Runtime::new(1);
    let r = request(Tool::SystemInfo);
    runtime.execute(r, 100, &mut s);
    assert_eq!(runtime.execute(r, 100, &mut s).status, Status::Invalid);
    assert_eq!(
        runtime
            .execute(
                Request {
                    session: 2,
                    id: 2,
                    ..r
                },
                100,
                &mut s
            )
            .status,
        Status::Invalid
    );
    assert_eq!(s.calls, 2);
}
#[test]
fn denials_consume_ids_and_budget() {
    let mut s = source();
    let mut runtime = Runtime::new(1);
    for id in 1..=64 {
        assert_eq!(
            runtime
                .execute(
                    Request {
                        id,
                        ..request(Tool::TerminalExecute)
                    },
                    100,
                    &mut s
                )
                .status,
            Status::ConfirmRequired
        );
    }
    assert_eq!(
        runtime
            .execute(
                Request {
                    id: 65,
                    ..request(Tool::SystemInfo)
                },
                100,
                &mut s
            )
            .status,
        Status::Exhausted
    );
    assert_eq!(s.calls, 0);
}
#[test]
fn cancellation_is_permanent() {
    let mut s = source();
    let mut runtime = Runtime::new(1);
    runtime.cancel();
    assert_eq!(
        runtime
            .execute(request(Tool::SystemInfo), 100, &mut s)
            .status,
        Status::Cancelled
    );
    assert_eq!(runtime.state(), State::Cancelled);
    assert_eq!(s.calls, 0);
}
#[test]
fn deadline_and_overflow_reject() {
    for (now, observed) in [(100, 600), (100, 99), (u64::MAX, u64::MAX)] {
        let mut s = source();
        s.time = observed;
        let mut runtime = Runtime::new(1);
        assert_eq!(
            runtime
                .execute(request(Tool::SystemInfo), now, &mut s)
                .status,
            Status::Expired
        );
    }
}
#[test]
fn invalid_evidence_retries_twice_then_fails() {
    let mut s = source();
    s.thread = 0;
    let mut runtime = Runtime::new(1);
    assert_eq!(
        runtime
            .execute(request(Tool::SystemInfo), 100, &mut s)
            .status,
        Status::Failed
    );
    assert_eq!(s.calls, 4);
    assert_eq!(runtime.audit().unwrap().attempts, 2);
}
#[test]
fn unavailable_is_failure_not_success() {
    let mut s = source();
    s.fail = true;
    let mut runtime = Runtime::new(1);
    assert_eq!(
        runtime
            .execute(request(Tool::SystemInfo), 100, &mut s)
            .status,
        Status::Failed
    );
    assert_eq!(s.calls, 2);
}
#[test]
fn request_codec_rejects_truncation_extension_reserved_and_unknown() {
    let r = request(Tool::SystemInfo);
    let bytes = wire::encode_request(r);
    assert_eq!(wire::decode_request(&bytes), Some(r));
    for len in 0..bytes.len() {
        assert_eq!(wire::decode_request(&bytes[..len]), None);
    }
    let mut extended = [0; 25];
    extended[..24].copy_from_slice(&bytes);
    assert_eq!(wire::decode_request(&extended), None);
    for offset in [0, 3, 4, 6, 7] {
        let mut bad = bytes;
        bad[offset] = 255;
        assert_eq!(wire::decode_request(&bad), None);
    }
    for offset in [8, 16] {
        let mut bad = bytes;
        bad[offset..offset + 8].fill(0);
        assert_eq!(wire::decode_request(&bad), None);
    }
}
#[test]
fn response_codec_validates_status_and_evidence() {
    let mut s = source();
    let response = Runtime::new(1).execute(request(Tool::SystemInfo), 100, &mut s);
    let bytes = wire::encode_response(response);
    assert_eq!(wire::decode_response(&bytes), Some(response));
    for len in 0..bytes.len() {
        assert_eq!(wire::decode_response(&bytes[..len]), None);
    }
    let mut bad = bytes;
    bad[4] = 1;
    assert_eq!(wire::decode_response(&bad), None);
    bad[4] = 255;
    assert_eq!(wire::decode_response(&bad), None);
}
#[test]
fn changing_thread_cannot_verify() {
    struct Changing(u64);
    impl SystemSource for Changing {
        fn snapshot(&mut self) -> Result<Snapshot, Unavailable> {
            self.0 += 1;
            Ok(Snapshot {
                uptime_ms: 100,
                thread_id: self.0,
            })
        }
    }
    assert_eq!(
        Runtime::new(1)
            .execute(request(Tool::SystemInfo), 100, &mut Changing(0))
            .status,
        Status::Failed
    );
}
#[test]
fn bounded_correction_can_recover() {
    struct Recover(u8);
    impl SystemSource for Recover {
        fn snapshot(&mut self) -> Result<Snapshot, Unavailable> {
            self.0 += 1;
            if self.0 == 1 {
                Err(Unavailable)
            } else {
                Ok(Snapshot {
                    uptime_ms: 100,
                    thread_id: 2,
                })
            }
        }
    }
    let mut runtime = Runtime::new(1);
    assert_eq!(
        runtime
            .execute(request(Tool::SystemInfo), 100, &mut Recover(0))
            .status,
        Status::Verified
    );
    assert_eq!(runtime.audit().unwrap().attempts, 2);
}

#[test]
fn policy_classification_does_not_expand_service_authority() {
    for (tool, policy, service) in [
        (Tool::SystemInfo, Ok(()), Ok(())),
        (Tool::FileRead, Ok(()), Err(Status::Unsupported)),
        (
            Tool::FileWrite,
            Err(Status::ConfirmRequired),
            Err(Status::ConfirmRequired),
        ),
        (
            Tool::FileRemove,
            Err(Status::ConfirmRequired),
            Err(Status::ConfirmRequired),
        ),
        (
            Tool::TerminalExecute,
            Err(Status::ConfirmRequired),
            Err(Status::ConfirmRequired),
        ),
        (
            Tool::ProcessStop,
            Err(Status::ConfirmRequired),
            Err(Status::ConfirmRequired),
        ),
        (
            Tool::SettingsWrite,
            Err(Status::Privileged),
            Err(Status::Privileged),
        ),
        (
            Tool::NetworkConnect,
            Err(Status::ConfirmRequired),
            Err(Status::ConfirmRequired),
        ),
        (
            Tool::KernelMemory,
            Err(Status::Blocked),
            Err(Status::Blocked),
        ),
    ] {
        assert_eq!(permitted(tool), policy, "{tool:?}");
        assert_eq!(authorize(tool), service, "{tool:?}");
        let mut backend = source();
        let response = Runtime::new(1).execute(request(tool), 100, &mut backend);
        assert_eq!(response.status, service.err().unwrap_or(Status::Verified));
        assert_eq!(backend.calls, if tool == Tool::SystemInfo { 2 } else { 0 });
    }
}

#[test]
fn unattributable_error_reply_round_trips_without_becoming_a_request() {
    let response = Response::error(0, 0, Status::Invalid);
    assert_eq!(
        wire::decode_response(&wire::encode_response(response)),
        Some(response)
    );
    assert_eq!(
        wire::decode_request(&wire::encode_request(Request {
            session: 0,
            id: 0,
            tool: Tool::SystemInfo,
        })),
        None
    );
}
