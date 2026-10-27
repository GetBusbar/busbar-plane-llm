// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE DROP-NAMES CENSUS: how many (dialect, IR name) drops are named by the IR name because the
//! dialect's map file has no row for the member (design F3 "Drops"; never silent, driven to 0).

// ───────────────────────────── the census ─────────────────────────────

/// The IR members a seam gate or a writer drops by name, with the rows each is looked up by.
const NAMED: &[(&str, &[&str])] = &[
    ("n", &[]),
    (
        "reasoning",
        &["reasoning_effort", "reasoning", "thinking_budget"],
    ),
    ("cache_control", &[]),
    ("stop", &[]),
    ("tool_choice", &[]),
    ("parallel_tool_calls", &[]),
    ("response_format", &[]),
    ("tools", &[]),
    ("metadata", &[]),
    ("top_k", &[]),
    ("top_logprobs", &[]),
    ("output_modalities", &[]),
    ("logprobs", &[]),
    ("service_tier", &[]),
    ("temperature", &[]),
    ("top_p", &[]),
];

/// The six request dialects, by the name the drop path knows each by, and the file stem of its wire
/// lock (testing/llm-conformance/wire/<stem>.wire.json).
const DIALECTS: &[(&str, &str)] = &[
    ("anthropic", "anthropic"),
    ("bedrock", "bedrock"),
    ("cohere", "cohere"),
    ("gemini", "gemini"),
    ("openai", "openai"),
    ("responses", "responses"),
];

/// The IR request members each dialect's reader never sets: the dialect has no wire form for them
/// (or keeps its own spelling of the concept for a same-dialect hop only), so a caller of that
/// dialect never has one dropped and no drop is ever named for it.
fn unread(dialect: &str) -> &'static [&'static str] {
    use crate::codec::drops::name::*;
    match dialect {
        "anthropic" => &[N, METADATA, TOP_LOGPROBS, OUTPUT_MODALITIES, LOGPROBS],
        "bedrock" => &[
            N,
            CACHE_CONTROL,
            PARALLEL_TOOL_CALLS,
            TOP_LOGPROBS,
            OUTPUT_MODALITIES,
            LOGPROBS,
        ],
        "cohere" => &[
            N,
            CACHE_CONTROL,
            PARALLEL_TOOL_CALLS,
            METADATA,
            TOP_LOGPROBS,
            OUTPUT_MODALITIES,
            SERVICE_TIER,
        ],
        "gemini" => &[CACHE_CONTROL, PARALLEL_TOOL_CALLS],
        "openai" => &[CACHE_CONTROL, TOP_K],
        "responses" => &[N, CACHE_CONTROL, STOP, TOP_K, OUTPUT_MODALITIES],
        _ => &[],
    }
}

/// Every (dialect, IR name) pair a drop could be named for is named by the caller's wire path: a
/// map-file row, or the path the dialect's reader carries the member from by code. A member the
/// dialect's reader never sets (`unread`) is never dropped from that dialect's caller. Measured 41
/// at DF-NAMES; DF-SITES drove it to 0, and it stays there.
#[test]
fn every_drop_is_named_by_the_callers_wire_path() {
    let mut unresolved = Vec::new();
    for (dialect, _) in DIALECTS {
        let unread = unread(dialect);
        for (name, rows) in NAMED {
            if crate::codec::drops::resolve(dialect, name, rows).is_none() && !unread.contains(name)
            {
                unresolved.push(format!("{dialect}:{name}"));
            }
        }
    }
    assert!(
        unresolved.is_empty(),
        "a drop would be named by its IR name (give the dialect a row, a request code name, or \
         declare the member unread): {unresolved:?}"
    );
}

/// An `unread` member really has no wire path in that dialect: it is not also a row or a code name.
#[test]
fn an_unread_member_has_no_wire_path() {
    for (dialect, _) in DIALECTS {
        let unread = unread(dialect);
        for name in unread {
            assert_eq!(
                crate::codec::drops::resolve(dialect, name, &[]),
                None,
                "{dialect} declares `{name}` unread but names a wire path for it"
            );
        }
    }
}

/// A request code name's wire path is in the dialect's wire lock (a union arm's selector may be
/// left out; below an opaque document member, the member itself is what the lock lists).
#[test]
fn every_request_code_name_is_in_the_wire_lock() {
    let root =
        std::path::PathBuf::from(std::env::var_os("BUSBAR_CHECKOUT").expect(
            "BUSBAR_CHECKOUT is unset: busbar's tree at .busbar-ref (plugin CI exports it)",
        ))
        .join("testing/llm-conformance/wire");
    for (dialect, stem) in DIALECTS {
        let text = std::fs::read_to_string(root.join(format!("{stem}.wire.json"))).expect("lock");
        let lock: serde_json::Value = serde_json::from_str(&text).expect("lock json");
        let paths: Vec<String> = lock["request"]
            .as_object()
            .expect("request paths")
            .keys()
            .map(|k| {
                k.split('.')
                    .filter(|step| !step.contains('='))
                    .collect::<Vec<_>>()
                    .join(".")
            })
            .collect();
        let names =
            crate::codec::proto_codec::with_reader(dialect, |r| r.grammar().request_code_names)
                .unwrap_or(&[]);
        for (name, path) in names {
            let steps: Vec<&str> = path.split('.').collect();
            let opaque = |p: &str| {
                paths.iter().any(|l| l == p)
                    && !paths.iter().any(|l| l.starts_with(&format!("{p}.")))
            };
            let found = paths.iter().any(|l| l == path)
                || (1..steps.len()).any(|n| opaque(&steps[..n].join(".")));
            assert!(
                found,
                "{dialect}: `{name}` is named `{path}`, which its wire lock does not list"
            );
        }
    }
}
