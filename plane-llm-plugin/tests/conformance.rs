// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE PUBLISHED CONFORMANCE SUITE, run over this plane (OWNER 2026-10-03: plugins test themselves
//! against busbar's published suite). The linked door (`busbar_plane_llm::door`) and this crate's
//! dropped-in image (the `busbar-plane-llm-plugin` cdylib, which `cargo test` builds beside the
//! test), each through the one loader, driven by busbar-plugin-loader's plane script over
//! `conformance.json`, every step at its pinned crossing count, the two folds equal. Its sessions
//! are the far ends' own answers, one per dialect that reports a class (Anthropic server tools, an
//! OpenAI stated total, Cohere billed units, three Bedrock guardrail traces, an OpenAI per-image
//! answer, a whisper duration), each wanting its counts exactly, so every billable class the tail
//! declares is reported by a case. This is the plugin's own both-ways witness at the path every
//! plugin repo keeps it (`tests/conformance.rs`). The op-by-op script the crate carried in busbar is
//! `tests/both_ways.rs` beside it.

/// This plugin's own door, named for the loader.
fn loaded_door() -> busbar_plugin_loader::dispatch::LoadedDoor {
    // SAFETY: `busbar_plane_llm::door` is this crate's own door: it answers the `'static` door of
    // this plugin.
    #[allow(unused_unsafe)]
    let loaded =
        unsafe { busbar_plugin_loader::dispatch::LoadedDoor::from_raw(busbar_plane_llm::door) };
    loaded
}

busbar_plugin_loader::conformance_suite! {
    loaded: loaded_door(),
    cdylib: "busbar_plane_llm_plugin",
    inputs: include_str!("conformance.json"),
}
