// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! **THE 1.5.5 UPGRADE THROUGH BOTH DOORS** (`busbar --migrate-config`; FLEET "Plugin-side migrate
//! entries", lead rulings in llm-migrate-design.md): 1.5.5 dialled a private or loopback provider
//! upstream with no allowlist, 1.6.0 refuses one unless `advanced.allow_destinations` names it. The
//! plane's migrate hands core every host a provider's `base_url` or `token_url` names, as
//! `AllowDestinationIfPrivate`, once each; core resolves and judges it with the destination guard's
//! own predicates. The sections come back as they went in (`providers:` is handed read-only).
//!
//! RED before: the door stated no `migrate`, so the host passed the sections through and raised
//! nothing: a 1.5.5 deployment on a local Ollama stopped working after the upgrade.

use std::path::PathBuf;

use busbar_contract::abi::sdk::migrate::MigrateNeed;
use busbar_plugin_loader::dispatch::load::raw;
use busbar_plugin_loader::dispatch::migrate::{migrate_door, migrate_library};
use serde_json::json;

/// This crate's built cdylib (uplifted or under `deps`, newest wins). A missing artifact is a
/// failure, never a skip.
fn cdylib() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    let profile = exe
        .parent()
        .and_then(|d| d.parent())
        .expect("target/<profile>");
    let file = busbar_plugin_loader::plugin_library_filename("busbar_plane_llm_plugin");
    [profile.join(&file), profile.join("deps").join(&file)]
        .into_iter()
        .filter_map(|p| Some((std::fs::metadata(&p).ok()?.modified().ok()?, p)))
        .max()
        .map(|(_, p)| p)
        .unwrap_or_else(|| panic!("the built cdylib {file} is missing: build the workspace first"))
}

/// The private-destinations corpus shape (busbar `tests/migration-corpus/private-destinations`), as
/// core hands it to the plane: its own sections, and the merged catalog+deployment `providers:`.
fn sections() -> serde_json::Value {
    json!({
        "pools": { "local": { "members": [{ "model": "llama-local" }] } },
        "providers": {
            "openai": { "protocol": "openai", "base_url": "https://api.openai.com", "api_key": { "env": "OPENAI_KEY" } },
            "local-ollama": { "protocol": "openai", "base_url": "http://127.0.0.1:11434" },
            "lab-vllm": { "protocol": "openai", "base_url": "http://10.0.0.5:8000", "api_key": { "env": "LAB_KEY" } },
            "dev-box": { "protocol": "anthropic", "base_url": "http://localhost:8080" },
            "ollama": { "protocol": "openai", "base_url": "http://ollama.internal:11434" },
            "in-rack": { "protocol": "openai", "base_url": "http://192.168.40.12:8000/v1" },
            "corp-gateway": {
                "protocol": "openai",
                "base_url": "https://gateway.example.com/v1",
                "token_url": "http://10.9.8.7/oauth2/token"
            },
            "imds-proxy": { "protocol": "openai", "base_url": "http://169.254.169.254" },
            "again": { "protocol": "openai", "base_url": "http://127.0.0.1:11434/v2" },
            "v6": { "protocol": "openai", "base_url": "http://[fd00::5]:8000" }
        },
        "models": { "llama-local": { "provider": "local-ollama" } }
    })
}

#[test]
fn the_door_raises_every_provider_host_for_core_to_judge_both_ways() {
    let bytes = std::fs::read(cdylib()).expect("the cdylib reads");
    let s = sections();
    let linked = migrate_door(raw(busbar_plane_llm::plane_door::door), &s, "1.5.5")
        .expect("migrates")
        .expect("the door states migrate");
    let dropped = migrate_library(&bytes, "busbar-plane-llm", &s, "1.5.5")
        .expect("migrates")
        .expect("the door states migrate");
    let if_private = |h: &str| MigrateNeed::AllowDestinationIfPrivate(h.to_string());
    for m in [linked, dropped] {
        assert_eq!(m.settings, s, "the sections come back as they went in");
        assert!(m.report.is_empty(), "core reports each host it decides");
        let mut needs = m.needs.clone();
        needs.sort_by_key(|n| format!("{n:?}"));
        let mut want = vec![
            if_private("api.openai.com"),
            if_private("127.0.0.1"),
            if_private("10.0.0.5"),
            if_private("localhost"),
            if_private("ollama.internal"),
            if_private("192.168.40.12"),
            if_private("gateway.example.com"),
            if_private("10.9.8.7"),
            if_private("169.254.169.254"),
            if_private("[fd00::5]"),
        ];
        want.sort_by_key(|n| format!("{n:?}"));
        assert_eq!(needs, want, "one need per host, base_url and token_url alike");
    }
}

#[test]
fn sections_without_providers_raise_nothing() {
    let s = json!({ "pools": {} });
    let m = migrate_door(raw(busbar_plane_llm::plane_door::door), &s, "1.5.5")
        .expect("migrates")
        .expect("the door states migrate");
    assert_eq!(m.settings, s);
    assert!(m.needs.is_empty() && m.report.is_empty());
}
