use super::*;

#[test]
fn titan_response_shapes_must_not_become_empty_successes() {
    for body in [r#"{}"#, r#"{"images":null}"#, r#"{"images":[7]}"#] {
        assert!(handler::read_image_response(body.as_bytes()).is_err());
    }
    let images = handler::read_image_response(br#"{"images":["a","b"]}"#).unwrap();
    assert_eq!(images.images.len(), 2);
    assert_eq!(images.cost_basis.unwrap().count, 2);
    for body in [r#"{}"#, r#"{"embedding":null}"#, r#"{"embedding":"bad"}"#] {
        assert!(handler::read_embeddings_response(body.as_bytes()).is_err());
    }
}

#[test]
fn converse_requires_response_content_array() {
    for message in [
        serde_json::Value::Null,
        serde_json::json!({}),
        serde_json::json!({"content": "bad"}),
    ] {
        assert!(BedrockReader
            .read_response(
                crate::codec::test_host::HOST,
                &serde_json::json!({"output":{"message":message},"stopReason":"end_turn"})
            )
            .is_err());
    }
    assert!(BedrockReader
        .read_response(
            crate::codec::test_host::HOST,
            &serde_json::json!({"output":{"message":{"content":[{"text":"answer"}]}}})
        )
        .is_ok());
}

#[test]
fn explicit_invalid_max_tokens_must_not_become_default_budget() {
    for value in [
        serde_json::json!(-1),
        serde_json::json!(0),
        serde_json::json!(4294967296_u64),
        serde_json::json!(1.5),
        serde_json::json!("10"),
        serde_json::Value::Null,
    ] {
        assert!(BedrockReader
            .read_request(&serde_json::json!({"inferenceConfig":{"maxTokens":value}}))
            .is_err());
    }
    assert_eq!(
        BedrockReader
            .read_request(&serde_json::json!({"inferenceConfig":{"maxTokens":4294967295_u64}}))
            .unwrap()
            .max_tokens,
        Some(u32::MAX)
    );
    assert_eq!(
        BedrockReader
            .read_request(&serde_json::json!({}))
            .unwrap()
            .max_tokens,
        None
    );
}

#[test]
fn invalid_stream_indices_cannot_mutate_live_tool_blocks() {
    let mut state = crate::codec::ir::StreamDecodeState::default();
    state.started = true;
    state.open_tools.insert(1023);
    for kind in [
        ET_CONTENT_BLOCK_START,
        ET_CONTENT_BLOCK_DELTA,
        ET_CONTENT_BLOCK_STOP,
    ] {
        for index in [
            serde_json::json!(1024),
            serde_json::json!(-1),
            serde_json::json!("1023"),
        ] {
            let events = BedrockReader.read_response_events(crate::codec::test_host::HOST, "",
                &serde_json::json!({"type":kind,"contentBlockIndex":index,"delta":{"toolUse":{"input":"corruption"}}}),
                &mut state);
            assert!(matches!(events.as_slice(), [IrStreamEvent::Error(_)]));
            assert!(state.open_tools.contains(&1023));
        }
    }
    let events = BedrockReader.read_response_events(
        crate::codec::test_host::HOST,
        "",
        &serde_json::json!({"type":ET_CONTENT_BLOCK_STOP,"contentBlockIndex":1023}),
        &mut state,
    );
    assert!(matches!(
        events.as_slice(),
        [IrStreamEvent::BlockStop { index: 1023 }]
    ));
}

#[test]
fn served_tier_is_not_declared_dropped() {
    assert!(!STREAM_DROPS.contains(&"metadata.serviceTier"));
}
