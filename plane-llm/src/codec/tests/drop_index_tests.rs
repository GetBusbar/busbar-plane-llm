// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Semantics and a hardware-instruction probe for caller-sized drop collections.

use super::*;
use crate::codec::translate::{TranslateCodec as _, TranslateReqInput};
use busbar_contract::codec::EgressWire;
use busbar_contract::operation::OpVerb;
use busbar_test_kit::warn_capture::WarnCapture;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

const SEAM: Seam<'static> = Seam {
    direction: Direction::Request,
    ingress: "openai",
    egress: "anthropic",
};

fn report(path: &'static str) {
    note(Dropped::new(
        path,
        &crate::codec::diagnostics::IR_DROP_UNMODELED_KEYS,
        "fixture drop",
    ));
}

#[test]
fn drop_index_extra_aliases_keep_first_encounter_order() {
    const PARKED: &[Parked] = &[
        Parked {
            key: "a",
            holds: Holds::Path("zeta"),
        },
        Parked {
            key: "b",
            holds: Holds::Path("alpha"),
        },
        Parked {
            key: "c",
            holds: Holds::Path("zeta"),
        },
        Parked {
            key: "d",
            holds: Holds::Items("parts[]", &["kept"]),
        },
    ];
    let extra = json!({
        "a": "private-a", "b": "private-b", "c": "private-c",
        "d": {"0": {"beta": 1, "kept": 2}, "1": {"beta": 3, "gamma": 4}},
        "zeta": "private-zeta"
    });
    assert_eq!(
        extra_paths(extra.as_object().unwrap(), PARKED, |_| false),
        ["zeta", "alpha", "parts[].beta", "parts[].gamma"]
    );
}

#[test]
fn drop_index_seeded_scope_without_notes_keeps_the_vector_verbatim() {
    let mut seen = vec!["zeta".to_string(), "alpha".to_string(), "zeta".to_string()];
    let cap = WarnCapture::default();
    let answer =
        tracing::subscriber::with_default(cap.clone(), || scope_more(SEAM, &mut seen, || 17));
    assert_eq!(answer, 17);
    assert_eq!(seen, ["zeta", "alpha", "zeta"]);
    assert!(cap.messages().is_empty());
    assert!(!is_open());
}

#[test]
fn drop_index_seeded_paths_restore_after_nested_unwind() {
    let mut seen = vec!["seed".to_string(), "seed".to_string()];
    let cap = WarnCapture::default();
    tracing::subscriber::with_default(cap.clone(), || {
        scope_more(SEAM, &mut seen, || {
            report("seed");
            report("new");
            let unwind = std::panic::catch_unwind(|| {
                scope(SEAM, || {
                    report("seed");
                    report("inner");
                    panic!("fixture nested attempt failed");
                });
            });
            assert!(unwind.is_err());
            assert!(
                is_open(),
                "the outer attempt resumes after the inner unwinds"
            );
            report("new");
            report("tail");
        });
        report("outside");
    });
    assert_eq!(seen, ["seed", "seed", "new", "tail"]);
    assert_eq!(
        cap.count("path=seed"),
        1,
        "only the nested attempt warns seed"
    );
    assert_eq!(cap.count("path=new"), 1);
    assert_eq!(cap.count("path=tail"), 1);
    assert_eq!(cap.count("path=outside"), 0);
    assert!(!is_open());
}

struct Reentrant(Arc<AtomicUsize>);
impl tracing::Subscriber for Reentrant {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        self.0.fetch_add(1, Ordering::Relaxed);
        assert!(is_open());

        report("callback_path");
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn drop_index_records_before_a_reentrant_subscriber_callback() {
    let events = Arc::new(AtomicUsize::new(0));
    let (_, paths) = tracing::subscriber::with_default(Reentrant(events.clone()), || {
        scope(SEAM, || {
            report("callback_path");
            report("callback_path");
        })
    });
    assert_eq!(paths, ["callback_path"]);
    assert_eq!(events.load(Ordering::Relaxed), 1);
    assert!(!is_open());
}

fn request_with_unknown_keys(n: usize) -> Value {
    let mut body = json!({"model": "m", "messages": [{"role": "user", "content": "hello"}]});
    let object = body.as_object_mut().unwrap();
    for index in 0..n {
        object.insert(
            format!("fixture_unknown_{index:06}"),
            json!("private fixture payload"),
        );
    }
    body
}

#[test]
fn drop_index_full_request_keeps_each_warning_and_audit_without_payload_values() {
    let body = request_with_unknown_keys(1024);
    let (wire, audited, cap) = super::tests::translate("openai", "anthropic", &body);
    let expected: Vec<_> = (0..1024)
        .map(|n| format!("fixture_unknown_{n:06}"))
        .collect();
    assert_eq!(audited, expected);
    let warnings = cap.messages();
    assert_eq!(warnings.len(), expected.len());
    assert!(warnings
        .iter()
        .all(|message| !message.contains("private fixture payload")));
    assert_eq!(wire["messages"][0]["content"][0]["text"], "hello");
    assert!(wire
        .as_object()
        .unwrap()
        .keys()
        .all(|key| !key.starts_with("fixture_unknown_")));
    let (_, baseline, baseline_cap) =
        super::tests::translate("openai", "anthropic", &request_with_unknown_keys(0));
    assert!(baseline.is_empty());
    assert!(baseline_cap.messages().is_empty());
}

#[test]
fn drop_index_unknown_block_kinds_reach_the_full_request_walk_in_encounter_order() {
    let body = json!({"model": "m", "messages": [{"role": "user", "content": [
        {"type": "text", "text": "hello"},
        {"type": "fixture_z", "private": "not logged"},
        {"type": "fixture_a", "private": "not logged"},
        {"type": "fixture_z", "private": "not logged"}
    ]}]});
    let (wire, paths, cap) = super::tests::translate("openai", "anthropic", &body);
    assert_eq!(
        paths,
        [
            "messages[].content[].type=fixture_z",
            "messages[].content[].type=fixture_a",
        ]
    );
    assert_eq!(cap.messages().len(), 2);
    assert!(cap
        .messages()
        .iter()
        .all(|message| !message.contains("not logged")));
    assert_eq!(
        wire["messages"][0]["content"],
        json!([{"type": "text", "text": "hello"}])
    );
}

struct CountWarnings(Arc<AtomicUsize>);
impl tracing::Subscriber for CountWarnings {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, _: &tracing::Event<'_>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Run the exact test binary under `sudo perf stat -e instructions` at 2048 and 8192 keys,

#[test]
#[ignore = "manual hardware-instruction scaling witness; see the drop-index design report"]
fn drop_index_full_request_instruction_probe() {
    let n: usize = std::env::var("BUSBAR_DROP_INDEX_PROBE_KEYS")
        .expect("explicit probe cardinality")
        .parse()
        .expect("integer probe cardinality");
    assert!((1..=100_000).contains(&n));
    let mode = std::env::var("BUSBAR_DROP_INDEX_PROBE_KIND").unwrap_or_else(|_| "keys".into());
    let body = match mode.as_str() {
        "keys" => request_with_unknown_keys(n),
        "blocks" => {
            let mut blocks = vec![json!({"type": "text", "text": "hello"})];
            blocks.extend((0..n).map(|index| json!({"type": format!("fixture_{index:06}")})));
            json!({"model": "m", "messages": [{"role": "user", "content": blocks}]})
        }
        _ => panic!("probe mode must be keys or blocks"),
    };
    let encoded = serde_json::to_vec(&body).unwrap();
    assert!(encoded.len() < busbar_contract::transport::DEFAULT_REQUEST_BODY_MAX_BYTES);
    let handler = crate::codec::decl_of("openai")
        .unwrap()
        .handler
        .unwrap()
        .operation_handler(OpVerb::CHAT)
        .unwrap();
    let warnings = Arc::new(AtomicUsize::new(0));
    let output = tracing::subscriber::with_default(CountWarnings(warnings.clone()), || {
        handler.translate_request(
            crate::codec::test_host::HOST,
            TranslateReqInput::Json(&body),
            Some("anthropic"),
            &super::tests::prep("openai"),
            "m",
        )
    });
    let Ok(output) = output else {
        panic!("the bounded request must translate")
    };
    let EgressWire::Json(wire) = output.wire else {
        panic!("JSON output")
    };
    assert_eq!(output.dropped_controls.len(), n);
    for (index, path) in output.dropped_controls.iter().enumerate() {
        let expected = match mode.as_str() {
            "keys" => format!("fixture_unknown_{index:06}"),
            "blocks" => format!("messages[].content[].type=fixture_{index:06}"),
            _ => unreachable!(),
        };
        assert_eq!(*path, expected);
    }
    assert_eq!(warnings.load(Ordering::Relaxed), n);
    assert_eq!(wire["messages"][0]["content"][0]["text"], "hello");
    println!(
        "probe_kind={mode} entries={n} body_bytes={} warnings={n} audited={n}",
        encoded.len()
    );
}
