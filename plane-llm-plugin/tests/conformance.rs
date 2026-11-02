// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Published conformance tests exercised against the LLM plane.

busbar_test_kit::conformance_suite! {
    door: busbar_plane_llm::plane_door::door,
    cdylib: "busbar_plane_llm_plugin",
    inputs: include_str!("conformance.json"),
}
