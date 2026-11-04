use super::*;

#[test]
fn malformed_complete_frame_is_not_a_successful_array() {
    for prefix in ["", "data: {\"ok\":true}\n\n"] {
        let mut framer = GeminiJsonArrayFramer::new();
        let mut bytes =
            framer.feed(format!("{prefix}data: malformed\n\ndata: {{}}\n\n").as_bytes());
        bytes.extend(framer.finish());
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let entries = value.as_array().unwrap();
        assert!(entries.last().unwrap().get("error").is_some());
        assert_eq!(entries.len(), if prefix.is_empty() { 1 } else { 2 });
        assert!(framer.finish().is_empty());
    }
}

#[test]
fn call_ledger_preserves_latest_turn_fifo_and_explicit_id_consumption() {
    let mut ledger = GeminiCallLedger::default();
    ledger.record_call("old", "f", 0);
    ledger.record_call("first", "f", 1);
    ledger.record_call("second", "f", 1);
    assert_eq!(ledger.pair_response(Some("second"), "other"), "second");
    assert_eq!(ledger.pair_response(None, "f"), "first");
    assert_eq!(ledger.pair_response(None, "f"), "old");
    assert_eq!(ledger.pair_response(None, "f"), "f");
}

#[test]
fn cumulative_sources_only_walk_new_citation_offsets() {
    let text = "x".repeat(1024);
    let mut marks = Vec::new();
    let mut chars = 0;
    extend_char_marks(&mut marks, &mut chars, 0, &text);
    let mut state = citations::GeminiCitationState::default();
    let mut emitted = 0;
    let source = serde_json::json!({"uri":"https://example.com", "startIndex": 999});
    let candidate = serde_json::json!({"citationMetadata":{"citationSources":vec![source; 100]}});
    assert_eq!(
        citations::read_gemini_streamed_citations(
            &candidate,
            &text,
            &marks,
            &mut state,
            &mut emitted
        )
        .unwrap()
        .len(),
        100
    );
    citations::CHARS_WALKED.with(|count| count.set(0));
    assert!(citations::read_gemini_streamed_citations(
        &candidate,
        &text,
        &marks,
        &mut state,
        &mut emitted
    )
    .unwrap()
    .is_empty());
    citations::CHARS_WALKED.with(|count| assert_eq!(count.get(), 0));
}

#[test]
fn block_lookup_tracks_interleaved_sparse_indices() {
    let writer = GeminiWriter;
    for (index, text) in [(9000, "é"), (1, "b"), (9000, "c")] {
        writer.write_response_event(
            crate::codec::test_host::HOST,
            &IrStreamEvent::BlockDelta {
                index,
                delta: crate::codec::ir::IrDelta::TextDelta(text.to_string()),
            },
        );
    }
    let stream = writer.stream.lock();
    assert_eq!(stream.stream_text.block_starts.len(), 2);
    assert!(!stream.stream_text.block_starts[&9000].contiguous);
    assert!(stream.stream_text.block_starts[&1].contiguous);
    assert_eq!(stream.stream_text.block_starts[&1].start, 2);
}

#[test]
fn writer_refuses_cap_plus_one_tools_and_recovers_after_flush() {
    let writer = GeminiWriter;
    let start = |index| IrStreamEvent::BlockStart {
        index,
        block: crate::codec::ir::IrBlockMeta::ToolUse {
            id: format!("call-{index}"),
            name: "tool".to_string(),
        },
        refusal: false,
    };
    let host = crate::codec::test_host::HOST;
    for index in 0..MAX_GEMINI_TOOL_FRAMES {
        assert!(writer
            .try_write_response_events(host, &start(index))
            .unwrap()
            .is_empty());
    }
    // Updating an already tracked tool must still work at capacity.
    assert!(writer.try_write_response_events(host, &start(0)).is_ok());
    let error = writer
        .try_write_response_events(host, &start(MAX_GEMINI_TOOL_FRAMES))
        .unwrap_err();
    assert_eq!(error.class, StatusClass::ServerError);
    assert_eq!(
        error.provider_signal.as_deref(),
        Some("tool count exceeds the translation limit")
    );
    // The compatibility entry point must emit an error too, never silently lose the call.
    let (_, frame) = writer
        .write_response_event(host, &start(MAX_GEMINI_TOOL_FRAMES))
        .unwrap();
    assert!(frame.get("error").is_some());
    assert_eq!(
        writer.stream.lock().open_tools.len(),
        MAX_GEMINI_TOOL_FRAMES
    );
    assert_eq!(
        writer
            .try_write_response_events(host, &IrStreamEvent::BlockStop { index: 0 })
            .unwrap()
            .len(),
        1
    );
    assert!(writer
        .try_write_response_events(host, &start(MAX_GEMINI_TOOL_FRAMES))
        .is_ok());
}
