//! The LLM plane: what bytes mean, for six dialects of one protocol, served through its door
//! ([`plane_door::door`]) over the dialect codecs ([`codec`]).

#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]
#![deny(missing_docs)]

#[allow(missing_docs)]
mod codec;
mod dialect;
mod exchange;
mod meta;
mod migrate;
mod options;
mod plane_door;
mod refusal;

/// THE PLANE'S DECLARED METADATA (`declares.json`): the `declares` section its signed tarball carries
/// and the one the composition root reads for its linked door (`linked-declares`), one file for both
/// — the transport scheme it needs and every code its catalog holds
/// ([`codec::diagnostics::DIAGNOSTICS`]; a test holds the two equal), so the host's catalogue learns
/// this plane's codes from its door's declaration whichever way the plane came in.
pub const DECLARES: &str = include_str!("../declares.json");

#[cfg(test)]
#[path = "../tests/support/codec_diagnostics.rs"]
mod codec_diagnostics;

/// The validated entry and generated registration descriptor.
pub use plane_door::{door, MACRO_DOOR};

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/alloc_gate.rs"]
mod regression_alloc_gate;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/anthropic_error_marker.rs"]
mod regression_anthropic_error_marker;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/anthropic_schema_text.rs"]
mod regression_anthropic_schema_text;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/anthropic_stream_error_request_id.rs"]
mod regression_anthropic_stream_error_request_id;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/anthropic_usage_recovery.rs"]
mod regression_anthropic_usage_recovery;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/arrival_media_type.rs"]
mod regression_arrival_media_type;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/bedrock_identity_regressions.rs"]
mod regression_bedrock_identity_regressions;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/buffered_stream_regressions.rs"]
mod regression_buffered_stream_regressions;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/cohere_embedding_identity.rs"]
mod regression_cohere_embedding_identity;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/declared_protocols.rs"]
mod regression_declared_protocols;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/exchange_arrive.rs"]
mod regression_exchange_arrive;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/exchange_attempt.rs"]
mod regression_exchange_attempt;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/exchange_catalog.rs"]
mod regression_exchange_catalog;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/exchange_invoke.rs"]
mod regression_exchange_invoke;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/exchange_multipart.rs"]
mod regression_exchange_multipart;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/exchange_probe.rs"]
mod regression_exchange_probe;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/exchange_refuse.rs"]
mod regression_exchange_refuse;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/exchange_reply.rs"]
mod regression_exchange_reply;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/exchange_shaping.rs"]
mod regression_exchange_shaping;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/exchange_user_agent.rs"]
mod regression_exchange_user_agent;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/exchange_webhook.rs"]
mod regression_exchange_webhook;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/handler_contract.rs"]
mod regression_handler_contract;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/hook_absent_projection.rs"]
mod regression_hook_absent_projection;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/hook_max_tokens_saturation_1_5_5.rs"]
mod regression_hook_max_tokens_saturation_1_5_5;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/hook_unknown_role_1_5_5.rs"]
mod regression_hook_unknown_role_1_5_5;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/llm_provider_regressions.rs"]
mod regression_llm_provider_regressions;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/plane_door.rs"]
mod regression_plane_door;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/provider_count_refusal.rs"]
mod regression_provider_count_refusal;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/provider_keys.rs"]
mod regression_provider_keys;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/provider_options.rs"]
mod regression_provider_options;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/refusal_envelopes.rs"]
mod regression_refusal_envelopes;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/refusal_statuses.rs"]
mod regression_refusal_statuses;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/relay_decode_once.rs"]
mod regression_relay_decode_once;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/relay_over_cap.rs"]
mod regression_relay_over_cap;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/relay_over_cap_leaf_billing.rs"]
mod regression_relay_over_cap_leaf_billing;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/relay_over_cap_titan.rs"]
mod regression_relay_over_cap_titan;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/responses_regressions.rs"]
mod regression_responses_regressions;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/transcription_format_regressions.rs"]
mod regression_transcription_format_regressions;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/truncated_usage_regressions.rs"]
mod regression_truncated_usage_regressions;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/whole_failure.rs"]
mod regression_whole_failure;

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod common;

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "tests/both_ways.rs"]
mod regression_both_ways;

#[cfg(test)]
#[path = "tests/driver_private.rs"]
mod regression_driver_private;

include!(concat!(env!("OUT_DIR"), "/context_test_source.rs"));
