use super::*;

#[test]
fn document_page_and_chunk_citations_roundtrip() {
    for member in ["documentPage", "documentChunk"] {
        let wire =
            serde_json::json!({"location": {(member): {"documentIndex": 2, "start": 3, "end": 5}}});
        let citation = citations::read_bedrock_citation(&wire);
        assert_eq!(citations::write_bedrock_citation(&citation), Some(wire));
    }
}

#[test]
fn titan_embedding_rejects_corrupt_components_without_shifting_dimensions() {
    for body in [
        br#"{"embedding":[0.1,"bad",0.3]}"#.as_slice(),
        br#"{"embedding":[1e100]}"#.as_slice(),
    ] {
        assert!(matches!(
            handler::read_embeddings_response(body),
            Err(crate::codec::cells::CodecError::Malformed(_))
        ));
    }
    let result = handler::read_embeddings_response(br#"{"embedding":[0.1,0.2,0.3]}"#).unwrap();
    assert_eq!(
        result.embeddings[0]
            .vectors
            .get(&crate::codec::ir::embeddings::EncFmt::Float),
        Some(&crate::codec::ir::embeddings::VectorData::Float(vec![
            0.1, 0.2, 0.3
        ]))
    );
}

#[test]
fn document_name_limits_preserve_valid_names_and_replace_oversize_names() {
    let source = crate::codec::ir::IrImageSource::Base64 {
        media_type: "application/pdf".into(),
        data: "JVBERi0=".into(),
    };
    for length in [200, 201] {
        let name = "a".repeat(length);
        let block = bedrock_media_content_block(
            crate::codec::ir::IrMediaKind::Document,
            &source,
            Some(&name),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            block["document"]["name"],
            if length == 200 {
                name.as_str()
            } else {
                "attachment"
            }
        );
    }
}
#[test]
fn image_guidance_scale_refuses_float_overflow() {
    assert!(handler::read_image_request(
        br#"{"imageGenerationConfig":{"cfgScale":1e100}}"#,
        "application/json"
    )
    .is_err());
    assert_eq!(
        handler::read_image_request(
            br#"{"imageGenerationConfig":{"cfgScale":7.5}}"#,
            "application/json"
        )
        .unwrap()
        .guidance_scale,
        Some(7.5)
    );
}
