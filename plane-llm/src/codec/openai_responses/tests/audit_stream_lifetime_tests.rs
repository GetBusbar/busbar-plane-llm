use super::*;
use crate::codec::ir::{IrBlockMeta, IrDelta, StreamDecodeState, MAX_STREAM_BLOCKS};

#[test]
fn closed_chat_and_gemini_origins_are_lifetime_bounded() {
    let chat = crate::codec::openai_chat::protocol();
    let gemini = crate::codec::gemini::protocol();
    for (protocol, media) in [(&chat, false), (&gemini, true)] {
        let mut state = StreamDecodeState::default();
        for index in 0..=MAX_STREAM_BLOCKS {
            let data = if media {
                serde_json::json!({"candidates":[{"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"YQ=="}}]}}]})
            } else if index % 2 == 0 {
                serde_json::json!({"choices":[{"delta":{"content":"x"}}]})
            } else {
                serde_json::json!({"choices":[{"delta":{"refusal":"x"}}]})
            };
            let events = protocol.reader().read_response_events(
                crate::codec::test_host::HOST,
                "",
                &data,
                &mut state,
            );
            assert_eq!(
                events.iter().any(|e| matches!(e, IrStreamEvent::Error(_))),
                index == MAX_STREAM_BLOCKS
            );
            assert!(state.block_origins.len() <= MAX_STREAM_BLOCKS);
        }
        assert_eq!(state.next_ir_index, MAX_STREAM_BLOCKS);
    }
}

#[test]
fn finalized_items_consume_aggregate_budget_before_cloning() {
    let writer = ResponsesWriter;
    writer.store_translate_cap(8);
    writer
        .record_output_item(0, &serde_json::json!("abc"))
        .unwrap();
    assert!(writer
        .record_output_item(1, &serde_json::json!("def"))
        .is_err());
    assert_eq!(writer.stream().output_items.len(), 1);
    assert_eq!(writer.stream().finalized_bytes, 5);
}

#[test]
fn closed_items_do_not_reset_payload_or_block_budget() {
    let writer = ResponsesWriter;
    writer.store_translate_cap(4);
    for index in 0..2 {
        writer
            .reserve_stream_event(&IrStreamEvent::BlockStart {
                index,
                block: IrBlockMeta::Text,
                refusal: false,
            })
            .unwrap();
        writer
            .reserve_stream_event(&IrStreamEvent::BlockDelta {
                index,
                delta: IrDelta::TextDelta("ab".into()),
            })
            .unwrap();
        writer
            .reserve_stream_event(&IrStreamEvent::BlockStop { index })
            .unwrap();
    }
    assert!(writer
        .reserve_stream_event(&IrStreamEvent::BlockDelta {
            index: 2,
            delta: IrDelta::TextDelta("c".into())
        })
        .is_err());
    assert_eq!(writer.stream().lifetime_payload_bytes, 4);
    let writer = ResponsesWriter;
    for index in 0..MAX_STREAM_BLOCKS {
        writer
            .reserve_stream_event(&IrStreamEvent::BlockStart {
                index,
                block: IrBlockMeta::Text,
                refusal: false,
            })
            .unwrap();
        writer
            .reserve_stream_event(&IrStreamEvent::BlockStop { index })
            .unwrap();
    }
    assert!(writer
        .reserve_stream_event(&IrStreamEvent::BlockStart {
            index: MAX_STREAM_BLOCKS,
            block: IrBlockMeta::Text,
            refusal: false
        })
        .is_err());
}

#[test]
fn normal_closed_text_is_preserved_and_later_overflow_is_explicit() {
    let writer = ResponsesWriter;
    writer.store_translate_cap(2048);
    let host = crate::codec::test_host::HOST;
    for index in 0..2 {
        writer
            .try_write_response_events(
                host,
                &IrStreamEvent::BlockStart {
                    index,
                    block: IrBlockMeta::Text,
                    refusal: index == 1,
                },
            )
            .unwrap();
        writer
            .try_write_response_events(
                host,
                &IrStreamEvent::BlockDelta {
                    index,
                    delta: IrDelta::TextDelta("hello".into()),
                },
            )
            .unwrap();
        let done = writer
            .try_write_response_events(host, &IrStreamEvent::BlockStop { index })
            .unwrap();
        assert!(done.iter().any(|(name, _)| name == EVT_OUTPUT_ITEM_DONE));
    }
    assert_eq!(writer.stream().output_items.len(), 2);
    let err = writer
        .try_write_response_events(
            host,
            &IrStreamEvent::BlockDelta {
                index: 2,
                delta: IrDelta::TextDelta("x".repeat(2048)),
            },
        )
        .unwrap_err();
    assert_eq!(
        err.provider_signal.as_deref(),
        Some(ACCUMULATION_FAILURE_MSG)
    );
    assert_eq!(writer.stream().output_items.len(), 2);
}

#[test]
fn empty_citations_still_consume_retained_storage_budget() {
    let writer = ResponsesWriter;
    writer.store_translate_cap(std::mem::size_of::<crate::codec::ir::IrCitation>());
    let event = IrStreamEvent::BlockDelta {
        index: 0,
        delta: IrDelta::CitationsDelta(vec![Default::default()]),
    };
    writer.reserve_stream_event(&event).unwrap();
    assert!(writer.reserve_stream_event(&event).is_err());
}

#[test]
fn chat_metadata_only_blocks_report_exhausted_indices() {
    let protocol = crate::codec::openai_chat::protocol();
    let frames = [
        serde_json::json!({"choices":[{"delta":{},"logprobs":{"content":[{"token":"x","logprob":-0.5,"bytes":[120],"top_logprobs":[]}]}}]}),
        serde_json::json!({"choices":[{"delta":{"annotations":[{"type":"url_citation","url_citation":{"start_index":0,"end_index":1,"title":"source","url":"https://example.test"}}]}}]}),
    ];
    for frame in frames {
        for exhausted in [false, true] {
            let mut state = StreamDecodeState::default();
            if exhausted {
                state.next_ir_index = MAX_STREAM_BLOCKS;
            }
            let events = protocol.reader().read_response_events(
                crate::codec::test_host::HOST,
                "",
                &frame,
                &mut state,
            );
            assert_eq!(
                events
                    .iter()
                    .any(|event| matches!(event, IrStreamEvent::Error(_))),
                exhausted
            );
            assert_eq!(
                events
                    .iter()
                    .any(|event| matches!(event, IrStreamEvent::BlockStart { .. })),
                !exhausted
            );
        }
    }
}
