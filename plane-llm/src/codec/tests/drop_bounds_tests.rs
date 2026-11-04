// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors
use super::*;
use serde_json::json;

fn assert_bounds(paths: &[String]) {
    assert!(paths.len() <= MAX_DROP_PATHS);
    assert!(paths.iter().map(String::len).sum::<usize>() <= MAX_DROP_BYTES);
    assert!(paths.iter().all(|p| p.len() <= MAX_DROP_PATH_BYTES));
    assert_eq!(paths.last().map(String::as_str), Some(OMITTED_PATH));
}

#[test]
fn drop_bounds_stream_retains_bounded_prefix_across_frames() {
    let seam = Seam {
        direction: Direction::Response,
        ingress: "openai",
        egress: "gemini",
    };
    let mut seen = Vec::new();
    for frame in 0..4096 {
        scope_more(seam, &mut seen, || {
            note(Dropped::new(
                format!("unknown_{frame}"),
                &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
                "fixture",
            ));
        });
    }
    assert_bounds(&seen);
    assert_eq!(seen.len(), MAX_DROP_PATHS);
    assert_eq!(seen[0], "unknown_0");
    assert_eq!(
        seen[MAX_DROP_PATHS - 2],
        format!("unknown_{}", MAX_DROP_PATHS - 2)
    );
}

#[test]
fn drop_bounds_bytes_and_oversized_paths_are_not_retained() {
    let mut paths = Paths::default();
    for n in 0..100 {
        paths.push(&format!("{n:04}{}", "x".repeat(4000)));
    }
    assert_bounds(&paths.ordered);
    assert!(paths.ordered.len() < MAX_DROP_PATHS);
    let mut paths = Paths::default();
    paths.push_parts(&["a", &"x".repeat(MAX_DROP_PATH_BYTES)]);
    assert_eq!(paths.ordered, [OMITTED_PATH]);
}

#[test]
fn drop_bounds_all_walk_collections_are_bounded_and_ordered() {
    let body = Value::Object(
        (0..10000)
            .map(|n| (format!("unknown_{n:05}"), json!(1)))
            .collect(),
    );
    let carried = Carried {
        map: &[],
        code: &[],
        drops: &[],
        reroot: None,
        defaults: &[],
    };
    let paths = carried.unmapped("", &body);
    assert_bounds(&paths);
    assert_eq!(paths[0], "unknown_00000");
    assert_eq!(
        extra_paths(body.as_object().unwrap(), &[], |_| false),
        paths
    );
    assert_bounds(&carried.unmapped(&"x".repeat(MAX_DROP_PATH_BYTES + 1), &body));
    let grammar = Blocks {
        at: &["blocks[]"],
        tag: Some("type"),
        modelled: &[],
        companions: &[],
    };
    let body = json!({"blocks": (0..10000).map(|n| json!({"type": format!("unknown_{n}")})).collect::<Vec<_>>()});
    assert_bounds(&grammar.unmodelled(&body));
    let mut visits = 0;
    visit(&body, grammar.at, &mut |_| {
        visits += 1;
        visits < MAX_DROP_PATHS
    });
    assert_eq!(
        visits, MAX_DROP_PATHS,
        "visitor stops without collecting all input blocks"
    );
}

#[test]
fn drop_bounds_duplicate_paths_keep_first_encounter_order() {
    let carried = Carried {
        map: &["items[].kept"],
        code: &[],
        drops: &[],
        reroot: None,
        defaults: &[],
    };
    assert_eq!(
        carried.unmapped("", &json!({"items": [{"z": 1}, {"a": 2}, {"z": 3}]})),
        ["items[].z", "items[].a"]
    );
}
