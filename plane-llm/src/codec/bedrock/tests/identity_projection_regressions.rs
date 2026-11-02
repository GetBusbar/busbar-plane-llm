use super::*;
use busbar_test_kit::warn_capture::WarnCapture;
use serde_json::json;
#[test]
fn search_source_without_coordinates_never_invents_web_or_index() {
    for (index, start, end) in [
        (None, None, None),
        (Some(-1), Some(3), Some(9)),
        (Some(7), Some(9), Some(3)),
    ] {
        let citation = crate::codec::ir::IrCitation {
            kind: Some("search_result_location".into()),
            url: Some("opaque:source#7".into()),
            document_index: index,
            start_index: start,
            end_index: end,
            ..Default::default()
        };
        let cap = WarnCapture::default();
        let out =
            tracing::subscriber::with_default(cap.clone(), || write_bedrock_citation(&citation))
                .unwrap();
        assert_eq!(out, json!({"source":"opaque:source#7"}));
        assert!(!cap.contains("union"));
    }
}
#[test]
fn web_and_document_location_controls_preserve_existing_semantics() {
    for citation in [
        json!({"location":{"web":{"url":"https://source.invalid/","domain":"source.invalid"}},"title":"Web","sourceContent":[{"text":"quoted"}]}),
        json!({"location":{"documentChar":{"documentIndex":4,"start":3,"end":9}},"title":"Doc","sourceContent":[{"text":"quoted"}]}),
    ] {
        let ir = read_bedrock_citation(&citation);
        assert_eq!(write_bedrock_citation(&ir), Some(citation));
    }
}
#[test]
fn direct_image_writer_warns_truthful_path_without_url_value() {
    let mut response = crate::codec::ir::image::ImageResp::default();
    response.images.push(crate::codec::framework::ImageOutput {
        url: Some("https://private.invalid/SECRET".into()),
        ..Default::default()
    });
    let cap = WarnCapture::default();
    tracing::subscriber::with_default(cap.clone(), || handler::write_image_response(&response));
    assert!(cap.contains("ir.images[].url"));
    assert!(cap.contains("native image source metadata is unavailable"));
    assert!(!cap.contains("SECRET"));
}

#[test]
fn streamed_citations_use_the_same_source_projection() {
    use crate::codec::ir::{IrDelta, IrStreamEvent, StreamDecodeState};
    let citation = json!({"source":"opaque:source#7","title":"Policy","sourceContent":[{"text":"quoted"}],"location":{"searchResultLocation":{"searchResultIndex":7,"start":3,"end":9}}});
    let mut state = StreamDecodeState::default();
    let host = crate::codec::test_host::HOST;
    BedrockReader.read_response_events(
        host,
        "messageStart",
        &json!({"role":"assistant"}),
        &mut state,
    );
    let events = BedrockReader.read_response_events(
        host,
        "contentBlockDelta",
        &json!({"contentBlockIndex":2,"delta":{"citation":citation}}),
        &mut state,
    );
    let event = events
        .iter()
        .find(|e| {
            matches!(
                e,
                IrStreamEvent::BlockDelta {
                    delta: IrDelta::CitationsDelta(_),
                    ..
                }
            )
        })
        .unwrap();
    let IrStreamEvent::BlockDelta {
        delta: IrDelta::CitationsDelta(citations),
        ..
    } = event
    else {
        unreachable!()
    };
    assert_eq!(citations[0].url.as_deref(), Some("opaque:source#7"));
    let writer = BedrockWriter;
    // Writer tracks open text blocks; start it before the citation delta.
    for event in &events {
        if let Some((_, out)) = writer.write_response_event(host, event) {
            let out: serde_json::Value = serde_json::from_str(&out).unwrap();
            if out.get("delta").is_some() {
                assert_eq!(out["delta"]["citation"], citation);
            }
        }
    }
}
