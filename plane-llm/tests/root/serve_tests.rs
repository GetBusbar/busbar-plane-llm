// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use crate::root::serve::door_tests::*;
use busbar_kernel::governance::NewKeySpec;

#[cfg(linked_driver_proofs)]
#[derive(Clone)]
enum ProviderReply {
    Ok {
        status: axum::http::StatusCode,
        body: serde_json::Value,
    },
    ServerError {
        status: axum::http::StatusCode,
        body: serde_json::Value,
    },
    RateLimit {
        status: axum::http::StatusCode,
        provider_signal: Option<&'static str>,
        retry_after: Option<u64>,
    },
    Billing {
        status: axum::http::StatusCode,
        code: &'static str,
        message: &'static str,
    },
    Auth {
        status: axum::http::StatusCode,
    },
    Sse {
        events: Vec<String>,
        abort_at_index: Option<usize>,
    },
    SseTransportError {
        ok_events: Vec<String>,
    },
    Gated {
        status: axum::http::StatusCode,
        body: serde_json::Value,
        started: std::sync::Arc<tokio::sync::Notify>,
        release: std::sync::Arc<tokio::sync::Notify>,
    },
}

#[cfg(linked_driver_proofs)]
impl Default for ProviderReply {
    fn default() -> Self {
        Self::Ok {
            status: axum::http::StatusCode::OK,
            body: serde_json::json!({"ok": true}),
        }
    }
}

#[cfg(linked_driver_proofs)]
#[derive(Default)]
struct ProviderRequests {
    replies: std::sync::Mutex<Vec<ProviderReply>>,
    last: std::sync::Mutex<Option<(axum::http::HeaderMap, Vec<u8>)>>,
}

#[cfg(linked_driver_proofs)]
impl ProviderRequests {
    fn new() -> Self {
        Self::default()
    }
    fn push(&self, reply: ProviderReply) {
        self.replies.lock().unwrap().push(reply);
    }
    fn next_response(&self) -> Option<ProviderReply> {
        self.replies.lock().unwrap().pop()
    }
    fn get_last_request_body(&self) -> Option<Vec<u8>> {
        self.last
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, body)| body.clone())
    }
    fn get_last_request_header(&self, name: &str) -> Option<String> {
        self.last
            .lock()
            .unwrap()
            .as_ref()?
            .0
            .get(name)?
            .to_str()
            .ok()
            .map(str::to_owned)
    }
}

#[cfg(linked_driver_proofs)]
struct ProviderEndpoint {
    address: std::net::SocketAddr,
    accept: tokio::task::JoinHandle<()>,
}

#[cfg(linked_driver_proofs)]
impl Drop for ProviderEndpoint {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

#[cfg(linked_driver_proofs)]
impl ProviderEndpoint {
    fn address(&self) -> std::net::SocketAddr {
        self.address
    }

    async fn new(requests: std::sync::Arc<ProviderRequests>) -> Self {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let accept = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let requests = requests.clone();
                tokio::spawn(async move {
                    let mut received = Vec::new();
                    let mut chunk = [0u8; 8192];
                    let (headers, body) = loop {
                        let count = socket.read(&mut chunk).await.unwrap();
                        if count == 0 {
                            return;
                        }
                        received.extend_from_slice(&chunk[..count]);
                        let Some(end) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n")
                        else {
                            continue;
                        };
                        let head = std::str::from_utf8(&received[..end]).unwrap();
                        let mut headers = axum::http::HeaderMap::new();
                        for line in head.lines().skip(1) {
                            let (name, value) = line.split_once(':').expect("an HTTP header");
                            headers.append(
                                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                                axum::http::HeaderValue::from_str(value.trim()).unwrap(),
                            );
                        }
                        let length = headers
                            .get("content-length")
                            .map(|value| value.to_str().unwrap().parse::<usize>().unwrap())
                            .unwrap_or(0);
                        if received.len() >= end + 4 + length {
                            break (headers, received[end + 4..end + 4 + length].to_vec());
                        }
                    };
                    *requests.last.lock().unwrap() = Some((headers, body));
                    let reply = requests.next_response().unwrap_or_default();
                    let mut extra = String::new();
                    let mut gated = None;
                    let mut transport_error = false;
                    let mut content_type = "application/json";
                    let (status, body) = match reply {
                        ProviderReply::Ok { status, body }
                        | ProviderReply::ServerError { status, body } => (status, body.to_string()),
                        ProviderReply::Auth { status } => (
                            status,
                            serde_json::json!({"error": "Unauthorized"}).to_string(),
                        ),
                        ProviderReply::Billing {
                            status,
                            code,
                            message,
                        } => (
                            status,
                            serde_json::json!({"error": {"code": code, "message": message}})
                                .to_string(),
                        ),
                        ProviderReply::RateLimit {
                            status,
                            provider_signal,
                            retry_after,
                        } => {
                            if let Some(wait) = retry_after {
                                extra = format!("retry-after: {wait}\r\n");
                            }
                            let message = if provider_signal == Some("1302") {
                                "rate_limit"
                            } else {
                                "Rate limit exceeded"
                            };
                            (status, serde_json::json!({"error": {"message": message, "code": provider_signal.unwrap_or("429")}}).to_string())
                        }
                        ProviderReply::Sse {
                            events,
                            abort_at_index,
                        } => {
                            content_type = "text/event-stream";
                            let count = abort_at_index.unwrap_or(events.len());
                            let mut body = events
                                .into_iter()
                                .take(count)
                                .map(|event| format!("data: {event}\n\n"))
                                .collect::<String>();
                            if abort_at_index.is_some() {
                                let error = serde_json::json!({"type": "error", "error": {"message": "upstream abort", "source": "upstream"}});
                                body.push_str(&format!("event: error\ndata: {error}\n\n"));
                            } else {
                                body.push_str("data: [DONE]\n\n");
                            }
                            (axum::http::StatusCode::OK, body)
                        }
                        ProviderReply::SseTransportError { ok_events } => {
                            content_type = "text/event-stream";
                            transport_error = true;
                            (
                                axum::http::StatusCode::OK,
                                ok_events
                                    .into_iter()
                                    .map(|event| format!("data: {event}\n\n"))
                                    .collect::<String>(),
                            )
                        }
                        ProviderReply::Gated {
                            status,
                            body,
                            started,
                            release,
                        } => {
                            gated = Some((started, release));
                            (status, body.to_string())
                        }
                    };
                    let length = body.len() + usize::from(transport_error);
                    let head = format!(
                        "HTTP/1.1 {} {}\r\ncontent-type: {content_type}\r\ncontent-length: {length}\r\n{extra}connection: close\r\n\r\n",
                        status.as_u16(),
                        status.canonical_reason().unwrap_or("response")
                    );
                    if socket.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    if let Some((started, release)) = gated {
                        started.notify_one();
                        release.notified().await;
                    }
                    if socket.write_all(body.as_bytes()).await.is_err() {
                        return;
                    }
                    if transport_error {
                        socket.flush().await.unwrap();
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { address, accept }
    }
}

#[cfg(linked_driver_proofs)]
use super::hook_seat_tests::{
    chunk, far_end_answering as seat_far_end, far_end_scripted, rig, word as door_word, RigOpts,
    Script as SeatScript, REWRITTEN,
};
#[cfg(linked_driver_proofs)]
use super::planes_tests::PUBLISHING as ONE_PUBLISHER;

#[cfg(linked_driver_proofs)]
const SERVED_BY_TWIN: &str = r#"{"id":"chatcmpl-2","object":"chat.completion","created":0,"model":"m1","choices":[{"index":0,"message":{"role":"assistant","content":"served-by-the-twin"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}"#;

#[cfg(linked_driver_proofs)]
const OVERLOADED: &str = r#"{"error":{"message":"overloaded","type":"server_error"}}"#;

#[cfg(linked_driver_proofs)]
const MALFORMED: &str = r#"{"error":{"message":"bad request","type":"invalid_request_error"}}"#;

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_trips_a_failing_members_breaker_cell() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-breaker-trip";
    let (down, twin) = (
        seat_far_end(503, OVERLOADED).await,
        seat_far_end(200, SERVED_BY_TWIN).await,
    );
    let rig = rig(
        instance,
        RigOpts {
            members: &[(down.port, 100), (twin.port, 1)],
            ..RigOpts::default()
        },
    )
    .await;
    let ready = || rig.app.store.ready_in("p", 0, busbar_kernel::store::now());
    assert!(ready(), "the member's cell admits before any failure");
    let (status, _, _) = rig.chat().await;
    assert_eq!(status, 200, "the twin serves");
    assert_eq!(down.served(), 1, "the failing member was dialled once");
    assert!(
        !ready(),
        "the failure recorded into the member's (pool, lane) cell and opened it"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_refuses_a_tripped_member_before_dispatch() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-breaker-fastfail";
    let (down, twin) = (
        seat_far_end(503, OVERLOADED).await,
        seat_far_end(200, SERVED_BY_TWIN).await,
    );
    let rig = rig(
        instance,
        RigOpts {
            members: &[(down.port, 100), (twin.port, 1)],
            ..RigOpts::default()
        },
    )
    .await;
    assert_eq!(rig.chat().await.0, 200);
    assert_eq!(down.served(), 1);
    let started = std::time::Instant::now();
    assert_eq!(rig.chat().await.0, 200, "the twin serves the next unit");
    assert_eq!(
        down.served(),
        1,
        "the tripped member is refused before dispatch: never dialled again"
    );
    assert_eq!(twin.served(), 2);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "refused at once, not after a timeout"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_reroutes_to_the_twin_before_the_first_byte() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-failover";
    let (down, twin) = (
        seat_far_end(503, OVERLOADED).await,
        seat_far_end(200, SERVED_BY_TWIN).await,
    );
    let rig = rig(
        instance,
        RigOpts {
            members: &[(down.port, 100), (twin.port, 1)],
            ..RigOpts::default()
        },
    )
    .await;
    let (status, _, body) = rig.chat().await;
    assert_eq!(status, 200);
    assert!(
        String::from_utf8_lossy(&body).contains("served-by-the-twin"),
        "the twin's answer reached the caller: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!((down.served(), twin.served()), (1, 1));
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_classifies_a_refused_request_as_the_callers_fault() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-disposition";
    let (refusing, twin) = (
        seat_far_end(400, MALFORMED).await,
        seat_far_end(200, SERVED_BY_TWIN).await,
    );
    let rig = rig(
        instance,
        RigOpts {
            members: &[(refusing.port, 100), (twin.port, 1)],
            ..RigOpts::default()
        },
    )
    .await;
    let (status, _, _) = rig.chat().await;
    assert_eq!(status, 400, "the far end's refusal is relayed");
    assert_eq!(
        (refusing.served(), twin.served()),
        (1, 0),
        "a caller's fault is not failed over"
    );
    assert!(
        rig.app.store.ready_in("p", 0, busbar_kernel::store::now()),
        "nor charged to the member's cell"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_injects_the_members_credential_not_the_callers() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-egress-auth";
    let far = seat_far_end(200, SERVED_BY_TWIN).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            ..RigOpts::default()
        },
    )
    .await;
    assert_eq!(rig.chat().await.0, 200);
    let seen = far.authorizations.lock().unwrap().clone();
    assert_eq!(seen, vec!["Bearer sk-seats".to_string()]);
    assert!(
        !seen[0].contains(&rig.token),
        "the caller's token never crosses"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failover_lane_presents_its_own_credential_never_the_primarys() {
    const PRIMARY_KEY: &str = "sk-primary-6c2f81";
    const FAILOVER_KEY: &str = "sk-failover-0d94e3";
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-failover-credential";
    let primary = seat_far_end(
        500,
        r#"{"error":{"message":"upstream down","type":"server_error"}}"#,
    )
    .await;
    let failover = seat_far_end(200, SERVED_BY_TWIN).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(primary.port, 100), (failover.port, 1)],
            member_keys: &[PRIMARY_KEY, FAILOVER_KEY],
            ..RigOpts::default()
        },
    )
    .await;
    let (status, _, body) = rig.chat().await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        (primary.served(), failover.served()),
        (1, 1),
        "the primary failed and the walk failed over"
    );
    assert_eq!(
        primary.authorizations.lock().unwrap().clone(),
        vec![format!("Bearer {PRIMARY_KEY}")],
        "the primary is reached with its own key"
    );
    let seen = failover.authorizations.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![format!("Bearer {FAILOVER_KEY}")],
        "the failover lane presents its own key"
    );
    assert!(
        !seen[0].contains(PRIMARY_KEY) && !seen[0].contains(&rig.token),
        "never the primary's key, never the caller's token"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_doors_traffic_appears_on_the_metrics_scrape() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-metrics";
    let pool = "t065-attempt-pool";
    let far = seat_far_end(200, SERVED_BY_TWIN).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            more_pools: &[(pool, &[(0, 1)])],
            ..RigOpts::default()
        },
    )
    .await;
    let attempts = || {
        crate::root::serve::door_tests::metric_sum(
            busbar_kernel::telemetry::UPSTREAM_ATTEMPTS_TOTAL,
            &[("pool", pool), ("lane", "m0")],
        )
    };
    let before = attempts();
    let (status, _, _) = rig
        .send(
            "POST",
            "/v1/chat/completions",
            Some(serde_json::json!({"model": pool,
                "messages": [{"role": "user", "content": "hi"}]})),
        )
        .await;
    assert_eq!(status, 200);
    let after = attempts();
    assert!(
        after > before,
        "this pool's attempt was counted: {before} -> {after}"
    );
    let scrape = crate::root::serve::metrics_fixture::render_metrics();
    assert!(
        scrape
            .lines()
            .any(|l| l.starts_with("busbar_upstream_attempts_total{")
                && l.contains("pool=\"t065-attempt-pool\"")
                && l.contains("lane=\"m0\"")),
        "the attempt is on the scrape under its pool and lane:\n{scrape}"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_attributes_spend_and_a_spent_budget_refuses() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-budget";
    let far = seat_far_end(200, SERVED_BY_TWIN).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            budget_cents: Some(1),
            ..RigOpts::default()
        },
    )
    .await;
    assert_eq!(rig.chat().await.0, 200);
    let (requests, spend, _, billable) = rig.ledger_after(1).await;
    assert_eq!(
        (requests, spend, billable),
        (1, 1, 1),
        "the one fee on the key"
    );
    let (status, head, _) = rig.chat().await;
    assert_eq!(status, 429, "the spent budget refuses");
    assert!(
        head.contains_key("retry-after"),
        "with a Retry-After: {head:?}"
    );
    assert_eq!(far.served(), 1, "the refused unit dialled nothing");
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_refunds_a_failed_units_fee_and_keeps_its_admission() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-failed-fee-refund";
    let down = seat_far_end(503, OVERLOADED).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(down.port, 1)],
            budget_cents: Some(5),
            ..RigOpts::default()
        },
    )
    .await;
    assert_ne!(rig.chat().await.0, 200, "the far end fails");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut ledger = rig.ledger_after(0).await;
    while (ledger.0 == 0 || ledger.1 != 0) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        ledger = rig.ledger_after(0).await;
    }
    let (requests, spend, _, billable) = ledger;
    assert_eq!(requests, 1, "the admission slot is never refunded");
    assert_eq!(spend, 0, "the fee base the admission charged is refunded");
    assert_eq!(billable, 0, "nothing billable");
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_seals_one_audit_record_per_unit() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-audit";
    let far = seat_far_end(200, SERVED_BY_TWIN).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            ..RigOpts::default()
        },
    )
    .await;
    assert_eq!(rig.chat().await.0, 200);
    let _ = rig.ledger_after(1).await;
    assert_eq!(rig.audit_records(), 1, "one record for the one unit");
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_seals_every_unit_served_or_refused_on_one_tamper_evident_chain() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-pools-audit-chain";
    let far = seat_far_end(200, SERVED_BY_TWIN).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            budget_cents: Some(1),
            ..RigOpts::default()
        },
    )
    .await;
    assert_eq!(rig.chat().await.0, 200, "the first unit is served");
    let _ = rig.ledger_after(1).await;
    assert_eq!(
        rig.chat().await.0,
        429,
        "the second is refused: its budget is spent"
    );
    assert_eq!(far.served(), 1, "the refused unit dialled nothing");

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while rig.audit_records() < 2 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let records = rig.records();
    assert_eq!(records.len(), 2, "one record per unit: {records:?}");
    let (served, refused) = (&records[0], &records[1]);
    assert_eq!(
        served.outcome.unit_end,
        busbar_contract::caps::Outcome::Completed,
        "the served unit: {served:?}"
    );
    assert!(
        matches!(
            refused.outcome.unit_end,
            busbar_contract::caps::Outcome::Refused(..)
        ),
        "the refusal is chained as one: {:?}",
        refused.outcome
    );
    assert_eq!(refused.seq, served.seq + 1, "contiguous");
    assert_eq!(
        refused.prev_hash, served.hash,
        "linked to the record before"
    );
    assert!(busbar_kernel_audit::AuditChain::verify_window(&records).is_ok());

    let mut forged = records.clone();
    forged[0].outcome.unit_end = refused.outcome.unit_end;
    assert!(
        busbar_kernel_audit::AuditChain::verify_window(&forged).is_err(),
        "a record altered after its seal breaks the chain"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_refuses_a_new_unit_503_with_the_reason_while_its_journal_is_full() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-full-journal";
    let far = seat_far_end(200, SERVED_BY_TWIN).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            full_journal: true,
            ..RigOpts::default()
        },
    )
    .await;
    let (status, head, body) = rig.chat().await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, 503, "{text}");
    assert_eq!(
        head.get("content-type").and_then(|v| v.to_str().ok()),
        Some("application/json"),
        "the caller's dialect envelope: {text}"
    );
    let answer: serde_json::Value = serde_json::from_slice(&body).expect("a JSON envelope");
    let message = answer["error"]["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("native journal custody buffer is full"),
        "the reason identifies the full native journal: {answer}"
    );
    assert!(answer["error"]["type"].is_string(), "{answer}");
    assert_eq!(far.served(), 0, "nothing dispatched");
    let (requests, spend, _, billable) = rig.ledger_after(0).await;
    assert_eq!((requests, spend, billable), (0, 0, 0), "nothing charged");
    assert_eq!(
        rig.book.lock().unwrap().journal.pending_records(),
        1,
        "the original reserved record remains in custody"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_doors_gate_hook_refuses_before_dispatch() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-gate";
    let far = seat_far_end(200, SERVED_BY_TWIN).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            reject_at_gate: true,
            ..RigOpts::default()
        },
    )
    .await;
    assert_eq!(rig.chat().await.0, 451, "the gate's own status");
    assert_eq!(far.served(), 0, "nothing was dispatched");
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_doors_gate_veto_keeps_the_admission_and_refunds_the_fee() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-gate-money";
    let far = seat_far_end(200, SERVED_BY_TWIN).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            reject_at_gate: true,
            budget_cents: Some(5),
            ..RigOpts::default()
        },
    )
    .await;
    assert_eq!(rig.chat().await.0, 451, "the gate's own status");
    assert_eq!(far.served(), 0, "nothing was dispatched");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut ledger = rig.ledger_after(0).await;
    while (ledger.0 == 0 || ledger.1 != 0) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        ledger = rig.ledger_after(0).await;
    }
    let (requests, spend, _, billable) = ledger;
    assert_eq!(requests, 1, "the admission stands");
    assert_eq!(spend, 0, "the vetoed unit's fee is refunded");
    assert_eq!(billable, 0, "nothing billable");
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_doors_tap_hooks_observe_the_rewritten_request() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-tap";
    let far = seat_far_end(200, SERVED_BY_TWIN).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            ..RigOpts::default()
        },
    )
    .await;
    assert_eq!(rig.chat().await.0, 200);
    let payload = rig
        .request_tap_payload(2_000)
        .await
        .expect("the request-stage tap is delivered");
    let text = String::from_utf8_lossy(&payload);
    assert!(text.contains(REWRITTEN), "{text}");
    assert!(!text.contains("the original prompt"), "{text}");
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_payload_never_carries_auth_headers() {
    const CALLER_API_KEY: &str = "caller-x-api-key-5b1e0c";
    const PROXY_SECRET: &str = "Basic cHJveHk6c2VjcmV0LTkxZDM=";
    const COOKIE: &str = "session=cookie-secret-3a7f";
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-tap-no-auth";
    let far = seat_far_end(200, SERVED_BY_TWIN).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            no_rewrite: true,
            ..RigOpts::default()
        },
    )
    .await;
    let bearer = format!("Bearer {}", rig.token);
    let (status, _, body) = rig
        .send_bare(
            "POST",
            "/v1/chat/completions",
            Some(serde_json::json!({"model": "p", "max_tokens": 16,
                "messages": [{"role": "user", "content": "the caller's prompt"}]})),
            &[
                ("authorization", bearer.as_str()),
                ("x-api-key", CALLER_API_KEY),
                ("proxy-authorization", PROXY_SECRET),
                ("cookie", COOKIE),
            ],
        )
        .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let payload = rig
        .request_tap_payload(2_000)
        .await
        .expect("the request-stage tap is delivered");
    let text = String::from_utf8_lossy(&payload);
    assert!(
        text.contains("the caller's prompt"),
        "the tap holds the prompt grant, so the payload is the full projection: {text}"
    );
    for secret in [
        rig.token.as_str(),
        CALLER_API_KEY,
        PROXY_SECRET,
        COOKIE,
        "sk-seats",
    ] {
        assert!(
            !text.contains(secret),
            "a credential reached the hook: {text}"
        );
    }
    let lower = text.to_ascii_lowercase();
    for field in ["authorization", "x-api-key", "cookie"] {
        assert!(
            !lower.contains(field),
            "an auth field name reached the hook ({field}): {text}"
        );
    }
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_doors_catalogue_shows_a_restricted_key_only_what_it_reaches() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-catalogue";
    let (a, b) = (
        seat_far_end(200, SERVED_BY_TWIN).await,
        seat_far_end(200, SERVED_BY_TWIN).await,
    );
    let rig = rig(
        instance,
        RigOpts {
            members: &[(a.port, 1), (b.port, 1)],
            pooled: Some(1),
            allowed_pools: Some(&["p"]),
            ..RigOpts::default()
        },
    )
    .await;
    let (status, _, body) = rig.send("GET", "/v1/models", None).await;
    assert_eq!(status, 200);
    let listed: serde_json::Value = serde_json::from_slice(&body).expect("a JSON list");
    let ids: Vec<&str> = listed["data"]
        .as_array()
        .expect("the list")
        .iter()
        .filter_map(|m| m["id"].as_str())
        .collect();
    assert_eq!(
        ids,
        ["p", "m0"],
        "the reachable pool and its member, nothing else"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_audits_a_control_the_far_dialect_cannot_carry() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-dropped-controls";

    let far_dialect = door_word("untranslatable_dialect");
    let far = seat_far_end(200, door_word("untranslatable_answer")).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            dialect: Some(far_dialect),
            ..RigOpts::default()
        },
    )
    .await;
    busbar_kernel::boot::hydrate_all(&rig.app)
        .await
        .expect("the actual boot registers the durable audit stream before serving units");
    let rows = |resource: &str| {
        busbar_kernel::audit::auditlog::AUDIT_LOG
            .list_filtered(
                0,
                1000,
                Some("egress.control_unrepresentable"),
                Some(resource),
            )
            .into_iter()
            .filter(|e| e.principal == rig.key_id && e.outcome == "degraded")
            .count()
    };
    let (status, _, _) = rig
        .send(
            "POST",
            "/v1/chat/completions",
            Some(serde_json::json!({"model": "p", "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}]})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        rows(format!("logit_bias on {far_dialect}").as_str()),
        0,
        "nothing set, nothing dropped"
    );
    let (status, _, _) = rig
        .send(
            "POST",
            "/v1/chat/completions",
            Some(serde_json::json!({"model": "p", "max_tokens": 16,
                "logit_bias": {"50256": -100},
                "messages": [{"role": "user", "content": "hi"}]})),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        rows(format!("logit_bias on {far_dialect}").as_str()),
        1,
        "the dropped control, one row"
    );
    assert!(
        rows(format!("usage.output_tokens_details from {far_dialect}").as_str()) >= 1,
        "the answer member the caller's dialect cannot carry"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_doors_hooks_see_each_candidates_standing() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-signals";
    let (a, b) = (
        seat_far_end(200, SERVED_BY_TWIN).await,
        seat_far_end(200, SERVED_BY_TWIN).await,
    );
    let rig = rig(
        instance,
        RigOpts {
            members: &[(a.port, 1), (b.port, 1)],
            described: true,
            ..RigOpts::default()
        },
    )
    .await;
    assert_eq!(rig.chat().await.0, 200);
    let first = rig.gate_saw();
    assert_eq!(first.len(), 2, "{first:?}");
    assert!(
        first[0].starts_with("m0 tier=Some(\"t0\") tags=[\"g0\"] latency=false avail=true"),
        "{first:?}"
    );
    assert!(
        first[0].contains("CandidateBreakerState=closed"),
        "the declared breaker state: {first:?}"
    );
    assert!(
        !first[0].contains("CandidateLatencyP95Ms"),
        "no p95 before a sample: {first:?}"
    );
    assert_eq!(rig.chat().await.0, 200);
    let second = rig.gate_saw();
    let second: Vec<String> = second
        .into_iter()
        .filter(|c| c.contains("latency=true"))
        .collect();
    assert!(
        !second.is_empty(),
        "the served attempt's latency sample: {second:?}"
    );
    assert!(
        second[0].contains("CandidateLatencyP95Ms=true")
            && second[0].contains("CandidateErrorRate=true"),
        "the p95 reservoir and the error rate, once fed: {second:?}"
    );
}

#[cfg(linked_driver_proofs)]
fn event_stream(events: Vec<String>) -> ProviderReply {
    ProviderReply::Sse {
        events,
        abort_at_index: None,
    }
}

#[cfg(linked_driver_proofs)]
mod pools_door_walk_and_breaker {
    use super::*;

    use std::sync::Arc;

    use axum::http::StatusCode;

    fn now() -> u64 {
        busbar_kernel::store::now()
    }

    async fn mock(replies: Vec<ProviderReply>) -> (Arc<ProviderRequests>, ProviderEndpoint) {
        let state = Arc::new(ProviderRequests::new());
        for reply in replies.into_iter().rev() {
            state.push(reply);
        }
        let server = ProviderEndpoint::new(Arc::clone(&state)).await;
        (state, server)
    }

    fn port_of(server: &ProviderEndpoint) -> u16 {
        server.address().port()
    }

    fn messages_answer_saying(text: &str) -> serde_json::Value {
        let mut answer: serde_json::Value =
            serde_json::from_str(door_word("messages_answer")).expect("the answer is JSON");
        answer["content"][0]["text"] = serde_json::Value::from(text);
        answer
    }

    fn messages_request(stream: bool) -> serde_json::Value {
        serde_json::json!({"model": "p", "max_tokens": 100, "stream": stream,
            "messages": [{"role": "user", "content": "hi"}]})
    }

    async fn send_messages(rig: &DoorRig, stream: bool) -> (u16, axum::http::HeaderMap, String) {
        let (status, head, body) = rig
            .send(
                "POST",
                door_word("messages_path"),
                Some(messages_request(stream)),
            )
            .await;
        (status, head, String::from_utf8_lossy(&body).to_string())
    }

    fn events(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("event-{i}")).collect()
    }

    const ERROR_MAP: &str = "error_map:\n  '1113': billing\n  '1302': rate_limit";

    fn refusing_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        listener.local_addr().expect("its address").port()
    }

    use super::super::hook_seat_tests::DoorRig;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_upgrades_a_null_stream_options_and_keeps_the_usage_chunk_back() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-null-stream-options";
        let (state, server) = mock(vec![event_stream(vec![
            r#"{"id":"chatcmpl-x","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"role":"assistant","content":"Hi"},"finish_reason":null}]}"#.to_string(),
            r#"{"id":"chatcmpl-x","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#.to_string(),
            r#"{"id":"chatcmpl-x","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":7,"completion_tokens":17,"total_tokens":24}}"#.to_string(),
        ])])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&server), 1)],
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, body) = rig
            .send(
                "POST",
                "/v1/chat/completions",
                Some(serde_json::json!({"model": "p", "stream": true,
                    "stream_options": null,
                    "messages": [{"role": "user", "content": "hi"}]})),
            )
            .await;
        let text = String::from_utf8_lossy(&body);
        assert_eq!(status, 200, "the stream is served: {text}");
        let egress: serde_json::Value = serde_json::from_slice(
            &state
                .get_last_request_body()
                .expect("the far end received the request"),
        )
        .expect("the far end's request is JSON");
        assert_eq!(
            egress.pointer("/stream_options/include_usage"),
            Some(&serde_json::Value::Bool(true)),
            "a null stream_options asks the far end for its usage: {egress}"
        );
        assert!(text.contains("Hi"), "the answer reaches the caller: {text}");
        assert!(
            !text.contains("completion_tokens"),
            "a caller that did not opt in is not sent the usage chunk: {text}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_replaces_every_caller_credential_with_the_members() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-caller-credentials";
        let (state, server) = mock(vec![ProviderReply::Ok {
            status: StatusCode::OK,
            body: serde_json::from_str(SERVED_BY_TWIN).expect("JSON"),
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&server), 1)],
                open_door: true,
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, body) = rig
            .send_bare(
                "POST",
                "/v1/chat/completions",
                Some(serde_json::json!({"model": "p", "max_tokens": 16,
                    "messages": [{"role": "user", "content": "hi"}]})),
                &[
                    ("authorization", "Bearer busbar-caller-key"),
                    ("x-api-key", "busbar-caller-key"),
                    ("x-goog-api-key", "busbar-caller-key"),
                    ("api-key", "busbar-caller-key"),
                ],
            )
            .await;
        let seen = |name: &str| state.get_last_request_header(name);
        assert_eq!(
            seen("authorization"),
            Some("Bearer sk-seats".to_string()),
            "the member's own credential ({status}: {})",
            String::from_utf8_lossy(&body)
        );
        for carrier in ["x-api-key", "x-goog-api-key", "api-key"] {
            assert_eq!(
                seen(carrier),
                None,
                "{carrier}: the caller's key never goes to the far end"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_answers_an_all_refused_pool_as_an_all_down_pool() {
        let _one = ONE_PUBLISHER.lock().await;
        let without_request_id = |body: &[u8]| {
            let mut answer: serde_json::Value =
                serde_json::from_slice(body).expect("a JSON error body");
            if let Some(id) = answer.get_mut("request_id") {
                *id = serde_json::Value::Null;
            }
            answer
        };
        let failures = |rig: &DoorRig| {
            (0..2)
                .map(|lane| rig.app.store.snapshot(lane, now()).err)
                .sum::<u64>()
        };

        let instance = "rehome-walk-all-down";
        let down = rig(
            instance,
            RigOpts {
                members: &[(refusing_port(), 1), (refusing_port(), 1)],
                ..RigOpts::default()
            },
        )
        .await;
        let (down_status, _, down_body) = down.chat().await;
        let down_failures = failures(&down);
        drop(down);

        let instance = "rehome-walk-all-refused";
        let (first, second) = (
            seat_far_end(200, SERVED_BY_TWIN).await,
            seat_far_end(200, SERVED_BY_TWIN).await,
        );
        let judged = rig(
            instance,
            RigOpts {
                members: &[(first.port, 1), (second.port, 1)],
                refuse_private: true,
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, body) = judged.chat().await;
        assert_eq!(
            down_status, 503,
            "the all-down baseline is the 503 terminal"
        );
        assert_eq!(status, down_status, "{}", String::from_utf8_lossy(&body));
        assert_eq!(
            without_request_id(&body),
            without_request_id(&down_body),
            "the all-down body"
        );
        assert_eq!(
            (first.served(), second.served()),
            (0, 0),
            "a refused dial never leaves"
        );
        assert_eq!(
            failures(&judged),
            down_failures,
            "both members were tried, as both down members were"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_audits_an_unmapped_content_part_by_its_wire_path() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-unmapped-part";
        let far_dialect = door_word("messages_dialect");
        let (state, server) = mock(vec![ProviderReply::Ok {
            status: StatusCode::OK,
            body: messages_answer_saying("hi"),
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&server), 1)],
                dialect: Some(far_dialect),

                no_rewrite: true,
                ..RigOpts::default()
            },
        )
        .await;
        busbar_kernel::boot::hydrate_all(&rig.app)
            .await
            .expect("the actual boot registers the durable audit stream before serving units");
        let (status, _, body) = rig
            .send(
                "POST",
                "/v1/chat/completions",
                Some(serde_json::json!({"model": "p", "max_tokens": 16,
                "logit_bias": {"50256": -100},
                "messages": [{"role": "user", "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "x_future_part", "x": 1}
                ]}]})),
            )
            .await;
        assert_eq!(
            status,
            200,
            "the dropped members do not refuse the request: {}",
            String::from_utf8_lossy(&body)
        );
        let rebuilt: serde_json::Value = serde_json::from_slice(
            &state
                .get_last_request_body()
                .expect("the far end received the request"),
        )
        .expect("JSON");
        assert_eq!(
            rebuilt
                .pointer("/messages/0/content")
                .and_then(|parts| parts.as_array())
                .map(Vec::len),
            Some(1),
            "the unknown part is dropped, not replaced: {rebuilt}"
        );
        busbar_kernel::audit::auditlog::flush_pending()
            .await
            .expect("the native audit stream confirms both degraded wire-field rows");
        for resource in [
            format!("logit_bias on {far_dialect}"),
            format!("messages[].content[].type=x_future_part on {far_dialect}"),
        ] {
            let rows = busbar_kernel::audit::auditlog::AUDIT_LOG
                .list_filtered(
                    0,
                    1000,
                    Some("egress.control_unrepresentable"),
                    Some(resource.as_str()),
                )
                .into_iter()
                .filter(|e| e.principal == rig.key_id && e.outcome == "degraded")
                .count();
            assert_eq!(rows, 1, "one degraded row names `{resource}`");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_relays_every_event_of_a_streamed_answer() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-ten-events";
        let (_state, server) = mock(vec![ProviderReply::Sse {
            events: events(10),
            abort_at_index: None,
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&server), 1)],
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await;
        let (status, head, text) = send_messages(&rig, true).await;
        assert_eq!(status, 200, "{text}");
        assert_eq!(
            head.get("content-type").and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        assert_eq!(
            text.lines()
                .filter(|line| line.starts_with("data: event-"))
                .count(),
            10,
            "every event reached the caller: {text}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_fails_over_a_fault_before_the_first_byte_and_cools_the_member() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-pre-first-byte";
        let (_failing, down) = mock(vec![ProviderReply::ServerError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: serde_json::json!({ "error": "lane 0 failed" }),
        }])
        .await;
        let (_serving, up) = mock(vec![ProviderReply::Sse {
            events: events(3),
            abort_at_index: None,
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&down), 100), (port_of(&up), 1)],
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, text) = send_messages(&rig, true).await;
        assert_eq!(status, 200, "{text}");
        assert!(
            !rig.app.store.ready_in("p", 0, now()),
            "member 0 is in its transient cooldown"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_records_a_cut_after_the_first_byte_and_does_not_fail_over() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-midstream-abort";
        let (_cut, cutting) = mock(vec![ProviderReply::Sse {
            events: events(5),
            abort_at_index: Some(1),
        }])
        .await;
        let (sibling_state, sibling) = mock(vec![ProviderReply::Sse {
            events: vec!["lane1-ok".to_string()],
            abort_at_index: None,
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&cutting), 100), (port_of(&sibling), 1)],
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, text) = send_messages(&rig, true).await;
        assert_eq!(status, 200, "{text}");
        assert!(
            text.contains("event: error"),
            "the cut ends on an error event: {text}"
        );
        let first = rig.app.store.snapshot(0, now());
        assert!(first.err > 0, "member 0's failure is recorded");
        assert!(first.cooldown_remaining_s > 0, "and cools it");
        assert_eq!(
            rig.app.store.snapshot(1, now()).err,
            0,
            "member 1 is untouched"
        );
        assert!(
            sibling_state.get_last_request_body().is_none(),
            "member 1 was never dialled: no failover after the first byte"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_trips_on_an_own_credential_401_but_not_on_a_passthrough_one() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-passthrough-401";
        let (_state, server) = mock(vec![ProviderReply::Auth {
            status: StatusCode::UNAUTHORIZED,
        }])
        .await;
        let passthrough = rig(
            instance,
            RigOpts {
                members: &[(port_of(&server), 1)],
                dialect: Some(door_word("messages_dialect")),
                pool_yaml: "upstream_credentials: passthrough",
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, text) = send_messages(&passthrough, false).await;
        assert_eq!(status, 401, "the far end's 401 is relayed: {text}");
        assert!(
            passthrough.app.store.ready_in("p", 0, now()),
            "a passthrough 401 trips nothing"
        );
        let snap = passthrough.app.store.snapshot(0, now());
        assert_eq!((snap.err, snap.streak), (0, 0), "nothing recorded");
        assert!(!snap.dead);
        drop(passthrough);

        let instance = "rehome-walk-own-401";
        let (_state, server) = mock(vec![ProviderReply::Auth {
            status: StatusCode::UNAUTHORIZED,
        }])
        .await;
        let own = rig(
            instance,
            RigOpts {
                members: &[(port_of(&server), 1)],
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, text) = send_messages(&own, false).await;
        assert_eq!(status, 401, "{text}");
        assert!(
            !own.app.store.ready_in("p", 0, now()),
            "the member's own credential failed: it is down"
        );
        assert!(
            !own.app.store.snapshot(0, now()).dead,
            "recoverable hard-down (long cooldown and probe), never permanently dead"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_forwards_the_callers_token_on_a_passthrough_pool() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-passthrough-token";
        let far = seat_far_end(200, SERVED_BY_TWIN).await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                pool_yaml: "upstream_credentials: passthrough",
                ..RigOpts::default()
            },
        )
        .await;
        assert_eq!(rig.chat().await.0, 200);
        let seen = far.authorizations.lock().unwrap().clone();
        assert_eq!(seen, vec![format!("Bearer {}", rig.token)]);
        assert!(
            !seen[0].contains("sk-seats"),
            "the configured key never stands in"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_tries_each_member_once_before_its_terminal() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-exclusions";
        let fars = [
            seat_far_end(500, r#"{"error":"transient failure"}"#).await,
            seat_far_end(500, r#"{"error":"transient failure"}"#).await,
        ];
        let rig = rig(
            instance,
            RigOpts {
                members: &[(fars[0].port, 1), (fars[1].port, 1)],
                ..RigOpts::default()
            },
        )
        .await;
        assert_eq!(rig.chat().await.0, 503);
        let errs: u64 = (0..2)
            .map(|lane| rig.app.store.snapshot(lane, now()).err)
            .sum();
        assert_eq!(errs, 2, "one attempt per distinct member, no retry");
        assert_eq!((fars[0].served(), fars[1].served()), (1, 1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_caps_the_walk_at_one_attempt_per_member() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-cap";
        let fars = [
            seat_far_end(500, r#"{"error":"transient failure"}"#).await,
            seat_far_end(500, r#"{"error":"transient failure"}"#).await,
            seat_far_end(500, r#"{"error":"transient failure"}"#).await,
        ];
        let rig = rig(
            instance,
            RigOpts {
                members: &[(fars[0].port, 1), (fars[1].port, 1), (fars[2].port, 1)],
                ..RigOpts::default()
            },
        )
        .await;
        assert_eq!(rig.chat().await.0, 503);
        let errs: u64 = (0..3)
            .map(|lane| rig.app.store.snapshot(lane, now()).err)
            .sum();
        assert_eq!(errs, 3, "three members, three attempts");
        assert!(fars.iter().all(|far| far.served() == 1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_never_retries_a_callers_fault_on_another_member() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-client-fault-once";
        let fars = [
            seat_far_end(
                400,
                r#"{"error":{"message":"bad input","type":"invalid_request_error"}}"#,
            )
            .await,
            seat_far_end(
                400,
                r#"{"error":{"message":"bad input","type":"invalid_request_error"}}"#,
            )
            .await,
        ];
        let rig = rig(
            instance,
            RigOpts {
                members: &[(fars[0].port, 1), (fars[1].port, 1)],
                ..RigOpts::default()
            },
        )
        .await;
        assert_eq!(rig.chat().await.0, 400, "the caller's fault is relayed");
        assert_eq!(
            fars[0].served() + fars[1].served(),
            1,
            "one member dialled: a caller's fault is not retried"
        );
    }

    async fn mapped_member(
        instance: &'static str,
        reply: ProviderReply,
    ) -> (
        DoorRig,
        (u16, axum::http::HeaderMap, String),
        ProviderEndpoint,
    ) {
        let (_state, server) = mock(vec![reply]).await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&server), 1)],
                dialect: Some(door_word("messages_dialect")),
                provider_yaml: ERROR_MAP,
                ..RigOpts::default()
            },
        )
        .await;
        let answer = send_messages(&rig, false).await;
        (rig, answer, server)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_takes_a_member_down_on_a_mapped_billing_code() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-billing-code";
        let (rig, _, _server) = mapped_member(
            instance,
            ProviderReply::Billing {
                status: StatusCode::PAYMENT_REQUIRED,
                code: "1113",
                message: "insufficient balance",
            },
        )
        .await;
        assert!(!rig.app.store.ready_in("p", 0, now()), "down after billing");
        let snap = rig.app.store.snapshot(0, now());
        assert!(
            !snap.dead,
            "billing is recoverable (long cooldown and probe)"
        );
        assert_eq!(snap.dead_reason, "billing / insufficient balance");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_cools_a_member_on_a_mapped_rate_limit_code() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-rate-limit-code";
        let (rig, _, _server) = mapped_member(
            instance,
            ProviderReply::RateLimit {
                status: StatusCode::TOO_MANY_REQUESTS,
                provider_signal: Some("1302"),
                retry_after: None,
            },
        )
        .await;
        assert!(!rig.app.store.ready_in("p", 0, now()), "cooling down");
        let snap = rig.app.store.snapshot(0, now());
        assert!(snap.err > 0, "the rate limit is counted");
        assert_eq!(snap.streak, 1, "the first rate limit");
        assert!(!snap.dead);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_cools_a_member_on_a_bare_rate_limit() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-rate-limit-bare";
        let (rig, _, _server) = mapped_member(
            instance,
            ProviderReply::RateLimit {
                status: StatusCode::TOO_MANY_REQUESTS,
                provider_signal: None,
                retry_after: None,
            },
        )
        .await;
        assert!(!rig.app.store.ready_in("p", 0, now()), "cooling down");
        assert!(rig.app.store.snapshot(0, now()).err > 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_cools_a_member_on_a_server_error() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-server-error";
        let (rig, _, _server) = mapped_member(
            instance,
            ProviderReply::ServerError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: serde_json::json!({ "error": "server error" }),
            },
        )
        .await;
        assert!(!rig.app.store.ready_in("p", 0, now()), "cooling down");
        let snap = rig.app.store.snapshot(0, now());
        assert!(snap.err > 0);
        assert!(!snap.dead);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_takes_a_member_down_on_a_refused_own_credential() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-auth-down";
        let (rig, (status, _, text), _server) = mapped_member(
            instance,
            ProviderReply::Auth {
                status: StatusCode::UNAUTHORIZED,
            },
        )
        .await;
        assert!(!rig.app.store.ready_in("p", 0, now()), "down after auth");
        let snap = rig.app.store.snapshot(0, now());
        assert!(!snap.dead, "recoverable (long cooldown and probe)");
        assert!(
            snap.dead_reason.contains("auth"),
            "the reason names auth: {}",
            snap.dead_reason
        );
        assert_eq!(status, 401, "{text}");
        let answer: serde_json::Value = serde_json::from_str(&text).expect("a JSON envelope");
        assert_eq!(answer["type"], "error", "{answer}");
        assert_eq!(answer["error"]["type"], "authentication_error", "{answer}");
        let message = answer["error"]["message"].as_str().unwrap_or("");
        assert_eq!(
            message, "invalid x-api-key",
            "the dialect's own sentence: {answer}"
        );
        assert!(!message.contains("lane"), "{answer}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_classifies_by_the_mapped_code_not_the_status() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-code-drives-mapped";
        let (_state, server) = mock(vec![ProviderReply::Billing {
            status: StatusCode::PAYMENT_REQUIRED,
            code: "1113",
            message: "insufficient balance",
        }])
        .await;
        let mapped = rig(
            instance,
            RigOpts {
                members: &[(port_of(&server), 1)],
                dialect: Some(door_word("messages_dialect")),
                provider_yaml: "error_map:\n  '1113': billing",
                ..RigOpts::default()
            },
        )
        .await;
        let _ = send_messages(&mapped, false).await;
        assert!(
            !mapped.app.store.ready_in("p", 0, now()),
            "the mapped billing code takes the member down"
        );
        drop(mapped);

        let instance = "rehome-walk-code-drives-unmapped";
        let (_state, server) = mock(vec![ProviderReply::Ok {
            status: StatusCode::BAD_REQUEST,
            body: serde_json::json!({ "error": "not a known error" }),
        }])
        .await;
        let unmapped = rig(
            instance,
            RigOpts {
                members: &[(port_of(&server), 1)],
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, text) = send_messages(&unmapped, false).await;
        assert_eq!(status, 400, "the caller's fault is relayed: {text}");
        assert!(
            unmapped.app.store.ready_in("p", 0, now()),
            "and records nothing against the member"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_relays_a_callers_fault_verbatim_and_counts_it_apart() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-client-fault";
        let (_state, server) = mock(vec![ProviderReply::Ok {
            status: StatusCode::BAD_REQUEST,
            body: serde_json::json!({ "error": "invalid_request", "message": "bad input" }),
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&server), 1)],
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, text) = send_messages(&rig, false).await;
        assert_eq!(status, 400);
        assert_eq!(
            text, r#"{"error":"invalid_request","message":"bad input"}"#,
            "relayed verbatim"
        );
        assert!(matches!(
            rig.app.store.breaker_state_in("p", 0),
            busbar_kernel::store::BreakerState::Closed
        ));
        let snap = rig.app.store.snapshot(0, now());
        assert_eq!(
            (snap.err, snap.streak, snap.cooldown_remaining_s),
            (0, 0, 0)
        );
        assert_eq!(snap.client_fault, 1, "the caller's fault is counted apart");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_sheds_an_exhausted_pool_with_a_retry_after() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-exhaustion";
        let limited = || ProviderReply::RateLimit {
            status: StatusCode::TOO_MANY_REQUESTS,
            provider_signal: Some("1302"),
            retry_after: None,
        };
        let (_first_state, first) = mock(vec![limited()]).await;
        let (_second_state, second) = mock(vec![limited()]).await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&first), 1), (port_of(&second), 1)],
                ..RigOpts::default()
            },
        )
        .await;
        let (status, head, _) = rig.chat().await;
        assert_eq!(status, 503, "the exhausted terminal");
        let wait: u64 = head
            .get("retry-after")
            .expect("a Retry-After")
            .to_str()
            .expect("text")
            .parse()
            .expect("whole seconds");
        assert!(wait >= 1, "at least one second: {wait}");
    }

    struct HeldClock {
        secs: u64,
        origin: std::time::Instant,
    }

    impl HeldClock {
        fn at(secs: u64) -> Arc<dyn busbar_kernel_egress::ports::Clock> {
            Arc::new(HeldClock {
                secs,
                origin: std::time::Instant::now(),
            })
        }
    }

    impl busbar_kernel_egress::ports::Clock for HeldClock {
        fn now_secs(&self) -> u64 {
            self.secs
        }

        fn now_millis(&self) -> u128 {
            self.origin.elapsed().as_millis()
        }

        fn now_micros(&self) -> u128 {
            self.origin.elapsed().as_micros()
        }

        fn sleep(&self, ms: u64) -> busbar_kernel_egress::ports::BoxFut<'_, ()> {
            Box::pin(tokio::time::sleep(std::time::Duration::from_millis(ms)))
        }
    }

    const HELD_AT: u64 = 1_700_000_000;

    async fn benched_once(
        instance: &'static str,
        reply: ProviderReply,
        band: std::ops::RangeInclusive<u64>,
    ) {
        let _one = ONE_PUBLISHER.lock().await;
        let clock = HeldClock::at(HELD_AT);
        let (state, server) = mock(vec![reply, ProviderReply::default()]).await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&server), 1)],
                clock: Some(&clock),
                ..RigOpts::default()
            },
        )
        .await;

        let (status, _, _) = rig.chat().await;
        assert_ne!(status, 200, "{instance}: the far end's one failure");
        assert!(
            matches!(
                rig.app.store.breaker_state_in("p", 0),
                busbar_kernel::store::BreakerState::Closed
            ),
            "{instance}: one failure is below the trip"
        );
        let cooling = rig.app.store.cooldown_remaining_in("p", 0, HELD_AT);
        assert!(
            band.contains(&cooling),
            "{instance}: the member is benched for 1.5.5's cooldown {band:?}, on the walk's clock: \
             {cooling}"
        );

        let (status, head, _) = rig.chat().await;
        assert_eq!(status, 503, "{instance}: the benched sole member sheds");
        let wait: u64 = head
            .get("retry-after")
            .expect("a Retry-After")
            .to_str()
            .expect("text")
            .parse()
            .expect("whole seconds");
        assert_eq!(
            wait, cooling,
            "{instance}: the Retry-After is the cooldown, read on the same clock"
        );
        assert!(
            state.next_response().is_some(),
            "{instance}: the refused call never reached the far end"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_single_member_pool_benches_one_failure_below_the_trip_with_1_5_5s_wait() {
        benched_once(
            "rehome-walk-single-bench-unstated",
            ProviderReply::RateLimit {
                status: StatusCode::TOO_MANY_REQUESTS,
                provider_signal: None,
                retry_after: None,
            },
            27..=33,
        )
        .await;
        benched_once(
            "rehome-walk-single-bench-stated",
            ProviderReply::RateLimit {
                status: StatusCode::TOO_MANY_REQUESTS,
                provider_signal: None,
                retry_after: Some(60),
            },
            60..=60,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_transient_failure_is_benched_on_the_walks_clock() {
        benched_once(
            "rehome-walk-single-bench-walk-clock",
            ProviderReply::ServerError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                body: serde_json::json!({ "error": "server error" }),
            },
            27..=33,
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_yields_a_session_pinned_to_a_tripped_member() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-sticky-yields";
        let (tripped, healthy) = (
            seat_far_end(200, SERVED_BY_TWIN).await,
            seat_far_end(200, SERVED_BY_TWIN).await,
        );
        let rig = rig(
            instance,
            RigOpts {
                members: &[(tripped.port, 1), (healthy.port, 1)],
                ..RigOpts::default()
            },
        )
        .await;
        rig.app.store.force_open_in("p", 0, now() + 600);
        let chat = serde_json::json!({"model": "p", "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]});
        for session in [
            "session-abc",
            "session-1",
            "session-2",
            "session-3",
            "session-4",
            "session-5",
            "session-6",
            "session-7",
        ] {
            let response = rig
                .open(
                    "POST",
                    "/v1/chat/completions",
                    Some(chat.clone()),
                    &[("x-session-id", session)],
                )
                .await;
            assert_eq!(response.status().as_u16(), 200, "{session}");
            let body = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .expect("the body");
            assert!(
                String::from_utf8_lossy(&body).contains("served-by-the-twin"),
                "{session} is served by the healthy member"
            );
        }
        assert_eq!(tripped.served(), 0, "the tripped member receives nothing");
        assert_eq!(healthy.served(), 8);
    }

    fn too_long(window: u64) -> ProviderReply {
        ProviderReply::ServerError {
            status: StatusCode::BAD_REQUEST,
            body: serde_json::json!({"error": {"type": "invalid_request_error",
                "message": format!("prompt is too long: 250000 tokens > {window} maximum")}}),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_fails_a_context_length_refusal_over_without_a_penalty() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-context-length";
        let (_refusing_state, refusing) = mock(vec![too_long(200_000)]).await;
        let (_serving_state, serving) = mock(vec![ProviderReply::Ok {
            status: StatusCode::OK,
            body: messages_answer_saying("ok"),
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&refusing), 100), (port_of(&serving), 1)],
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, text) = send_messages(&rig, false).await;
        assert_eq!(status, 200, "the context-length refusal fails over: {text}");
        for lane in 0..2 {
            assert_eq!(
                rig.app.store.snapshot(lane, now()).cooldown_remaining_s,
                0,
                "member {lane} is not cooled for a request too big for it"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_serves_a_too_long_request_from_the_larger_window() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-larger-window";
        let (_small_state, small) = mock(vec![too_long(8_000)]).await;
        let (large_state, large) = mock(vec![ProviderReply::Ok {
            status: StatusCode::OK,
            body: messages_answer_saying("ok"),
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&small), 100), (port_of(&large), 1)],
                dialect: Some(door_word("messages_dialect")),
                member_yaml: &["context_max: 8000", "context_max: 200000"],
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, text) = send_messages(&rig, false).await;
        assert_eq!(status, 200, "{text}");
        assert!(
            large_state.get_last_request_body().is_some(),
            "the larger window served"
        );
        assert!(
            rig.app.store.ready_in("p", 0, now()),
            "the smaller member stays ready"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_ends_a_too_long_request_at_its_terminal_without_a_penalty() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-same-window";
        let (_first_state, first) = mock(vec![too_long(8_000)]).await;
        let (_second_state, second) = mock(vec![too_long(8_000)]).await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&first), 1), (port_of(&second), 1)],
                dialect: Some(door_word("messages_dialect")),
                member_yaml: &["context_max: 8000", "context_max: 8000"],
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, text) = send_messages(&rig, false).await;
        assert_eq!(status, 503, "{text}");
        for lane in 0..2 {
            assert_eq!(
                rig.app.store.snapshot(lane, now()).cooldown_remaining_s,
                0,
                "member {lane} is not cooled"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_records_one_success_per_clean_stream_end() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-clean-ends";
        const RUNS: usize = 8;
        let clean = || {
            ProviderReply::Sse {
            events: vec![
                r#"{"type":"message_start"}"#.to_string(),
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#.to_string(),
                r#"{"type":"message_delta","usage":{"input_tokens":3,"output_tokens":2}}"#.to_string(),
                r#"{"type":"message_stop"}"#.to_string(),
            ],
            abort_at_index: None,
        }
        };
        let (_state, server) = mock((0..RUNS).map(|_| clean()).collect()).await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&server), 1)],
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await;
        for _ in 0..RUNS {
            let (status, _, text) = send_messages(&rig, true).await;
            assert_eq!(status, 200, "{text}");
        }
        let snap = rig.app.store.snapshot(0, now());
        assert_eq!(snap.err, 0, "a clean end records no failure");
        assert_eq!(snap.ok, RUNS as u64, "one success per clean stream");
        assert!(rig.app.store.ready_in("p", 0, now()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_presents_no_credential_for_a_passthrough_caller_with_none() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-passthrough-none";
        let (state, server) = mock(vec![ProviderReply::Ok {
            status: StatusCode::OK,
            body: serde_json::from_str(SERVED_BY_TWIN).expect("JSON"),
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&server), 1)],
                pool_yaml: "upstream_credentials: passthrough",
                open_door: true,
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, body) = rig
            .send_bare(
                "POST",
                "/v1/chat/completions",
                Some(serde_json::json!({"model": "p", "max_tokens": 50,
                    "messages": [{"role": "user", "content": "hi"}]})),
                &[],
            )
            .await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        if let Some(seen) = state.get_last_request_header("authorization") {
            let what = if seen.contains("sk-seats") {
                "the operator's key (LEAK)"
            } else {
                "an unexpected credential"
            };
            panic!("a passthrough caller with no credential is forwarded none; got {what}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_keeps_a_real_mid_stream_failure_with_its_member() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-real-midstream";
        let (_bad_state, bad) = mock(vec![ProviderReply::SseTransportError {
            ok_events: vec![
                r#"{"choices":[{"delta":{"content":"MEMBER-1-REAL-FIRST-FRAME"}}]}"#.to_string(),
            ],
        }])
        .await;
        let (_good_state, good) = mock(vec![ProviderReply::Sse {
            events: vec![
                r#"{"choices":[{"delta":{"content":"MEMBER-2-MUST-NEVER-APPEAR"}}]}"#.to_string(),
            ],
            abort_at_index: None,
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&bad), 100), (port_of(&good), 1)],
                ..RigOpts::default()
            },
        )
        .await;
        let err_before = rig.app.store.snapshot(0, now()).err;
        let (status, _, body) = rig
            .send(
                "POST",
                "/v1/chat/completions",
                Some(serde_json::json!({"model": "p", "stream": true,
                    "messages": [{"role": "user", "content": "hi"}]})),
            )
            .await;
        let text = String::from_utf8_lossy(&body);
        assert_eq!(status, 200, "member 1's first byte began the answer");
        assert!(text.contains("MEMBER-1-REAL-FIRST-FRAME"), "{text}");
        assert!(!text.contains("MEMBER-2-MUST-NEVER-APPEAR"), "{text}");
        let last_data = text
            .split("\n\n")
            .filter(|piece| !piece.trim().is_empty())
            .last()
            .and_then(|piece| piece.lines().find_map(|l| l.strip_prefix("data: ")))
            .expect("a trailing data frame");
        let terminal: serde_json::Value =
            serde_json::from_str(last_data).expect("the caller dialect's JSON envelope");
        assert!(
            terminal.get("error").is_some(),
            "the in-band terminal frame is an error envelope: {terminal}"
        );
        assert_eq!(
            rig.app.store.snapshot(0, now()).err,
            err_before + 1,
            "member 1's cell records exactly one mid-stream failure"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_reroutes_a_tripped_primary_to_its_twin_without_dialling_it() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-walk-tripped-primary";
        let (primary_state, primary) = mock(vec![ProviderReply::Ok {
            status: StatusCode::OK,
            body: messages_answer_saying("from the primary"),
        }])
        .await;
        let (twin_state, twin) = mock(vec![ProviderReply::Ok {
            status: StatusCode::OK,
            body: messages_answer_saying("from the twin"),
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(port_of(&primary), 1), (port_of(&twin), 1)],
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await;
        rig.app.store.force_open_in("p", 0, now() + 1_000_000);
        let (status, _, text) = send_messages(&rig, false).await;
        assert_eq!(status, 200, "the twin is admissible: {text}");
        assert!(text.contains("from the twin"), "{text}");
        assert!(
            primary_state.get_last_request_body().is_none(),
            "the tripped primary is never dispatched to"
        );
        assert!(
            twin_state.get_last_request_body().is_some(),
            "the twin received the request"
        );
    }
}

#[cfg(linked_driver_proofs)]
mod pools_door_refusals {
    use super::*;

    use busbar_kernel::config::groups::{LimitCfg, LimitMetric, LimitWindow};

    use super::super::hook_seat_tests::{DoorRig, FarEnd};

    fn limit(metric: LimitMetric, amount: u64, per: LimitWindow) -> LimitCfg {
        LimitCfg {
            metric,
            amount,
            per: Some(per),
            scope: None,
            on_exhaust: None,
            downgrade_to: None,
            admission: None,
            on_exhaustion: None,
        }
    }

    fn body_word(key: &str) -> serde_json::Value {
        serde_json::from_str(door_word(key)).expect("a JSON request body")
    }

    fn chat_on_p() -> serde_json::Value {
        serde_json::json!({"model": "p", "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]})
    }

    fn every_dialect() -> Vec<(&'static str, serde_json::Value)> {
        vec![
            ("/v1/chat/completions", chat_on_p()),
            (door_word("messages_path"), chat_on_p()),
            (door_word("chat_v2_path"), body_word("chat_v2_body")),
            (door_word("respond_path"), body_word("respond_body")),
            (door_word("generate_path"), body_word("generate_body")),
            (door_word("converse_path"), body_word("converse_body")),
        ]
    }

    fn json_of(body: &[u8]) -> serde_json::Value {
        serde_json::from_slice(body)
            .unwrap_or_else(|e| panic!("a JSON envelope ({e}): {}", String::from_utf8_lossy(body)))
    }

    fn field<'h>(head: &'h axum::http::HeaderMap, name: &str) -> Option<&'h str> {
        head.get(name).and_then(|v| v.to_str().ok())
    }

    fn assert_uuid_v4_shaped(id: &str) {
        let groups: Vec<&str> = id.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "UUID-v4 shaped (8-4-4-4-12), got '{id}'"
        );
        assert!(
            id.chars()
                .all(|ch| ch == '-' || (ch.is_ascii_hexdigit() && !ch.is_ascii_uppercase())),
            "lowercase hex with dashes only, got '{id}'"
        );
        assert_eq!(groups[2].chars().next(), Some('4'), "version 4: '{id}'");
        assert!(
            matches!(groups[3].chars().next(), Some('8' | '9' | 'a' | 'b')),
            "variant nibble 8/9/a/b: '{id}'"
        );
    }

    async fn with_wrong_key(rig: &DoorRig, path: &str) -> (u16, axum::http::HeaderMap, Vec<u8>) {
        rig.send_bare(
            "POST",
            path,
            Some(serde_json::json!({"model": "p",
                "messages": [{"role": "user", "content": "hi"}]})),
            &[("x-api-key", "wrong-token")],
        )
        .await
    }

    async fn keyed(instance: &'static str) -> (DoorRig, FarEnd) {
        let far = seat_far_end(200, SERVED_BY_TWIN).await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                ..RigOpts::default()
            },
        )
        .await;
        (rig, far)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_a_wrong_key_in_every_dialects_own_envelope() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-wrong-key-envelopes";
        let (rig, far) = keyed(instance).await;

        for path in [door_word("generate_path"), door_word("generate_v1_path")] {
            let (status, head, body) = with_wrong_key(&rig, path).await;
            assert_eq!(status, 400, "{path}");
            assert_eq!(
                field(&head, "content-type"),
                Some("application/json"),
                "{path}"
            );
            let answer = json_of(&body);
            assert_eq!(answer["error"]["code"], 400, "{path}: {answer}");
            assert_eq!(
                answer["error"]["status"], "INVALID_ARGUMENT",
                "{path}: {answer}"
            );
        }

        let (status, _, body) = with_wrong_key(&rig, door_word("messages_path")).await;
        assert_eq!(status, 401);
        let answer = json_of(&body);
        assert_eq!(answer["type"], "error", "{answer}");
        assert_eq!(answer["error"]["type"], "authentication_error", "{answer}");

        let (status, _, body) = with_wrong_key(&rig, "/v1/chat/completions").await;
        assert_eq!(status, 401);
        let answer = json_of(&body);
        assert!(answer.get("type").is_none(), "no top-level type: {answer}");
        assert_eq!(answer["error"]["type"], "authentication_error", "{answer}");
        assert_eq!(answer["error"]["code"], "invalid_api_key", "{answer}");

        let (_, _, body) = with_wrong_key(&rig, door_word("respond_path")).await;
        let answer = json_of(&body);
        assert_eq!(answer["error"]["type"], "authentication_error", "{answer}");
        assert_eq!(answer["error"]["code"], "invalid_api_key", "{answer}");
        assert!(answer["error"].get("param").is_some(), "{answer}");

        let (_, _, body) = with_wrong_key(&rig, door_word("chat_v2_path")).await;
        let answer = json_of(&body);
        assert!(answer.get("message").is_some(), "a bare message: {answer}");
        assert!(
            answer.get("error").is_none() && answer.get("type").is_none(),
            "no error, no type: {answer}"
        );

        let (status, head, body) = with_wrong_key(&rig, door_word("converse_path")).await;
        assert_eq!(status, 403, "a signature-style auth failure is 403");
        assert_eq!(
            field(&head, "x-amzn-errortype"),
            Some("AccessDeniedException")
        );
        assert_uuid_v4_shaped(field(&head, "x-amzn-requestid").expect("a request id"));
        let answer = json_of(&body);
        assert_eq!(answer["__type"], "AccessDeniedException", "{answer}");
        assert!(answer.get("error").is_none(), "{answer}");
        assert_eq!(far.served(), 0, "no refusal is dialled");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_a_wrong_key_on_the_second_chat_and_respond_surfaces() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-wrong-key-chat-v2";
        let (rig, _far) = keyed(instance).await;

        let (status, head, body) = with_wrong_key(&rig, door_word("chat_v2_path")).await;
        assert_eq!(status, 401);
        assert_eq!(field(&head, "content-type"), Some("application/json"));
        let answer = json_of(&body);
        assert!(answer.get("message").is_some(), "{answer}");
        assert!(
            answer.get("error").is_none() && answer.get("type").is_none(),
            "{answer}"
        );

        let (status, head, body) = with_wrong_key(&rig, door_word("respond_path")).await;
        assert_eq!(status, 401);
        assert_eq!(field(&head, "content-type"), Some("application/json"));
        let answer = json_of(&body);
        assert_eq!(answer["error"]["type"], "authentication_error", "{answer}");
        assert_eq!(answer["error"]["code"], "invalid_api_key", "{answer}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_a_wrong_bearer_on_converse_with_a_403() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-wrong-bearer-converse";
        let (rig, _far) = keyed(instance).await;
        let (status, head, body) = rig
            .send_bare(
                "POST",
                door_word("converse_path"),
                Some(body_word("converse_body")),
                &[("authorization", "Bearer wrong-token")],
            )
            .await;
        assert_eq!(status, 403, "{}", String::from_utf8_lossy(&body));
        assert_eq!(
            field(&head, "x-amzn-errortype"),
            Some("AccessDeniedException")
        );
        assert_uuid_v4_shaped(field(&head, "x-amzn-requestid").expect("a request id"));
        assert_eq!(field(&head, "content-type"), Some("application/json"));
        assert_eq!(json_of(&body)["__type"], "AccessDeniedException");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_a_wrong_key_on_both_generate_paths_as_a_bad_argument() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-wrong-key-generate";
        let (rig, _far) = keyed(instance).await;
        for path in [door_word("generate_path"), door_word("generate_v1_path")] {
            let (status, head, body) = rig
                .send_bare(
                    "POST",
                    path,
                    Some(body_word("generate_body")),
                    &[("x-goog-api-key", "wrong-token")],
                )
                .await;
            assert_eq!(status, 400, "{path}: {}", String::from_utf8_lossy(&body));
            assert_eq!(
                field(&head, "content-type"),
                Some("application/json"),
                "{path}"
            );
            let answer = json_of(&body);
            assert_eq!(answer["error"]["code"], 400, "{path}: {answer}");
            assert_eq!(
                answer["error"]["status"], "INVALID_ARGUMENT",
                "{path}: {answer}"
            );
        }
    }

    async fn over_budget(instance: &'static str) -> (DoorRig, FarEnd) {
        let far = seat_far_end(200, SERVED_BY_TWIN).await;
        let limits = [limit(LimitMetric::Budget, 100, LimitWindow::Total)];
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                group: Some(("bgrp", &limits)),
                seed: &[("group:bgrp@total", 250)],
                ..RigOpts::default()
            },
        )
        .await;
        (rig, far)
    }

    async fn over_rpm(instance: &'static str) -> (DoorRig, FarEnd) {
        let far = seat_far_end(200, SERVED_BY_TWIN).await;
        let limits = [limit(LimitMetric::Requests, 0, LimitWindow::Minute)];
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                group: Some(("rl0", &limits)),
                ..RigOpts::default()
            },
        )
        .await;
        (rig, far)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_a_spent_budget_in_the_messages_dialect() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-budget-messages";
        let (rig, far) = over_budget(instance).await;
        let (status, head, body) = rig
            .send("POST", door_word("messages_path"), Some(chat_on_p()))
            .await;
        assert_eq!(status, 429, "{}", String::from_utf8_lossy(&body));
        assert_eq!(field(&head, "content-type"), Some("application/json"));
        let answer = json_of(&body);
        assert_eq!(answer["type"], "error", "{answer}");
        assert_eq!(answer["error"]["type"], "billing_error", "{answer}");
        assert_eq!(far.served(), 0, "refused before any far dial");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_a_spent_budget_on_chat() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-budget-chat";
        let (rig, _far) = over_budget(instance).await;
        let (status, head, body) = rig.chat().await;
        assert_eq!(status, 429);
        assert_eq!(field(&head, "content-type"), Some("application/json"));
        let answer = json_of(&body);
        assert_eq!(answer["error"]["type"], "insufficient_quota", "{answer}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_a_spent_budget_on_the_respond_surface() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-budget-respond";
        let (rig, _far) = over_budget(instance).await;
        let (status, _, body) = rig
            .send(
                "POST",
                door_word("respond_path"),
                Some(body_word("respond_body")),
            )
            .await;
        assert_eq!(status, 429);
        let answer = json_of(&body);
        assert_eq!(answer["error"]["type"], "insufficient_quota", "{answer}");
        assert_eq!(answer["error"]["code"], "insufficient_quota", "{answer}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_a_spent_budget_on_the_second_chat_surface() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-budget-chat-v2";
        let (rig, _far) = over_budget(instance).await;
        let (status, _, body) = rig
            .send(
                "POST",
                door_word("chat_v2_path"),
                Some(body_word("chat_v2_body")),
            )
            .await;
        assert_eq!(status, 429);
        let answer = json_of(&body);
        assert!(answer["message"].is_string(), "{answer}");
        assert!(
            answer.get("error").is_none() && answer.get("type").is_none(),
            "{answer}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_a_spent_budget_on_generate() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-budget-generate";
        let (rig, _far) = over_budget(instance).await;
        let (status, _, body) = rig
            .send(
                "POST",
                door_word("generate_path"),
                Some(body_word("generate_body")),
            )
            .await;
        assert_eq!(status, 429);
        let answer = json_of(&body);
        assert_eq!(answer["error"]["code"], 429, "{answer}");
        assert_eq!(answer["error"]["status"], "RESOURCE_EXHAUSTED", "{answer}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_a_spent_budget_on_converse() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-budget-converse";
        let (rig, _far) = over_budget(instance).await;
        let (status, head, body) = rig
            .send(
                "POST",
                door_word("converse_path"),
                Some(body_word("converse_body")),
            )
            .await;
        assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
        assert_eq!(
            field(&head, "x-amzn-errortype"),
            Some("ServiceQuotaExceededException")
        );
        assert!(head.contains_key("x-amzn-requestid"));
        assert_eq!(json_of(&body)["__type"], "ServiceQuotaExceededException");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_the_request_past_its_groups_rate() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-rate-two";
        let far = seat_far_end(200, SERVED_BY_TWIN).await;
        let limits = [limit(LimitMetric::Requests, 2, LimitWindow::Minute)];
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                group: Some(("rl2", &limits)),
                ..RigOpts::default()
            },
        )
        .await;

        let into_window = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs()
            % 60;
        if into_window >= 57 {
            tokio::time::sleep(std::time::Duration::from_secs(60 - into_window + 1)).await;
        }
        for n in 0..2 {
            assert_eq!(rig.chat().await.0, 200, "request {n} is under the rate");
        }
        let (status, head, _) = rig.chat().await;
        assert_eq!(status, 429, "the third request is over the rate");
        assert!(
            head.contains_key("retry-after"),
            "with a Retry-After: {head:?}"
        );
        assert_eq!(far.served(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_an_exhausted_rate_on_chat() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-rate-chat";
        let (rig, far) = over_rpm(instance).await;
        let (status, head, body) = rig.chat().await;
        assert_eq!(status, 429);
        assert_eq!(field(&head, "content-type"), Some("application/json"));
        assert!(head.contains_key("retry-after"));
        assert_eq!(json_of(&body)["error"]["type"], "rate_limit_error");
        assert_eq!(far.served(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_an_exhausted_rate_on_the_respond_surface() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-rate-respond";
        let (rig, _far) = over_rpm(instance).await;
        let (status, head, body) = rig
            .send(
                "POST",
                door_word("respond_path"),
                Some(body_word("respond_body")),
            )
            .await;
        assert_eq!(status, 429);
        assert!(head.contains_key("retry-after"));
        assert_eq!(json_of(&body)["error"]["type"], "rate_limit_error");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_an_exhausted_rate_on_the_second_chat_surface() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-rate-chat-v2";
        let (rig, _far) = over_rpm(instance).await;
        let (status, head, body) = rig
            .send(
                "POST",
                door_word("chat_v2_path"),
                Some(body_word("chat_v2_body")),
            )
            .await;
        assert_eq!(status, 429);
        assert!(head.contains_key("retry-after"));
        let answer = json_of(&body);
        assert!(answer["message"].is_string(), "{answer}");
        assert!(
            answer.get("error").is_none() && answer.get("type").is_none(),
            "{answer}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_an_exhausted_rate_on_generate() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-rate-generate";
        let (rig, _far) = over_rpm(instance).await;
        let (status, head, body) = rig
            .send(
                "POST",
                door_word("generate_path"),
                Some(body_word("generate_body")),
            )
            .await;
        assert_eq!(status, 429);
        assert!(head.contains_key("retry-after"));
        let answer = json_of(&body);
        assert_eq!(answer["error"]["code"], 429, "{answer}");
        assert_eq!(answer["error"]["status"], "RESOURCE_EXHAUSTED", "{answer}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_an_exhausted_rate_on_converse() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-rate-converse";
        let (rig, _far) = over_rpm(instance).await;
        let (status, head, body) = rig
            .send(
                "POST",
                door_word("converse_path"),
                Some(body_word("converse_body")),
            )
            .await;
        assert_eq!(status, 429, "{}", String::from_utf8_lossy(&body));
        assert!(head.contains_key("retry-after"));
        assert_eq!(
            field(&head, "x-amzn-errortype"),
            Some("ThrottlingException")
        );
        assert!(head.contains_key("x-amzn-requestid"));
        assert_eq!(json_of(&body)["__type"], "ThrottlingException");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_puts_no_internal_prefix_on_a_forward_layer_error() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-no-router-prefix";
        let closed = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
            listener.local_addr().expect("its address").port()
        };
        let rig = rig(
            instance,
            RigOpts {
                members: &[(closed, 1)],
                ..RigOpts::default()
            },
        )
        .await;
        for (path, request) in every_dialect() {
            let (status, _, body) = rig.send("POST", path, Some(request.clone())).await;
            let text = String::from_utf8_lossy(&body);
            assert!(
                !text.contains("router:"),
                "all members down on {path} ({status}) leaked the prefix: {text}"
            );
            if request.get("model").is_some() {
                let mut missing = request;
                missing["model"] = serde_json::Value::from("missingpool");
                let (status, _, body) = rig.send("POST", path, Some(missing)).await;
                let text = String::from_utf8_lossy(&body);
                assert!(
                    !text.contains("router:"),
                    "a missing pool on {path} ({status}) leaked the prefix: {text}"
                );
            }
        }
    }

    fn assert_leak_free(body: &str, key_id: &str, pool: &str) {
        assert!(!body.contains("virtual key"), "{body}");
        assert!(!body.contains(key_id), "the key id: {body}");
        assert!(!body.contains(pool), "the pool name: {body}");
        assert!(!body.to_lowercase().contains("busbar"), "{body}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_doors_governance_refusals_leak_no_internal_vocabulary() {
        const EXACT: &str = concat!(
            module_path!(),
            "::the_pools_doors_governance_refusals_leak_no_internal_vocabulary"
        );
        let Some(case) = crate::root::serve::door_tests::independent_cases(
            EXACT,
            &["restricted", "budget", "rate"],
        ) else {
            return;
        };
        let _one = ONE_PUBLISHER.lock().await;
        let far = seat_far_end(200, SERVED_BY_TWIN).await;
        let pool = "any-pool";
        let request = serde_json::json!({"model": pool, "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]});

        if case == "restricted" {
            let instance = "rehome-gate-leak-restricted";
            let restricted = rig(
                instance,
                RigOpts {
                    members: &[(far.port, 1)],
                    more_pools: &[(pool, &[(0, 1)])],
                    allowed_pools: Some(&["allowed-only"]),
                    ..RigOpts::default()
                },
            )
            .await;
            let (status, _, body) = restricted
                .send("POST", "/v1/chat/completions", Some(request.clone()))
                .await;
            assert_eq!(status, 403, "a pool the key may not reach");
            assert_leak_free(&String::from_utf8_lossy(&body), &restricted.key_id, pool);
            drop(restricted);
            crate::root::serve::door_tests::independent_case_done(EXACT, &case);
        }

        if case == "budget" {
            let empty = [limit(LimitMetric::Budget, 0, LimitWindow::Total)];
            let instance = "rehome-gate-leak-broke";
            let broke = rig(
                instance,
                RigOpts {
                    members: &[(far.port, 1)],
                    more_pools: &[(pool, &[(0, 1)])],
                    group: Some(("empty", &empty)),
                    ..RigOpts::default()
                },
            )
            .await;
            let (status, _, body) = broke
                .send("POST", "/v1/chat/completions", Some(request.clone()))
                .await;
            assert_eq!(status, 429, "a zero budget");
            assert_leak_free(&String::from_utf8_lossy(&body), &broke.key_id, pool);
            let converse_on_pool =
                door_word("converse_path").replacen("/p/", &format!("/{pool}/"), 1);
            let (status, _, body) = broke
                .send("POST", &converse_on_pool, Some(body_word("converse_body")))
                .await;
            let text = String::from_utf8_lossy(&body);
            assert_eq!(status, 400, "a zero budget on converse: {text}");
            assert!(text.contains("ServiceQuotaExceededException"), "{text}");
            assert_leak_free(&text, &broke.key_id, pool);
            drop(broke);
            crate::root::serve::door_tests::independent_case_done(EXACT, &case);
        }

        if case == "rate" {
            let closed = [limit(LimitMetric::Requests, 0, LimitWindow::Minute)];
            let instance = "rehome-gate-leak-throttled";
            let throttled = rig(
                instance,
                RigOpts {
                    members: &[(far.port, 1)],
                    more_pools: &[(pool, &[(0, 1)])],
                    group: Some(("closed", &closed)),
                    ..RigOpts::default()
                },
            )
            .await;
            let (status, head, body) = throttled
                .send("POST", "/v1/chat/completions", Some(request))
                .await;
            assert_eq!(status, 429, "no request a minute");
            assert!(head.contains_key("retry-after"), "{head:?}");
            assert_leak_free(&String::from_utf8_lossy(&body), &throttled.key_id, pool);
            crate::root::serve::door_tests::independent_case_done(EXACT, &case);
        }
        assert_eq!(far.served(), 0, "no refusal is dialled");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_an_ad_hoc_route_naming_the_wrong_provider() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-adhoc-mismatch";
        let (first, second) = (
            seat_far_end(200, SERVED_BY_TWIN).await,
            seat_far_end(200, SERVED_BY_TWIN).await,
        );
        let rig = rig(
            instance,
            RigOpts {
                members: &[(first.port, 1), (second.port, 1)],
                ..RigOpts::default()
            },
        )
        .await;
        let (status, head, body) = rig
            .send(
                "POST",
                &format!("/oai1/m0{}", door_word("messages_path")),
                Some(serde_json::json!({"model": "m0", "messages": [], "max_tokens": 16})),
            )
            .await;
        assert_eq!(status, 400, "{}", String::from_utf8_lossy(&body));
        assert!(
            field(&head, "content-type").is_some_and(|ct| ct.starts_with("application/json")),
            "{head:?}"
        );
        let answer = json_of(&body);
        assert_eq!(answer["type"], "error", "{answer}");
        assert_eq!(answer["error"]["type"], "invalid_request_error", "{answer}");
        assert_eq!((first.served(), second.served()), (0, 0));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_an_exhausted_rate_in_every_dialect() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-rate-every-dialect";
        let (rig, far) = over_rpm(instance).await;
        for (path, request) in every_dialect() {
            let (status, head, body) = rig.send("POST", path, Some(request)).await;
            assert_eq!(status, 429, "{path}: {}", String::from_utf8_lossy(&body));
            assert!(head.contains_key("retry-after"), "{path}: {head:?}");
            if path == door_word("generate_path") {
                assert_eq!(json_of(&body)["error"]["status"], "RESOURCE_EXHAUSTED");
            }
            if path == door_word("converse_path") {
                assert!(head.contains_key("x-amzn-errortype"), "{head:?}");
                assert!(head.contains_key("x-amzn-requestid"), "{head:?}");
            }
        }
        assert_eq!(far.served(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_a_spent_budget_in_every_dialect() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-budget-every-dialect";
        let (rig, far) = over_budget(instance).await;
        for (path, request) in every_dialect() {
            let (status, _, body) = rig.send("POST", path, Some(request)).await;
            if path == door_word("converse_path") {
                assert_eq!(status, 400, "{path}: {}", String::from_utf8_lossy(&body));
                assert_eq!(json_of(&body)["__type"], "ServiceQuotaExceededException");
            } else {
                assert_eq!(status, 429, "{path}: {}", String::from_utf8_lossy(&body));
            }
        }
        assert_eq!(far.served(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_fails_closed_on_a_key_naming_an_unconfigured_group() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-ghost-group";
        let far = seat_far_end(200, SERVED_BY_TWIN).await;
        let finance = [limit(LimitMetric::Budget, 0, LimitWindow::Total)];
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                group: Some(("finance", &finance)),
                key_group: Some("ghost"),
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, body) = rig.chat().await;
        let text = String::from_utf8_lossy(&body);
        assert_eq!(status, 429, "{text}");
        assert!(text.contains("group 'ghost' is not configured"), "{text}");
        assert_eq!(far.served(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_doors_missing_group_refusal_is_the_direct_admissions_bytes() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-gate-ghost-group-bytes";
        let far = seat_far_end(200, SERVED_BY_TWIN).await;
        let finance = [limit(LimitMetric::Budget, 0, LimitWindow::Total)];
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                group: Some(("finance", &finance)),
                key_group: Some("ghost"),
                ..RigOpts::default()
            },
        )
        .await;
        let key = rig.gov.lookup_by_sub(&rig.key_id).expect("the rig's key");
        let Err(blocked) = rig
            .gov
            .try_admit(&rig.app.cost, &key, "p", busbar_kernel::store::now())
        else {
            panic!("the direct admission refuses a key naming an unconfigured group");
        };
        let (direct_status, direct_kind, direct_message, _) =
            busbar_kernel::ingress::limit_refusal("", &blocked);

        assert_eq!(direct_kind, "insufficient_quota");

        let (status, _, body) = rig.chat().await;
        let text = String::from_utf8_lossy(&body);
        assert_eq!(status, direct_status.as_u16(), "{text}");
        let answer: serde_json::Value = serde_json::from_slice(&body).expect("the door's JSON");
        assert_eq!(
            answer,
            serde_json::json!({ "error": {
                "message": direct_message,
                "type": direct_kind,
                "param": null,
                "code": direct_kind,
            } }),
            "the whole refusal is the direct admission's, nothing reworded or added: {text}"
        );
        assert_eq!(far.served(), 0);
    }
}

#[cfg(linked_driver_proofs)]
const STREAM_HEAD: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                           transfer-encoding: chunked\r\nconnection: close\r\n\r\n";

#[cfg(linked_driver_proofs)]
const FRAMES: [&str; 4] = [
    r#"{"id":"c1","object":"chat.completion.chunk","created":1,"model":"m0","choices":[{"index":0,"delta":{"role":"assistant","content":"hi"},"finish_reason":null}]}"#,
    r#"{"id":"c1","object":"chat.completion.chunk","created":1,"model":"m0","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
    r#"{"id":"c1","object":"chat.completion.chunk","created":1,"model":"m0","choices":[],"usage":{"prompt_tokens":11,"completion_tokens":7,"total_tokens":18}}"#,
    "[DONE]",
];

#[cfg(linked_driver_proofs)]
fn frame(data: &str) -> Vec<u8> {
    chunk(format!("data: {data}\n\n").as_bytes())
}

#[cfg(linked_driver_proofs)]
fn stream_pausing(pause_ms: u64) -> SeatScript {
    SeatScript {
        head: STREAM_HEAD.to_string(),
        pieces: vec![
            (0, frame(FRAMES[0])),
            (pause_ms, frame(FRAMES[1])),
            (0, frame(FRAMES[2])),
            (0, frame(FRAMES[3])),
        ],
        finish: Some(b"0\r\n\r\n".to_vec()),
    }
}

#[cfg(linked_driver_proofs)]
fn streamed_chat() -> serde_json::Value {
    serde_json::json!({"model": "p", "stream": true, "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]})
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_bounds_a_stream_by_the_stream_ceiling_not_the_pools_timeout() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-stream-outlives-timeout";
    let far = far_end_scripted(stream_pausing(3_000)).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            failover_timeout_secs: Some(2),
            ..RigOpts::default()
        },
    )
    .await;
    let (status, _, body) = rig
        .send("POST", "/v1/chat/completions", Some(streamed_chat()))
        .await;
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("[DONE]") && body.contains("\"hi\""),
        "the stream outlived the pool's timeout whole: {body}"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_cuts_a_stream_at_the_stream_ceiling() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-stream-ceiling";
    let far = far_end_scripted(stream_pausing(4_000)).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            failover_timeout_secs: Some(60),
            stream_ceiling_secs: Some(1),
            ..RigOpts::default()
        },
    )
    .await;
    let started = std::time::Instant::now();
    let (status, _, body) = rig
        .send("POST", "/v1/chat/completions", Some(streamed_chat()))
        .await;
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, 200, "the stream had begun: {body}");
    assert!(
        body.contains("\"hi\""),
        "the first frame reached the caller: {body}"
    );
    assert!(!body.contains("[DONE]"), "the ceiling cut it: {body}");
    assert!(
        started.elapsed() < std::time::Duration::from_millis(3_500),
        "cut at the one-second ceiling, not when the far end ended: {:?}",
        started.elapsed()
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_keeps_the_budget_unit_of_a_stream_cut_after_its_first_byte() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-stream-cut";
    let far = far_end_scripted(SeatScript {
        head: STREAM_HEAD.to_string(),
        pieces: vec![(0, frame(FRAMES[0])), (50, frame(FRAMES[1]))],
        finish: None,
    })
    .await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            lane_budget: Some(5),
            ..RigOpts::default()
        },
    )
    .await;
    let (status, _, body) = rig
        .send("POST", "/v1/chat/completions", Some(streamed_chat()))
        .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        rig.app.store.lane_budget_remaining(0),
        Some(4),
        "the streamed bytes were delivered: the unit is spent, not refunded"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_refunds_the_budget_unit_of_a_buffered_answer_cut() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-buffered-cut";
    let far = far_end_scripted(SeatScript {
        head: format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n",
            WHOLE.len()
        ),
        pieces: vec![(0, WHOLE.as_bytes()[..40].to_vec())],
        finish: None,
    })
    .await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            lane_budget: Some(5),
            ..RigOpts::default()
        },
    )
    .await;
    let _ = rig.chat().await;
    assert_eq!(rig.app.store.lane_budget_remaining(0), Some(5));
}

#[cfg(linked_driver_proofs)]
const WHOLE: &str = r#"{"id":"chatcmpl-9","object":"chat.completion","created":0,"model":"m0","choices":[{"index":0,"message":{"role":"assistant","content":"hello there"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1500,"completion_tokens":90,"total_tokens":1590}}"#;

#[cfg(linked_driver_proofs)]
fn whole_cut_after(prefix: usize, hold_ms: u64) -> SeatScript {
    SeatScript {
        head: format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n",
            WHOLE.len()
        ),
        pieces: vec![
            (0, WHOLE.as_bytes()[..prefix].to_vec()),
            (hold_ms, Vec::new()),
        ],
        finish: None,
    }
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_bills_a_cut_non_stream_answer_only_what_its_far_end_reported() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-nonstream-cut";
    let far = far_end_scripted(whole_cut_after(WHOLE.len() - 1, 0)).await;
    let late = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            ..RigOpts::default()
        },
    )
    .await;
    let _ = late.chat().await;
    assert_eq!(late.tokens_after().await, 1590, "the far end's own report");
    drop(late);

    let instance = "serve-door-nonstream-cut-early";
    let usage_at = WHOLE.find(r#","usage""#).expect("the usage member");
    let far = far_end_scripted(whole_cut_after(usage_at, 0)).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            ..RigOpts::default()
        },
    )
    .await;
    let _ = rig.chat().await;
    assert_eq!(rig.tokens_after().await, 0, "no report, no charge");
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_bills_nothing_for_a_non_stream_answer_the_caller_dropped_mid_relay() {
    use http_body_util::BodyExt as _;
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-nonstream-drop";
    let usage_at = WHOLE.find(r#","usage""#).expect("the usage member");
    let far = far_end_scripted(whole_cut_after(usage_at, 5_000)).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            ..RigOpts::default()
        },
    )
    .await;
    let response = rig
        .open(
            "POST",
            "/v1/chat/completions",
            Some(serde_json::json!({"model": "p", "max_tokens": 16,
                "messages": [{"role": "user", "content": "hi"}]})),
            &[],
        )
        .await;
    assert_eq!(response.status().as_u16(), 200);
    let mut body = response.into_body();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
        .await
        .expect("the first bytes arrive before the far end's hold")
        .expect("a frame")
        .expect("relayed");
    assert!(first.data_ref().is_some_and(|b| !b.is_empty()));

    drop(body);

    let rig = std::sync::Arc::new(rig);
    let next = {
        let rig = std::sync::Arc::clone(&rig);
        tokio::spawn(async move { rig.chat().await })
    };
    assert_eq!(
        rig.tokens_after().await,
        0,
        "a non-stream answer dropped mid-relay bills no tokens, as in 1.5.5"
    );
    next.abort();
}

#[cfg(linked_driver_proofs)]
mod pools_door_money {
    use super::*;

    use std::sync::Arc;

    use axum::http::StatusCode;
    use busbar_kernel::config::groups::{LimitCfg, LimitMetric, LimitWindow};

    use super::super::hook_seat_tests::DoorRig;

    fn now() -> u64 {
        busbar_kernel::store::now()
    }

    async fn mock(replies: Vec<ProviderReply>) -> (Arc<ProviderRequests>, ProviderEndpoint) {
        let state = Arc::new(ProviderRequests::new());
        for reply in replies.into_iter().rev() {
            state.push(reply);
        }
        let server = ProviderEndpoint::new(Arc::clone(&state)).await;
        (state, server)
    }

    fn limit(metric: LimitMetric, amount: u64, per: LimitWindow) -> LimitCfg {
        LimitCfg {
            metric,
            amount,
            per: Some(per),
            scope: None,
            on_exhaust: None,
            downgrade_to: None,
            admission: None,
            on_exhaustion: None,
        }
    }

    fn answer_1590() -> &'static str {
        door_word("messages_answer_usage")
    }

    fn cut_after(body: &str, prefix: usize, hold_ms: u64) -> SeatScript {
        SeatScript {
            head: format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n",
                body.len()
            ),
            pieces: vec![
                (0, body.as_bytes()[..prefix].to_vec()),
                (hold_ms, Vec::new()),
            ],
            finish: None,
        }
    }

    fn messages_on_p(stream: bool) -> serde_json::Value {
        serde_json::json!({"model": "p", "max_tokens": 50, "stream": stream,
            "messages": [{"role": "user", "content": "hi"}]})
    }

    async fn messages_member(instance: &'static str, port: u16) -> DoorRig {
        rig(
            instance,
            RigOpts {
                members: &[(port, 1)],
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await
    }

    async fn sealed_as(
        rig: &DoorRig,
        finish: busbar_kernel_audit::FinishClass,
    ) -> busbar_kernel_audit::AuditRecord {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            if let Some(record) = rig
                .records()
                .into_iter()
                .find(|record| record.outcome.finish == finish)
            {
                return record;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "a record sealed {finish:?}: {:?}",
                rig.records()
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    fn token_lines(
        record: &busbar_kernel_audit::AuditRecord,
    ) -> Vec<(String, busbar_contract::Count)> {
        record
            .usage
            .lines
            .iter()
            .filter(|line| busbar_contract::records::RESERVED_UNITS.contains(&line.class.as_str()))
            .map(|line| (line.class.as_str().to_string(), line.quantity))
            .collect()
    }

    fn reported(record: &busbar_kernel_audit::AuditRecord) -> busbar_contract::Count {
        busbar_contract::Count::total(
            token_lines(record)
                .into_iter()
                .map(|(_, quantity)| quantity),
        )
        .expect("the reported fixture counts fit exactly")
    }

    async fn inside_one_minute() {
        let into_window = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("after the epoch")
            .as_secs()
            % 60;
        if into_window >= 55 {
            tokio::time::sleep(std::time::Duration::from_secs(60 - into_window + 1)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_bills_nothing_for_a_whole_answer_its_caller_dropped() {
        use http_body_util::BodyExt as _;
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-money-dropped-floor";
        let whole = answer_1590();
        let prefix = whole.find(r#""stop_reason""#).expect("a stop reason");
        let far = far_end_scripted(cut_after(whole, prefix, 5_000)).await;
        let rig = messages_member(instance, far.port).await;
        let response = rig
            .open(
                "POST",
                door_word("messages_path"),
                Some(messages_on_p(false)),
                &[],
            )
            .await;
        assert_eq!(response.status().as_u16(), 200);
        let mut body = response.into_body();
        let mut relayed: Vec<u8> = Vec::new();
        while relayed.len() < prefix {
            let piece = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
                .await
                .expect("the prefix arrives before the far end's hold")
                .expect("a frame")
                .expect("relayed");
            if let Some(bytes) = piece.data_ref() {
                relayed.extend_from_slice(bytes);
            }
        }
        assert_eq!(
            relayed,
            whole.as_bytes()[..prefix].to_vec(),
            "the prefix relays verbatim"
        );

        drop(body);

        let rig = Arc::new(rig);
        let next = {
            let rig = Arc::clone(&rig);
            tokio::spawn(async move { rig.chat().await })
        };
        let billed = rig.tokens_after().await;
        next.abort();
        assert_eq!(
            billed, 0,
            "a whole answer dropped mid-relay bills no tokens"
        );
        let record = sealed_as(&rig, busbar_kernel_audit::FinishClass::Error).await;
        assert_eq!(
            reported(&record),
            0,
            "the record reports what the ledger accrued: {:?}",
            token_lines(&record)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_bills_a_whole_answer_its_own_usage_once() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-money-whole-once";
        let far = seat_far_end(200, answer_1590()).await;
        let rig = messages_member(instance, far.port).await;
        let (status, _, body) = rig
            .send(
                "POST",
                door_word("messages_path"),
                Some(messages_on_p(false)),
            )
            .await;
        assert_eq!(status, 200);
        assert_eq!(
            body,
            answer_1590().as_bytes(),
            "relayed whole, as one answer"
        );
        assert_eq!(
            rig.tokens_after().await,
            1590,
            "the far end's own figure, once"
        );
        let record = sealed_as(&rig, busbar_kernel_audit::FinishClass::Complete).await;
        assert_eq!(reported(&record), 1590);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_bills_nothing_for_a_whole_answer_cut_before_its_usage() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-money-cut-before-usage";
        let whole = answer_1590();
        let prefix = whole.find(r#""stop_reason""#).expect("a stop reason");
        let far = far_end_scripted(cut_after(whole, prefix, 0)).await;
        let rig = messages_member(instance, far.port).await;
        let _ = rig
            .send(
                "POST",
                door_word("messages_path"),
                Some(messages_on_p(false)),
            )
            .await;
        assert_eq!(rig.tokens_after().await, 0, "no reported usage, no charge");
        let record = sealed_as(&rig, busbar_kernel_audit::FinishClass::Error).await;
        assert_eq!(
            reported(&record),
            0,
            "the record reports no usage either: {:?}",
            token_lines(&record)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_bills_what_a_cut_whole_answer_reported() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-money-cut-after-usage";
        let whole = answer_1590();
        let far = far_end_scripted(cut_after(whole, whole.len() - 1, 0)).await;
        let rig = messages_member(instance, far.port).await;
        let _ = rig
            .send(
                "POST",
                door_word("messages_path"),
                Some(messages_on_p(false)),
            )
            .await;
        assert_eq!(rig.tokens_after().await, 1590, "the far end's own report");
        let _ = sealed_as(&rig, busbar_kernel_audit::FinishClass::Error).await;
    }

    fn chat_160() -> serde_json::Value {
        serde_json::json!({
            "model": "glm-4.5",
            "choices": [{"index": 0, "message": {"role": "assistant",
                "content": "Hello there friend"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 60}
        })
    }

    async fn thirty_tokens_a_minute(
        instance: &'static str,
        replies: Vec<ProviderReply>,
    ) -> (DoorRig, ProviderEndpoint) {
        let (_state, server) = mock(replies).await;
        let limits = [limit(LimitMetric::Tokens, 30, LimitWindow::Minute)];
        let rig = rig(
            instance,
            RigOpts {
                members: &[(server.address().port(), 1)],
                group: Some(("tpmgrp", &limits)),
                allowed_pools: Some(&["p"]),
                ..RigOpts::default()
            },
        )
        .await;
        (rig, server)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_charges_a_translated_answers_tokens_against_the_token_limit() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-money-tpm-whole";
        inside_one_minute().await;
        let ok = || ProviderReply::Ok {
            status: StatusCode::OK,
            body: chat_160(),
        };
        let (rig, _server) = thirty_tokens_a_minute(instance, vec![ok(), ok()]).await;
        let (status, _, body) = rig
            .send(
                "POST",
                door_word("messages_path"),
                Some(messages_on_p(false)),
            )
            .await;
        assert_eq!(
            status,
            200,
            "under the limit: {}",
            String::from_utf8_lossy(&body)
        );
        let answer: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(answer["model"], "glm-4.5", "the far end's model preserved");
        let _ = rig.tokens_after().await;
        let (status, _, _) = rig
            .send(
                "POST",
                door_word("messages_path"),
                Some(messages_on_p(false)),
            )
            .await;
        assert_eq!(status, 429, "the charged tokens make the limit refuse");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_charges_a_translated_stream_tokens_against_the_token_limit() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-money-tpm-stream";
        inside_one_minute().await;
        let streamed = || {
            event_stream(vec![
                r#"{"choices":[{"delta":{"role":"assistant"}}]}"#.to_string(),
                r#"{"choices":[{"delta":{"content":"Hello there friend"}}]}"#.to_string(),
                r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":60}}"#.to_string(),
            ])
        };
        let (rig, _server) = thirty_tokens_a_minute(instance, vec![streamed(), streamed()]).await;
        let (status, _, body) = rig
            .send(
                "POST",
                door_word("messages_path"),
                Some(messages_on_p(true)),
            )
            .await;
        assert_eq!(
            status,
            200,
            "under the limit: {}",
            String::from_utf8_lossy(&body)
        );
        let _ = rig.tokens_after().await;
        let (status, _, _) = rig
            .send(
                "POST",
                door_word("messages_path"),
                Some(messages_on_p(true)),
            )
            .await;
        assert_eq!(
            status, 429,
            "the charged stream-end tokens make the limit refuse"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_keeps_the_budget_unit_of_an_over_cap_answer() {
        let _one = ONE_PUBLISHER.lock().await;
        const CAP: usize = 65_536;
        struct RestoreLimits(Option<busbar_kernel::config::limits::LimitsResolved>);
        impl Drop for RestoreLimits {
            fn drop(&mut self) {
                busbar_kernel::config::limits::swap_installed(self.0.take());
            }
        }
        let _cap = RestoreLimits(busbar_kernel::config::limits::swap_installed(Some(
            busbar_kernel::config::limits::LimitsResolved::with_request_body_max_bytes(CAP),
        )));
        let instance = "rehome-money-over-cap";
        let (_state, server) = mock(vec![ProviderReply::Ok {
            status: StatusCode::OK,
            body: serde_json::json!({
                "id": "chatcmpl-huge",
                "object": "chat.completion",
                "created": 1234567890,
                "model": "m0",
                "choices": [{"index": 0, "message": {"role": "assistant",
                    "content": "x".repeat(CAP + 1024)}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 5, "completion_tokens": 999999}
            }),
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(server.address().port(), 1)],
                lane_budget: Some(1),

                request_body_max_bytes: Some(CAP),
                ..RigOpts::default()
            },
        )
        .await;
        assert_eq!(rig.app.store.lane_budget_remaining(0), Some(1));
        let (status, _, _) = rig
            .send(
                "POST",
                door_word("messages_path"),
                Some(messages_on_p(false)),
            )
            .await;
        assert_eq!(status, 500, "over the translation cap");
        assert_eq!(
            rig.app.store.lane_budget_remaining(0),
            Some(0),
            "an over-cap body is our cap, not the far end's fault: never refunded"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_counts_a_pool_refusal_and_charges_no_fee() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-money-pool-refusal";
        let far = seat_far_end(200, SERVED_BY_TWIN).await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                allowed_pools: Some(&["denied-elsewhere"]),
                ..RigOpts::default()
            },
        )
        .await;
        let duration_count = format!(
            "{}_count",
            busbar_kernel::snapshot::REQUEST_DURATION_SECONDS
        );
        let counted = || {
            (
                metric_sum(
                    busbar_kernel::snapshot::REQUESTS_TOTAL,
                    &[("outcome", "client_error")],
                ),
                metric_sum(&duration_count, &[]),
            )
        };
        let before = counted();
        assert_eq!(rig.chat().await.0, 403, "a pool the key may not reach");
        let after = counted();
        assert!(
            after.0 > before.0,
            "outcome=client_error: {before:?} -> {after:?}"
        );
        assert!(
            after.1 > before.1,
            "its duration observed: {before:?} -> {after:?}"
        );
        let (_, spend, _, billable) = rig.ledger_after(0).await;
        assert_eq!((spend, billable), (0, 0), "no fee for a refused request");
        assert_eq!(far.served(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_names_the_blocking_group_and_records_nothing() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-money-finance-blocked";
        let far = seat_far_end(200, SERVED_BY_TWIN).await;
        let finance = [limit(LimitMetric::Budget, 0, LimitWindow::Total)];
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                group: Some(("finance", &finance)),
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, body) = rig.chat().await;
        let text = String::from_utf8_lossy(&body);
        assert_eq!(status, 429, "{text}");
        assert!(text.contains("insufficient_quota"), "{text}");
        assert!(
            text.contains("group 'finance' budget per total exhausted"),
            "the blocking bucket is named: {text}"
        );
        assert!(!text.contains(&rig.key_id), "never the key id: {text}");
        let (requests, _, _, _) = rig.ledger_after(0).await;
        assert_eq!(requests, 0, "the refused request is not recorded");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_bills_zero_tokens_for_a_stream_with_no_usage_in_every_dialect() {
        let _one = ONE_PUBLISHER.lock().await;
        let streamed = |mut body: serde_json::Value| {
            body["stream"] = serde_json::Value::Bool(true);
            body
        };
        let chat = serde_json::json!({"model": "p", "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]});
        let word_body = |key: &str| -> serde_json::Value {
            serde_json::from_str(door_word(key)).expect("a JSON request body")
        };
        let dialects = [
            (
                "rehome-money-no-usage-chat",
                "/v1/chat/completions",
                streamed(chat.clone()),
            ),
            (
                "rehome-money-no-usage-messages",
                door_word("messages_path"),
                streamed(chat.clone()),
            ),
            (
                "rehome-money-no-usage-chat-v2",
                door_word("chat_v2_path"),
                streamed(word_body("chat_v2_body")),
            ),
            (
                "rehome-money-no-usage-respond",
                door_word("respond_path"),
                streamed(word_body("respond_body")),
            ),
            (
                "rehome-money-no-usage-generate",
                door_word("generate_stream_path"),
                word_body("generate_body"),
            ),
            (
                "rehome-money-no-usage-converse",
                door_word("converse_stream_path"),
                word_body("converse_body"),
            ),
        ];
        for (instance, path, request) in dialects {
            let (_state, server) = mock(vec![event_stream(vec![
                r#"{"id":"chatcmpl-nousage","object":"chat.completion.chunk","created":0,"model":"m0","choices":[{"index":0,"delta":{"role":"assistant","content":"hello"},"finish_reason":null}]}"#.to_string(),
                r#"{"id":"chatcmpl-nousage","object":"chat.completion.chunk","created":0,"model":"m0","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#.to_string(),
            ])])
            .await;
            let rig = rig(
                instance,
                RigOpts {
                    members: &[(server.address().port(), 1)],
                    ..RigOpts::default()
                },
            )
            .await;
            let charged_at = now();
            let (status, _, body) = rig.send("POST", path, Some(request)).await;
            assert_eq!(status, 200, "{path}: {}", String::from_utf8_lossy(&body));
            let (requests, spend, _, billable) = rig.ledger_after(1).await;
            assert_eq!(requests, 1, "{path}");
            assert_eq!(
                rig.tokens_after().await,
                0,
                "{path}: no usage frame, no tokens"
            );
            assert_eq!(
                spend,
                i64::try_from(billable).expect("a count"),
                "{path}: the fee alone, no token spend"
            );
            rig.gov.flush_metering().await;
            let rows = rig
                .gov
                .metering_for_async(busbar_kernel::governance::metering_bucket(charged_at))
                .await
                .expect("metering read");
            let mine: Vec<_> = rows.iter().filter(|row| row.key_id == rig.key_id).collect();
            assert_eq!(mine.len(), 1, "{path}: one metering row: {rows:?}");
            assert_eq!(mine[0].model, "m0", "{path}");
            assert_eq!(mine[0].requests, 1, "{path}");
            assert_eq!(
                (
                    mine[0].tokens_input,
                    mine[0].tokens_output,
                    mine[0].tokens_cache_read,
                    mine[0].tokens_cache_write
                ),
                (0, 0, 0, 0),
                "{path}: every token tier is zero"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_refuses_a_spent_budget_without_charging_anything() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-money-over-budget-untouched";
        let far = seat_far_end(200, SERVED_BY_TWIN).await;
        let limits = [limit(LimitMetric::Budget, 100, LimitWindow::Total)];
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                group: Some(("bgrp", &limits)),
                seed: &[("group:bgrp@total", 250)],
                ..RigOpts::default()
            },
        )
        .await;
        let group_before = rig.bucket("group:bgrp@total").await;
        assert_eq!(
            group_before,
            (250, 250, 250, 250),
            "250 seeded requests at a one-cent fee: 250 cents over a 100-cent cap"
        );
        let key_before = rig.ledger_after(0).await;
        assert_eq!(key_before, (0, 0, 0, 0), "the key has been charged nothing");
        let (status, _, _) = rig.chat().await;
        assert_eq!(status, 429, "an exhausted budget is a 429");
        assert_eq!(
            rig.bucket("group:bgrp@total").await,
            group_before,
            "the refusal moved nothing on the blocking bucket"
        );
        assert_eq!(
            rig.ledger_after(0).await,
            key_before,
            "nor on the key's own"
        );
        assert_eq!(far.served(), 0);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while rig.audit_records() == 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let records = rig.records();
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(
            records[0].outcome.unit_end,
            busbar_contract::caps::Outcome::Refused(
                busbar_contract::caps::StepName::Admit,
                busbar_contract::caps::ReasonCode::OverBudget
            ),
            "refused at the door, for the budget"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_leaves_a_streamed_and_a_failed_units_money_where_1_5_5_left_it() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-money-streamed-row";
        let far = far_end_scripted(stream_pausing(0)).await;
        let streamed = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                ..RigOpts::default()
            },
        )
        .await;
        let charged_at = now();
        let (status, _, _) = streamed
            .send("POST", "/v1/chat/completions", Some(streamed_chat()))
            .await;
        assert_eq!(status, 200);
        let (requests, _, _, _) = streamed.ledger_after(1).await;
        assert_eq!(requests, 1, "one admitted request");
        assert_eq!(
            streamed.tokens_after().await,
            18,
            "the stream-end report, once"
        );
        streamed.gov.flush_metering().await;
        let rows = streamed
            .gov
            .metering_for_async(busbar_kernel::governance::metering_bucket(charged_at))
            .await
            .expect("metering read");
        let mine: Vec<String> = rows
            .iter()
            .filter(|row| row.key_id == streamed.key_id)
            .map(|row| {
                assert_eq!(row.scale, busbar_contract::count::COUNT_SCALE);
                format!(
                    "{} in={} out={} cr={} cw={} req={} billable={}",
                    row.model,
                    row.tokens_input,
                    row.tokens_output,
                    row.tokens_cache_read,
                    row.tokens_cache_write,
                    row.requests,
                    row.billable_requests
                )
            })
            .collect();
        assert_eq!(
            mine,
            vec![format!(
                "m0 in={} out={} cr=0 cw=0 req=1 billable=1",
                busbar_contract::Count::from_integer(11)
                    .expect("eleven whole input units fit")
                    .micros(),
                busbar_contract::Count::from_integer(7)
                    .expect("seven whole output units fit")
                    .micros(),
            )]
        );
        drop(streamed);

        let instance = "rehome-money-failed-row";
        let down = seat_far_end(503, OVERLOADED).await;
        let failed = rig(
            instance,
            RigOpts {
                members: &[(down.port, 1)],
                ..RigOpts::default()
            },
        )
        .await;
        let charged_at = now();
        assert_ne!(failed.chat().await.0, 200, "the far end fails");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        let mut ledger = failed.ledger_after(0).await;
        while ledger.0 == 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            ledger = failed.ledger_after(0).await;
        }
        assert_eq!(ledger.0, 1, "the failed unit keeps its admitted request");
        assert_eq!(failed.tokens_after().await, 0, "and accrues no tokens");
        failed.gov.flush_metering().await;
        let rows = failed
            .gov
            .metering_for_async(busbar_kernel::governance::metering_bucket(charged_at))
            .await
            .expect("metering read");
        assert!(
            rows.iter().all(|row| row.key_id != failed.key_id),
            "no metering row for a unit that delivered nothing: {rows:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_bills_a_dead_stream_tokens_and_keeps_its_fee() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-money-dead-stream";
        let far = far_end_scripted(SeatScript {
            head: STREAM_HEAD.to_string(),
            pieces: vec![
                (0, frame(FRAMES[0])),
                (0, frame(FRAMES[2])),
                (50, Vec::new()),
            ],
            finish: None,
        })
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, _) = rig
            .send("POST", "/v1/chat/completions", Some(streamed_chat()))
            .await;
        assert_eq!(status, 200, "the stream had begun");
        let (requests, _, _, billable) = rig.ledger_after(1).await;
        assert_eq!(requests, 1, "the request unit still counts");
        assert_eq!(billable, 1, "the fee it earned is kept");
        assert_eq!(
            rig.tokens_after().await,
            18,
            "the streamed tokens are the charge"
        );
        let record = sealed_as(&rig, busbar_kernel_audit::FinishClass::Partial).await;
        let mut lines = token_lines(&record);
        lines.retain(|(_, quantity)| *quantity > 0);
        lines.sort();
        assert_eq!(
            lines,
            vec![
                (busbar_contract::records::UNIT_INPUT.to_string(), 11.into()),
                (busbar_contract::records::UNIT_OUTPUT.to_string(), 7.into())
            ],
            "one line per streamed tier"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_seals_a_2xx_stream_with_the_end_its_tap_saw() {
        for (instance, pieces, finish) in [
            (
                "rehome-sealed-end-cut",
                vec![(0, frame(FRAMES[0])), (50, Vec::new())],
                busbar_kernel_audit::FinishClass::Partial,
            ),
            (
                "rehome-sealed-end-failed",
                vec![(50, Vec::new())],
                busbar_kernel_audit::FinishClass::Error,
            ),
        ] {
            let _one = ONE_PUBLISHER.lock().await;
            let far = far_end_scripted(SeatScript {
                head: STREAM_HEAD.to_string(),
                pieces,
                finish: None,
            })
            .await;
            let rig = rig(
                instance,
                RigOpts {
                    members: &[(far.port, 1)],
                    ..RigOpts::default()
                },
            )
            .await;
            let (status, _, _) = rig
                .send("POST", "/v1/chat/completions", Some(streamed_chat()))
                .await;
            assert_eq!(status, 200, "{instance}: served on 2xx headers");
            let record = sealed_as(&rig, finish).await;
            assert_ne!(
                record.outcome.finish,
                busbar_kernel_audit::FinishClass::Complete,
                "{instance}"
            );
        }
    }
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_pins_a_session_to_one_member() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-affinity";
    let fars = [
        seat_far_end(200, SERVED_BY_TWIN).await,
        seat_far_end(200, SERVED_BY_TWIN).await,
        seat_far_end(200, SERVED_BY_TWIN).await,
    ];
    let rig = rig(
        instance,
        RigOpts {
            members: &[(fars[0].port, 1), (fars[1].port, 1), (fars[2].port, 1)],
            ..RigOpts::default()
        },
    )
    .await;
    let served = || fars.iter().map(|f| f.served()).collect::<Vec<_>>();
    let chat = serde_json::json!({"model": "p", "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]});
    for _ in 0..6 {
        let r = rig
            .open(
                "POST",
                "/v1/chat/completions",
                Some(chat.clone()),
                &[("x-session-id", "session-42")],
            )
            .await;
        assert_eq!(r.status().as_u16(), 200);
        let _ = axum::body::to_bytes(r.into_body(), 1 << 20).await;
    }
    let pinned = served();
    assert_eq!(
        pinned.iter().filter(|n| **n == 6).count(),
        1,
        "one member served the whole session: {pinned:?}"
    );

    let system = serde_json::json!({"model": "p", "max_tokens": 16, "system": "be brief",
        "messages": [{"role": "user", "content": "hi"}]});
    let before = served();
    for _ in 0..3 {
        assert_eq!(
            rig.send("POST", "/v1/chat/completions", Some(system.clone()))
                .await
                .0,
            200
        );
    }
    let after = served();
    let moved: Vec<usize> = after.iter().zip(&before).map(|(a, b)| a - b).collect();
    assert_eq!(
        moved.iter().filter(|n| **n == 3).count(),
        1,
        "one member served the system-keyed session: {moved:?}"
    );

    let before = served();
    for _ in 0..3 {
        assert_eq!(rig.chat().await.0, 200);
    }
    let after = served();
    let moved = after.iter().zip(&before).filter(|(a, b)| a > b).count();
    assert!(moved >= 2, "unkeyed units spread: {before:?} -> {after:?}");
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_reads_a_session_from_the_pools_own_affinity_header() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-affinity-header";
    let fars = [
        seat_far_end(200, SERVED_BY_TWIN).await,
        seat_far_end(200, SERVED_BY_TWIN).await,
        seat_far_end(200, SERVED_BY_TWIN).await,
    ];
    let rig = rig(
        instance,
        RigOpts {
            members: &[(fars[0].port, 1), (fars[1].port, 1), (fars[2].port, 1)],
            affinity_header: Some("x-user-id"),
            ..RigOpts::default()
        },
    )
    .await;
    let served = || fars.iter().map(|f| f.served()).collect::<Vec<_>>();
    let chat = serde_json::json!({"model": "p", "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]});
    let send = |name: &'static str, value: &'static str| {
        let (rig, chat) = (&rig, chat.clone());
        async move {
            let r = rig
                .open("POST", "/v1/chat/completions", Some(chat), &[(name, value)])
                .await;
            assert_eq!(r.status().as_u16(), 200);
            let _ = axum::body::to_bytes(r.into_body(), 1 << 20).await;
        }
    };
    for _ in 0..4 {
        send("x-user-id", "user-7").await;
    }
    assert_eq!(
        served().iter().filter(|n| **n == 4).count(),
        1,
        "the pool's header pins: {:?}",
        served()
    );
    let before = served();
    for _ in 0..3 {
        send("x-session-id", "session-42").await;
    }
    let after = served();
    let moved = after.iter().zip(&before).filter(|(a, b)| a > b).count();
    assert!(
        moved >= 2,
        "the default header is not this pool's key: {before:?} -> {after:?}"
    );
}

#[cfg(linked_driver_proofs)]
mod pools_door_counts_and_routes {
    use super::*;

    use std::sync::Arc;

    use axum::http::StatusCode;
    use busbar_contract::Signal;
    use busbar_kernel::store::BreakerState;

    use super::super::hook_seat_tests::DoorRig;

    fn now() -> u64 {
        busbar_kernel::store::now()
    }

    async fn mock(replies: Vec<ProviderReply>) -> (Arc<ProviderRequests>, ProviderEndpoint) {
        let state = Arc::new(ProviderRequests::new());
        for reply in replies.into_iter().rev() {
            state.push(reply);
        }
        let server = ProviderEndpoint::new(Arc::clone(&state)).await;
        (state, server)
    }

    fn messages_answer() -> serde_json::Value {
        serde_json::from_str(door_word("messages_answer")).expect("the answer is JSON")
    }

    fn messages_naming(model: &str) -> serde_json::Value {
        serde_json::json!({"model": model, "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]})
    }

    async fn send_bytes(rig: &DoorRig, path: &str, body: &str) -> u16 {
        use tower::ServiceExt as _;
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("authorization", format!("Bearer {}", rig.token))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(body.to_string()))
            .expect("a request");
        let response = rig
            .router
            .clone()
            .oneshot(req)
            .await
            .expect("the router answers");
        let status = response.status().as_u16();
        let _ = axum::body::to_bytes(response.into_body(), 1 << 20).await;
        status
    }

    fn unresolved_client_errors() -> f64 {
        metric_sum(
            busbar_kernel::snapshot::REQUESTS_TOTAL,
            &[("pool", "unresolved"), ("outcome", "client_error")],
        )
    }

    fn refusing_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a loopback port");
        listener.local_addr().expect("its address").port()
    }

    async fn serving(instance: &'static str) -> (DoorRig, super::super::hook_seat_tests::FarEnd) {
        let far = seat_far_end(200, SERVED_BY_TWIN).await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                ..RigOpts::default()
            },
        )
        .await;
        (rig, far)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_counts_times_and_attributes_a_served_unit() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-count-served";
        let (rig, _far) = serving(instance).await;
        let duration_count = format!(
            "{}_count",
            busbar_kernel::snapshot::REQUEST_DURATION_SECONDS
        );
        let requests = || {
            metric_sum(
                busbar_kernel::snapshot::REQUESTS_TOTAL,
                &[("pool", "p"), ("outcome", "ok")],
            )
        };
        let timed = || metric_sum(&duration_count, &[("pool", "p")]);
        let attempts = || {
            metric_sum(
                busbar_kernel::telemetry::UPSTREAM_ATTEMPTS_TOTAL,
                &[("pool", "p"), ("lane", "m0")],
            )
        };
        let before = (requests(), timed(), attempts());
        assert_eq!(rig.chat().await.0, 200);
        let _ = rig.ledger_after(1).await;
        let after = (requests(), timed(), attempts());
        assert!(
            after.0 > before.0,
            "outcome=ok counted: {before:?} -> {after:?}"
        );
        assert!(
            after.1 > before.1,
            "its duration observed: {before:?} -> {after:?}"
        );
        assert!(
            after.2 > before.2,
            "its attempt counted: {before:?} -> {after:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_counts_an_unknown_model_under_the_bounded_label() {
        const CHILD: &str = "BUSBAR_T065_UNKNOWN_MODEL_CHILD";
        if std::env::var_os(CHILD).is_none() {
            struct Child(std::process::Child);
            impl Drop for Child {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            let module = module_path!().split_once("::").expect("crate and module").1;
            let test =
                format!("{module}::the_pools_door_counts_an_unknown_model_under_the_bounded_label");
            let mut child = Child(
                std::process::Command::new(std::env::current_exe().expect("the test binary"))
                    .args(["--exact", &test, "--test-threads", "1"])
                    .env(CHILD, "1")
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .expect("the isolated test starts"),
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            let status = loop {
                if let Some(status) = child.0.try_wait().expect("read child status") {
                    break status;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "isolated metric test timed out"
                );
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            };
            let mut output = String::new();
            std::io::Read::read_to_string(
                &mut child.0.stdout.take().expect("child output"),
                &mut output,
            )
            .expect("read child output");
            assert!(status.success(), "isolated metric test failed: {output}");
            assert!(
                output.contains("1 passed"),
                "exact child ran no test: {output}"
            );
            return;
        }
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-count-unknown-model";
        let (rig, _far) = serving(instance).await;
        let probe = "zzz-unbounded-cardinality-probe-9f3a";
        let before = unresolved_client_errors();
        let (status, _, _) = rig
            .send(
                "POST",
                "/v1/chat/completions",
                Some(serde_json::json!({"model": probe,
                    "messages": [{"role": "user", "content": "hi"}]})),
            )
            .await;
        assert_eq!(status, 404, "an unknown model is a 404");
        let after = unresolved_client_errors();
        assert_eq!(after - before, 1.0, "this unknown request was counted once");
        let scrape = crate::root::serve::metrics_fixture::render_metrics();
        assert!(
            !scrape.contains(probe),
            "the raw model is never a label:\n{scrape}"
        );
        assert!(
            scrape.contains("pool=\"unresolved\""),
            "the bounded sentinel:\n{scrape}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_counts_a_malformed_body() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-count-malformed";
        let (rig, _far) = serving(instance).await;
        let before = unresolved_client_errors();
        assert_eq!(
            send_bytes(&rig, "/v1/chat/completions", "{ this is not json ").await,
            400
        );
        let after = unresolved_client_errors();
        assert!(after > before, "{before} -> {after}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_counts_an_unresolvable_invoke_body() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-count-invoke";
        let (rig, _far) = serving(instance).await;
        let before = unresolved_client_errors();
        let (status, _, _) = rig
            .send(
                "POST",
                door_word("invoke_path"),
                Some(serde_json::json!({"nonsense": 1})),
            )
            .await;
        assert_eq!(status, 400);
        let after = unresolved_client_errors();
        assert!(after > before, "{before} -> {after}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_counts_a_missing_model() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-count-missing-model";
        let (rig, _far) = serving(instance).await;
        let before = unresolved_client_errors();
        let (status, _, _) = rig
            .send(
                "POST",
                "/v1/chat/completions",
                Some(serde_json::json!({"messages": [{"role": "user", "content": "hi"}]})),
            )
            .await;
        assert_eq!(status, 400);
        let after = unresolved_client_errors();
        assert!(after > before, "{before} -> {after}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_counts_a_non_object_body() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-count-non-object";
        let (rig, _far) = serving(instance).await;
        let before = unresolved_client_errors();
        let (status, _, _) = rig
            .send(
                "POST",
                door_word("converse_path"),
                Some(serde_json::json!([1, 2, 3])),
            )
            .await;
        assert_eq!(status, 400);
        let after = unresolved_client_errors();
        assert!(after > before, "{before} -> {after}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_counts_an_unsupported_action() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-count-unsupported";
        let (rig, _far) = serving(instance).await;
        let before = unresolved_client_errors();
        let (status, _, _) = rig
            .send(
                "POST",
                door_word("count_tokens_path"),
                Some(serde_json::json!({"contents": []})),
            )
            .await;
        assert_eq!(status, 404);
        let after = unresolved_client_errors();
        assert!(after > before, "{before} -> {after}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_bounds_its_pool_label_to_what_is_configured() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-count-label-bounds";
        let (rig, _far) = serving(instance).await;
        let counted =
            |pool: &str| metric_sum(busbar_kernel::snapshot::REQUESTS_TOTAL, &[("pool", pool)]);
        for (model, label) in [
            ("p", "p"),
            ("m0", "m0"),
            ("anything-else", "unresolved"),
            ("", "unresolved"),
        ] {
            let before = counted(label);
            let _ = rig
                .send(
                    "POST",
                    "/v1/chat/completions",
                    Some(serde_json::json!({"model": model, "max_tokens": 16,
                        "messages": [{"role": "user", "content": "hi"}]})),
                )
                .await;
            let after = counted(label);
            assert!(
                after > before,
                "`{model}` counts under `{label}`: {before} -> {after}"
            );
        }
        let scrape = crate::root::serve::metrics_fixture::render_metrics();
        assert!(!scrape.contains("pool=\"anything-else\""), "{scrape}");
        assert!(!scrape.contains("pool=\"\""), "{scrape}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_serves_a_fallback_pool_the_key_may_reach() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-route-fallback-acl";
        let (state, server) = mock(vec![ProviderReply::Ok {
            status: StatusCode::OK,
            body: messages_answer(),
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(refusing_port(), 1), (server.address().port(), 1)],
                pooled: Some(1),
                more_pools: &[("q", &[(1, 1)])],
                pool_yaml: "on_exhausted: { fallback_pool: q }",
                allowed_pools: Some(&["p", "q"]),
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, body) = rig
            .send(
                "POST",
                door_word("messages_path"),
                Some(messages_naming("p")),
            )
            .await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        assert!(
            state.get_last_request_body().is_some(),
            "the fallback's member served"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_serves_an_ad_hoc_provider_route() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-route-adhoc";
        let (state, server) = mock(vec![ProviderReply::Ok {
            status: StatusCode::OK,
            body: messages_answer(),
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(server.address().port(), 1)],
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, body) = rig
            .send(
                "POST",
                &format!("/oai0/m0{}", door_word("messages_path")),
                Some(serde_json::json!({"model": "m0", "messages": [], "max_tokens": 16})),
            )
            .await;
        assert!(
            (200..300).contains(&status),
            "{status}: {}",
            String::from_utf8_lossy(&body)
        );
        assert!(
            state.get_last_request_body().is_some(),
            "the model's far end served"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_serves_a_model_named_directly() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-route-direct-model";
        let (state, server) = mock(vec![ProviderReply::Ok {
            status: StatusCode::OK,
            body: messages_answer(),
        }])
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(server.address().port(), 1)],
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, body) = rig
            .send(
                "POST",
                &format!("/m0{}", door_word("messages_path")),
                Some(serde_json::json!({"model": "m0", "messages": [], "max_tokens": 16})),
            )
            .await;
        assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
        assert!(
            state.get_last_request_body().is_some(),
            "the model's far end received the forwarded request"
        );
    }

    async fn queued(instance: &'static str, port: u16, max_ms: u64) -> Arc<DoorRig> {
        let pool_yaml = format!("on_exhausted: {{ queue: {{ max_ms: {max_ms} }} }}");
        Arc::new(
            rig(
                instance,
                RigOpts {
                    members: &[(port, 1)],
                    dialect: Some(door_word("messages_dialect")),
                    model_yaml: &["max_concurrent: 1"],
                    pool_yaml: &pool_yaml,
                    failover_timeout_secs: Some(300),
                    ..RigOpts::default()
                },
            )
            .await,
        )
    }

    fn spawn_request(
        rig: &Arc<DoorRig>,
    ) -> tokio::task::JoinHandle<(u16, axum::http::HeaderMap, Vec<u8>)> {
        let rig = Arc::clone(rig);
        tokio::spawn(async move {
            rig.send(
                "POST",
                door_word("messages_path"),
                Some(messages_naming("p")),
            )
            .await
        })
    }

    fn gated(
        status: StatusCode,
        started: &Arc<tokio::sync::Notify>,
        release: &Arc<tokio::sync::Notify>,
    ) -> ProviderReply {
        ProviderReply::Gated {
            status,
            body: serde_json::json!({"error": {"message": "boom"}}),
            started: Arc::clone(started),
            release: Arc::clone(release),
        }
    }

    async fn left_half_open(rig: &DoorRig, what: &str) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while matches!(
            rig.app.store.breaker_state_in("p", 0),
            BreakerState::HalfOpen
        ) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{what} left the cell stuck HalfOpen: the won probe leaked"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_releases_the_probe_of_a_dropped_queued_dispatch() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-probe-queued-drop";
        let (holding, holding_release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let (probing, probing_release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let (_state, server) = mock(vec![
            gated(
                StatusCode::INTERNAL_SERVER_ERROR,
                &holding,
                &holding_release,
            ),
            gated(
                StatusCode::INTERNAL_SERVER_ERROR,
                &probing,
                &probing_release,
            ),
        ])
        .await;
        let rig = queued(instance, server.address().port(), 30_000).await;

        let holder = spawn_request(&rig);
        tokio::time::timeout(std::time::Duration::from_secs(5), holding.notified())
            .await
            .expect("the holder reaches its far end");
        rig.app.store.force_open_in("p", 0, 0);
        let waiter = spawn_request(&rig);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        holder.abort();
        let _ = holder.await;
        tokio::time::timeout(std::time::Duration::from_secs(5), probing.notified())
            .await
            .expect("the queued dispatch reaches its far end once the slot frees");
        assert!(
            matches!(
                rig.app.store.breaker_state_in("p", 0),
                BreakerState::HalfOpen
            ),
            "precondition: the queued dispatch won the recovery probe"
        );
        waiter.abort();
        let _ = waiter.await;
        left_half_open(&rig, "the dropped queued dispatch").await;
        assert!(
            matches!(
                rig.app.store.breaker_state_in("p", 0),
                BreakerState::Open { .. }
            ),
            "the probe released (HalfOpen to Open), the member not benched"
        );
        holding_release.notify_waiters();
        probing_release.notify_waiters();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_sheds_a_queued_request_at_the_queues_bound() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-queue-times-out";
        let answer = door_word("messages_answer");
        let slow = far_end_scripted(SeatScript {
            head: format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n",
                answer.len()
            ),
            pieces: vec![(3_000, answer.as_bytes().to_vec())],
            finish: None,
        })
        .await;
        let rig = queued(instance, slow.port, 300).await;
        let holder = spawn_request(&rig);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while slow.served() == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the holder dispatches"
            );
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let started = std::time::Instant::now();
        let (status, head, _) = tokio::time::timeout(
            std::time::Duration::from_secs(4),
            rig.send(
                "POST",
                door_word("messages_path"),
                Some(messages_naming("p")),
            ),
        )
        .await
        .expect("the queue's wait is bounded and never hangs");
        let waited = started.elapsed();
        assert_eq!(status, 503, "a queue that never gets its slot sheds");
        assert!(head.contains_key("retry-after"), "{head:?}");
        assert!(
            waited >= std::time::Duration::from_millis(250),
            "it waited for the slot: {waited:?}"
        );
        assert!(
            waited < std::time::Duration::from_secs(2),
            "bounded by the queue's 300 ms, not the failover budget: {waited:?}"
        );
        assert_eq!(
            rig.app.engine_tables_view().queued_depth("p"),
            0,
            "the depth is back to 0"
        );
        holder.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_never_reverts_a_newer_probe_on_a_stale_end() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-probe-stale-end";
        let (started, release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let (_state, server) = mock(vec![ProviderReply::Gated {
            status: StatusCode::BAD_REQUEST,
            body: serde_json::json!({"error": {"message": "malformed request"}}),
            started: Arc::clone(&started),
            release: Arc::clone(&release),
        }])
        .await;
        let rig = Arc::new(
            rig(
                instance,
                RigOpts {
                    members: &[(server.address().port(), 1)],
                    dialect: Some(door_word("messages_dialect")),
                    ..RigOpts::default()
                },
            )
            .await,
        );
        let stale = spawn_request(&rig);
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .expect("the request reaches its far end");
        let epoch_before = rig.app.store.probe_epoch_in("p", 0);
        rig.app
            .store
            .record_success_in("p", 0, busbar_kernel::store::now());
        rig.app.store.force_open_in("p", 0, 0);
        assert!(
            rig.app
                .store
                .acquire_for_dispatch_in("p", 0, now().saturating_add(86_400)),
            "a NEW probe is won on the re-opened cell"
        );
        assert_ne!(
            epoch_before,
            rig.app.store.probe_epoch_in("p", 0),
            "the second win bumps the epoch"
        );
        assert!(matches!(
            rig.app.store.breaker_state_in("p", 0),
            BreakerState::HalfOpen
        ));
        release.notify_one();
        let (status, _, _) = stale.await.expect("the stale request ends");
        assert_eq!(status, 400, "the held 400 relays through");
        assert!(
            matches!(
                rig.app.store.breaker_state_in("p", 0),
                BreakerState::HalfOpen
            ),
            "the stale end left the newer probe's cell HalfOpen"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_releases_the_probe_of_a_dropped_dispatch() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-probe-dropped";
        let (started, release) = (
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tokio::sync::Notify::new()),
        );
        let (_state, server) = mock(vec![gated(
            StatusCode::INTERNAL_SERVER_ERROR,
            &started,
            &release,
        )])
        .await;
        let rig = Arc::new(
            rig(
                instance,
                RigOpts {
                    members: &[(server.address().port(), 1)],
                    dialect: Some(door_word("messages_dialect")),
                    ..RigOpts::default()
                },
            )
            .await,
        );
        rig.app.store.force_open_in("p", 0, 0);
        let request = spawn_request(&rig);
        tokio::time::timeout(std::time::Duration::from_secs(5), started.notified())
            .await
            .expect("the probing dispatch reaches its far end");
        assert!(
            matches!(
                rig.app.store.breaker_state_in("p", 0),
                BreakerState::HalfOpen
            ),
            "precondition: the dispatch won the recovery probe"
        );
        request.abort();
        let _ = request.await;
        left_half_open(&rig, "the dropped dispatch").await;
        assert!(matches!(
            rig.app.store.breaker_state_in("p", 0),
            BreakerState::Open { .. }
        ));
        assert!(
            rig.app
                .store
                .acquire_for_dispatch_in("p", 0, now().saturating_add(86_400)),
            "a later dispatch can win the probe again"
        );
        release.notify_waiters();
    }

    async fn ranked(
        instance: &'static str,
        declared: &[Signal],
    ) -> (DoorRig, [super::super::hook_seat_tests::FarEnd; 2]) {
        let fars = [
            seat_far_end(200, SERVED_BY_TWIN).await,
            seat_far_end(200, SERVED_BY_TWIN).await,
        ];
        let rig = rig(
            instance,
            RigOpts {
                members: &[(fars[0].port, 1), (fars[1].port, 1)],
                described: true,
                declared: Some(declared),
                ..RigOpts::default()
            },
        )
        .await;
        (rig, fars)
    }

    fn bag_of(rig: &DoorRig, member: &str) -> String {
        let bags = rig.gate_signals();
        bags.iter()
            .find(|line| line.split(' ').next() == Some(member))
            .cloned()
            .unwrap_or_else(|| panic!("the ranking policy was shown {member}: {bags:?}"))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_shows_no_signal_nobody_declared() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-signal-undeclared";
        let (rig, _fars) = ranked(instance, &[Signal::CandidateBreakerState]).await;
        assert_eq!(rig.chat().await.0, 200);
        let bag = bag_of(&rig, "m0");
        assert!(bag.contains("CandidateBreakerState="), "{bag}");
        assert!(!bag.contains("CandidateErrorRate"), "never declared: {bag}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_shows_a_tripped_members_breaker_open() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-signal-open";
        let (rig, _fars) = ranked(instance, &[Signal::CandidateBreakerState]).await;
        rig.app.store.force_open_in("p", 0, now() + 3600);
        let _ = rig.chat().await;
        let bag = bag_of(&rig, "m0");
        assert!(
            bag.contains(r#"CandidateBreakerState=Str("open")"#),
            "the tripped member's state: {bag}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_shows_the_error_rate_as_the_windows_fraction() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-signal-error-rate";
        let (rig, _fars) = ranked(instance, &[Signal::CandidateErrorRate]).await;
        let cfg = busbar_kernel::store::BreakerCfg::default();
        rig.app
            .store
            .record_transient_in("p", 0, "test", &cfg, None);
        for _ in 0..3 {
            rig.app
                .store
                .record_success_in("p", 0, busbar_kernel::store::now());
        }
        let _ = rig.chat().await;
        let bag = bag_of(&rig, "m0");
        assert!(
            bag.contains("CandidateErrorRate=F64(0.25)"),
            "one failure in four outcomes: {bag}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_never_collects_an_undeclared_p95() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-signal-undeclared-p95";
        let (rig, _fars) = ranked(instance, &[Signal::CandidateErrorRate]).await;
        assert_eq!(rig.chat().await.0, 200);
        assert!(
            (0..2).any(|lane| rig.app.store.lane_latency_ms(lane).is_some()),
            "the latency figure is always on"
        );
        assert_eq!(rig.chat().await.0, 200);
        let bags = rig.gate_signals();
        assert!(
            bags.iter()
                .all(|bag| !bag.contains("CandidateLatencyP95Ms")),
            "never declared, never shown: {bags:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_seals_an_error_end_as_one_error_record() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-audit-error-end";
        let answer = door_word("messages_answer");
        let far = far_end_scripted(SeatScript {
            head: format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
                 connection: close\r\n\r\n",
                answer.len()
            ),
            pieces: vec![(0, answer.as_bytes()[..answer.len() / 2].to_vec())],
            finish: None,
        })
        .await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                dialect: Some(door_word("messages_dialect")),
                ..RigOpts::default()
            },
        )
        .await;
        let (status, _, body) = rig.chat().await;
        assert_eq!(status, 502, "{}", String::from_utf8_lossy(&body));
        let envelope: serde_json::Value =
            serde_json::from_slice(&body).expect("the caller's JSON error envelope");
        assert!(envelope.get("error").is_some(), "{envelope}");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while rig.audit_records() == 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let records = rig.records();
        assert_eq!(records.len(), 1, "one record for the one unit: {records:?}");
        assert!(!records[0].what.op_class.as_str().is_empty());
        assert_eq!(
            records[0].outcome.finish,
            busbar_kernel_audit::FinishClass::Error,
            "a client-facing 502 is sealed as an error end"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_pools_door_counts_one_ok_per_probe_not_per_pool() {
        let _one = ONE_PUBLISHER.lock().await;
        let instance = "rehome-probe-ok-once";
        let far = seat_far_end(200, SERVED_BY_TWIN).await;
        let rig = rig(
            instance,
            RigOpts {
                members: &[(far.port, 1)],
                more_pools: &[("q", &[(0, 1)]), ("r", &[(0, 1)])],
                provider_yaml: "health:\n  mode: active\n  interval_secs: 1",
                ..RigOpts::default()
            },
        )
        .await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let ok = rig.app.store.snapshot(0, now()).ok;
            let probes = far.served() as u64;
            assert!(
                ok <= probes,
                "ok {ok} ran ahead of the {probes} probes answered: counted per pool"
            );
            if probes >= 2 && ok == probes {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "two probes answered and counted: {probes} probes, ok {ok}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_answers_an_unknown_model_with_1_5_5s_404_naming_it() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-unknown-model";
    let far = seat_far_end(200, SERVED_BY_TWIN).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            ..RigOpts::default()
        },
    )
    .await;
    let (status, _, body) = rig
        .send(
            "POST",
            "/v1/chat/completions",
            Some(serde_json::json!({"model": "nope",
                "messages": [{"role": "user", "content": "hi"}]})),
        )
        .await;
    assert_eq!(status, 404, "{}", String::from_utf8_lossy(&body));
    let answer: serde_json::Value = serde_json::from_slice(&body).expect("a JSON error body");
    assert_eq!(answer["error"]["type"], "not_found_error", "{answer}");
    assert_eq!(
        answer["error"]["message"],
        "The model 'nope' does not exist or you do not have access to it.",
        "{answer}"
    );
    assert_eq!(far.served(), 0, "no far end is dialled");
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_bills_no_tokens_for_an_answer_it_cannot_translate() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-untranslatable-2xx";
    let far = seat_far_end(200, door_word("unreadable_answer")).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            dialect: Some(door_word("untranslatable_dialect")),
            ..RigOpts::default()
        },
    )
    .await;
    let (status, _, body) = rig.chat().await;
    assert_eq!(status, 500, "{}", String::from_utf8_lossy(&body));
    let answer: serde_json::Value = serde_json::from_slice(&body).expect("a JSON error body");
    assert!(
        answer.get("error").is_some(),
        "the caller's own error: {answer}"
    );
    assert!(
        answer.get("choices").is_none(),
        "no completion reaches the caller: {answer}"
    );
    assert_eq!(far.served(), 1, "the far end answered once");
    assert_eq!(
        rig.tokens_after().await,
        0,
        "an untranslated answer bills no tokens"
    );
}

#[cfg(linked_driver_proofs)]
#[path = "serve_door_money_gaps.rs"]
mod door_money_gaps;

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_refunds_the_budget_unit_of_an_answer_it_cannot_translate() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-untranslatable-budget";
    let far = seat_far_end(200, door_word("unreadable_answer")).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            dialect: Some(door_word("untranslatable_dialect")),
            lane_budget: Some(5),
            ..RigOpts::default()
        },
    )
    .await;
    let (status, _, body) = rig.chat().await;
    assert_eq!(status, 500, "{}", String::from_utf8_lossy(&body));
    assert_eq!(far.served(), 1, "the far end was dialled and answered");
    assert_eq!(
        rig.app.store.lane_budget_remaining(0),
        Some(5),
        "a non-delivery refunds the budget unit"
    );
    assert_eq!(rig.tokens_after().await, 0, "and bills no tokens");
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pools_door_bills_a_delivered_cross_dialect_answer_once_and_keeps_its_budget_unit() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-crossdialect-delivered-budget";
    let far = seat_far_end(200, door_word("messages_answer")).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            dialect: Some(door_word("messages_dialect")),
            lane_budget: Some(5),
            ..RigOpts::default()
        },
    )
    .await;
    let (status, _, body) = rig.chat().await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let answer: serde_json::Value = serde_json::from_slice(&body).expect("a JSON answer");
    assert!(
        answer.get("choices").is_some(),
        "the caller's own dialect: {answer}"
    );
    assert_eq!(
        rig.tokens_after().await,
        2,
        "its 1 + 1 tokens bill exactly once"
    );
    assert_eq!(
        rig.app.store.lane_budget_remaining(0),
        Some(4),
        "a delivered answer keeps the budget unit"
    );
}

#[cfg(linked_driver_proofs)]
const SILENCE_MARKERS: &[&str] = &["alarm", "dispute"];

#[cfg(linked_driver_proofs)]
fn mentions_a_silence_marker(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    SILENCE_MARKERS.iter().any(|m| lower.contains(m))
}

#[cfg(linked_driver_proofs)]
fn exposition_metric_name(line: &str) -> Option<&str> {
    if let Some(rest) = line.strip_prefix("# ") {
        let mut it = rest.split_whitespace();
        let _kind = it.next()?;
        return it.next();
    }
    let end = line.find(['{', ' ']).unwrap_or(line.len());
    let name = &line[..end];
    (!name.is_empty()).then_some(name)
}

#[cfg(linked_driver_proofs)]
#[test]
fn a_1_5_5_request_lifecycle_emits_no_alarm_or_dispute_event_or_metric() {
    busbar_kernel::snapshot::init_for_test(std::time::Duration::from_secs(10));
    #[derive(Clone)]
    struct EventWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for EventWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = EventWriter(events.clone());
    let dispatch = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    );
    let mut statuses: Vec<(&str, u16)> = Vec::new();
    tracing::dispatcher::with_default(&dispatch, || {
        let on_workers = dispatch.clone();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .on_thread_start(move || {
                std::mem::forget(tracing::dispatcher::set_default(&on_workers));
            })
            .enable_all()
            .build()
            .expect("a runtime");
        rt.block_on(async {
            let _one = ONE_PUBLISHER.lock().await;
            let instance = "serve-door-alarm-silence";
            let far = seat_far_end(200, door_word("messages_answer")).await;

            let rig = rig(
                instance,
                RigOpts {
                    members: &[(far.port, 1), (1, 1)],
                    dialect: Some(door_word("messages_dialect")),
                    pooled: Some(1),
                    more_pools: &[("pb", &[(1, 1)])],
                    ..RigOpts::default()
                },
            )
            .await;
            let ask = |pool: &str| {
                serde_json::json!({
                    "model": pool,
                    "messages": [{"role": "user", "content": "hi"}],
                    "max_tokens": 8
                })
            };
            let path = door_word("messages_path");
            statuses.push(("ok request", rig.send("POST", path, Some(ask("p"))).await.0));
            statuses.push((
                "failed-upstream request",
                rig.send("POST", path, Some(ask("pb"))).await.0,
            ));
            statuses.push(("healthz", rig.send("GET", "/healthz", None).await.0));
            statuses.push(("stats", rig.send("GET", "/stats", None).await.0));
        });
    });

    let status_of = |what: &str| {
        statuses
            .iter()
            .find(|(w, _)| *w == what)
            .map(|(_, s)| *s)
            .unwrap()
    };
    assert_eq!(status_of("ok request"), 200, "statuses: {statuses:?}");
    assert_ne!(
        status_of("failed-upstream request"),
        200,
        "statuses: {statuses:?}"
    );
    assert_eq!(status_of("healthz"), 200, "statuses: {statuses:?}");

    let events: Vec<String> = String::from_utf8(events.lock().unwrap().clone())
        .expect("tracing emits UTF-8")
        .lines()
        .map(str::to_owned)
        .collect();
    let hits: Vec<&String> = events
        .iter()
        .filter(|m| mentions_a_silence_marker(m))
        .collect();
    assert!(
        hits.is_empty(),
        "no tracing event may carry an alarm/dispute message or field on a 1.5.5-shaped \
         deployment; found:\n{}",
        hits.iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );

    let exposition = crate::root::serve::metrics_fixture::render_metrics();
    let leaked: Vec<&str> = exposition
        .lines()
        .filter_map(exposition_metric_name)
        .filter(|n| mentions_a_silence_marker(n))
        .collect();
    assert!(
        leaked.is_empty(),
        "no metric name may mention an alarm or a dispute on a 1.5.5-shaped deployment; found: \
         {leaked:?}"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_key_row_whose_expires_at_is_in_the_past_is_refused_on_the_data_plane() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-key-row-expired";
    let far = seat_far_end(200, door_word("messages_answer")).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            dialect: Some(door_word("messages_dialect")),
            allowed_pools: Some(&["p"]),
            ..RigOpts::default()
        },
    )
    .await;
    let ask = serde_json::json!({
        "model": "p",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 8
    });
    let path = door_word("messages_path");
    let (before, _, body) = rig.send("POST", path, Some(ask.clone())).await;
    assert_eq!(
        before,
        200,
        "with no expires_at on its row the key is admitted: {}",
        String::from_utf8_lossy(&body)
    );

    let core = rig.gov.core_store_handle().unwrap();
    let store = core.identity();
    let mut row = store
        .get_key(&rig.key_id)
        .await
        .expect("store read")
        .expect("the minted row");
    assert_eq!(
        row.expires_at, None,
        "mint never stamps expires_at; it is a stored field nothing in the engine writes"
    );
    let before = row.clone();
    let past = busbar_kernel::store::now() - 30 * 86_400;
    row.expires_at = Some(past);
    store
        .compare_key(Some(&before), &row)
        .await
        .expect("rewrite the row");
    rig.gov.refresh_async().await.expect("reload caches");

    let (expired, _, expired_body) = rig.send("POST", path, Some(ask.clone())).await;
    assert_eq!(
        expired,
        401,
        "a key row with a past expires_at is refused (owner-signed 2026-10-30): {}",
        String::from_utf8_lossy(&expired_body)
    );
    let (refused, _, refused_body) = rig
        .send_bare(
            "POST",
            path,
            Some(ask),
            &[("authorization", "Bearer bbk_not_a_real_token")],
        )
        .await;
    assert_eq!(
        refused, 401,
        "the keys chain is a real gate on this app: a bad token is refused"
    );
    let mut expired_answer: serde_json::Value =
        serde_json::from_slice(&expired_body).expect("the expired-key JSON refusal");
    let mut refused_answer: serde_json::Value =
        serde_json::from_slice(&refused_body).expect("the bad-credential JSON refusal");
    let expired_id = expired_answer["request_id"]
        .as_str()
        .expect("the expired-key request identifier")
        .to_owned();
    let refused_id = refused_answer["request_id"]
        .as_str()
        .expect("the bad-credential request identifier")
        .to_owned();
    assert!(!expired_id.is_empty() && !refused_id.is_empty());
    assert_ne!(
        expired_id, refused_id,
        "each refusal identifies its own request"
    );
    expired_answer.as_object_mut().unwrap().remove("request_id");
    refused_answer.as_object_mut().unwrap().remove("request_id");
    assert_eq!(
        expired_answer, refused_answer,
        "an expired key row is refused with the same error as any bad credential"
    );
    assert_eq!(
        rig.gov
            .lookup_by_sub(&rig.key_id)
            .expect("the binding stays resolvable")
            .expires_at,
        Some(past),
        "the stored expires_at is kept, so the admin views keep showing it"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_virtual_key_is_refused() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-key-token-expired";
    let far = seat_far_end(200, door_word("messages_answer")).await;
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            dialect: Some(door_word("messages_dialect")),
            allowed_pools: Some(&["p"]),
            ..RigOpts::default()
        },
    )
    .await;

    let now = busbar_kernel::store::now();
    let (_, expired) = rig
        .gov
        .mint_signed_async(
            NewKeySpec {
                name: "expired".to_string(),
                allowed_pools: Some(vec!["p".to_string()]),
                ..Default::default()
            },
            now - 3_000,
            now - 3_600,
        )
        .await
        .expect("mint");
    let expired = expired.expose(&busbar_kernel::seal::lend()).to_string();

    let ask = serde_json::json!({
        "model": "p",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 8
    });
    let path = door_word("messages_path");
    let bearer = format!("Bearer {expired}");
    let (refused, _, body) = rig
        .send_bare(
            "POST",
            path,
            Some(ask.clone()),
            &[("authorization", bearer.as_str())],
        )
        .await;
    assert_eq!(
        refused,
        401,
        "an expired virtual key is refused: {}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(far.served(), 0, "the expired key never reached the far end");

    let (admitted, _, body) = rig.send("POST", path, Some(ask)).await;
    assert_eq!(
        admitted,
        200,
        "the live key is admitted on the same app: {}",
        String::from_utf8_lossy(&body)
    );
}

#[cfg(linked_driver_proofs)]
fn exposition_label_keys(line: &str) -> Vec<String> {
    line.split_once('{')
        .and_then(|(_, rest)| rest.rsplit_once('}'))
        .map(|(inner, _)| {
            let mut ks: Vec<String> = inner
                .split(',')
                .filter_map(|kv| kv.split_once('=').map(|(k, _)| k.trim().to_string()))
                .collect();
            ks.sort();
            ks
        })
        .unwrap_or_default()
}

#[cfg(linked_driver_proofs)]
fn exposition_family<'a>(exposition: &'a str, family: &str) -> Vec<&'a str> {
    exposition
        .lines()
        .filter(|l| !l.starts_with('#') && l.starts_with(family))
        .collect()
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_plane_request_is_counted_exactly_once() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-counted-once";
    busbar_kernel::snapshot::init_for_test(std::time::Duration::from_secs(10));
    let rig = rig(
        instance,
        RigOpts {
            members: &[(1, 1)],
            ..RigOpts::default()
        },
    )
    .await;

    let labels = [("pool", "p")];
    let before = crate::root::serve::door_tests::metric_sum(
        busbar_kernel::snapshot::REQUESTS_TOTAL,
        &labels,
    );
    let _ = rig.chat().await;
    let after = crate::root::serve::door_tests::metric_sum(
        busbar_kernel::snapshot::REQUESTS_TOTAL,
        &labels,
    );
    assert_eq!(
        (after - before).round() as u64,
        1,
        "one model-plane request must produce ONE count, not one from `finish_inner` plus one from \
         the plane ingress boundary"
    );
}

#[cfg(linked_driver_proofs)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_model_plane_families_stay_v1_5_4_shaped_on_a_real_scrape() {
    let _one = ONE_PUBLISHER.lock().await;
    let instance = "serve-door-model-families";
    let pool = "t065-model-family-pool";
    let far = seat_far_end(200, SERVED_BY_TWIN).await;
    busbar_kernel::snapshot::init_for_test(std::time::Duration::from_secs(10));
    let rig = rig(
        instance,
        RigOpts {
            members: &[(far.port, 1)],
            more_pools: &[(pool, &[(0, 1)])],
            ..RigOpts::default()
        },
    )
    .await;
    let labels = [("pool", pool)];
    let duration_count = format!(
        "{}_count",
        busbar_kernel::snapshot::REQUEST_DURATION_SECONDS
    );
    let counts = || {
        (
            crate::root::serve::door_tests::metric_sum(
                busbar_kernel::snapshot::REQUESTS_TOTAL,
                &labels,
            ),
            crate::root::serve::door_tests::metric_sum(&duration_count, &labels),
        )
    };
    let before = counts();
    let (model_status, _, _) = rig
        .send(
            "POST",
            "/v1/chat/completions",
            Some(serde_json::json!({
                "model": pool,
                "messages": [{ "role": "user", "content": "hi" }],
            })),
        )
        .await;
    assert_eq!(model_status, 200);
    let after = counts();
    assert!(
        after.0 > before.0,
        "this pool's request counted: {before:?} -> {after:?}"
    );
    assert!(
        after.1 > before.1,
        "this pool's duration observed: {before:?} -> {after:?}"
    );
    let exposition = crate::root::serve::metrics_fixture::render_metrics();
    let model_counters = exposition_family(&exposition, busbar_kernel::snapshot::REQUESTS_TOTAL);
    assert!(
        model_counters
            .iter()
            .any(|line| line.contains("pool=\"t065-model-family-pool\"")
                && exposition_label_keys(line)
                    == vec![
                        "ingress_protocol".to_string(),
                        "outcome".to_string(),
                        "pool".to_string()
                    ]),
        "no v1.5.4-shaped `{}` series {{ingress_protocol,pool,outcome}} after driving model traffic \
         (model {model_status}). Exposition:\n{exposition}",
        busbar_kernel::snapshot::REQUESTS_TOTAL,
    );
    assert!(
        exposition_family(
            &exposition,
            busbar_kernel::snapshot::REQUEST_DURATION_SECONDS
        )
        .iter()
        .any(|line| line.contains("pool=\"t065-model-family-pool\"")),
        "no `{}` series after driving model traffic. Exposition:\n{exposition}",
        busbar_kernel::snapshot::REQUEST_DURATION_SECONDS,
    );
    for family in [
        busbar_kernel::snapshot::REQUESTS_TOTAL,
        busbar_kernel::snapshot::REQUEST_DURATION_SECONDS,
    ] {
        for line in exposition_family(&exposition, family) {
            assert!(
                !line.contains("plane=\""),
                "a `plane` label leaked onto the v1.5.4 model family `{family}`: {line}"
            );
        }
    }
}
