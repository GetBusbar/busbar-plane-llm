use super::*;

#[test]
fn test_gemini_upstream_path_for_embeds_model() {
    let gemini_writer = GeminiWriter;
    assert_eq!(
        gemini_writer.upstream_path_for("gemini-1.5-pro"),
        "/v1beta/models/gemini-1.5-pro:generateContent"
    );

    assert_eq!(
        anthropic_writer().upstream_path_for("anything"),
        "/v1/messages"
    );
    assert_eq!(
        openai_writer().upstream_path_for("anything"),
        "/v1/chat/completions"
    );
}

#[test]
fn test_gemini_registered_in_builtins() {
    let g = crate::codec::proto_codec::protocol_for("gemini").expect("gemini should be registered");
    assert_eq!(g.name_static(), "gemini");
    assert_eq!(
        g.writer().upstream_path_for("m"),
        "/v1beta/models/m:generateContent"
    );

    let decl = decl_for("gemini").expect("gemini declares itself");
    let raw = busbar_contract::protocol::CredentialHeader::Raw {
        header: "x-goog-api-key",
        trim_start: false,
    };
    assert!(
        matches!(
            decl.egress_scheme,
            Some(busbar_contract::protocol::EgressScheme::Static { families: [], own, passthrough })
                if own == raw && passthrough == raw
        ),
        "gemini's declared egress auth is the raw key in x-goog-api-key — no Bearer"
    );
}
