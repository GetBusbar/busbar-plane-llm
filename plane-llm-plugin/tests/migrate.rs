// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Upgrade 1.5.5 LLM configuration through linked and dynamically loaded migration doors.

use std::path::PathBuf;

use busbar_contract::abi::sdk::migrate::MigrateNeed;
use busbar_plugin_loader::dispatch::load::raw;
use busbar_plugin_loader::dispatch::migrate::{migrate_door, migrate_library};
use serde_json::json;

fn cdylib() -> PathBuf {
    let path = PathBuf::from(
        std::env::var_os("BUSBAR_PLUGIN_CDYLIB")
            .expect("build this plugin and set BUSBAR_PLUGIN_CDYLIB to its fresh cdylib"),
    );
    assert!(
        path.is_absolute(),
        "BUSBAR_PLUGIN_CDYLIB must be absolute: {}",
        path.display()
    );
    assert!(
        path.is_file(),
        "the built plugin cdylib is missing: {}",
        path.display()
    );
    path
}

/// The private-destinations corpus shape (busbar `tests/migration-corpus/private-destinations`), as
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
            if_private("fd00::5"),
        ];
        want.sort_by_key(|n| format!("{n:?}"));
        assert_eq!(
            needs, want,
            "one need per host, base_url and token_url alike"
        );
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
