// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The LLM wire codecs: six dialects (Anthropic Messages, OpenAI Chat Completions, Google Gemini,
//! AWS Bedrock Converse, OpenAI Responses, Cohere v2) translated between any pair through one
//! neutral IR. This module holds the readers, writers, IR, stream translator and the
//! answer-normalization pass every ingress runs between reading and writing an answer.
//!
//! It lives in `busbar-plane-llm`, a pure plane kind: the crate carries no `hyper`, `reqwest` or
//! socket-capable `tokio`. Its door (`plane_door`) and request exchange (`exchange`) sit beside this
//! module and drive these codecs; the network engine stays in the host.
//!
//! Each dialect module owns its `ProtocolDecl`, its wire codec (`reader.rs`/`writer.rs`), its
//! `RequestHandler` and operation cells (`handler.rs`), its wire constants and its tests. A seventh
//! dialect is a seventh module plus one registration line in [`DIALECTS`].
//!
//! Shared vocabulary lives in two places. What both sides of the plane seam must spell alike (the
//! declaration a dialect fills, the codec-cell traits, IR shapes, the error-`type` bank, billing and
//! wire carriers) is the contract's (`busbar_contract::...`). Machinery only this plane's dialects
//! speak (event-stream framing, the JSON seam, SSE and OpenAI-family wire helpers, the translate
//! pipeline, coded diagnostics) is in the modules below. Nothing here names the host crate: entropy,
//! the wall clock, the translation cap and the usage-tap count come through the host services the
//! contract carries.
//!
//! A dialect never names a sibling dialect (design F3 self-contained). What two share is a wire
//! mechanism, kept in a module named for it (`rerank_wire`, `logprob_wire`, the bearer-envelope
//! helpers in `dialect`), never in one dialect for another to import.

/// The concrete chat IR and leaf-op IR (the substrate keeps the neutral `ir::facts`, `ir::handle`,
/// `ir::invoke`, `ir::subscribe` traits).
pub mod ir;

/// The chat `IrHandle` (`ChatReqHandle`/`ChatRespHandle`) and its `prepare_for_egress`/`_ingress`/
/// `usage` bodies; the handle writes itself onto the egress dialect by protocol string.
///
/// [`chat_handle::chat_prepare_for_ingress`], [`chat_handle::chat_prepare_for_egress`] and
/// [`chat_handle::chat_usage`] are the answer-normalization pass the forward path runs between
/// reading an answer and writing it. They are public because the plane is a different crate.
pub mod chat_handle;

/// The six leaf-op `IrHandle`s (embeddings/image/rerank/moderation/transcription/speech), writing
/// themselves onto the peer dialect via the `leaf_codec` `(op,proto)` dispatchers.
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
/// rendering, and the shared error kinds, signals and head reader.
pub mod framework;

/// The dialects' shared wire helpers: the OpenAI-family error helpers, the SSE `[DONE]` terminator
/// and frame probe/parse/write, the base62 id alphabet, the `usage` stripper.
pub mod dialect;

/// The plane's own coded diagnostics, and the slice the composition root installs.
pub mod diagnostics;

/// The one drop path of a translate attempt: drop + warn + the path for the seam's audit, and the
/// walkers that name what a dialect does not map.
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
/// because the plane's metering locators name the same isolation.
pub mod usage_tail;

/// The `url_citation` annotations wire mapping the Chat and Responses codecs share.
pub mod url_citation_wire;

/// The per-token logprob wire object (`{content: [{token, logprob, bytes, top_logprobs}]}`) the
/// Chat and Responses dialects read and write alike.
pub mod logprob_wire;

/// The rerank wire shape (`documents[]`, `results[]`) the rerank dialects read alike.
pub mod rerank_wire;

/// IR → wire encode helpers (image source, tool-result detection, strict-drop warn) shared by the
/// dialect writers.
pub mod ir_encode;

/// The per-`(operation, egress-protocol)` leaf-op writer dispatch — the non-chat twin of chat's
/// `protocol_for(proto).writer()`.
pub mod leaf_codec;

/// The concrete wire-codec surface (`ProtocolReader`/`ProtocolWriter`/`StreamFraming`/`Protocol`/
/// `protocol_for`/`DialectRef`/`ToolIdRemap`).
pub mod proto_codec;

/// The concrete streaming byte-translator (`StreamTranslate`) the relay drives.
pub mod proto_stream;

/// The two body-shaping helpers a caller needs on either side of a translate, kept here because
/// they are pure over the protocol registry and the neutral billing carrier — and because the
/// suites that pin them are codec suites.
pub mod wire_shim;

/// This plane's own declaration of `name`, read off [`DECLS`]. Every codec lookup by dialect name
/// (path-model shape, array-stream shim key, native tool-id prefix, provider-metadata reporter) is a
/// fact about one of these six dialects, so it reads this table, never the host's registry. `None`
/// for a name this plane does not declare.
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
/// binary's host provides, supplied by the test so the codec tests never reach the host crate.
#[cfg(test)]
#[path = "tests/test_host.rs"]
pub(crate) mod test_host;
#[cfg(test)]
#[path = "tests/usage_census.rs"]
pub(crate) mod usage_census;

/// Every dialect this plugin declares, in operator-visible order.
///
/// The order is load-bearing, not alphabetical. The composition root hands this slice to the
/// registry's `install_protocols`, which folds it ahead of core's built-in declarations; the result
/// is what `known_protocols()` reports (the "must be one of:" tail on a bad `protocol:`) and what
/// `telemetry` banks per-protocol metric families against, finding a family by list position. So the
/// order is `anthropic, openai, gemini, bedrock, responses, cohere` (pinned by shadow-oracle
/// `boot.refusal|BOOT-020|validate`). A dialect appended keeps every family's index; inserting one
/// renumbers them all.
///
/// One registration line per dialect: each declares its own [`proto_codec::DialectEntry`] in its
/// module and this list is the only place the plane names it. [`DECLS`] is read off it.
pub(crate) const DIALECTS: &[&proto_codec::DialectEntry] = &[
    &anthropic::ENTRY,
    &openai_chat::ENTRY,
    &gemini::ENTRY,
    &bedrock::ENTRY,
    &openai_responses::ENTRY,
    &cohere::ENTRY,
];

/// Every registered dialect's declaration, in [`DIALECTS`] order (the operator-visible order
/// documented there).
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
/// `testing/shadow-oracle/golden/1.5.5/cells/` and `testing/llm-conformance/fixtures/`. Crate-level
/// because the four providers share one loader for that evidence and the four are the same defect.
#[cfg(test)]
#[path = "tests/cache_tier_capture_tests.rs"]
mod cache_tier_capture_tests;

/// TODO 585: the operator warning when an image's `detail` is dropped for a dialect with no slot.
#[cfg(test)]
#[path = "tests/image_detail_warn_tests.rs"]
mod image_detail_warn_tests;

/// The codec/IR test suites: the detection, translate-parity, streaming, round-trip and IR goldens
/// that name the dialects and the concrete wire codecs (see the module header for the `super::*`
/// prelude reconstruction).
#[cfg(test)]
#[path = "tests/proto/mod.rs"]
mod relocated_proto_tests;

/// The bedrock buffered-response to ConverseStream eventstream synthesis suite, driving the
/// witnessed `bedrock::bedrock_response_to_eventstream`.
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
/// its reason, whichever cell or direct call raised it.
#[cfg(test)]
#[path = "tests/usage_tap_count_tests.rs"]
mod usage_tap_count_tests;
