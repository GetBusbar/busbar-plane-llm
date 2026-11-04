//! Embedding identity survives indexed foreign responses and positional Cohere responses.

mod common;

use busbar_contract::operation::OpVerb;
use busbar_plane_llm::codec::cohere::handler::write_embeddings_response;
use busbar_plane_llm::codec::ir::embeddings::{EmbeddingItem, EmbeddingsResp, EncFmt, VectorData};
use busbar_plane_llm::codec::openai_chat::handler::read_embeddings_response;
use busbar_plane_llm::exchange::arrive::Arrived;
use busbar_plane_llm::exchange::attempt::StreamIntent;
use busbar_plane_llm::exchange::reply::{At, Piece, Reply, ReplyCtx};
use busbar_plane_llm::exchange::shaping::Lane;
use common::SevensHost;
use serde_json::{json, Value};

fn answer<'a>(ingress: &'static str, egress: &'static str, body: &'a [u8]) -> Piece<'a> {
    let request = json!({"model":"model","texts":["A","B"],"input_type":"search_document"});
    let arrived = Arrived {
        dialect: ingress,
        operation: OpVerb::EMBEDDINGS,
        model: "model".into(),
        content_type: "application/json".into(),
        body: serde_json::to_vec(&request).unwrap(),
        parsed: Some(request),
        path: "/v2/embed".into(),
        query: None,
        path_model: None,
    };
    let lane = Lane {
        model: "model".into(),
        provider: "provider".into(),
        dialect: egress,
        path: None,
        path_base: None,
        options: Vec::new(),
        upstream_model: None,
        default_max_tokens: None,
        context_max: None,
        reasoning: false,
        prompt_caching: false,
        caps: Default::default(),
        error_map: Default::default(),
        statics: &[],
        invoke: false,
    };
    let ctx = ReplyCtx {
        arrived: &arrived,
        lane: &lane,
        intent: StreamIntent {
            wants_stream: false,
            client_include_usage: false,
        },
        passthrough: ingress == egress,
        translate_cap: 4096,
    };
    Reply::new(
        &SevensHost,
        &ctx,
        200,
        &[(b"content-type", b"application/json")],
    )
    .feed(
        &SevensHost,
        &ctx,
        body,
        true,
        At {
            now_s: 1,
            elapsed_ms: None,
        },
    )
}

fn openai(data: Value) -> Vec<u8> {
    serde_json::to_vec(&json!({"object":"list","model":"model","data":data,
        "usage":{"prompt_tokens":17,"total_tokens":17}}))
    .unwrap()
}

#[test]
fn actual_openai_reader_to_cohere_writer_orders_by_explicit_index() {
    for (a, b, encoding) in [
        (json!([1.0, 2.0]), json!([3.0, 4.0]), "float"),
        (json!("AACAPw=="), json!("AAAAQA=="), "base64"),
    ] {
        let raw = openai(json!([{"index":1,"embedding":b},{"index":0,"embedding":a}]));
        let ir =
            read_embeddings_response(&raw).expect("the actual OpenAI reader accepts reordering");
        assert_eq!(
            ir.embeddings.iter().map(|i| i.index).collect::<Vec<_>>(),
            vec![1, 0]
        );
        let wire = write_embeddings_response(&ir);
        let body: Value = serde_json::from_slice(&wire.bytes).unwrap();
        assert_eq!(body["embeddings"][encoding], json!([a, b]));
        assert_eq!(body["meta"]["billed_units"]["input_tokens"], 17);
    }
}

#[test]
fn production_reply_preserves_reordered_vectors_and_reported_billing() {
    for (a, b, encoding) in [
        (json!([1.0]), json!([2.0]), "float"),
        (json!("AACAPw=="), json!("AAAAQA=="), "base64"),
    ] {
        let raw = openai(json!([{"index":1,"embedding":b},{"index":0,"embedding":a}]));
        let piece = answer("cohere", "openai", &raw);
        assert_eq!(piece.head.as_ref().unwrap().status, 200);
        assert!(piece.done);
        let body: Value = serde_json::from_slice(&piece.bytes).unwrap();
        assert_eq!(body["embeddings"][encoding], json!([a, b]));
        assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (17, 0));
    }
}

const ENCODINGS: [(EncFmt, &str); 6] = [
    (EncFmt::Float, "float"),
    (EncFmt::Base64, "base64"),
    (EncFmt::Int8, "int8"),
    (EncFmt::Uint8, "uint8"),
    (EncFmt::Binary, "binary"),
    (EncFmt::Ubinary, "ubinary"),
];

fn vector(enc: EncFmt, index: usize) -> VectorData {
    match enc {
        EncFmt::Float => VectorData::Float(vec![index as f32 + 1.0]),
        EncFmt::Base64 => {
            VectorData::Base64(if index == 0 { "AACAPw==" } else { "AAAAQA==" }.into())
        }
        _ => VectorData::Int(vec![index as i32 + 1]),
    }
}

fn item(index: usize, encs: &[EncFmt]) -> EmbeddingItem {
    EmbeddingItem {
        index,
        vectors: encs.iter().map(|&enc| (enc, vector(enc, index))).collect(),
        ..Default::default()
    }
}

#[test]
fn all_six_encodings_share_the_same_dense_input_order() {
    let encs: Vec<_> = ENCODINGS.iter().map(|&(enc, _)| enc).collect();
    let response = EmbeddingsResp {
        id: Some("id".into()),
        input_echo: Some(vec!["A".into(), "B".into()]),
        embeddings: vec![item(1, &encs), item(0, &encs)],
        ..Default::default()
    };
    let wire = write_embeddings_response(&response);
    let body: Value = serde_json::from_slice(&wire.bytes).unwrap();
    for (_, name) in ENCODINGS {
        let expected = if name == "float" {
            json!([[1.0], [2.0]])
        } else if name == "base64" {
            json!(["AACAPw==", "AAAAQA=="])
        } else {
            json!([[1], [2]])
        };
        assert_eq!(body["embeddings"][name], expected, "{name}");
    }
    assert_eq!(body["id"], "id");
    assert_eq!(body["texts"], json!(["A", "B"]));
}

#[test]
fn disjoint_encoding_items_at_the_same_index_remain_losslessly_representable() {
    let response = EmbeddingsResp {
        embeddings: vec![
            item(1, &[EncFmt::Int8]),
            item(0, &[EncFmt::Float]),
            item(0, &[EncFmt::Int8]),
            item(1, &[EncFmt::Float]),
        ],
        ..Default::default()
    };
    let wire = write_embeddings_response(&response);
    let body: Value = serde_json::from_slice(&wire.bytes).unwrap();
    assert_eq!(body["embeddings"]["float"], json!([[1.0], [2.0]]));
    assert_eq!(body["embeddings"]["int8"], json!([[1], [2]]));
}

#[test]
fn duplicate_sparse_missing_zero_and_huge_indices_refuse_every_encoding() {
    for (enc, _) in ENCODINGS {
        for indices in [
            vec![0, 0],
            vec![0, 2],
            vec![1],
            vec![usize::MAX],
            vec![0, usize::MAX],
        ] {
            let response = EmbeddingsResp {
                embeddings: indices.iter().map(|&index| item(index, &[enc])).collect(),
                ..Default::default()
            };

            let mut response = response;
            for item in &mut response.embeddings {
                item.vectors.insert(enc, vector(enc, 0));
            }
            assert!(
                write_embeddings_response(&response).bytes.is_empty(),
                "{enc:?} {indices:?} must refuse rather than renumber/pad"
            );
        }
    }
}

#[test]
fn missing_encoding_coverage_and_empty_vector_items_refuse_without_placeholders() {
    for response in [
        EmbeddingsResp {
            embeddings: vec![
                item(0, &[EncFmt::Float, EncFmt::Int8]),
                item(1, &[EncFmt::Float]),
            ],
            ..Default::default()
        },
        EmbeddingsResp {
            embeddings: vec![item(0, &[EncFmt::Float]), item(1, &[])],
            ..Default::default()
        },
        EmbeddingsResp {
            embeddings: vec![item(0, &[])],
            ..Default::default()
        },
    ] {
        assert!(write_embeddings_response(&response).bytes.is_empty());
    }
    let empty = write_embeddings_response(&EmbeddingsResp::default());
    let body: Value = serde_json::from_slice(&empty.bytes).unwrap();
    assert_eq!(body["embeddings"], json!({"float":[]}));
}

#[test]
fn production_refusal_is_native_and_does_not_bill_a_translation_abort() {
    for indices in [vec![0_u64, 0], vec![0, 2], vec![1], vec![u64::MAX]] {
        let raw = openai(Value::Array(
            indices
                .iter()
                .map(|&index| json!({"index":index,"embedding":[1.0]}))
                .collect(),
        ));
        let piece = answer("cohere", "openai", &raw);
        assert_eq!(piece.head.as_ref().unwrap().status, 500, "{indices:?}");
        let body: Value = serde_json::from_slice(&piece.bytes).unwrap();
        assert!(body["message"].is_string(), "native Cohere error: {body}");
        assert!(body.get("embeddings").is_none());
        assert_eq!((piece.units.tokens_in, piece.units.tokens_out), (0, 0));
    }
}

#[test]
fn native_openai_and_cohere_response_bytes_and_billing_are_unchanged() {
    for (dialect,raw) in [
        ("openai",br#"{ "data":[{"index":1,"embedding":[2.0]},{"index":0,"embedding":[1.0]}],"usage":{"prompt_tokens":17,"total_tokens":17} }"#.as_slice()),
        ("cohere",br#"{ "id":"native", "embeddings":{"float":[[1.0],[2.0]],"int8":[[1],[2]]},"meta":{"billed_units":{"input_tokens":17}} }"#.as_slice()),
    ] {
        let piece = answer(dialect,dialect,raw);
        assert_eq!(piece.head.as_ref().unwrap().status,200);
        assert_eq!(piece.bytes.as_ref(),raw);
        assert_eq!((piece.units.tokens_in,piece.units.tokens_out),(17,0));
    }
}
