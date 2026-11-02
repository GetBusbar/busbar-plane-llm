// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE PLANE-CODE DRIFT GUARD (#83a O4): every code this plane defines is the published catalog's

use super::DIAGNOSTICS;

fn published() -> Vec<serde_json::Value> {
    let path =
        std::path::PathBuf::from(std::env::var_os("BUSBAR_CHECKOUT").expect(
            "BUSBAR_CHECKOUT is unset: busbar's tree at .busbar-ref (plugin CI exports it)",
        ))
        .join("docs/diagnostics.json");
    let text = std::fs::read_to_string(path).expect("docs/diagnostics.json is readable");
    serde_json::from_str(&text).expect("docs/diagnostics.json is a JSON array")
}

#[test]
fn every_plane_code_is_the_catalog_entry_of_its_number() {
    let catalog = published();
    for &d in DIAGNOSTICS {
        let entry = catalog
            .iter()
            .find(|e| e["number"] == u64::from(d.code))
            .unwrap_or_else(|| panic!("BUSBAR-{:04} is not in the published catalog", d.code));
        let class = format!("{:?}", d.class).to_lowercase();
        let ours = serde_json::json!({
            "code": format!("BUSBAR-{:04}", d.code),
            "number": d.code,
            "class": class,
            "slug": d.slug,
            "title": d.title,
            "severity": d.severity.as_str(),
            "summary": d.summary,
            "action": d.action,
            "since": d.since,
            "retired": d.retired,
        });
        assert_eq!(
            &ours, entry,
            "BUSBAR-{:04} differs from the published catalog entry",
            d.code
        );
    }
}

#[test]
fn no_plane_code_is_listed_twice() {
    let mut codes: Vec<u16> = DIAGNOSTICS.iter().map(|d| d.code).collect();
    codes.sort_unstable();
    codes.dedup();
    assert_eq!(
        codes.len(),
        DIAGNOSTICS.len(),
        "a plane code is listed twice"
    );
}

/// THE DOOR'S `declares.json` IS THE PLANE'S CATALOG: the declared section the host folds this plane's
#[test]
fn the_declares_file_states_the_catalog() {
    let declared: serde_json::Value =
        serde_json::from_str(crate::DECLARES).expect("declares.json is JSON");
    let want: Vec<serde_json::Value> = DIAGNOSTICS
        .iter()
        .map(|d| {
            assert!(!d.retired, "a declared code is a live one");
            serde_json::json!({
                "code": d.code,
                "slug": d.slug,
                "title": d.title,
                "severity": d.severity.as_str(),
                "summary": d.summary,
                "action": d.action,
                "since": d.since,
            })
        })
        .collect();
    let whole = serde_json::json!({ "needs": ["http"], "diagnostics": want });
    assert_eq!(
        declared,
        whole,
        "declares.json drifted from the catalog; expected:\n{}",
        serde_json::to_string_pretty(&whole).unwrap()
    );
}
