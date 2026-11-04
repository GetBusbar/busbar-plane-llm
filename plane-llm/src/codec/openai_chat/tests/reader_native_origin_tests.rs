use super::*;
use crate::codec::drops;
use crate::codec::ir::IrBlockOrigin;
use serde_json::json;

fn projected_paths(origin: Option<&IrBlockOrigin>) -> Vec<String> {
    let (_, paths) = drops::scope(
        drops::Seam {
            direction: drops::Direction::Request,
            ingress: VENDOR_NAME,
            egress: crate::codec::anthropic::DECL.name,
        },
        || {
            drops::with_block_origin(origin, || {
                drops::note(drops::Dropped::member(
                    drops::IMAGE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    "projection probe",
                ));
            })
        },
    );
    paths
}

#[test]
fn folded_system_sources_and_aggregate_tool_children_keep_native_containers() {
    let body = json!({"messages": [
        {"role": "system", "content": "rules"},
        {"role": "developer", "content": [{"type": "futurePart"}, {"type": "text", "text": "rules"}]},
        {"role": "user", "content": [{"type": "futurePart"}, {"type": "text", "text": "question"}]},
        {"role": "tool", "tool_call_id": "call_lookup", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]},
        {"role": "tool", "tool_call_id": "call_missing"}
    ]});
    let request = OpenAiReader.read_request(&body).unwrap();
    let [scalar, array] = request.block_origins.system.as_slice() else {
        panic!("two folded native entries");
    };
    assert_eq!(projected_paths(Some(scalar)), ["messages[].content"]);
    assert_eq!(projected_paths(Some(array)), ["messages[].content[]"]);
    let [user, tool, missing] = request.block_origins.messages.as_slice() else {
        panic!("system entries fold out of messages");
    };
    let [user_part] = user.as_slice() else {
        panic!("unsupported part has no origin");
    };
    assert_eq!(projected_paths(Some(user_part)), ["messages[].content[]"]);
    let [tool] = tool.as_slice() else {
        panic!("tool result");
    };
    let [child] = tool.children.as_slice() else {
        panic!("array folds to one text child");
    };
    assert_eq!(projected_paths(Some(child)), ["messages[].content"]);
    let [missing] = missing.as_slice() else {
        panic!("missing tool result");
    };
    let [synthetic] = missing.children.as_slice() else {
        panic!("synthetic empty text");
    };
    assert!(synthetic.source.is_none());
}

#[test]
fn response_origin_does_not_shift_after_a_skipped_image_part() {
    let body = json!({"choices": [{"message": {
        "reasoning": "consider", "content": [
            {"type": "image_url", "image_url": {"url": "https://example.test/image"}},
            {"type": "text", "text": "answer"}
        ], "refusal": "cannot comply"
    }}]});
    let response = OpenAiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .unwrap();
    let [reasoning, content, refusal] = response.block_origins.response.as_slice() else {
        panic!("image output is omitted");
    };
    assert_eq!(
        projected_paths(Some(reasoning)),
        ["choices[].message.reasoning"]
    );
    assert_eq!(
        projected_paths(Some(content)),
        ["choices[].message.content[]"]
    );
    assert_eq!(
        projected_paths(Some(refusal)),
        ["choices[].message.refusal"]
    );
    assert_eq!(
        response.block_origins.response.len(),
        response.content.len()
    );
}

#[test]
fn stream_sources_distinguish_legacy_tools_refusals_and_annotations() {
    let cases = [
        (
            json!({"choices": [{"delta": {"function_call": {"name": "lookup", "arguments": "{}"}}}]}),
            "choices[].delta.function_call",
        ),
        (
            json!({"choices": [{"delta": {"tool_calls": [{"index": 17, "id": "call_lookup", "function": {"name": "lookup", "arguments": "{}"}}]}}]}),
            "choices[].delta.tool_calls[]",
        ),
        (
            json!({"choices": [{"delta": {"refusal": "cannot comply"}}]}),
            "choices[].delta.refusal",
        ),
        (
            json!({"choices": [{"delta": {"reasoning": "consider"}}]}),
            "choices[].delta.reasoning",
        ),
        (
            json!({"choices": [{"delta": {"annotations": [{"type": "url_citation", "url_citation": {"url": "https://example.test/source", "title": "source"}}]}}]}),
            "choices[].delta.annotations[]",
        ),
    ];
    for (body, expected) in cases {
        let mut state = crate::codec::ir::StreamDecodeState::default();
        let events =
            OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &body, &mut state);
        let mut saw_start = false;
        for event in events {
            if let IrStreamEvent::BlockStart { index, .. } = event {
                saw_start = true;
                assert_eq!(projected_paths(state.block_origins.get(&index)), [expected]);
            }
        }
        assert!(saw_start);
    }
    let mut state = crate::codec::ir::StreamDecodeState::default();
    OpenAiReader.read_response_events(crate::codec::test_host::HOST, "", &json!({"choices": [{"delta": {"tool_calls": [{"index": 17, "function": {"arguments": "{}"}}]}}]}), &mut state);
    assert!(
        state.block_origins.is_empty(),
        "arguments without a successful BlockStart create no origin"
    );
}
