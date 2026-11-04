use super::*;
use crate::codec::ir::{IrBlock, IrImageSource, IrMessage, IrRequest, IrRole};

#[test]
fn explicit_file_mime_survives_regular_and_tool_attachment_writers() {
    for mime in [
        "audio/wav",
        "text/plain",
        "video/webm",
        "image/avif",
        "application/octet-stream",
    ] {
        let part = serde_json::json!({"fileData": {"fileUri": "https://example.test/opaque", "mimeType": mime}});
        let block = read_gemini_media_part(&part).unwrap();
        let source = match &block {
            IrBlock::Image { source, .. } | IrBlock::Media { source, .. } => source,
            _ => panic!("expected media"),
        };
        assert!(
            matches!(source, IrImageSource::UrlWithMime { media_type, .. } if media_type == mime)
        );
        assert_eq!(write_gemini_media_part(&block).unwrap(), part);
        assert_eq!(
            crate::codec::ir_encode::image_url_from_ir(source).as_deref(),
            Some("https://example.test/opaque")
        );
        let request = IrRequest {
            messages: vec![IrMessage {
                role: IrRole::User,
                content: vec![block],
            }],
            ..Default::default()
        };
        let writer = GeminiWriter;
        let output = writer.write_request(crate::codec::test_host::HOST, &request);
        assert_eq!(output["contents"][0]["parts"][0], part);
    }
}

#[test]
fn legacy_url_remains_supported_without_explicit_mime() {
    let block = read_gemini_media_part(
        &serde_json::json!({"fileData": {"fileUri": "https://example.test/image.png"}}),
    )
    .unwrap();
    assert!(matches!(
        &block,
        IrBlock::Image {
            source: IrImageSource::Url(_),
            ..
        }
    ));
    assert_eq!(
        write_gemini_media_part(&block).unwrap()["fileData"]["mimeType"],
        "image/png"
    );
}

#[test]
fn mime_bearing_image_urls_transfer_to_url_capable_dialects() {
    let block = read_gemini_media_part(&serde_json::json!({"fileData": {
        "fileUri": "https://example.test/opaque", "mimeType": "image/png"
    }}))
    .unwrap();
    let request = IrRequest {
        messages: vec![IrMessage {
            role: IrRole::User,
            content: vec![block],
        }],
        ..Default::default()
    };
    let openai_writer = crate::codec::openai_chat::OpenAiWriter;
    let openai = openai_writer.write_request(crate::codec::test_host::HOST, &request);
    assert_eq!(
        openai["messages"][0]["content"][0]["image_url"]["url"],
        "https://example.test/opaque"
    );
    let anthropic_writer = crate::codec::anthropic::AnthropicWriter;
    let anthropic = anthropic_writer.write_request(crate::codec::test_host::HOST, &request);
    assert_eq!(
        anthropic["messages"][0]["content"][0]["source"]["url"],
        "https://example.test/opaque"
    );
    // Writing a dialect without a MIME field must not mutate the shared IR.
    let gemini_writer = GeminiWriter;
    assert_eq!(
        gemini_writer.write_request(crate::codec::test_host::HOST, &request)["contents"][0]
            ["parts"][0]["fileData"]["mimeType"],
        "image/png"
    );
}
