// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE PLANE'S 1.5.5 UPGRADE (`busbar --migrate-config`): 1.5.5 dialled a private or loopback
//! provider upstream with no allowlist; 1.6.0 refuses one unless `advanced.allow_destinations`
//! names it. For every host a provider's `base_url` or `token_url` names (the merged
//! catalog+deployment `providers:` core hands the plane, read-only), the plane raises
//! `AllowDestinationIfPrivate`, once per host: core resolves it at migrate time and adds exactly
//! that host when every answer is private or loopback and none is cloud metadata, with a report
//! line for each decision. The sections are answered unchanged.

use busbar_contract::abi::sdk::migrate::{Migrate, MigrateNeed, Migrated};
use busbar_contract::net::url_host;
use serde_json::Value;

use crate::exchange::shaping::sections;

/// The provider keys whose URL the plane dials.
const DIALLED: [&str; 2] = ["base_url", "token_url"];

/// The plane's migration.
#[derive(Debug)]
pub struct LlmMigration;

impl Migrate for LlmMigration {
    fn migrate(settings: Value, _from: &str) -> Migrated {
        let mut needs: Vec<MigrateNeed> = Vec::new();
        let providers = settings.get(sections::PROVIDERS).and_then(Value::as_object);
        for provider in providers.into_iter().flat_map(|p| p.values()) {
            for key in DIALLED {
                let host = provider.get(key).and_then(Value::as_str).and_then(url_host);
                if let Some(need) = host.map(MigrateNeed::AllowDestinationIfPrivate) {
                    if !needs.contains(&need) {
                        needs.push(need);
                    }
                }
            }
        }
        Migrated {
            settings,
            report: Vec::new(),
            needs,
        }
    }
}
