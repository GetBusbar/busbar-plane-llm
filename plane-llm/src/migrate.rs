// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE PLANE'S 1.5.5 UPGRADE (`busbar --migrate-config`): 1.5.5 dialled a private or loopback
//! provider upstream with no allowlist; 1.6.0 refuses one unless `advanced.allow_destinations`
//! names it.

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
    type Kind = busbar_contract::plugin::markers::PlaneKind;
    fn migrate_references(settings: Value, from: &str) -> Migrated {
        let mut needs = Vec::new();
        if from == "1.5.5" {
            credential_needs(&settings, &mut needs);
        }
        Migrated::new(settings, Vec::new(), needs)
    }
    fn migrate(settings: Value, from: &str) -> Migrated {
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
        if from == "1.5.5" {
            credential_needs(&settings, &mut needs);
        }
        Migrated::new(settings, Vec::new(), needs)
    }
}

fn credential_needs(settings: &Value, needs: &mut Vec<MigrateNeed>) {
    use busbar_contract::abi::sdk::migrate::SOURCE_DOCUMENTS;
    let sources = settings
        .get(SOURCE_DOCUMENTS)
        .and_then(|s| s.get(sections::PROVIDERS));
    let authored = sources
        .and_then(|s| s.get("authored"))
        .or_else(|| settings.get(sections::PROVIDERS));
    let catalog = sources
        .and_then(|s| s.get("catalog"))
        .filter(|v| !v.is_null());
    for (name, provider) in authored.and_then(Value::as_object).into_iter().flatten() {
        let explicit = provider.get("auth").filter(|v| !v.is_null());
        let auth = explicit.or_else(|| catalog?.get(name)?.get("auth"));
        let path = vec![sections::PROVIDERS.into(), name.clone(), "api_key".into()];
        if auth.and_then(Value::as_str) == Some("jwt-bearer") {
            needs.push(MigrateNeed::MigrateCredential {
                path,
                module: "oauth".into(),
            });
        } else if sources.is_some()
            && explicit.is_none()
            && catalog.is_none()
            && provider.get("api_key").is_some()
        {
            needs.push(MigrateNeed::Review { path, reason: "supply the original providers catalog so its auth owner can check historical credential migration; the original reference was preserved".into() });
        }
    }
}

#[cfg(test)]
/// The settings a sealed [`Migrated`] carries, read back under the dev-only test kernel seal.
fn settings_of(answer: &Migrated) -> serde_json::Value {
    let seal = busbar_contract::caps::KernelSeal::acquire_for_kernel();
    serde_json::to_value(
        busbar_contract::abi::sdk::migrate::MigrationAnswer::from(answer.clone()).authoring(&seal),
    )
    .expect("migrated settings serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_auth_selects_its_owner_without_lending_catalog_credentials() {
        let settings = serde_json::json!({"providers": {
            "jwt": {"auth":"jwt-bearer"}, "other": {"auth":"bearer"},
            "structured": {"auth":{"module":"oauth"}}
        }});
        let answer = LlmMigration::migrate(settings.clone(), "1.5.5");
        assert_eq!(settings_of(&answer), settings);
        assert_eq!(
            answer.needs,
            vec![MigrateNeed::MigrateCredential {
                path: vec!["providers".into(), "jwt".into(), "api_key".into()],
                module: "oauth".into()
            }]
        );
        assert!(LlmMigration::migrate(settings, "1.6.0").needs.is_empty());
    }
}

#[cfg(test)]
mod provenance_tests {
    use super::*;
    #[test]
    fn deployment_override_null_fallback_and_catalog_only_credentials_are_distinct() {
        use busbar_contract::abi::sdk::migrate::SOURCE_DOCUMENTS;
        let settings = serde_json::json!({"providers": {}, SOURCE_DOCUMENTS: {"providers": {
            "authored": {
                "inherited": {"api_key":{"env":"SOURCE"}},
                "null": {"auth":null,"api_key":{"env":"SOURCE"}},
                "override": {"auth":"bearer","api_key":{"env":"SOURCE"}},
                "structured": {"auth":{"module":"other"},"api_key":{"env":"SOURCE"}}
            },
            "catalog": {
                "inherited": {"auth":"jwt-bearer"}, "null": {"auth":"jwt-bearer"},
                "override": {"auth":"jwt-bearer"}, "structured": {"auth":"jwt-bearer"},
                "catalog-only": {"auth":"jwt-bearer","api_key":{"env":"NEVER_IMPORT"}}
            }
        }}});
        let answer = LlmMigration::migrate(settings.clone(), "1.5.5");
        assert_eq!(settings_of(&answer), settings);
        assert_eq!(answer.needs.len(), 2);
        for (need, name) in answer.needs.iter().zip(["inherited", "null"]) {
            assert_eq!(
                need,
                &MigrateNeed::MigrateCredential {
                    path: vec!["providers".into(), name.into(), "api_key".into()],
                    module: "oauth".into()
                }
            );
        }
    }
}

#[cfg(test)]
mod discovery_tests {
    use super::*;
    #[test]
    fn provider_only_discovery_converts_credentials_without_destination_changes() {
        let settings = serde_json::json!({"providers":{"old":{"auth":"jwt-bearer","base_url":"http://127.0.0.1","api_key":{"env":"SOURCE"}}}});
        let answer = LlmMigration::migrate_references(settings.clone(), "1.5.5");
        assert_eq!(settings_of(&answer), settings);
        assert!(answer.report.is_empty());
        assert_eq!(answer.needs.len(), 1);
        assert!(matches!(
            answer.needs[0],
            MigrateNeed::MigrateCredential { .. }
        ));
    }
}
