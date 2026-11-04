use super::*;
use crate::codec::drops;
use crate::codec::ir::IrBlockOrigin;
use serde_json::json;

fn projected_paths(origin: Option<&IrBlockOrigin>) -> Vec<String> {
    let (_, paths) = drops::scope(
        drops::Seam {
            direction: drops::Direction::Request,
            ingress: COUNT_LABEL,
            egress: crate::codec::anthropic::DECL.name,
        },
        || {
            drops::with_block_origin(origin, || {
                drops::note(drops::Dropped::member(
                    drops::IMAGE,
                    &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                    "projection probe",
                ));
            });
        },
    );
    paths
}

#[test]
fn nested_result_sources_survive_skipped_parts_and_missing_response() {
    let body = json!({
        "systemInstruction": {"parts": [{"text": "rules"}]},
        "contents": [{"parts": [
            {"futurePart": {}},
            {"functionResponse": {"name": "lookup", "response": {"output": "ok"},
                "parts": [{"futurePart": {}}, {"fileData": {"mimeType": "image/png", "fileUri": "https://example.test/image"}}]}},
            {"functionResponse": {"name": "empty"}}
        ]}]
    });
    let request = GeminiReader.read_request(&body).unwrap();
    let [system] = request.block_origins.system.as_slice() else {
        panic!("system text");
    };
    assert_eq!(projected_paths(Some(system)), ["systemInstruction.parts[]"]);
    let [message] = request.block_origins.messages.as_slice() else {
        panic!("one native turn");
    };
    let [result, missing] = message.as_slice() else {
        panic!("unknown part contributes no block");
    };
    let [response, image] = result.children.as_slice() else {
        panic!("result text and decoded attachment");
    };
    assert_eq!(
        projected_paths(Some(response)),
        ["contents[].parts[].functionResponse.response"]
    );
    assert_eq!(
        projected_paths(Some(image)),
        ["contents[].parts[].functionResponse.parts[]"]
    );
    let [synthetic] = missing.children.as_slice() else {
        panic!("synthetic null text");
    };
    assert!(synthetic.source.is_none());
}

#[test]
fn buffered_grounding_and_multi_output_part_keep_their_actual_containers() {
    let body = json!({"candidates": [{
        "groundingMetadata": {"groundingChunks": [{"web": {"uri": "https://example.test/source"}}]},
        "content": {"parts": [{"futurePart": {}}, {
            "text": "answer", "functionCall": {"name": "lookup", "args": {}},
            "inlineData": {"mimeType": "image/png", "data": "aW1hZ2U="}
        }]}
    }]});
    let response = GeminiReader
        .read_response(crate::codec::test_host::HOST, &body)
        .unwrap();
    let (grounding, parts) = response.block_origins.response.split_first().unwrap();
    assert_eq!(
        projected_paths(Some(grounding)),
        ["candidates[].groundingMetadata.groundingChunks[]"]
    );
    assert!(!parts.is_empty());
    for origin in parts {
        assert_eq!(
            projected_paths(Some(origin)),
            ["candidates[].content.parts[]"]
        );
    }
    assert_eq!(
        response.block_origins.response.len(),
        response.content.len()
    );
}

#[test]
fn streaming_origins_follow_emitted_indices_across_text_tools_and_media() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let body = json!({"candidates": [{"content": {"parts": [
        {"futurePart": {}}, {"text": "before"},
        {"functionCall": {"name": "lookup", "args": {}}},
        {"inlineData": {"mimeType": "image/png", "data": "aW1hZ2U="}},
        {"text": "after"}
    ]}}]});
    let events =
        GeminiReader.read_response_events(crate::codec::test_host::HOST, "", &body, &mut state);
    let mut opened = std::collections::BTreeSet::new();
    for event in events {
        if let IrStreamEvent::BlockStart { index, .. } = event {
            assert!(opened.insert(index));
            assert_eq!(
                projected_paths(state.block_origins.get(&index)),
                ["candidates[].content.parts[]"]
            );
        }
    }
    assert!(!opened.is_empty());
    let recorded: std::collections::BTreeSet<_> = state.block_origins.keys().copied().collect();
    assert_eq!(opened, recorded);

    let mut omitted = crate::codec::ir::StreamDecodeState::default();
    GeminiReader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &json!({"candidates": [{"content": {"parts": [{"futurePart": {}}]}}]}),
        &mut omitted,
    );
    assert!(omitted.block_origins.is_empty());
}

#[test]
fn citation_only_aggregate_uses_the_native_candidate_without_inventing_a_leaf() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    let body = json!({"candidates": [{
        "citationMetadata": {"citationSources": [{"uri": "https://example.test/legacy"}]},
        "groundingMetadata": {"groundingChunks": [{"web": {"uri": "https://example.test/grounding"}}]}
    }]});
    let events =
        GeminiReader.read_response_events(crate::codec::test_host::HOST, "", &body, &mut state);
    let origin = events
        .iter()
        .find_map(|event| match event {
            IrStreamEvent::BlockStart { index, .. } => state.block_origins.get(index),
            _ => None,
        })
        .expect("citation-only text start");
    assert_eq!(projected_paths(Some(origin)), ["candidates[]"]);
}
