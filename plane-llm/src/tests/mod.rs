// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE CRATE'S TESTS THAT REACH PAST THE DOOR. The crate's only public item is `door`
//! (BUSBAR-1.6.0.md l.3560, Crate shape), so the suites that name its private modules are modules
//! of the crate, not external test crates. The door-driven proofs are in `busbar-plane-llm-plugin`
//! (`tests/both_ways.rs`, `tests/conformance.rs`), which names `door` alone. The two directories
//! the composition root compiles beside its rig (`tests/root`, `tests/on_driver`) are not built
//! here.

mod alloc_gate;
mod anthropic_stream_error_request_id;
mod claims;
mod claims_ladder;
mod content_facts;
mod dialect_self_contained;
mod exchange_arrive;
mod exchange_attempt;
mod exchange_catalog;
mod exchange_invoke;
mod exchange_multipart;
mod exchange_probe;
mod exchange_refuse;
mod exchange_reply;
mod exchange_shaping;
mod exchange_user_agent;
mod exchange_webhook;
mod golden_parity;
mod harness;
mod hook_absent_projection;
mod hook_max_tokens_saturation_1_5_5;
mod hook_unknown_role_1_5_5;
mod metering;
mod one_spelling;
mod plain_language;
mod plane_door;
mod plane_host_entropy;
mod provider_keys;
mod purity;
mod refusal_envelopes;
mod refusal_statuses;
mod relay_over_cap;
mod stream_state;
