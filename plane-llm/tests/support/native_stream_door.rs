// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::*;
use busbar_contract::abi::mechanism::{
    call::{Field, InHead},
    lifecycle::{slot, OpenIn, OpenOut, ReleaseIn},
};
use busbar_contract::abi::sdk::{
    door::Entry,
    plane::{conditional, observation},
};
use busbar_contract::host_records::{Decision, Expected};
use std::collections::BTreeMap;

const TICKET: Ticket = Ticket {
    slot: 71,
    generation: 1,
};
struct Backend {
    input: Vec<u8>,
    records: BTreeMap<Vec<u8>, Vec<u8>>,
    components: usize,
    closes: usize,
    commits: usize,
}
impl Backend {
    fn base() -> Vec<u8> {
        let mut input = conditional::encode_capability(conditional::Capability {
            ticket: TICKET,
            epoch: 1,
        });
        input.extend(extensions::encode(&[
            (observation::CAPABILITY, b"1"),
            (observation::SCOPE, &[7; 32]),
        ]));
        input
    }
    fn new() -> Self {
        Self {
            input: Self::base(),
            records: BTreeMap::new(),
            components: 0,
            closes: 0,
            commits: 0,
        }
    }
    fn apply(&mut self, extension: &[u8]) {
        self.input = Self::base();
        if let Some(request) = conditional::read_request(extension).unwrap() {
            for p in &request.candidate.predicates {
                match &p.expected {
                    Expected::Absent => assert!(!self.records.contains_key(&p.key)),
                    Expected::Bytes(v) => assert_eq!(self.records.get(&p.key), Some(v)),
                }
            }
            for mutation in &request.candidate.mutations {
                match mutation {
                    conditional::RecordMutation::Put { key, value, .. } => {
                        self.records.insert(key.clone(), value.clone());
                    }
                    conditional::RecordMutation::Delete { key, .. } => {
                        self.records.remove(key);
                    }
                }
            }
            self.commits += 1;
            self.input.extend(
                conditional::encode_result(
                    &request,
                    &conditional::ConditionalResult {
                        correlation: request.candidate.correlation,
                        decision: Ok(Decision::Applied),
                    },
                )
                .unwrap(),
            );
        } else if let Some(scope) = observation::ScopeRequest::from_extensions(extension).unwrap() {
            if scope.close {
                assert_eq!(self.components, 4);
                self.closes += 1;
            }
            self.input.extend(extensions::encode(&[(
                observation::RESULT,
                &serde_json::to_vec(&serde_json::json!({"scope":vec![7;32],"closed":scope.close}))
                    .unwrap(),
            )]));
        } else if let Some(observation) =
            observation::Observation::from_extensions(extension).unwrap()
        {
            assert!(!observation.evidence_ref.is_empty());
            assert!(self
                .records
                .values()
                .any(|r| String::from_utf8_lossy(r).contains("\"profile\"")));
            self.components += 1;
            let receipt = observation::Receipt {
                settlement_id: [self.components as u8; 16],
                duplicate: false,
                quarantined: false,
            };
            self.input.extend(extensions::encode(&[(
                observation::RESULT,
                &serde_json::to_vec(&receipt).unwrap(),
            )]));
        } else {
            panic!("unknown host exchange");
        }
    }
    fn ready(&mut self, session: &mut Session) {
        for _ in 0..32 {
            match session.poll(&self.input).unwrap() {
                Step::Ready => return,
                Step::Pending(b) => self.apply(&b),
            }
        }
        panic!("session did not become ready")
    }
}
fn initial(replay: bool) -> (Native, Backend) {
    let mut backend = Backend::new();
    let binding = Binding {
        scope: [0; 32],
        account_namespace: "account".into(),
        route_entry: "entry".into(),
        model: "gemini-pinned".into(),
        pricing_mode: "standard".into(),
        usage_schema: "gemini-interactions-2026-11-10".into(),
        input_class: 0,
        cache_read_class: 2,
        output_class: 1,
        tool_use_class: None,
        priced_classes: vec![0, 1, 2],
    };
    let mut session = Session::create(
        Partition::new(&"a".repeat(64), "account").unwrap(),
        "stream-intent".into(),
        binding,
        &backend.input,
    )
    .unwrap();
    backend.ready(&mut session);
    session.dispatch().unwrap();
    if replay {
        session.bind_provider_id("run-1", &backend.input).unwrap();
        backend.ready(&mut session);
        let run = session.run().unwrap().clone();
        let record = run.encode().unwrap();
        session = Session::stored(Found { run, record }, &backend.input).unwrap();
        backend.ready(&mut session);
        session.dispatch().unwrap();
    }
    let body = br#"{"model":"entry","input":"hi","stream":true}"#.to_vec();
    let native = Native {
        operation: if replay {
            Operation::Get {
                id: "run-1".into(),
                stream: true,
                last_event_id: Some("cursor".into()),
            }
        } else {
            Operation::Create
        },
        create: if replay {
            None
        } else {
            Some(interactions::create(&body).unwrap())
        },
        body,
        lookup: None,
        found: None,
        session: Some(session),
        head_sent: true,
        response: Vec::new(),
        response_fields: Vec::new(),
        response_status: 0,
        response_queued: false,
        streaming: true,
        stream: Some(
            State::new(
                if replay {
                    StreamMode::Replay
                } else {
                    StreamMode::Fresh
                },
                replay.then(|| "run-1".into()),
            )
            .unwrap(),
        ),
    };
    (native, backend)
}
struct FixtureOpen;
impl SafeSlot for FixtureOpen {
    type Kind = busbar_contract::plugin::markers::PlaneKind;
    type In = OpenIn;
    type Out = OpenOut;
    type State = LlmDoor;
    fn call(
        instance: Instance<'_, LlmDoor, Self::Kind>,
        input: Lent<'_, OpenIn>,
        _: Out<'_, OpenOut>,
    ) -> Outcome {
        let (native, _) = initial(input.settings().bytes() == b"replay");
        let shaping = Arc::new(Shaping::default());
        let mut unit = UnitState::new(shaping);
        unit.native = Some(native);
        let mut book = UnitBook::default();
        book.hold(1, unit);
        instance.open(LlmDoor {
            leases: Default::default(),
            native: Default::default(),
            generations: Generations::new(),
            shapings: Mutex::new(BTreeMap::new()),
            units: Mutex::new(book),
            services: None,
            webhooks: Webhooks::default(),
        });
        Outcome::Ready
    }
}
struct FixtureClose;
impl SafeSlot for FixtureClose {
    type Kind = busbar_contract::plugin::markers::PlaneKind;
    type In = InHead;
    type Out = OutHead;
    type State = LlmDoor;
    fn call(
        _: Instance<'_, LlmDoor, Self::Kind>,
        _: Lent<'_, InHead>,
        _: Out<'_, OutHead>,
    ) -> Outcome {
        Outcome::Ready
    }
}
struct Harness {
    instance: *mut std::ffi::c_void,
    backend: Backend,
    pending: usize,
}
impl Harness {
    fn open(replay: bool) -> Self {
        // SAFETY: zero-valid ABI records and static input, one new SDK-owned instance.
        unsafe {
            let mut input: OpenIn = std::mem::zeroed();
            input.settings = Blob::of(if replay { b"replay" } else { b"fresh" });
            let mut out: OpenOut = std::mem::zeroed();
            assert_eq!(
                Safe::<FixtureOpen>::enter(std::ptr::null_mut(), slot::OPEN, &input, &mut out),
                Outcome::Ready
            );
            Self {
                instance: out.instance,
                backend: initial(replay).1,
                pending: 0,
            }
        }
    }
    fn feed(&mut self, body: &[u8], flags: u32) -> (Vec<u8>, OnPieceOut) {
        let mut output = vec![0; 2 * 1024 * 1024];
        let mut arena = [0; 4096];
        let head = [Field {
            name: abi_str("content-type"),
            value: abi_str("text/event-stream"),
        }];
        // SAFETY: fixed live host buffers and borrowed inputs; callbacks run sequentially.
        unsafe {
            let mut fields: [OutField; 8] = std::mem::zeroed();
            let mut units: [UnitCount; 8] = std::mem::zeroed();
            let mut records: [RecordWrite; 4] = std::mem::zeroed();
            for _ in 0..40 {
                let mut input: OnPieceIn = std::mem::zeroed();
                input.head.ticket = TICKET;
                input.head.extensions = Blob::from_raw_parts(
                    self.backend.input.as_ptr(),
                    self.backend.input.len(),
                    0,
                    0,
                );
                input.unit = 1;
                input.from = FROM_FAR_END;
                input.flags = flags;
                input.status_code = 200;
                input.bytes = Blob::from_raw_parts(body.as_ptr(), body.len(), 0, 0);
                input.head_fields = head.as_ptr();
                input.head_fields_len = if flags & PIECE_HAS_STATUS != 0 { 1 } else { 0 };
                input.reply_buf = output.as_mut_ptr();
                input.reply_cap = output.len();
                input.arena_buf = arena.as_mut_ptr();
                input.arena_cap = arena.len();
                input.fields_buf = fields.as_mut_ptr();
                input.fields_cap = fields.len();
                input.units_buf = units.as_mut_ptr();
                input.units_cap = units.len();
                input.records_buf = records.as_mut_ptr();
                input.records_cap = records.len();
                let mut out: OnPieceOut = std::mem::zeroed();
                let result = Safe::<super::super::super::OnPiece>::enter(
                    self.instance,
                    busbar_contract::abi::plane::slot::ON_PIECE,
                    &input,
                    &mut out,
                );
                if result == Outcome::Pending {
                    assert_eq!(out.emitted, 0);
                    assert_eq!(out.fields_written, 0);
                    assert_eq!(out.reply_status, 0);
                    self.pending += 1;
                    let bytes = out.head.extensions.bytes().to_vec();
                    let mut release: ReleaseIn = std::mem::zeroed();
                    release.lease = out.head.lease;
                    let mut released: OutHead = std::mem::zeroed();
                    assert_eq!(
                        Safe::<busbar_contract::abi::sdk::plane_life::Release<LlmDoor>>::enter(
                            self.instance,
                            slot::RELEASE,
                            &release,
                            &mut released
                        ),
                        Outcome::Ready
                    );
                    self.backend.apply(&bytes);
                    continue;
                }
                assert_eq!(result, Outcome::Ready);
                assert_eq!(out.more, 0);
                return (output[..out.emitted as usize].to_vec(), out);
            }
        }
        panic!("pending stream failed to progress");
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        // SAFETY: close the owned SDK instance once with no outstanding callback/lease.
        unsafe {
            let input: InHead = std::mem::zeroed();
            let mut out: OutHead = std::mem::zeroed();
            assert_eq!(
                Safe::<FixtureClose>::enter(self.instance, slot::CLOSE, &input, &mut out),
                Outcome::Ready
            );
        }
    }
}
fn frame(value: Value) -> Vec<u8> {
    format!("data: {}\n\n", serde_json::to_string(&value).unwrap()).into_bytes()
}
fn created() -> Vec<u8> {
    frame(
        serde_json::json!({"event_type":"interaction.created","event_id":"created","interaction":{"id":"run-1","model":"gemini-pinned","status":"in_progress"}}),
    )
}
fn content() -> Vec<u8> {
    [
    frame(serde_json::json!({"event_type":"step.start","event_id":"start","index":0,"step":{"type":"model_output"}})),
    frame(serde_json::json!({"event_type":"step.delta","event_id":"delta","index":0,"delta":{"type":"text","text":"hello"}})),
    frame(serde_json::json!({"event_type":"step.stop","event_id":"stop","index":0})),
].concat()
}
fn completed(full: bool) -> Vec<u8> {
    let mut interaction = serde_json::json!({"id":"run-1","model":"gemini-pinned","status":"completed","usage":{"total_input_tokens":20,"total_cached_tokens":2,"total_output_tokens":5,"total_thought_tokens":0,"total_tool_use_tokens":0}});
    if full {
        interaction["steps"] =
            serde_json::json!([{"type":"model_output","content":[{"type":"text","text":"hello"}]}]);
    }
    frame(
        serde_json::json!({"event_type":"interaction.completed","event_id":"completed","interaction":interaction}),
    )
}
#[test]
fn live_stream_publishes_id_before_forward_and_settles_terminal_only_after_eof() {
    let mut h = Harness::open(false);
    let create = created();
    let text = content();
    let end = completed(false);
    let (first, out) = h.feed(&create, PIECE_HAS_STATUS);
    assert_eq!(first, create);
    assert_eq!(out.reply_status, 200);
    assert_eq!(h.pending, 1);
    assert!(h.backend.records.keys().any(|k| k.starts_with(b"ids/")));
    let (middle, out) = h.feed(&text, 0);
    assert_eq!(middle, text);
    assert_eq!(out.reply_status, 0);
    let (held, _) = h.feed(&end, 0);
    assert!(held.is_empty());
    assert_eq!(h.backend.components, 0);
    assert_eq!(h.backend.closes, 0);
    let (last, out) = h.feed(&[], PIECE_LAST);
    assert_eq!(last, end);
    assert_ne!(out.flags & EMIT_DONE, 0);
    assert_eq!(h.backend.components, 4);
    assert_eq!(h.backend.closes, 1);
}
#[test]
fn replay_uses_original_scope_and_requires_full_final_history_before_settlement() {
    for full in [false, true] {
        let mut h = Harness::open(true);
        let end = completed(full);
        let (wire, out) = h.feed(&end, PIECE_HAS_STATUS | PIECE_LAST);
        assert_eq!(wire, end);
        assert_eq!(out.reply_status, 200);
        assert_eq!(h.backend.components, if full { 4 } else { 0 });
        assert_eq!(h.backend.closes, usize::from(full));
    }
}
#[test]
fn late_cut_keeps_stream_status_and_does_not_fabricate_final_usage() {
    let mut h = Harness::open(false);
    h.feed(&created(), PIECE_HAS_STATUS);
    let (wire, out) = h.feed(&[], PIECE_CUT | PIECE_LAST);
    assert_eq!(out.reply_status, 0);
    assert_eq!(out.verdict, VERDICT_HARD);
    assert!(String::from_utf8_lossy(&wire).contains("event: error"));
    assert_eq!(h.backend.components, 0);
    assert_eq!(h.backend.closes, 0);
}
