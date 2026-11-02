//! Regression net for the image-source bug class (closed by the typed `IrImageSource`). The key

use super::*;
use crate::codec::ir::{IrBlock, IrImageSource, IrMessage, IrRole};

fn req_with_image(source: IrImageSource) -> crate::codec::ir::IrRequest {
    crate::codec::ir::IrRequest {
        messages: vec![IrMessage {
            role: IrRole::User,
            content: vec![IrBlock::Image {
                source,
                cache_control: None,
                detail: None,
            }],
        }],
        ..Default::default()
    }
}

/// A FOREIGN vendor reference (one the writer doesn't own) must never reach the wire as a corrupt

#[test]
fn foreign_vendor_image_ref_never_corrupts_any_writer() {
    let foreign = IrImageSource::Vendor {
        vendor: "responses",
        value: serde_json::json!({ "file_id": "file-x" }),
    };
    let req = req_with_image(foreign);
    let cohere = CohereWriter;
    let gemini = GeminiWriter;
    let bedrock = BedrockWriter;
    let o =
        serde_json::to_string(&openai_writer().write_request(crate::codec::test_host::HOST, &req))
            .unwrap();
    let a = serde_json::to_string(
        &anthropic_writer().write_request(crate::codec::test_host::HOST, &req),
    )
    .unwrap();
    let g =
        serde_json::to_string(&gemini.write_request(crate::codec::test_host::HOST, &req)).unwrap();
    let b =
        serde_json::to_string(&bedrock.write_request(crate::codec::test_host::HOST, &req)).unwrap();
    let c =
        serde_json::to_string(&cohere.write_request(crate::codec::test_host::HOST, &req)).unwrap();
    for (name, wire) in [
        ("openai", o),
        ("anthropic", a),
        ("gemini", g),
        ("bedrock", b),
        ("cohere", c),
    ] {
        assert!(
            !wire.contains("file-x"),
            "{name} writer must DROP a foreign vendor image ref, not leak it: {wire}"
        );
    }
}

/// A neutral base64 image projects to a real inline-image shape on every writer that supports

#[test]
fn base64_image_projects_intact_on_every_writer() {
    let req = req_with_image(IrImageSource::Base64 {
        media_type: "image/png".to_string(),
        data: "QUJD".to_string(),
    });

    let o = openai_writer().write_request(crate::codec::test_host::HOST, &req);
    let url = o["messages"][0]["content"][0]["image_url"]["url"].as_str();
    assert_eq!(
        url,
        Some("data:image/png;base64,QUJD"),
        "base64 image must reach the OpenAI wire as a complete data URI, got {o}"
    );

    let openai = OpenAiWriter;
    let gemini = GeminiWriter;
    let bedrock = BedrockWriter;
    let cohere = CohereWriter;
    for (name, media_token, wire) in [
        (
            "openai",
            "image/png",
            openai.write_request(crate::codec::test_host::HOST, &req),
        ),
        (
            "anthropic",
            "image/png",
            anthropic_writer().write_request(crate::codec::test_host::HOST, &req),
        ),
        (
            "gemini",
            "image/png",
            gemini.write_request(crate::codec::test_host::HOST, &req),
        ),
        (
            "bedrock",
            "\"png\"",
            bedrock.write_request(crate::codec::test_host::HOST, &req),
        ),
        (
            "cohere",
            "image/png",
            cohere.write_request(crate::codec::test_host::HOST, &req),
        ),
    ] {
        let s = serde_json::to_string(&wire).unwrap();
        assert!(
            s.contains("QUJD"),
            "{name} writer must carry the base64 payload to the wire intact: {s}"
        );

        assert!(
            s.contains(media_token),
            "{name} writer must carry the image media type beside the payload: {s}"
        );
    }
}
