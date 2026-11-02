// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The llm plane's door, its static half: a Statement and tail the contract accepts, the needs

#![allow(unused_unsafe)]

use busbar_contract::abi::mechanism::check::check_needs;
use busbar_contract::abi::plane::check::{check_refusal_statuses, check_sections, check_tail};
use busbar_contract::abi::plane::{TAIL_FALLBACK, TAIL_PROBES, UNITS_REPORTED};
use busbar_contract::abi::sdk::plane::UnitCancel;
use busbar_plane_llm::dialect::DIALECTS;
use busbar_plane_llm::exchange::reply::Units;
use busbar_plane_llm::plane_door::{
    claims, counts, read_settings, route_of, DENY_RESPONSE_HEADERS, EGRESS_STYLES, NEEDS,
    OPEN_CLASSES, STATEMENT, TAIL, VERSION,
};

#[test]
fn the_statement_names_the_crate_version() {
    assert_eq!(VERSION, env!("CARGO_PKG_VERSION"));
}

/// THE DOOR STATES THE PLANE'S `busbar --help` ROWS: its tail's `cli_help`, read in the contract's

#[test]
fn the_tail_states_the_planes_help_rows() {
    // SAFETY: a non-NULL tail string is `len` bytes of `'static` plugin data.
    let help = unsafe { std::slice::from_raw_parts(TAIL.cli_help.ptr(), TAIL.cli_help.len()) };
    let help = std::str::from_utf8(help).expect("the help is text");
    let rows: Vec<(&str, &str)> = busbar_contract::abi::plane::cli_help::rows(help).collect();
    assert_eq!(
        rows,
        [
            ("tagline", busbar_plane_llm::meta::HELP_TAGLINE),
            ("endpoint", busbar_plane_llm::meta::HELP_ENDPOINTS),
        ]
    );
}

/// THE TAIL STATES EACH DIALECT'S PROVIDER FLAG, read off its

#[test]
fn the_tail_flags_every_dialect_a_provider_dialect() {
    use busbar_contract::abi::plane::{check::check_dialect_auth, DIALECT_PROVIDER};
    let (lines, names) = (unsafe { TAIL.dialect_auth() }, unsafe { TAIL.dialects() });
    assert_eq!(check_dialect_auth(lines, names.len() as u64), Ok(()));
    let name = |i: u32| {
        let s = names[i as usize];
        // SAFETY: a tail string is `len` bytes of `'static` plugin data.
        std::str::from_utf8(unsafe { std::slice::from_raw_parts(s.ptr(), s.len()) }).expect("text")
    };
    let provider: Vec<&str> = lines
        .iter()
        .filter(|l| l.flags & DIALECT_PROVIDER != 0)
        .map(|l| name(l.dialect))
        .collect();
    assert_eq!(
        provider,
        [
            "anthropic",
            "openai",
            "gemini",
            "bedrock",
            "responses",
            "cohere"
        ],
        "the provider dialects, in 1.5.5's order"
    );
}

#[test]
fn the_tail_is_one_the_contract_accepts() {
    assert_eq!(check_tail(TAIL), Ok(()));
    let statement = STATEMENT;
    let sections = unsafe { statement.sections() };
    assert_eq!(check_sections(sections), Ok(()));
    assert_eq!(
        check_refusal_statuses(
            &busbar_plane_llm::refusal::REFUSAL_STATUSES,
            TAIL.dialects_len() as u64
        ),
        Ok(())
    );
    assert_eq!(TAIL.flags, TAIL_FALLBACK | TAIL_PROBES);
    assert_eq!(TAIL.dialects_len(), DIALECTS.len());
    assert_eq!(TAIL.op_classes_len(), 7);
    assert_eq!(
        TAIL.billable_classes_len(),
        20,
        "tokens in, out, cache read, cache write, the open classes, then the per-request fee unit"
    );

    assert_eq!(
        OPEN_CLASSES,
        &[
            ("search_units", "units"),
            ("classifications", "count"),
            ("web_fetch_requests", "count"),
            ("unitemized_tokens", "count"),
            ("images", "count"),
            ("audio_ms", "duration"),
            ("guardrail_automated_reasoning_policies", "count"),
            ("guardrail_automated_reasoning_policy_units", "count"),
            ("guardrail_content_policy_image_units", "count"),
            ("guardrail_content_policy_units", "count"),
            ("guardrail_contextual_grounding_policy_units", "count"),
            ("guardrail_sensitive_information_policy_free_units", "count"),
            ("guardrail_sensitive_information_policy_units", "count"),
            ("guardrail_topic_policy_units", "count"),
            ("guardrail_word_policy_units", "count"),
        ]
    );
    let stated = unsafe { TAIL.billable_classes() };

    let tokens = [
        ("input", "token"),
        ("output", "token"),
        ("cache_read", "token"),
        ("cache_write", "token"),
    ];
    for (k, (class, family)) in tokens.iter().chain(OPEN_CLASSES).enumerate() {
        let b = &stated[k];
        // SAFETY: each `AbiStr` names a `'static` string of `len` bytes.
        let (c, f) = unsafe {
            (
                std::slice::from_raw_parts(b.class.ptr(), b.class.len()),
                std::slice::from_raw_parts(b.family.ptr(), b.family.len()),
            )
        };
        assert_eq!(
            (c, f),
            (class.as_bytes(), family.as_bytes()),
            "tail class {k}"
        );
    }

    assert_eq!(TAIL.fee_units_len(), 1);
    let fee = unsafe { TAIL.fee_units() }[0];
    // SAFETY: a `'static` str the door states (abi_str).
    let fee = unsafe { std::slice::from_raw_parts(fee.ptr(), fee.len()) };
    assert_eq!(fee, busbar_contract::plane::PER_REQUEST.as_bytes());
    assert_eq!(
        busbar_plane_llm::plane_door::FEE_CLASS_INDEX as usize,
        TAIL.billable_classes_len() - 1
    );
}

#[test]
fn the_tail_prices_cheapest_with_the_default_input_output_blend() {
    use busbar_contract::abi::plane::check::check_route_cost;
    use busbar_contract::records::{UNIT_INPUT, UNIT_OUTPUT};

    const TOKEN_BLEND_WEIGHT: f64 = 0.5;
    const NANO_UNITS_PER_MICRO_UNIT: f64 = 1_000.0;

    let terms = unsafe { TAIL.route_cost() };
    let classes = unsafe { TAIL.billable_classes() };
    assert_eq!(check_route_cost(terms, classes.len() as u64), Ok(()));
    let recipe: Vec<(&str, f64)> = terms
        .iter()
        .map(|term| {
            let class = classes[term.class as usize].class;
            // SAFETY: the tail's class names are static strings.
            let name = unsafe { std::slice::from_raw_parts(class.ptr(), class.len()) };
            (std::str::from_utf8(name).expect("class name"), term.weight)
        })
        .collect();
    assert_eq!(
        recipe,
        [
            (UNIT_INPUT, TOKEN_BLEND_WEIGHT),
            (UNIT_OUTPUT, TOKEN_BLEND_WEIGHT)
        ]
    );

    let class_index = |name: &str| {
        classes
            .iter()
            .position(|class| {
                // SAFETY: each declared class name is a static string in this tail.
                let bytes =
                    unsafe { std::slice::from_raw_parts(class.class.ptr(), class.class.len()) };
                bytes == name.as_bytes()
            })
            .expect("declared token class")
    };

    let cases = [
        (0_u64, 0_u64, 0.0),
        (1, 3, 0.002),
        (1_000, 9_000, 5.0),
        (5_000, 5_000, 5.0),
        (9_000, 1_000, 5.0),
        (2_000, 20_000, 11.0),
        (20_000, 2_000, 11.0),
    ];
    let mut previous = 0.0;
    for (input, output, historical_per_mtok) in cases {
        let mut prices = vec![u64::MAX; classes.len()];
        prices[class_index(UNIT_INPUT)] = input;
        prices[class_index(UNIT_OUTPUT)] = output;
        let cost: f64 = terms
            .iter()
            .map(|term| prices[term.class as usize] as f64 * term.weight)
            .sum();

        assert_eq!(cost / NANO_UNITS_PER_MICRO_UNIT, historical_per_mtok);
        assert!(
            cost >= previous,
            "the declared blend preserves ordering for these exact nano-unit cases"
        );
        previous = cost;
    }
}

/// Every dialect states its default outbound style in the tail, and every

#[test]
fn every_dialects_default_style_is_stated_and_every_style_has_its_one_outbound_need() {
    // SAFETY: NEEDS and every nested string/header list belong to this plane's static
    // declaration and remain initialized and readable throughout the check.
    assert_eq!(unsafe { check_needs(NEEDS) }, Ok(()));
    assert_eq!(STATEMENT.needs_len(), NEEDS.len());
    assert_eq!(TAIL.dialect_auth_len(), DIALECTS.len());
    for (i, d) in DIALECTS.iter().enumerate() {
        let stated = unsafe { TAIL.dialect_auth() }[i];
        assert_eq!(stated.dialect as usize, i);
        // SAFETY: a `'static` str the door states (abi_str).
        let style = unsafe { std::slice::from_raw_parts(stated.style.ptr(), stated.style.len()) };
        assert_eq!(style, d.egress_style.as_bytes(), "{}", d.name);
        assert_eq!(
            EGRESS_STYLES
                .iter()
                .filter(|s| **s == d.egress_style)
                .count(),
            1,
            "{}",
            d.name
        );
    }
    for style in [
        "api-key",
        "bearer",
        "jwt-bearer",
        "oauth-client-credentials",
    ] {
        assert!(EGRESS_STYLES.contains(&style), "{style}");
    }
    assert_eq!(NEEDS.len(), EGRESS_STYLES.len());
}

#[test]
fn the_settings_read_as_the_tables_and_a_broken_section_is_refused() {
    assert!(read_settings(b"").is_ok(), "an empty blob is no section");
    let one =
        br#"{"providers":{"ant":{"protocol":"anthropic","base_url":"https://anthropic.example"}},
        "models":{"claude":{"provider":"ant"}},"pools":{"p":{"members":["claude"]}}}"#;
    let shaping = read_settings(one).expect("a well-formed generation reads");
    assert!(shaping.lane("claude").is_some());
    assert!(read_settings(b"not json").is_err());
    assert!(read_settings(br#"{"providers":7}"#).is_err());
}

#[test]
fn counts_are_the_far_ends_tokens_in_the_tails_class_order_and_nothing_before_one_is_reported() {
    assert!(counts(&Units::default()).is_empty());
    let units = Units {
        tokens_in: 7,
        tokens_out: 3,
        cache_read: 2,
        cache_write: 1,
        ..Units::default()
    };
    let got: Vec<(u32, u64)> = counts(&units).iter().map(|u| (u.class, u.amount)).collect();
    assert_eq!(got, vec![(0, 7), (1, 3), (2, 2), (3, 1)]);
    assert!(counts(&units).iter().all(|u| u.source == UNITS_REPORTED));
}

/// The claim a POST on `path` meets: the exact claim on it, else the prefix claim with the longest

fn claim_for(path: &str) -> busbar_contract::abi::sdk::publish::ClaimSpec {
    use busbar_contract::abi::plane::CLAIM_EXACT;
    let all = claims();
    let post = all.iter().filter(|c| c.verb == "POST");
    let exact = post
        .clone()
        .find(|c| c.flags & CLAIM_EXACT != 0 && c.target == path);
    let under = |c: &&busbar_contract::abi::sdk::publish::ClaimSpec| {
        let t = c.target.trim_end_matches('/');
        c.flags & CLAIM_EXACT == 0 && (path == c.target || path.starts_with(&format!("{t}/")))
    };
    exact
        .or_else(|| post.filter(under).max_by_key(|c| c.target.len()))
        .cloned()
        .unwrap_or_else(|| panic!("no claim takes POST {path}"))
}

fn dialect_of(claim: &busbar_contract::abi::sdk::publish::ClaimSpec) -> &'static str {
    DIALECTS[usize::from(claim.refusal_dialect)].name
}

/// THE PLANE STATES ITS PATHS AS CLAIMS: each dialect's own path is a

#[test]
fn every_dialects_path_is_a_claim_wearing_its_dialect() {
    use busbar_plane_llm::exchange::arrive::envelope_for;
    for path in [
        "/v1/messages",
        "/v1/chat/completions",
        "/v1/responses",
        "/v2/chat",
        "/v1beta/models/gemini-pro:generateContent",
        "/model/m/converse",
        "/model/m/converse-stream",
    ] {
        let claim = claim_for(path);
        assert_eq!(dialect_of(&claim), envelope_for(path), "POST {path}");
    }
}

/// THE PREVIOUS RELEASE'S FALLBACK IS A PREFIX CLAIM ON `/`, per verb, so a path no dialect names, at

#[test]
fn the_fallback_is_a_prefix_claim_on_the_root_for_every_verb() {
    use busbar_contract::abi::plane::{CLAIM_EXACT, CLAIM_OPEN};
    let all = claims();
    for verb in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
        let root = all
            .iter()
            .find(|c| c.verb == verb && c.target == "/")
            .unwrap_or_else(|| panic!("no fallback claim for {verb}"));
        assert_eq!(root.flags & (CLAIM_EXACT | CLAIM_OPEN), 0, "{verb} /");
        assert_eq!(
            dialect_of(root),
            busbar_plane_llm::exchange::arrive::envelope_for("/"),
            "{verb} /"
        );
    }
    for path in ["/definitely/unknown", "/x/v1/chat/completions", "/whatever"] {
        assert_eq!(claim_for(path).target, "/", "POST {path}");
    }

    assert!(all.iter().all(|c| c.flags & CLAIM_OPEN == 0));

    assert!(all.iter().all(|c| {
        let entry = c.flags & busbar_contract::abi::plane::CLAIM_ENTRY != 0;
        let named = if entry {
            busbar_contract::abi::plane::check::entry_fact(&c.target).is_some()
        } else {
            c.target.starts_with('/')
        };
        !c.verb.is_empty() && named && c.carrier == "http"
    }));
}

/// RED for the `$` G3 commit: an open count the far end reported reaches the kernel in its

#[test]
fn an_open_count_is_reported_in_its_declared_class_and_never_dropped() {
    let open = Units {
        open: std::collections::BTreeMap::from([("search_units".to_string(), 12)]),
        ..Units::default()
    };
    let got: Vec<(u32, u64, bool)> = counts(&open)
        .iter()
        .map(|u| (u.class, u.amount, u.source == UNITS_REPORTED))
        .collect();
    assert_eq!(got, vec![(4, 12, true)], "no token counts, one open count");
    let both = Units {
        tokens_in: 5,
        ..open.clone()
    };
    let classes: Vec<u32> = counts(&both).iter().map(|u| u.class).collect();
    assert_eq!(classes, vec![0, 1, 2, 3, 4]);
    let unknown = Units {
        open: std::collections::BTreeMap::from([("nothing_stated".to_string(), 3)]),
        ..Units::default()
    };
    assert!(
        counts(&unknown).is_empty(),
        "a class the plane does not state is warned, never counted under another class"
    );
}

/// THE PROTOCOL AXIS a composition root links (`busbar_plane_llm::plane_door::PROTOCOLS`, named by

#[test]
fn the_door_states_its_six_protocols_for_the_root_to_link() {
    let names: Vec<&str> = busbar_plane_llm::plane_door::PROTOCOLS
        .iter()
        .map(|d| d.name)
        .collect();
    assert_eq!(names.len(), 6, "{names:?}");
    let mut unique = names.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(unique.len(), 6, "each dialect once: {names:?}");
}

/// RED for the keep mode: every need relays the far end's whole head but the governed fields, and

#[test]
fn every_need_keeps_all_but_every_dialects_governed_response_fields() {
    use busbar_contract::abi::host::conn::connector::KEEP_ALL_EXCEPT_DENIED;
    let mut governed: Vec<&str> = DIALECTS
        .iter()
        .flat_map(|d| d.governed_response_headers.iter().copied())
        .chain(
            busbar_plane_llm::dialect::ACCOUNT_RESPONSE_HEADERS
                .iter()
                .copied(),
        )
        .collect();
    governed.sort_unstable();
    governed.dedup();
    let mut denied = DENY_RESPONSE_HEADERS.to_vec();
    denied.sort_unstable();
    assert_eq!(denied, governed);
    for n in NEEDS {
        assert_eq!(n.keep_mode, KEEP_ALL_EXCEPT_DENIED);
        assert_eq!(n.keep_response_headers_len(), 0);
        assert_eq!(n.deny_response_headers_len(), DENY_RESPONSE_HEADERS.len());
    }
}

/// RED: no dialect's tenant- or credential-derived response

#[test]
fn red_no_dialects_governed_response_field_leaks_through_a_need() {
    use busbar_contract::abi::host::conn::connector::keeps_response_field;
    let none: [&[u8]; 0] = [];
    for n in NEEDS {
        let denied: Vec<&str> = if n.deny_response_headers_len() == 0 {
            Vec::new()
        } else {
            let list = unsafe { n.deny_response_headers() };
            list.iter()
                .map(|s| {
                    // SAFETY: each entry is a `'static` str the door states (abi_str).
                    let bytes = unsafe { std::slice::from_raw_parts(s.ptr(), s.len()) };
                    std::str::from_utf8(bytes).expect("a field name is UTF-8")
                })
                .collect()
        };
        for d in DIALECTS {
            for governed in d.governed_response_headers {
                assert!(
                    !keeps_response_field(n.keep_mode, &[], &denied, governed, none),
                    "{} leaks `{governed}`",
                    d.name
                );
            }
        }
        for relayed in ["request-id", "x-amzn-requestid"] {
            assert!(
                keeps_response_field(n.keep_mode, &[], &denied, relayed, none),
                "`{relayed}` is the upstream's and crosses (F2)"
            );
        }

        for cookie in ["set-cookie", "set-cookie2"] {
            assert!(
                !keeps_response_field(n.keep_mode, &[], &denied, cookie, none),
                "`{cookie}` is the operator's connection state and is stripped"
            );
        }
    }
}

/// THE ROUTE AN ARRIVAL NAMES: its model, verbatim, as a pool

#[test]
fn an_arrival_routes_over_its_model_a_pool_first_then_a_direct_entry() {
    use busbar_contract::abi::plane::{ROUTE_DIRECT, ROUTE_POOL};
    let shaping = read_settings(
        br#"{"providers":{"ant":{"protocol":"anthropic","base_url":"https://anthropic.example"}},
        "models":{"claude":{"provider":"ant"},"both":{"provider":"ant"}},
        "pools":{"p":{"members":["claude"]},"both":{"members":["claude"]}}}"#,
    )
    .expect("a well-formed generation reads");
    assert_eq!(route_of(&shaping, "p"), (ROUTE_POOL, "p"));
    assert_eq!(route_of(&shaping, "claude"), (ROUTE_DIRECT, "claude"));
    assert_eq!(route_of(&shaping, "both"), (ROUTE_POOL, "both"));
    assert_eq!(route_of(&shaping, "nope"), (ROUTE_DIRECT, "nope"));
}

/// THE STICKY-ROUTING KEY, as 1.5.5 derived it: the pool's

#[test]
fn the_sticky_key_is_the_pools_header_else_the_chat_bodys_system() {
    use busbar_plane_llm::plane_door::affinity_key;
    let shaping = read_settings(
        br#"{"providers":{"ant":{"protocol":"anthropic","base_url":"https://anthropic.example"}},
        "models":{"claude":{"provider":"ant"}},
        "pools":{"p":{"members":["claude"]},
                 "u":{"members":["claude"],"affinity":{"mode":"session","header_name":"x-user-id"}}}}"#,
    )
    .expect("a well-formed generation reads");
    assert_eq!(shaping.affinity_header("p"), "x-session-id");
    assert_eq!(shaping.affinity_header("u"), "x-user-id");
    let chat = busbar_plane_llm::codec::DECLS
        .iter()
        .find(|d| d.name == "anthropic")
        .and_then(|d| d.handler)
        .and_then(|h| h.operation_handler(busbar_contract::operation::OpVerb::CHAT));
    let body = serde_json::json!({"model": "p", "system": "be brief", "messages": []});
    let session: &[(&[u8], &[u8])] = &[(b"X-Session-Id", b"s-1")];
    assert_eq!(
        affinity_key("x-session-id", session, chat, Some(&body)),
        Some("s-1".to_string())
    );
    assert_eq!(
        affinity_key("x-user-id", session, chat, Some(&body)),
        Some("be brief".to_string()),
        "another pool's header is not this pool's"
    );
    let unreadable: &[(&[u8], &[u8])] = &[(b"x-session-id", b"s\x01")];
    assert_eq!(
        affinity_key("x-session-id", unreadable, chat, Some(&body)),
        Some("be brief".to_string())
    );
    let plain = serde_json::json!({"model": "p", "system": "", "messages": []});
    assert_eq!(affinity_key("x-session-id", &[], chat, Some(&plain)), None);
    assert_eq!(affinity_key("x-session-id", &[], None, Some(&body)), None);
}

/// EVERY DECLARED OPEN CLASS REACHES THE DURABLE BOOK (owner LEDGER-100): one count of each open

#[test]
fn every_open_class_becomes_one_reported_count_at_its_tail_index() {
    use busbar_contract::abi::plane::UNITS_REPORTED;
    for (k, (class, _)) in OPEN_CLASSES.iter().enumerate() {
        let units = busbar_plane_llm::exchange::reply::Units {
            open: std::collections::BTreeMap::from([(class.to_string(), 7)]),
            ..Default::default()
        };
        let out = counts(&units);
        assert_eq!(out.len(), 1, "{class}: {out:?}");
        assert_eq!(
            (out[0].class, out[0].source, out[0].amount),
            (u32::try_from(4 + k).expect("small"), UNITS_REPORTED, 7),
            "{class}"
        );
    }
}

/// The declaration of the dialect named `name`, from the one table the plane serves through.
fn decl_named(name: &str) -> Option<&'static busbar_contract::protocol::ProtocolDecl> {
    busbar_plane_llm::codec::DECLS
        .iter()
        .copied()
        .find(|d| d.name == name)
}

/// The six dialects 1.5.5 shipped, by the names its registry keyed them under.
const SHIPPED_DIALECTS: [&str; 6] = [
    "openai",
    "anthropic",
    "gemini",
    "bedrock",
    "cohere",
    "responses",
];

/// LEGACY `crates/busbar-llm/src/engine/tests/dialect_registry_facts_tests.rs::registry_resolves_openai_and_its_moderation_handler`

#[test]
fn registry_resolves_openai_and_its_moderation_handler() {
    use busbar_contract::operation::OpVerb;
    let handler = decl_named("openai")
        .and_then(|d| d.handler)
        .expect("the shipped protocol's handler is registered");
    assert_eq!(handler.protocol_name(), "openai");
    assert!(handler.operation_handler(OpVerb::MODERATION).is_some());
    assert!(
        decl_named("zzz-unknown").is_none(),
        "unknown protocol -> None"
    );
}

/// LEGACY `crates/busbar-llm/src/engine/tests/dialect_registry_facts_tests.rs::every_protocol_serves_chat_via_its_request_handler`

#[test]
fn every_protocol_serves_chat_via_its_request_handler() {
    use busbar_contract::operation::OpVerb;
    for proto in SHIPPED_DIALECTS {
        let handler = decl_named(proto)
            .and_then(|d| d.handler)
            .unwrap_or_else(|| panic!("{proto}: protocol registered"));
        assert_eq!(handler.protocol_name(), proto);
        assert!(
            handler.operation_handler(OpVerb::CHAT).is_some(),
            "{proto} must serve chat via operation_handler(Chat)"
        );
    }
    let mut stated: Vec<&str> = DIALECTS.iter().map(|d| d.name).collect();
    stated.sort_unstable();
    let mut shipped = SHIPPED_DIALECTS.to_vec();
    shipped.sort_unstable();
    assert_eq!(
        stated, shipped,
        "the plane states exactly the six shipped dialects"
    );
}

/// LEGACY `crates/busbar-llm/src/engine/tests/dialect_registry_facts_tests.rs::chat_declares_its_capabilities`

#[test]
fn chat_declares_its_capabilities() {
    use busbar_contract::operation::OpVerb;
    let chat = decl_named("openai")
        .and_then(|d| d.handler)
        .and_then(|h| h.operation_handler(OpVerb::CHAT))
        .expect("the shipped protocol serves chat");
    assert!(chat.streaming(), "chat streams");
    assert!(chat.taps_usage(), "chat bills tokens from the body");
    assert!(
        chat.wants_stream(&serde_json::json!({"stream": true})),
        "chat reads the stream boolean"
    );
    assert!(!chat.wants_stream(&serde_json::json!({})));
    assert!(!chat.wants_stream(&serde_json::json!({"stream": false})));
    assert_eq!(
        chat.body_affinity_key(&serde_json::json!({"system": "you are helpful"})),
        Some("you are helpful")
    );
    assert_eq!(
        chat.body_affinity_key(&serde_json::json!({"system": ""})),
        None
    );
}

/// LEGACY `crates/busbar-llm/src/engine/tests/ingress_integration_tests.rs::test_affinity_header_session_mode_without_name_uses_default`

#[test]
fn test_affinity_header_session_mode_without_name_uses_default() {
    use busbar_plane_llm::plane_door::affinity_key;
    let shaping = read_settings(
        br#"{"providers":{"ant":{"protocol":"anthropic","base_url":"https://anthropic.example"}},
        "models":{"claude":{"provider":"ant"}},
        "pools":{"p":{"members":["claude"],"affinity":{"mode":"session"}}}}"#,
    )
    .expect("a session-mode pool with no header name reads");
    assert_eq!(shaping.affinity_header("p"), "x-session-id");
    let session: &[(&[u8], &[u8])] = &[(b"x-session-id", b"s-7")];
    assert_eq!(
        affinity_key(shaping.affinity_header("p"), session, None, None),
        Some("s-7".to_string()),
        "the default header is the one the pool keys on"
    );
}

/// A `'static` string the door states, as its bytes.
fn abi_text(s: busbar_contract::abi::mechanism::call::AbiStr) -> &'static [u8] {
    // SAFETY: every `AbiStr` the door states names a `'static` string of `len` bytes.
    unsafe { std::slice::from_raw_parts(s.ptr(), s.len()) }
}

/// LEGACY `crates/busbar-llm/src/tests/plane_decl_identity_tests.rs::the_plane_declares_its_published_key_and_is_the_fallback`

#[test]
fn the_plane_declares_its_published_key_and_is_the_fallback() {
    use busbar_contract::abi::mechanism::door::SECTION_DECLARING;
    assert_eq!(abi_text(STATEMENT.name), b"llm");
    assert_ne!(
        TAIL.flags & TAIL_FALLBACK,
        0,
        "the fallback catch-all every unclaimed path falls through to"
    );
    assert_eq!(abi_text(TAIL.scope), b"pools");
    let statement = STATEMENT;
    let sections = unsafe { statement.sections() };
    let declaring: Vec<&[u8]> = sections
        .iter()
        .filter(|s| s.flags & SECTION_DECLARING != 0)
        .map(|s| abi_text(s.name))
        .collect();
    assert_eq!(declaring, vec![b"pools".as_slice()]);
    let kinds = unsafe { TAIL.scope_kinds() };
    let kinds: Vec<&[u8]> = kinds.iter().map(|k| abi_text(*k)).collect();
    assert_eq!(kinds, vec![b"pool".as_slice()]);
}

/// EVERY WAY A UNIT ENDS LEAVES NOTHING OF IT IN THE DOOR'S BOOK. The `refusal` of a

#[test]
fn every_way_a_unit_ends_leaves_nothing_in_the_doors_book() {
    use busbar_contract::abi::mechanism::ticket::Ticket;
    use busbar_plane_llm::plane_door::UnitBook;
    let t = |slot| Ticket {
        slot,
        generation: 1,
    };
    let mut book = UnitBook::<&str>::default();

    book.hold(7, "refused");
    let unit = book.take(7).expect("arrived");
    book.bind(t(1), 7);
    book.hold(7, unit);
    assert_eq!(book.held(), (1, 1));
    let _rendered = book.take(7);
    book.ends(7);
    assert_eq!(book.held(), (0, 0), "a refused unit keeps no ticket");

    book.hold(8, "served");
    let _unit = book.take(8);
    book.bind(t(2), 8);
    book.ends(8);
    assert_eq!(book.held(), (0, 0), "a served unit keeps nothing");

    book.hold(9, "cancelled");
    let unit = book.take(9).expect("arrived");
    book.bind(t(3), 9);
    book.hold(9, unit);
    assert_eq!(
        book.cancel(UnitCancel {
            ticket: t(3),
            unit: 0
        }),
        Some("cancelled")
    );
    assert_eq!(book.held(), (0, 0), "a cancelled unit keeps nothing");

    book.hold(10, "moved");
    book.bind(t(4), 10);
    book.bind(t(5), 10);
    assert_eq!(book.held(), (1, 1), "the old ticket is unbound");
    assert_eq!(
        book.cancel(UnitCancel {
            ticket: t(4),
            unit: 0
        }),
        None
    );
    book.ends(10);
    assert_eq!(book.held(), (0, 0));

    book.hold(11, "unbound");
    assert_eq!(
        book.cancel(UnitCancel {
            ticket: t(6),
            unit: 11
        }),
        Some("unbound")
    );
    assert_eq!(
        book.held(),
        (0, 0),
        "a unit cancelled before its first piece keeps nothing"
    );

    book.hold(12, "bound");
    let unit = book.take(12).expect("arrived");
    book.bind(t(7), 12);
    book.hold(12, unit);
    assert_eq!(
        book.cancel(UnitCancel {
            ticket: t(8),
            unit: 12
        }),
        Some("bound")
    );
    assert_eq!(book.held(), (0, 0), "its ticket goes with it");
}

/// THE SIGNED PATHS ARE THE PREVIOUS RELEASE'S: the raw-affix claims the door flags `CLAIM_SIGNED_INGRESS`, matched against the raw

#[test]
fn the_signed_claims_name_the_paths_the_previous_release_signed() {
    use busbar_contract::abi::plane::check::raw_affix_matches;
    use busbar_contract::abi::plane::{CLAIM_RAW_AFFIX, CLAIM_SIGNED_INGRESS};
    let signed: Vec<_> = claims()
        .into_iter()
        .filter(|c| c.flags & CLAIM_SIGNED_INGRESS != 0)
        .collect();
    assert!(
        signed.iter().all(|c| c.flags & CLAIM_RAW_AFFIX != 0),
        "every signed claim is a raw affix"
    );
    assert!(!signed.is_empty(), "the door flags its signed paths");
    let rule = busbar_plane_llm::codec::DECLS
        .iter()
        .find(|d| d.uses_sigv4_ingress_auth())
        .and_then(|d| d.residual_claims)
        .expect("the signing dialect states its path rule");
    let arn = "arn:aws:bedrock:us-east-1:123456789012:inference-profile/us.m-v1:0";
    let encoded =
        "arn%3Aaws%3Abedrock%3Aus-east-1%3A123456789012%3Ainference-profile%2Fus.m-v1%3A0";
    for path in [
        "/model/m/converse".to_string(),
        "/model/m/converse-stream".to_string(),
        format!("/model/{arn}/converse"),
        format!("/model/{arn}/converse-stream"),
        format!("/model/{encoded}/converse"),
        format!("/model/{encoded}/converse-stream"),
        "/model/m/invoke".to_string(),
        "/model/m/invoke-with-response-stream".to_string(),
        format!("/model/{arn}/invoke"),
        "/v1/messages".to_string(),
        "/v1/chat/completions".to_string(),
        "/v1beta/models/m:generateContent".to_string(),
        "/x/model/m/converse".to_string(),
        "/model/converse".to_string(),
        "/model/converse-stream".to_string(),
        "/model//converse".to_string(),
        "/model/m/converse/".to_string(),
        "//model/m/converse".to_string(),
    ] {
        let by_claims = signed.iter().any(|c| raw_affix_matches(&c.target, &path));
        assert_eq!(by_claims, rule(&path).is_some(), "{path}");
    }

    for (path, was_signed) in [
        ("/model/converse", true),
        ("/model/converse-stream", true),
        ("/model//converse", true),
        ("/model/m/converse/", false),
        ("//model/m/converse", false),
    ] {
        let by_claims = signed.iter().any(|c| raw_affix_matches(&c.target, path));
        assert_eq!(by_claims, was_signed, "{path}");
    }
}

/// RED (CREDENTIAL-LOCKDOWN H8): AN AFFINITY HEADER NEVER NAMES A CREDENTIAL. A pool whose

#[test]
fn red_an_affinity_header_naming_a_credential_is_refused() {
    let settings = |header: &str| {
        format!(
            r#"{{"providers":{{"ant":{{"protocol":"anthropic","base_url":"https://anthropic.example"}}}},
            "models":{{"claude":{{"provider":"ant"}}}},
            "pools":{{"u":{{"members":["claude"],"affinity":{{"mode":"session","header_name":"{header}"}}}}}}}}"#
        )
    };
    let mut credentials: Vec<&str> = DIALECTS
        .iter()
        .flat_map(|d| d.governed_headers.iter().copied())
        .collect();
    credentials.extend(busbar_contract::abi::host::conn::connector::NEVER_KEPT);
    credentials.extend([
        "Authorization",
        "COOKIE",
        "Proxy-Authorization",
        "X-Api-Key",
    ]);
    for header in credentials {
        let refused = read_settings(settings(header).as_bytes());
        assert!(
            refused.is_err(),
            "affinity.header_name `{header}` was accepted"
        );
    }
    let ok = read_settings(settings("x-user-id").as_bytes()).expect("an ordinary header reads");
    assert_eq!(ok.affinity_header("u"), "x-user-id");
}
