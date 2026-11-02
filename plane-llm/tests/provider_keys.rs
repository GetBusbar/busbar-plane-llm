// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE PLANE STATES ITS PROVIDER KEYS: the descriptor in

use busbar_contract::provider_keys::{decode, encode, ProviderKeys};
use busbar_plane_llm::meta::PROVIDER_KEYS;

fn declared() -> ProviderKeys {
    decode(PROVIDER_KEYS.as_bytes()).expect("the plane's provider keys are a neutral descriptor")
}

/// The descriptor reads, names every form, and round-trips through the contract's encoding.
#[test]
fn the_declared_provider_keys_are_a_neutral_descriptor() {
    let keys = declared();
    for (form, table) in [
        ("catalog", &keys.catalog),
        ("deployment", &keys.deployment),
        ("entry", &keys.entry),
    ] {
        assert!(!table.keys.is_empty(), "the {form} form states its keys");
    }
    assert_eq!(decode(&encode(&keys)), Ok(keys));
}
