// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::merge_dropped_paths;

fn strings(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_string()).collect()
}

fn original_merge(controls: &[&str], dropped: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for path in controls {
        if !out.iter().any(|value: &String| value.as_str() == *path) {
            out.push((*path).to_string());
        }
    }
    for path in dropped {
        let covered = out.iter().any(|value| {
            value.as_str() == *path
                || value
                    .strip_prefix(*path)
                    .is_some_and(|rest| rest.starts_with('='))
        });
        if !covered {
            out.push((*path).to_string());
        }
    }
    out
}

#[test]
fn drop_index_merge_preserves_exact_prefix_semantics_and_encounter_order() {
    for (controls, dropped, expected) in [
        (
            vec!["tool_choice=none", "tool_choice=none"],
            vec!["tool_choice", "other"],
            vec!["tool_choice=none", "other"],
        ),
        (
            vec!["x.alpha", "x=value"],
            vec!["x"],
            vec!["x.alpha", "x=value"],
        ),
        (
            vec![],
            vec!["z", "x=y=z", "x=y", "x", "z"],
            vec!["z", "x=y=z"],
        ),
        (
            vec!["x"],
            vec!["x=value", "x.other"],
            vec!["x", "x=value", "x.other"],
        ),
        (
            vec!["=value", "é=文", "é.prefix"],
            vec!["", "é", "é=文"],
            vec!["=value", "é=文", "é.prefix"],
        ),
    ] {
        assert_eq!(
            merge_dropped_paths(strings(&controls), strings(&dropped)),
            expected
        );
    }
}

#[test]
fn drop_index_merge_matches_original_predicate_for_exhaustive_short_sequences() {
    const PATHS: &[&str] = &["", "x", "x=", "x=a", "x=a=b", "x.a", "x>a", "=x", "é=文"];
    for control in PATHS {
        for first in PATHS {
            for second in PATHS {
                for third in PATHS {
                    let controls = [*control, *control];
                    let dropped = [*first, *second, *third];
                    assert_eq!(
                        merge_dropped_paths(strings(&controls), strings(&dropped)),
                        original_merge(&controls, &dropped),
                        "controls={controls:?} dropped={dropped:?}",
                    );
                }
            }
        }
    }
}
