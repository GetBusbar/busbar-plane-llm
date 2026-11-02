// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The LLM wire codecs: six dialects (Anthropic Messages, OpenAI Chat Completions, Google Gemini,

/// The concrete chat IR and leaf-op IR (the substrate keeps the neutral `ir::facts`, `ir::handle`,
pub mod ir;

/// The chat `IrHandle` (`ChatReqHandle`/`ChatRespHandle`) and its `prepare_for_egress`/`_ingress`/
pub mod chat_handle;

/// This plane's request preparation context and dialect writer capabilities.
pub mod egress_prep;

/// The six leaf-op `IrHandle`s (embeddings/image/rerank/moderation/transcription/speech), writing
pub mod leaf_handles;

/// One leaf operation's `OperationHandler`, written once for every dialect. Every leaf cell answers
/// the same way: its error envelope is its protocol's, and its reads wrap the dialect's
/// `read_<op>_request`/`read_<op>_response` in the op's leaf handle. Only the dialect label and the
/// two reads differ per cell; anything else a cell says (a usage tap, an egress content type)
/// follows the `;` verbatim.
macro_rules! leaf_op {
    ($op:ident: $dialect:expr, $req:ident = $read_req:ident, $resp:ident = $read_resp:ident; $($extra:tt)*) => {
        impl busbar_contract::codec::OperationHandler for $op {
            fn extract_error(
                &self,
                status: u16,
                body: &[u8],
            ) -> busbar_contract::upstream::RawUpstreamError {
                $crate::codec::proto_codec::protocol_error($dialect, status, body)
            }
            fn read_request(
                &self,
                body: &[u8],
                content_type: &str,
            ) -> Result<Box<dyn busbar_contract::ir::handle::IrHandle>, busbar_contract::codec::IngressReject>
            {
                Ok(Box::new($crate::codec::leaf_handles::$req($read_req(body, content_type)?)))
            }
            fn read_response(
                &self,_host: &dyn busbar_contract::codec::CodecHost,
                wire: &[u8],
            ) -> Result<Box<dyn busbar_contract::ir::handle::IrHandle>, busbar_contract::codec::CodecError>
            {
                Ok(Box::new($crate::codec::leaf_handles::$resp($read_resp(wire)?)))
            }
            $($extra)*
        }
    };
}

/// A dialect's neutral codec facade as a static, so its `DECL` hands out a `&'static dyn` borrow
/// (pure memory, zero alloc per `dialect()` call).
macro_rules! dialect_codec {
    ($name:expr) => {{
        static CODEC: $crate::codec::proto_codec::DialectRef =
            $crate::codec::proto_codec::dialect_ref($name);
        Some(&CODEC)
    }};
}

/// The two `RequestHandler` answers every dialect gives alike: its protocol name, and the leaf
/// cell for an operation from its `CELLS` table.
macro_rules! dialect_identity {
    ($name:expr) => {
        fn protocol_name(&self) -> &'static str {
            $name
        }
        fn operation_handler(
            &self,
            op: busbar_contract::operation::OpVerb,
        ) -> Option<&dyn busbar_contract::codec::OperationHandler> {
            crate::codec::framework::cell_of(CELLS, op)
        }
    };
}

pub mod anthropic;
pub mod bedrock;
pub mod cohere;
pub mod gemini;
pub mod openai_chat;
pub mod openai_responses;

/// The flat field carry: the slot schema, the per-dialect field-table types and the one walker.
pub mod carry;

/// The wire words two or more dialects spell alike, one const each.
pub mod keys;

/// The dialect framework's plane-only items: codec-row lookups, media carriers, the duration
pub mod framework;

/// The dialects' shared wire helpers: the OpenAI-family error helpers, the SSE `[DONE]` terminator
pub mod dialect;

/// The plane's own coded diagnostics, and the slice the composition root installs.
pub mod diagnostics;

/// The one drop path of a translate attempt: drop + warn + the path for the seam's audit, and the
pub mod drops;

/// The AWS event-stream framing codec the signing dialect streams in.
pub mod eventstream;

/// The plane's own depth-guarded JSON seam.
pub mod json;

/// Byte-level member splices: how a relayed body's governed members are edited.
pub mod json_splice;

/// The cross-dialect translate pipeline (`TranslateCodec`).
pub mod translate;

/// The one seam every usage/billing count read goes through (see the module doc).
pub mod usage_count;

/// Dialect-neutral tail-usage isolation, shared by every reader's `recover_truncated_usage`. Public
pub mod usage_tail;

/// The `url_citation` annotations wire mapping the Chat and Responses codecs share.
pub mod url_citation_wire;

/// The per-token logprob wire object (`{content: [{token, logprob, bytes, top_logprobs}]}`) the
pub mod logprob_wire;

/// The rerank wire shape (`documents[]`, `results[]`) the rerank dialects read alike.
pub mod rerank_wire;

/// IR → wire encode helpers (image source, tool-result detection, strict-drop warn) shared by the
pub mod ir_encode;

/// The per-`(operation, egress-protocol)` leaf-op writer dispatch — the non-chat twin of chat's
pub mod leaf_codec;

/// The concrete wire-codec surface (`ProtocolReader`/`ProtocolWriter`/`StreamFraming`/`Protocol`/
pub mod proto_codec;

/// The concrete streaming byte-translator (`StreamTranslate`) the relay drives.
pub mod proto_stream;

/// The two body-shaping helpers a caller needs on either side of a translate, kept here because
pub mod wire_shim;

/// This plane's own declaration of `name`, read off [`DECLS`]. Every codec lookup by dialect name

#[must_use]
pub fn decl_of(name: &str) -> Option<&'static busbar_contract::protocol::ProtocolDecl> {
    DECLS.iter().copied().find(|d| d.name == name)
}

#[cfg(test)]
#[path = "tests/codec_host_tests.rs"]
mod codec_host_tests;
#[cfg(test)]
#[path = "tests/dialect_classify.rs"]
pub(crate) mod dialect_classify;
/// This crate's test host (the host and the classifier are dev-only): the codec host a shipped

#[cfg(test)]
#[path = "tests/test_host.rs"]
pub(crate) mod test_host;
#[cfg(test)]
#[path = "tests/usage_census.rs"]
pub(crate) mod usage_census;

/// Every dialect this plugin declares, in operator-visible order.

pub(crate) const DIALECTS: &[&proto_codec::DialectEntry] = &[
    &anthropic::ENTRY,
    &openai_chat::ENTRY,
    &gemini::ENTRY,
    &bedrock::ENTRY,
    &openai_responses::ENTRY,
    &cohere::ENTRY,
];

/// Every registered dialect's declaration, in [`DIALECTS`] order (the operator-visible order

pub static DECLS: &[&busbar_contract::protocol::ProtocolDecl] = &decls::<{ DIALECTS.len() }>();

/// [`DIALECTS`]' declarations as an array, evaluated at compile time.
const fn decls<const N: usize>() -> [&'static busbar_contract::protocol::ProtocolDecl; N] {
    let mut out = [DIALECTS[0].decl; N];
    let mut i = 0;
    while i < N {
        out[i] = DIALECTS[i].decl;
        i += 1;
    }
    out
}

#[cfg(test)]
#[path = "tests/write_error_frame_tests.rs"]
mod write_error_frame_tests;

#[cfg(test)]
#[path = "tests/decode_native_tool_id_tests.rs"]
mod decode_native_tool_id_tests;

#[cfg(test)]
#[path = "tests/leaf_write_dispatch_tests.rs"]
mod leaf_write_dispatch_tests;

/// The cache-tier read tests, anchored to real recorded upstream bodies under

#[cfg(test)]
#[path = "tests/cache_tier_capture_tests.rs"]
mod cache_tier_capture_tests;

/// TODO 585: the operator warning when an image's `detail` is dropped for a dialect with no slot.
#[cfg(test)]
#[path = "tests/image_detail_warn_tests.rs"]
mod image_detail_warn_tests;

/// The codec/IR test suites: the detection, translate-parity, streaming, round-trip and IR goldens

#[cfg(test)]
#[path = "tests/proto/mod.rs"]
mod relocated_proto_tests;

/// The bedrock buffered-response to ConverseStream eventstream synthesis suite, driving the

#[cfg(test)]
#[path = "tests/bedrock_eventstream_tests.rs"]
mod bedrock_eventstream_tests;

/// What the cross-protocol seam keeps and what it normalizes.
#[cfg(test)]
#[path = "tests/ir_seam_tests.rs"]
mod ir_seam_tests;

/// The typed slots through the shared stream machinery.
#[cfg(test)]
#[path = "tests/ir_slot_carry_tests.rs"]
mod ir_slot_carry_tests;

/// The members CARRY-TABLE parked: what crosses the seam is still the typed slot.
#[cfg(test)]
#[path = "tests/parked_rows_cross_dialect_tests.rs"]
mod parked_rows_cross_dialect_tests;

#[cfg(test)]
#[path = "tests/carry_probe_tests.rs"]
mod carry_probe_tests;

/// The usage-tap count pin: every same-protocol tap fault counts once on the host's counter, under

#[cfg(test)]
#[path = "tests/usage_tap_count_tests.rs"]
mod usage_tap_count_tests;
