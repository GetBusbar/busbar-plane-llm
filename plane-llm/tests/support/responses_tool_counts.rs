//! The real arrive/on_piece ABI reports physical quantities without a billing policy.
use super::*;
use busbar_contract::abi::mechanism::{
    call::InHead,
    lifecycle::{slot, OpenIn, OpenOut},
};
use busbar_contract::abi::sdk::door::Entry;

slot!(TestOpen, OpenIn, OpenOut, |instance, _input, _out| {
    let shaping = Shaping::from_settings(&serde_json::json!({
        "limits":{"request_body_max_bytes":4096},
        "providers":{"images":{"protocol":"responses"}},
        "models":{"entry":{"provider":"images","upstream_model":"base-pinned"}}
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
        Self::open_ingress(stream, false)
    }
    fn open_ingress(stream: bool, cross: bool) -> Self {
        // SAFETY: zero-valid ABI structures and live inputs; SDK owns this one instance.
        unsafe {
            let input: OpenIn = std::mem::zeroed();
            let mut out: OpenOut = std::mem::zeroed();
            assert_eq!(
                Safe::<TestOpen>::enter(std::ptr::null_mut(), slot::OPEN, &input, &mut out),
                Outcome::Ready
            );
            let harness = Self(out.instance);
            let request = if cross {
                serde_json::json!({"model":"entry","stream":stream,"messages":[{"role":"user","content":"draw and compute"}]})
            } else {
                serde_json::json!({"model":"entry","stream":stream,"input":"draw and compute","tools":[{"type":"image_generation"},{"type":"code_interpreter","container":{"type":"auto"}}]})
            };
            let body = serde_json::to_vec(&request).unwrap();
            let mut arrive: ArriveIn = std::mem::zeroed();
            arrive.unit = 1;
            arrive.method = abi_str("POST");
            arrive.target = abi_str(if cross {
                "/v1/chat/completions"
            } else {
                "/v1/responses"
            });
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
                "tool dispatch requires no plugin pricing policy"
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
            let mut units: [UnitCount; 64] = std::mem::zeroed();
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

fn count(units: &[UnitCount], class: u32) -> Option<u64> {
    let matches: Vec<_> = units.iter().filter(|u| u.class == class).collect();
    assert!(matches.len() <= 1, "one cumulative row per class");
    matches.first().map(|u| u.amount)
}
fn response(image: serde_json::Value, output: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"id":"resp_tools","object":"response","model":"base-pinned","status":"completed",
        "usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10},
        "tool_usage":{"image_gen":image},"output":output})
}
fn image() -> serde_json::Value {
    serde_json::json!({"input_tokens":100,"input_tokens_details":{"text_tokens":40,"image_tokens":60},
        "cached_tokens":30,"cached_tokens_details":{"cached_text_tokens":10,"cached_image_tokens":20},
        "output_tokens":50,"output_tokens_details":{"text_tokens":5,"image_tokens":45},"total_tokens":150})
}
fn calls() -> serde_json::Value {
    serde_json::json!([
        {"id":"ig_1","type":"image_generation_call","status":"completed","result":"payload"},
        {"id":"ci_1","type":"code_interpreter_call","status":"completed","container_id":"cntr_1","code":"1+1","outputs":[]},
        {"id":"sh_1","type":"shell_call","status":"completed","environment":{"type":"container_reference","container_id":"cntr_1"},"action":{"commands":["ls"]}},
        {"id":"sh_local","type":"shell_call","status":"completed","environment":{"type":"local"},"action":{"commands":["ls"]}},
        {"id":"ig_pending","type":"image_generation_call","status":"in_progress","result":null}
    ])
}
fn buffered(value: &serde_json::Value) -> (OnPieceOut, Vec<UnitCount>) {
    let h = Harness::open(false);
    let body = serde_json::to_vec(value).unwrap();
    let (out, units, _) = h.piece(FROM_FAR_END, &body, PIECE_HAS_STATUS | PIECE_LAST, false);
    (out, units)
}
#[test]
fn responses_tools_buffered_abi_disjoint_partitions_and_completed_calls() {
    let mut output = calls();
    let duplicate = output[0].clone();
    output.as_array_mut().unwrap().push(duplicate);
    let (out, units) = buffered(&response(image(), output));
    assert_eq!(out.verdict, VERDICT_OK);
    for (class, n) in [
        (0, 7),
        (1, 3),
        (29, 30),
        (30, 10),
        (32, 40),
        (33, 20),
        (38, 5),
        (39, 45),
        (41, 1),
        (42, 1),
        (43, 1),
    ] {
        assert_eq!(count(&units, class), Some(n), "class {class}");
    }
    for class in [31, 34, 35, 36, 37, 40] {
        assert_eq!(count(&units, class), None, "class {class}");
    }
    assert_eq!((29..=40).filter_map(|c| count(&units, c)).sum::<u64>(), 150);
}
#[test]
fn responses_tools_cache_absent_zero_and_aggregate_only_are_distinct() {
    for (image, expected) in [
        (
            serde_json::json!({"input_tokens":100,"input_tokens_details":{"text_tokens":40,"image_tokens":60},"output_tokens":50}),
            vec![(31, 40), (34, 60), (40, 50)],
        ),
        (
            serde_json::json!({"input_tokens":100,"cached_tokens":0,"output_tokens":50}),
            vec![(35, 100), (36, 0), (40, 50)],
        ),
        (
            serde_json::json!({"input_tokens":100,"input_tokens_details":{"text_tokens":40,"image_tokens":60},"cached_tokens":30,"output_tokens":50}),
            vec![(35, 70), (36, 30), (40, 50)],
        ),
        (
            serde_json::json!({"input_tokens":100,"output_tokens":50}),
            vec![(37, 100), (40, 50)],
        ),
    ] {
        let (out, units) = buffered(&response(image, serde_json::json!([])));
        assert_eq!(out.verdict, VERDICT_OK);
        for class in 29..=40 {
            assert_eq!(
                count(&units, class),
                expected.iter().find(|(c, _)| *c == class).map(|(_, n)| *n),
                "class {class}"
            );
        }
        assert_eq!((29..=40).filter_map(|c| count(&units, c)).sum::<u64>(), 150);
    }
}
#[test]
fn responses_tools_tool_only_and_unknown_measurements_are_not_seconds_or_zero() {
    let mut value = response(image(), calls());
    value.as_object_mut().unwrap().remove("usage");
    let (out, units) = buffered(&value);
    assert_eq!(out.verdict, VERDICT_OK);
    assert_eq!(count(&units, 29), Some(30));
    assert_eq!(count(&units, 0), None);
    assert_eq!(count(&units, 1), None);
    let (out, units) = buffered(
        &serde_json::json!({"id":"resp_unknown","object":"response","model":"base-pinned","status":"completed"}),
    );
    assert_eq!(out.verdict, VERDICT_OK);
    assert!((29..=43).all(|c| count(&units, c).is_none()));
}
#[test]
fn responses_tools_malformed_evidence_preserves_independent_parent_tokens() {
    for bad in [
        serde_json::json!(-1),
        serde_json::json!(0.5),
        serde_json::json!("3"),
        serde_json::json!({}),
        serde_json::json!(u64::MAX),
    ] {
        let mut image = image();
        image["cached_tokens"] = bad;
        let (out, units) = buffered(&response(image, calls()));
        assert_eq!(out.verdict, VERDICT_COUNT_REFUSED);
        assert!((29..=37).all(|c| count(&units, c).is_none()));
        assert_eq!(count(&units, 0), Some(7));
        assert_eq!(count(&units, 1), Some(3));
        assert_eq!(count(&units, 38), Some(5));
        assert_eq!(count(&units, 41), Some(1));
    }
    let mut bad = image();
    bad["input_tokens"] = serde_json::json!(99);
    let (out, units) = buffered(&response(bad, calls()));
    assert_eq!(out.verdict, VERDICT_COUNT_REFUSED);
    assert_eq!(count(&units, 0), Some(7));
    assert!((29..=40).all(|c| count(&units, c).is_none()));
    let (out, units) = buffered(&response(
        serde_json::json!({"input_tokens_details":{"text_tokens":40,"image_tokens":60},"output_tokens_details":{"text_tokens":5,"image_tokens":45},"total_tokens":149}),
        calls(),
    ));
    assert_eq!(out.verdict, VERDICT_COUNT_REFUSED);
    assert_eq!(count(&units, 0), Some(7));
    assert!((29..=40).all(|c| count(&units, c).is_none()));
    let mut output = calls();
    let mut duplicate = output[0].clone();
    duplicate["status"] = serde_json::json!("failed");
    output.as_array_mut().unwrap().push(duplicate);
    let (out, units) = buffered(&response(image(), output));
    assert_eq!(out.verdict, VERDICT_COUNT_REFUSED);
    assert_eq!(count(&units, 0), Some(7));
    assert_eq!(count(&units, 29), Some(30));
    assert_eq!(count(&units, 41), None);
}
#[test]
fn responses_tools_duplicate_json_and_stream_terminals_cannot_double_count() {
    for duplicate in [
        r#""cached_tokens":30,"cached_tokens":1"#,
        r#""cached_tokens":30,"input_tokens":100,"input_tokens":2"#,
    ] {
        let h = Harness::open(false);
        let body = format!(
            r#"{{"id":"r","object":"response","status":"completed","model":"base-pinned","output":[],"usage":{{"input_tokens":7,"output_tokens":3}},"tool_usage":{{"image_gen":{{"input_tokens":100,{duplicate},"output_tokens":50}}}}}}"#
        );
        let (out, units, _) = h.piece(
            FROM_FAR_END,
            body.as_bytes(),
            PIECE_HAS_STATUS | PIECE_LAST,
            false,
        );
        assert_eq!(out.verdict, VERDICT_COUNT_REFUSED);
        assert_eq!(count(&units, 0), Some(7));
        assert!((29..=40).all(|c| count(&units, c).is_none()));
    }
    let h = Harness::open(true);
    let first = response(image(), calls());
    let second = response(
        serde_json::json!({"input_tokens":999,"output_tokens":999}),
        calls(),
    );
    let frames = format!(
        "event: response.completed\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
        serde_json::json!({"type":"response.completed","response":first}),
        serde_json::json!({"type":"response.completed","response":second})
    );
    let (out, units, _) = h.piece(
        FROM_FAR_END,
        frames.as_bytes(),
        PIECE_HAS_STATUS | PIECE_LAST,
        true,
    );
    assert_eq!(out.verdict, VERDICT_OK);
    assert_eq!(count(&units, 29), Some(30));
    assert_eq!(count(&units, 37), None);
    assert_eq!(count(&units, 41), Some(1));
    assert_eq!(count(&units, 0), Some(7));
}
#[test]
fn responses_tools_large_image_projection_reports_original_metadata() {
    let h = Harness::open(false);
    let mut output = calls();
    output[0]["result"] = serde_json::json!("x".repeat(50000));
    let body = serde_json::to_vec(&response(image(), output)).unwrap();
    let mut final_units = Vec::new();
    for (i, chunk) in body.chunks(2048).enumerate() {
        let flags = if i == 0 { PIECE_HAS_STATUS } else { 0 }
            | if (i + 1) * 2048 >= body.len() {
                PIECE_LAST
            } else {
                0
            };
        let (out, units, _) = h.piece(FROM_FAR_END, chunk, flags, false);
        if flags & PIECE_LAST != 0 {
            assert_eq!(out.verdict, VERDICT_OK);
            final_units = units;
        }
    }
    assert_eq!(count(&final_units, 0), Some(7));
    assert_eq!(count(&final_units, 29), Some(30));
    assert_eq!(count(&final_units, 41), Some(1));
    assert_eq!(count(&final_units, 42), Some(1));
    assert_eq!(count(&final_units, 43), Some(1));
}

#[test]
fn responses_tools_stream_abi_waits_for_complete_evidence_and_retains_parent_on_refusal() {
    let h = Harness::open(true);
    let value = response(image(), calls());
    let frame = format!(
        "event: response.completed\ndata: {}\n\n",
        serde_json::json!({"type":"response.completed","response":value})
    );
    let split = frame.len() / 2;
    let (_, units, _) = h.piece(
        FROM_FAR_END,
        &frame.as_bytes()[..split],
        PIECE_HAS_STATUS,
        true,
    );
    assert!((29..=43).all(|c| count(&units, c).is_none()));
    let (out, units, _) = h.piece(FROM_FAR_END, &frame.as_bytes()[split..], PIECE_LAST, true);
    assert_eq!(out.verdict, VERDICT_OK);
    assert_eq!(count(&units, 29), Some(30));
    assert_eq!(count(&units, 41), Some(1));
    let h = Harness::open(true);
    let truncated = &frame.as_bytes()[..split];
    let (_, units, _) = h.piece(FROM_FAR_END, truncated, PIECE_HAS_STATUS | PIECE_LAST, true);
    assert!((29..=43).all(|c| count(&units, c).is_none()));
    let h = Harness::open(true);
    let duplicate="event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"status\":\"completed\",\"usage\":{\"input_tokens\":7,\"output_tokens\":3},\"tool_usage\":{\"image_gen\":{\"input_tokens\":2,\"input_tokens\":3}},\"output\":[]}}\n\n";
    let (out, units, _) = h.piece(
        FROM_FAR_END,
        duplicate.as_bytes(),
        PIECE_HAS_STATUS | PIECE_LAST,
        true,
    );
    assert_eq!(out.verdict, VERDICT_COUNT_REFUSED);
    assert_eq!(count(&units, 0), Some(7));
    assert_eq!(count(&units, 1), Some(3));
    assert!((29..=40).all(|c| count(&units, c).is_none()));
}
#[test]
fn responses_tools_shell_environment_absence_is_unknown_and_wrong_identity_is_refused() {
    let mut output = calls();
    output[2].as_object_mut().unwrap().remove("environment");
    let (out, units) = buffered(&response(image(), output));
    assert_eq!(out.verdict, VERDICT_OK);
    assert_eq!(count(&units, 43), None);
    assert_eq!(count(&units, 42), Some(1));
    let mut output = calls();
    output[2]["environment"]["container_id"] = serde_json::json!(false);
    let (out, units) = buffered(&response(image(), output));
    assert_eq!(out.verdict, VERDICT_COUNT_REFUSED);
    assert_eq!(count(&units, 43), None);
    assert_eq!(count(&units, 0), Some(7));
    let mut value = response(image(), calls());
    value["response"] = serde_json::json!({"tool_usage":{}});
    let bytes = serde_json::to_string(&value).unwrap().replace(
        "\"cached_tokens\":30",
        "\"cached_tokens\":30,\"cached_tokens\":1",
    );
    let h = Harness::open(false);
    let (out, units, _) = h.piece(
        FROM_FAR_END,
        bytes.as_bytes(),
        PIECE_HAS_STATUS | PIECE_LAST,
        false,
    );
    assert_eq!(out.verdict, VERDICT_COUNT_REFUSED);
    assert_eq!(count(&units, 0), Some(7));
    assert!((29..=40).all(|c| count(&units, c).is_none()));
}

fn event(name: &str, body: serde_json::Value) -> String {
    format!("event: {name}\ndata: {body}\n\n")
}
fn item_event(index: usize, item: serde_json::Value) -> String {
    event(
        "response.output_item.done",
        serde_json::json!({"type":"response.output_item.done","output_index":index,"sequence_number":index+1,"item":item}),
    )
}
#[test]
fn responses_completed_items_survive_cut_stream_on_same_and_cross_protocol_abi() {
    for cross in [false, true] {
        let h = Harness::open_ingress(true, cross);
        let items = calls();
        let frames = (0..3)
            .map(|i| item_event(i, items[i].clone()))
            .collect::<String>();
        let (_, units, _) = h.piece(
            FROM_FAR_END,
            frames.as_bytes(),
            PIECE_HAS_STATUS | PIECE_LAST,
            true,
        );
        for class in 41..=43 {
            assert_eq!(
                count(&units, class),
                Some(1),
                "class {class}, cross {cross}"
            );
        }
        assert!((29..=40).all(|c| count(&units, c).is_none()));
        assert_eq!(count(&units, 0), None);
        assert_eq!(count(&units, 1), None);
    }
}
#[test]
fn responses_completed_events_need_real_container_evidence_and_do_not_invent_other_counts() {
    let image = event(
        "response.image_generation_call.completed",
        serde_json::json!({"item_id":"ig_1","output_index":0,"sequence_number":1}),
    );
    let code = event(
        "response.code_interpreter_call.completed",
        serde_json::json!({"item_id":"ci_1","output_index":1,"sequence_number":2}),
    );
    for with_metadata in [false, true] {
        let h = Harness::open(true);
        let mut frames = String::new();
        if with_metadata {
            frames.push_str(&event("response.output_item.added",serde_json::json!({"output_index":1,"sequence_number":0,"item":{"id":"ci_1","type":"code_interpreter_call","status":"in_progress","container_id":"cntr_1"}})));
        }
        frames.push_str(&image);
        frames.push_str(&image);
        frames.push_str(&code);
        let (_, units, _) = h.piece(
            FROM_FAR_END,
            frames.as_bytes(),
            PIECE_HAS_STATUS | PIECE_LAST,
            true,
        );
        assert_eq!(count(&units, 41), Some(1));
        assert_eq!(count(&units, 42), with_metadata.then_some(1));
        assert_eq!(count(&units, 43), None);
    }
}
#[test]
fn responses_completed_events_and_final_snapshots_are_one_identity_union() {
    for omit_from_snapshot in [false, true] {
        let h = Harness::open(true);
        let items = calls();
        let prefix = item_event(0, items[0].clone());
        h.piece(FROM_FAR_END, prefix.as_bytes(), PIECE_HAS_STATUS, true);
        let mut final_response = response(
            image(),
            if omit_from_snapshot {
                serde_json::json!([])
            } else {
                items
            },
        );
        final_response["usage"] = serde_json::json!({"input_tokens":7,"output_tokens":3});
        let tail = format!(
            "{}{}",
            prefix,
            event(
                "response.completed",
                serde_json::json!({"response":final_response})
            )
        );
        let (out, units, _) = h.piece(FROM_FAR_END, tail.as_bytes(), PIECE_LAST, true);
        assert_eq!(out.verdict, VERDICT_OK);
        assert_eq!(count(&units, 41), Some(1));
        assert_eq!(count(&units, 29), Some(30));
        assert_eq!(count(&units, 0), Some(7));
    }
}
#[test]
fn responses_completed_event_conflicts_and_ambiguous_json_keep_independent_tokens() {
    let first = item_event(0, calls()[0].clone());
    let bad_events = [
        event(
            "response.image_generation_call.completed",
            serde_json::json!({"item_id":"ig_other","output_index":0}),
        ),
        item_event(
            0,
            serde_json::json!({"id":"ig_1","type":"image_generation_call","status":"failed"}),
        ),
        event(
            "response.image_generation_call.completed",
            serde_json::json!({"item_id":"ig_2","output_index":4096}),
        ),
        format!(
            "event: response.image_generation_call.completed\ndata: {}\n\n",
            r#"{"item_id":"ig_1","item_id":"ig_2","output_index":0}"#
        ),
        format!(
            "event: response.output_item.done\ndata: {}\n\n",
            r#"{"output_index":0,"item":{"type":"image_generation_call","id":"ig_1","id":"ig_2","status":"completed"}}"#
        ),
    ];
    for bad in bad_events {
        let h = Harness::open(true);
        let frames = format!(
            "{}{}{}",
            first,
            bad,
            event(
                "response.completed",
                serde_json::json!({"response":response(image(),calls())})
            )
        );
        let (out, units, _) = h.piece(
            FROM_FAR_END,
            frames.as_bytes(),
            PIECE_HAS_STATUS | PIECE_LAST,
            true,
        );
        assert_eq!(out.verdict, VERDICT_COUNT_REFUSED);
        assert!((41..=43).all(|c| count(&units, c).is_none()));
        assert_eq!(count(&units, 0), Some(7));
        assert_eq!(count(&units, 29), Some(30));
    }
}
