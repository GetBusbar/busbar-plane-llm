// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::*;
use busbar_contract::abi::mechanism::{
    call::InHead,
    lifecycle::{slot, OpenIn, OpenOut},
};
use busbar_contract::abi::sdk::door::Entry;

slot!(NativeTestOpen, OpenIn, OpenOut, |instance, input, _out| {
    let shaping = Shaping::from_settings(&serde_json::json!({
        "providers":{"google":{"protocol":"gemini"}},
        "models":{"entry":{"provider":"google","upstream_model":"gemini-pinned"}}
    }))
    .unwrap();
    let native = if input.settings().bytes() == b"enabled" {
        native::config(br#"{"interactions":{"models":{"entry":{"account_namespace":"account-a","pricing_modes":["standard"]}}}}"#,&shaping).unwrap()
    } else {
        Default::default()
    };
    instance.open(LlmDoor {
        leases: Default::default(),
        native,
        generations: Generations::new(),
        shapings: Mutex::new(BTreeMap::from([(1, Arc::new(shaping))])),
        units: Mutex::new(UnitBook::default()),
        services: None,
        webhooks: Webhooks::default(),
    });
    Outcome::Ready
});
slot!(NativeTestClose, InHead, OutHead, |_, _, _| {
    Outcome::Ready
});
struct InstanceGuard(*mut std::ffi::c_void);
impl InstanceGuard {
    fn open(enabled: bool) -> Self {
        // SAFETY: zero-valid ABI records, static settings, no prior instance.
        unsafe {
            let mut input: OpenIn = std::mem::zeroed();
            input.settings = Blob::of(if enabled { b"enabled" } else { b"disabled" });
            let mut out: OpenOut = std::mem::zeroed();
            assert_eq!(
                Safe::<NativeTestOpen>::enter(std::ptr::null_mut(), slot::OPEN, &input, &mut out),
                Outcome::Ready
            );
            Self(out.instance)
        }
    }
    fn arrive(
        &self,
        method: &'static str,
        target: &'static str,
        body: &[u8],
    ) -> (Outcome, ArriveOut) {
        // SAFETY: all strings/body remain borrowed across the synchronous call.
        unsafe {
            let mut input: ArriveIn = std::mem::zeroed();
            input.unit = 1;
            input.method = abi_str(method);
            input.target = abi_str(target);
            input.body = Blob::from_raw_parts(body.as_ptr(), body.len(), 0, 0);
            let mut out: ArriveOut = std::mem::zeroed();
            let outcome = Safe::<Arrive>::enter(
                self.0,
                busbar_contract::abi::plane::slot::ARRIVE,
                &input,
                &mut out,
            );
            (outcome, out)
        }
    }
}
impl Drop for InstanceGuard {
    fn drop(&mut self) {
        // SAFETY: close our one SDK instance once, after every outstanding call returned.
        unsafe {
            let input: InHead = std::mem::zeroed();
            let mut out: OutHead = std::mem::zeroed();
            assert_eq!(
                Safe::<NativeTestClose>::enter(self.0, slot::CLOSE, &input, &mut out),
                Outcome::Ready
            );
        }
    }
}
#[test]
fn native_claims_are_optional_and_create_is_at_most_once() {
    assert!(native::claims(false).is_empty());
    let disabled = InstanceGuard::open(false);
    assert_eq!(
        disabled
            .arrive(
                "POST",
                "/v1beta/interactions",
                br#"{"model":"entry","input":"hi"}"#
            )
            .0,
        Outcome::Refused
    );
    let enabled = InstanceGuard::open(true);
    let (outcome, out) = enabled.arrive(
        "POST",
        "/v1beta/interactions",
        br#"{"model":"entry","input":"hi"}"#,
    );
    assert_eq!(outcome, Outcome::Ready);
    assert_eq!(out.route, ROUTE_DIRECT);
    assert_ne!(out.route_flags & busbar_contract::abi::plane::ROUTE_ONCE, 0);
    assert_eq!(out.principal_need, PRINCIPAL_REQUIRED);
}
#[test]
fn stored_and_continuation_paths_require_authenticated_route_resolution() {
    for (method, target, body) in [
        ("GET", "/v1beta/interactions/provider-id", b"".as_slice()),
        ("DELETE", "/v1beta/interactions/provider-id", b"".as_slice()),
        (
            "POST",
            "/v1beta/interactions/provider-id/cancel",
            b"".as_slice(),
        ),
        (
            "POST",
            "/v1beta/interactions",
            br#"{"model":"entry","input":"next","previous_interaction_id":"provider-id"}"#
                .as_slice(),
        ),
    ] {
        let enabled = InstanceGuard::open(true);
        let (outcome, out) = enabled.arrive(method, target, body);
        assert_eq!(outcome, Outcome::Ready);
        assert_eq!(out.route, busbar_contract::abi::plane::ROUTE_AUTHENTICATED);
        // No pool/destination is guessed from the opaque provider ID.
        assert_eq!(out.pool.len, 0);
    }
}
#[test]
fn missing_runtime_capability_never_emits_paid_request_head_or_tokens() {
    let instance = InstanceGuard::open(true);
    assert_eq!(
        instance
            .arrive(
                "POST",
                "/v1beta/interactions",
                br#"{"model":"entry","input":"hi"}"#
            )
            .0,
        Outcome::Ready
    );
    let mut bytes = [0; 4096];
    let mut arena = [0; 4096];
    // SAFETY: zero-valid ABI rows and live non-overlapping buffers for the entire call.
    unsafe {
        let mut fields: [OutField; 4] = std::mem::zeroed();
        let mut units: [UnitCount; 4] = std::mem::zeroed();
        let mut input: OnPieceIn = std::mem::zeroed();
        input.unit = 1;
        input.from = FROM_KERNEL;
        input.member = abi_str("entry");
        input.attempt_no = 1;
        input.reply_buf = bytes.as_mut_ptr();
        input.reply_cap = bytes.len();
        input.arena_buf = arena.as_mut_ptr();
        input.arena_cap = arena.len();
        input.fields_buf = fields.as_mut_ptr();
        input.fields_cap = fields.len();
        input.units_buf = units.as_mut_ptr();
        input.units_cap = units.len();
        let mut out: OnPieceOut = std::mem::zeroed();
        assert_eq!(
            Safe::<OnPiece>::enter(
                instance.0,
                busbar_contract::abi::plane::slot::ON_PIECE,
                &input,
                &mut out
            ),
            Outcome::Ready
        );
        assert_eq!(out.reply_status, 503);
        assert_eq!(out.flags & EMIT_TO_FAR_END, 0);
        assert_eq!(out.verb.len, 0);
        assert_eq!(out.target.len, 0);
        assert_eq!(out.units_written, 0);
        assert_eq!(out.verdict, VERDICT_HARD);
    }
}

#[test]
fn native_forwarding_preserves_framing_and_configured_account_headers() {
    let shaping=Shaping::from_settings(&serde_json::json!({"providers":{"google":{"protocol":"gemini"}},"models":{"entry":{"provider":"google"}}})).unwrap();
    let own = vec![
        ("accept".into(), b"application/json".to_vec()),
        ("content-type".into(), b"application/json".to_vec()),
        ("x-goog-user-project".into(), b"configured-project".to_vec()),
    ];
    let fields = attempt::native_forward_fields(
        &shaping.lanes["entry"],
        own,
        &[
            (b"authorization", b"Bearer caller-secret"),
            (b"x-goog-api-key", b"caller-key"),
            (b"accept", b"text/event-stream"),
            (b"content-type", b"text/plain"),
            (b"x-goog-user-project", b"other-billing-project"),
            (b"x-client-trace", b"trace"),
        ],
    );
    assert!(!fields
        .iter()
        .any(|(k, _)| k == "authorization" || k == "x-goog-api-key"));
    assert!(fields.contains(&("x-goog-user-project".into(), b"configured-project".to_vec())));
    assert!(fields.contains(&("accept".into(), b"application/json".to_vec())));
    assert!(fields.contains(&("content-type".into(), b"application/json".to_vec())));
    assert!(fields.contains(&("x-client-trace".into(), b"trace".to_vec())));
}
