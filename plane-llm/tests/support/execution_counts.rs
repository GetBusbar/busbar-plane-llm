//! The real arrive/on_piece ABI reports physical quantities without a billing policy.
use super::*;
use busbar_contract::abi::mechanism::{
    call::InHead,
    lifecycle::{slot, OpenIn, OpenOut},
};
use busbar_contract::abi::sdk::door::Entry;

slot!(TestOpen, OpenIn, OpenOut, |instance, _input, _out| {
    let shaping = Shaping::from_settings(&serde_json::json!({
        "providers":{"claude":{"protocol":"anthropic"}},
        "models":{"entry":{"provider":"claude","upstream_model":"claude-pinned"}}
    }))
    .unwrap();
    instance.open(LlmDoor {
        leases: Default::default(),
        native: Default::default(),
        generations: Generations::new(),
        shapings: Mutex::new(BTreeMap::from([(1, Arc::new(shaping))])),
        units: Mutex::new(UnitBook::default()),
        services: None,
        webhooks: Webhooks::default(),
    });
    Outcome::Ready
});
slot!(TestClose, InHead, OutHead, |_, _, _| { Outcome::Ready });
struct Harness(*mut std::ffi::c_void);
impl Harness {
    fn open(stream: bool) -> Self {
        // SAFETY: zero-valid ABI structures and live inputs; SDK owns this one instance.
        unsafe {
            let input: OpenIn = std::mem::zeroed();
            let mut out: OpenOut = std::mem::zeroed();
            assert_eq!(
                Safe::<TestOpen>::enter(std::ptr::null_mut(), slot::OPEN, &input, &mut out),
                Outcome::Ready
            );
            let harness = Self(out.instance);
            let body = serde_json::to_vec(&serde_json::json!({
                "model":"entry", "max_tokens":100, "stream":stream,
                "messages":[{"role":"user","content":"run 1+1"}],
                "tools":[{"type":"code_execution_20260521","name":"code_execution"}]
            }))
            .unwrap();
            let mut arrive: ArriveIn = std::mem::zeroed();
            arrive.unit = 1;
            arrive.method = abi_str("POST");
            arrive.target = abi_str("/v1/messages");
            arrive.body = Blob::from_raw_parts(body.as_ptr(), body.len(), 0, 0);
            let mut arrived: ArriveOut = std::mem::zeroed();
            assert_eq!(
                Safe::<Arrive>::enter(
                    harness.0,
                    busbar_contract::abi::plane::slot::ARRIVE,
                    &arrive,
                    &mut arrived
                ),
                Outcome::Ready
            );
            let (head, _, _) = harness.piece(FROM_KERNEL, &[], 0, false);
            assert_ne!(
                head.flags & EMIT_TO_FAR_END,
                0,
                "execution dispatch requires no plugin pricing policy"
            );
            harness.piece(FROM_CALLER, &[], PIECE_LAST, false);
            harness
        }
    }
    fn piece(
        &self,
        from: u32,
        body: &[u8],
        flags: u32,
        stream: bool,
    ) -> (OnPieceOut, Vec<UnitCount>, Vec<u8>) {
        // SAFETY: all buffers and borrowed values outlive the synchronous ABI call.
        unsafe {
            let mut input: OnPieceIn = std::mem::zeroed();
            let mut out: OnPieceOut = std::mem::zeroed();
            let mut bytes = vec![0; 65536];
            let mut arena = vec![0; 4096];
            let mut fields: [OutField; 16] = std::mem::zeroed();
            let mut units: [UnitCount; 32] = std::mem::zeroed();
            let mut records: [RecordWrite; 16] = std::mem::zeroed();
            let headers = [busbar_contract::abi::mechanism::call::Field {
                name: abi_str("content-type"),
                value: abi_str(if stream {
                    "text/event-stream"
                } else {
                    "application/json"
                }),
            }];
            input.unit = 1;
            input.from = from;
            input.flags = flags;
            input.status_code = 200;
            input.member = abi_str("entry");
            input.bytes = Blob::from_raw_parts(body.as_ptr(), body.len(), 0, 0);
            input.head_fields = headers.as_ptr();
            input.head_fields_len = usize::from(flags & PIECE_HAS_STATUS != 0);
            input.reply_buf = bytes.as_mut_ptr();
            input.reply_cap = bytes.len();
            input.arena_buf = arena.as_mut_ptr();
            input.arena_cap = arena.len();
            input.fields_buf = fields.as_mut_ptr();
            input.fields_cap = fields.len();
            input.units_buf = units.as_mut_ptr();
            input.units_cap = units.len();
            input.records_buf = records.as_mut_ptr();
            input.records_cap = records.len();
            assert_eq!(
                Safe::<OnPiece>::enter(
                    self.0,
                    busbar_contract::abi::plane::slot::ON_PIECE,
                    &input,
                    &mut out
                ),
                Outcome::Ready
            );
            (
                out,
                units[..out.units_written as usize].to_vec(),
                bytes[..out.emitted as usize].to_vec(),
            )
        }
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        // SAFETY: close the unique SDK instance after all borrowed callbacks finish.
        unsafe {
            let input: InHead = std::mem::zeroed();
            let mut out: OutHead = std::mem::zeroed();
            assert_eq!(
                Safe::<TestClose>::enter(self.0, slot::CLOSE, &input, &mut out),
                Outcome::Ready
            );
        }
    }
}
fn response(counter: &str) -> Vec<u8> {
    format!(r#"{{"id":"msg_execution","type":"message","role":"assistant","model":"claude-pinned","content":[],"stop_reason":"end_turn","usage":{{"input_tokens":7,"output_tokens":3{counter}}}}}"#).into_bytes()
}
fn count(units: &[UnitCount], class: u32) -> Option<u64> {
    let matches: Vec<_> = units.iter().filter(|u| u.class == class).collect();
    assert!(matches.len() <= 1, "one cumulative row per class");
    matches.first().map(|u| u.amount)
}
#[test]
fn execution_count_buffered_abi_preserves_actual_zero_absence_and_tokens() {
    for (counter, expected) in [
        (
            ",\"server_tool_use\":{\"code_execution_requests\":2}",
            Some(2),
        ),
        (
            ",\"server_tool_use\":{\"code_execution_requests\":0}",
            Some(0),
        ),
        (
            ",\"server_tool_use\":{\"code_execution_requests\":null}",
            None,
        ),
        ("", None),
    ] {
        let h = Harness::open(false);
        let body = response(counter);
        let split = body.len() / 2;
        let (_, early, _) = h.piece(FROM_FAR_END, &body[..split], PIECE_HAS_STATUS, false);
        assert!(early.is_empty());
        let (out, units, _) = h.piece(FROM_FAR_END, &body[split..], PIECE_LAST, false);
        assert_eq!(out.verdict, VERDICT_OK);
        assert_eq!(count(&units, 28), expected);
        assert_eq!(count(&units, 0), Some(7));
        assert_eq!(count(&units, 1), Some(3));
    }
}
#[test]
fn execution_count_malformed_abi_retains_independent_tokens() {
    for value in ["-1", "0.5", "\"2\"", "{}", "18446744073709551616"] {
        let h = Harness::open(false);
        let body = response(&format!(
            ",\"server_tool_use\":{{\"code_execution_requests\":{value}}}"
        ));
        let (out, units, _) = h.piece(FROM_FAR_END, &body, PIECE_HAS_STATUS | PIECE_LAST, false);
        assert_eq!(out.verdict, VERDICT_COUNT_REFUSED, "{value}");
        assert_eq!(count(&units, 28), None);
        assert_eq!(count(&units, 0), Some(7));
        assert_eq!(count(&units, 1), Some(3));
    }
}
#[test]
fn execution_count_stream_abi_reports_provider_quantity_without_seconds() {
    let h = Harness::open(true);
    let start=b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_execution\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-pinned\",\"content\":[],\"usage\":{\"input_tokens\":7,\"output_tokens\":0}}}\n\n";
    h.piece(FROM_FAR_END, start, PIECE_HAS_STATUS, true);
    let delta=b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3,\"server_tool_use\":{\"code_execution_requests\":2}}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let (out, units, _) = h.piece(FROM_FAR_END, delta, PIECE_LAST, true);
    assert_eq!(out.verdict, VERDICT_OK);
    assert_eq!(count(&units, 28), Some(2));
    assert_eq!(count(&units, 0), Some(7));
    assert_eq!(count(&units, 1), Some(3));
    assert!(units
        .iter()
        .all(|u| [0, 1, 2, 3, 19, 28].contains(&u.class)));
}
#[test]
fn execution_count_duplicate_json_cannot_report_the_last_value() {
    let h = Harness::open(false);
    let body = response(
        ",\"server_tool_use\":{\"code_execution_requests\":2,\"code_execution_requests\":100}",
    );
    let (_, units, _) = h.piece(FROM_FAR_END, &body, PIECE_HAS_STATUS | PIECE_LAST, false);
    assert_eq!(count(&units, 28), None);
}
#[test]
fn execution_billing_policy_is_absent_from_schema_and_descriptor() {
    assert!(!std::str::from_utf8(SETTINGS_SCHEMA)
        .unwrap()
        .contains("anthropic_execution_policy"));
    assert!(!PROVIDER_KEYS.contains("anthropic_execution_policy"));
    let stated = unsafe { TAIL.billable_classes() };
    let class = stated[28];
    assert_eq!(
        unsafe { std::slice::from_raw_parts(class.class.ptr(), class.class.len()) },
        b"code_execution_requests"
    );
    assert_eq!(
        unsafe { std::slice::from_raw_parts(class.family.ptr(), class.family.len()) },
        b"count"
    );
}
