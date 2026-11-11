//! Request-scoped Anthropic execution exemption, acquired on the actual outbound path.
//!
//! The provider exempts execution in requests containing the documented web tools. This is
//! not an assertion of zero duration, an organization allowance, or a container-wide exemption.
//! <https://platform.claude.com/docs/en/agents-and-tools/tool-use/code-execution-tool>

use super::*;
use serde::Deserialize;

pub(super) const UNRESOLVED: &str = crate::codec::usage_count::ANTHROPIC_EXECUTION_UNRESOLVED;
const POLICY: &str = "web_tools_exempt_2026_11_10";

#[derive(Deserialize)]
struct Request {
    model: Option<String>,
    #[serde(default)]
    tools: Vec<Tool>,
    #[serde(default)]
    stream: bool,
    container: Option<Value>,
}

#[derive(Deserialize)]
struct Tool {
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
}

/// Frozen outbound facts and bounded provider bytes. Never constructed from response claims.
pub(super) struct Guard {
    model: String,
    provider: String,
    exemption: Option<String>,
    body: zeroize::Zeroizing<Vec<u8>>,
    cap: usize,
    overflow: bool,
}

impl Guard {
    /// Refuse known execution liabilities before the connector receives the outbound head.
    pub(super) fn prepare(
        request: &FarRequest,
        lane: &crate::exchange::shaping::Lane,
        cap: usize,
        policy_bound: bool,
        passthrough: bool,
    ) -> Result<Option<Self>, &'static str> {
        if lane.dialect == "responses" {
            responses_admission(&request.body)?;
            return Ok(None);
        }
        if lane.dialect != "anthropic" {
            return Ok(None);
        }
        crate::observations::native_stream::validate_unique_json(&request.body)
            .map_err(|_| UNRESOLVED)?;
        let parsed: Request = serde_json::from_slice(&request.body).map_err(|_| UNRESOLVED)?;
        let mut execution = parsed.container.is_some();
        let mut exemption = None;
        for tool in &parsed.tools {
            let kind = tool.kind.as_deref().unwrap_or("");
            execution |= kind.starts_with("code_execution");
            if kind.starts_with("code_execution")
                && (!matches!(kind, "code_execution_20260120" | "code_execution_20260521")
                    || tool.name.as_deref() != Some("code_execution"))
            {
                return Err(UNRESOLVED);
            }
            let supported = matches!(
                (kind, tool.name.as_deref()),
                (
                    "web_search_20260209" | "web_search_20260318",
                    Some("web_search")
                ) | (
                    "web_fetch_20260209" | "web_fetch_20260309" | "web_fetch_20260318",
                    Some("web_fetch"),
                )
            );
            // Only the known legacy web tools predate execution-backed filtering. Unknown
            // versions are not presumed harmless or granted the current exemption.
            execution |= kind.starts_with("web_search_") && kind != "web_search_20250305"
                || kind.starts_with("web_fetch_") && kind != "web_fetch_20250910";
            if supported {
                exemption = Some(kind.to_owned());
            }
        }
        let direct = !lane.invoke && lane.path_base.is_none() && request.target == "/v1/messages";
        if execution
            && (exemption.is_none() || parsed.stream || !direct || !policy_bound || passthrough)
        {
            return Err(UNRESOLVED);
        }
        // Ordinary streams retain the codec's execution-count refusal. Exempt execution streams
        // are rejected above: a partial response cannot use the buffered proof below.
        if parsed.stream || !direct {
            return Ok(None);
        }
        let model = parsed.model.filter(|m| !m.is_empty()).ok_or(UNRESOLVED)?;
        if model != lane.wire_model() {
            return Err(UNRESOLVED);
        }
        Ok(Some(Self {
            model,
            provider: lane.provider.clone(),
            exemption,
            body: zeroize::Zeroizing::new(Vec::new()),
            cap,
            overflow: false,
        }))
    }

    pub(super) fn feed(&mut self, bytes: &[u8]) {
        if self.overflow || bytes.len() > self.cap.saturating_sub(self.body.len()) {
            self.overflow = true;
            self.body.clear();
        } else {
            self.body.extend_from_slice(bytes);
        }
    }

    /// Exemption proof must precede any successful accounting output, even when a whole JSON
    /// message arrives before transport EOF and the ordinary usage tap sees it early.
    pub(super) fn buffered_body(&self) -> Option<&[u8]> {
        self.exemption.as_ref().map(|_| self.body.as_slice())
    }

    /// A successful full provider message is the only evidence that can discharge the codec's
    /// execution refusal. Return a bounded audit proof, never an execution duration/count.
    pub(super) fn finish(&self) -> Result<Option<Vec<u8>>, &'static str> {
        if self.overflow {
            return Err(UNRESOLVED);
        }
        crate::observations::native_stream::validate_unique_json(&self.body)
            .map_err(|_| UNRESOLVED)?;
        #[derive(Deserialize)]
        struct Message {
            id: String,
            #[serde(rename = "type")]
            kind: String,
            model: String,
            stop_reason: String,
            usage: Usage,
            container: Option<Value>,
            content: Vec<Block>,
        }
        #[derive(Deserialize)]
        struct Usage {
            // These required token fields also prevent an empty/malformed usage object from
            // discharging execution while the ordinary decoder substitutes absent counts.
            input_tokens: u64,
            output_tokens: u64,
            server_tool_use: Option<ServerUsage>,
        }
        #[derive(Deserialize)]
        struct ServerUsage {
            #[serde(default, deserialize_with = "present_count")]
            code_execution_requests: Option<u64>,
        }
        fn present_count<'de, D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<u64>, D::Error> {
            u64::deserialize(deserializer).map(Some)
        }
        #[derive(Deserialize)]
        struct Block {
            #[serde(rename = "type")]
            kind: String,
            name: Option<String>,
        }
        let message: Message = serde_json::from_slice(&self.body).map_err(|_| UNRESOLVED)?;
        let _known_tokens = (message.usage.input_tokens, message.usage.output_tokens);
        let execution = message.container.is_some()
            || message
                .usage
                .server_tool_use
                .as_ref()
                .is_some_and(|u| u.code_execution_requests.is_some())
            || message.content.iter().any(|b| {
                b.kind.contains("code_execution")
                    || b.kind == "server_tool_use"
                        && b.name
                            .as_deref()
                            .is_some_and(|n| n.contains("code_execution"))
            });
        let Some(tool) = &self.exemption else {
            return if execution { Err(UNRESOLVED) } else { Ok(None) };
        };
        if message.kind != "message"
            || !message.id.starts_with("msg_")
            || message.id.len() == 4
            || message.id.len() > 192
            || !message
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
            || message.model != self.model
            || !matches!(
                message.stop_reason.as_str(),
                "end_turn" | "max_tokens" | "stop_sequence" | "tool_use" | "pause_turn" | "refusal"
            )
        {
            return Err(UNRESOLVED);
        }
        serde_json::to_vec(&serde_json::json!({
            "policy": POLICY,
            "message_id": message.id,
            "model": self.model,
            "provider": self.provider,
            "qualifying_tool": tool,
            "additional_execution_charge": "exempt",
            "scope": "request"
        }))
        .map(Some)
        .map_err(|_| UNRESOLVED)
    }
}

/// No live complete hosted settlement path exists for these Responses components. Their
/// declarations are rejected before dispatch, while explicitly local/client tools remain valid.
fn responses_admission(body: &[u8]) -> Result<(), &'static str> {
    let refusal = crate::codec::usage_count::RESPONSES_HOSTED_UNRESOLVED;
    crate::observations::native_stream::validate_unique_json(body).map_err(|_| refusal)?;
    let request: Value = serde_json::from_slice(body).map_err(|_| refusal)?;
    let Some(tools) = request.get("tools") else {
        return Ok(());
    };
    let tools = tools.as_array().ok_or(refusal)?;
    for tool in tools {
        let kind = tool.get("type").and_then(Value::as_str).ok_or(refusal)?;
        let environment = tool.get("environment");
        let hosted_environment = environment
            .and_then(|e| e.get("type"))
            .and_then(Value::as_str)
            .is_some_and(|e| matches!(e, "container_auto" | "container_reference"));
        if matches!(kind, "image_generation" | "code_interpreter")
            || hosted_environment
            || kind == "shell"
                && environment
                    .and_then(|e| e.get("type"))
                    .and_then(Value::as_str)
                    != Some("local")
        {
            return Err(refusal);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_responses(tools: Value) -> (UnitState, Answer) {
        let body =
            serde_json::to_vec(&serde_json::json!({"model":"entry","input":"hi","tools":tools}))
                .unwrap();
        let shaping = Arc::new(
            Shaping::from_settings(&serde_json::json!({
                "providers":{"openai":{"protocol":"responses"}},
                "models":{"entry":{"provider":"openai","upstream_model":"gpt-pinned"}}
            }))
            .unwrap(),
        );
        let arrived = arrive::arrive("POST", "/v1/responses", &[], &body, &*shaping).unwrap();
        let mut unit = UnitState::new(shaping);
        unit.arrived = Some(arrived);
        let answer = attempt(
            &DoorHost(None),
            &mut unit,
            &PieceIn {
                last: false,
                cut: false,
                status: None,
                fields: vec![],
                bytes: &[],
                member: b"entry",
                pool: b"",
                passthrough: false,
            },
        );
        (unit, answer)
    }

    #[test]
    fn live_responses_blocks_unpriceable_hosted_tools_before_dispatch() {
        for tool in [
            serde_json::json!({"type":"image_generation","model":"gpt-image-pinned"}),
            serde_json::json!({"type":"code_interpreter","container":{"type":"auto"}}),
            serde_json::json!({"type":"code_interpreter","container":"cntr_existing"}),
            serde_json::json!({"type":"shell","environment":{"type":"container_auto"}}),
            serde_json::json!({"type":"shell","environment":{"type":"container_reference","container_id":"cntr_existing"}}),
            serde_json::json!({"type":"shell"}),
        ] {
            let (unit, answer) = start_responses(serde_json::json!([tool]));
            assert_eq!(answer.status, 400);
            assert!(!answer.to_far_end && answer.request.is_none());
            assert!(unit.request.is_none());
        }
        for tools in [
            serde_json::json!([]),
            serde_json::json!([{"type":"function","name":"lookup","parameters":{"type":"object","properties":{}}}]),
            serde_json::json!([{"type":"shell","environment":{"type":"local"}}]),
        ] {
            let (_, answer) = start_responses(tools);
            assert!(answer.to_far_end && answer.request.is_some());
        }
    }

    #[test]
    fn live_responses_unexpected_hosted_output_refuses_accounting_but_keeps_tokens() {
        for output in [
            serde_json::json!({"type":"image_generation_call","id":"ig_1","status":"completed","result":"aQ=="}),
            serde_json::json!({"type":"code_interpreter_call","id":"ci_1","status":"completed","container_id":"cntr_1","code":"1+1","outputs":[]}),
            serde_json::json!({"type":"shell_call","id":"sh_1","call_id":"call_1","status":"completed","action":{"commands":["pwd"]},"environment":{"type":"container_reference","container_id":"cntr_1"}}),
        ] {
            let (mut unit, outgoing) = start_responses(serde_json::json!([]));
            assert!(outgoing.request.is_some());
            let body = serde_json::to_vec(&serde_json::json!({
                "id":"resp_1","object":"response","model":"gpt-pinned","status":"completed",
                "output":[output],"usage":{"input_tokens":7,"output_tokens":3,"total_tokens":10,
                "input_tokens_details":{"cached_tokens":0},"output_tokens_details":{"reasoning_tokens":0}}
            })).unwrap();
            let done = reply(&mut unit, &body, true, true, false);
            assert_eq!(done.verdict, VERDICT_COUNT_REFUSED);
            assert_known_counts(&done);
        }
    }

    fn request(tools: Value) -> Value {
        serde_json::json!({
            "model":"entry", "max_tokens":32, "messages":[{"role":"user","content":"hi"}],
            "tools":tools
        })
    }
    fn exempt_request() -> Value {
        request(serde_json::json!([
            {"type":"code_execution_20260521","name":"code_execution"},
            {"type":"web_fetch_20260209","name":"web_fetch"}
        ]))
    }
    fn response() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id":"msg_test1","type":"message","role":"assistant","model":"claude-pinned",
            "stop_reason":"end_turn","stop_sequence":null,
            "container":{"id":"container_shared","expires_at":"2026-12-01T00:00:00Z"},
            "content":[{"type":"text","text":"done"}],
            "usage":{"input_tokens":7,"output_tokens":3,"server_tool_use":{
                "code_execution_requests":1,"web_search_requests":2,"web_fetch_requests":1
            }}
        }))
        .unwrap()
    }
    fn start(body: &[u8]) -> (UnitState, Answer) {
        start_with_policy(body, true)
    }
    fn start_with_policy(body: &[u8], selected: bool) -> (UnitState, Answer) {
        start_with_mode(body, selected, false)
    }
    fn start_with_mode(body: &[u8], selected: bool, passthrough: bool) -> (UnitState, Answer) {
        let mut providers = serde_json::json!({
            "claude":{"protocol":"anthropic"},
            "other":{"protocol":"anthropic"}
        });
        providers[if selected { "claude" } else { "other" }]["anthropic_execution_policy"] =
            POLICY.into();
        let shaping = Arc::new(
            Shaping::from_settings(&serde_json::json!({
                "providers":providers,
                "models":{"entry":{"provider":"claude","upstream_model":"claude-pinned"}}
            }))
            .unwrap(),
        );
        let arrived = arrive::arrive("POST", "/v1/messages", &[], body, &*shaping).unwrap();
        let mut unit = UnitState::new(shaping);
        unit.arrived = Some(arrived);
        let answer = attempt(
            &DoorHost(None),
            &mut unit,
            &PieceIn {
                last: false,
                cut: false,
                status: None,
                fields: vec![],
                bytes: &[],
                member: b"entry",
                pool: b"",
                passthrough,
            },
        );
        (unit, answer)
    }
    fn reply(unit: &mut UnitState, bytes: &[u8], first: bool, last: bool, cut: bool) -> Answer {
        far_end(
            &DoorHost(None),
            unit,
            &PieceIn {
                last,
                cut,
                status: first.then_some(200),
                fields: if first {
                    vec![(b"content-type", b"application/json")]
                } else {
                    vec![]
                },
                bytes,
                member: b"entry",
                pool: b"",
                passthrough: false,
            },
        )
    }
    fn assert_known_counts(answer: &Answer) {
        assert!(answer.units.iter().any(|u| u.class == 0 && u.amount == 7));
        assert!(answer.units.iter().any(|u| u.class == 1 && u.amount == 3));
        assert!(answer.units.iter().all(|u| u.class <= 6));
    }

    #[test]
    fn live_exempt_execution_completes_accounting_only_after_provider_eof() {
        let (mut unit, outgoing) = start(&serde_json::to_vec(&exempt_request()).unwrap());
        assert!(outgoing.to_far_end && outgoing.request.is_some());
        let response = response();
        // A complete JSON value is insufficient until the provider transport finishes.
        let pending = reply(&mut unit, &response, true, false, false);
        assert!(pending.bytes.is_empty() && pending.units.is_empty());
        let done = reply(&mut unit, &[], false, true, false);
        assert_eq!(done.verdict, VERDICT_OK);
        assert_known_counts(&done);
        let proof = &done
            .audits
            .iter()
            .find(|a| a.1 == b"llm.execution.exempt")
            .unwrap()
            .2;
        let proof: Value = serde_json::from_slice(proof).unwrap();
        assert_eq!(proof["model"], "claude-pinned");
        assert_eq!(proof["scope"], "request");
        assert!(proof.get("duration_seconds").is_none());
        assert!(proof.get("allocated_free_seconds").is_none());
    }

    #[test]
    fn live_billable_and_unproven_execution_never_dispatch() {
        let mut streamed = exempt_request();
        streamed["stream"] = true.into();
        let mut reused = request(serde_json::json!([]));
        reused["container"] = "container_shared".into();
        for body in [
            request(
                serde_json::json!([{"type":"code_execution_20260521","name":"code_execution"}]),
            ),
            request(serde_json::json!([{"type":"web_fetch_20990101","name":"web_fetch"}])),
            request(serde_json::json!([{"type":"web_fetch_20260209","name":"pretend"}])),
            streamed,
            reused,
        ] {
            let (unit, answer) = start(&serde_json::to_vec(&body).unwrap());
            assert_eq!(answer.status, 400);
            assert!(!answer.to_far_end && answer.request.is_none());
            assert!(unit.request.is_none());
        }
    }

    #[test]
    fn live_exemption_requires_policy_on_the_actual_selected_provider() {
        let body = serde_json::to_vec(&exempt_request()).unwrap();
        let (unit, answer) = start_with_policy(&body, false);
        assert_eq!(answer.status, 400);
        assert!(unit.request.is_none());
        let (unit, answer) = start_with_mode(&body, true, true);
        assert_eq!(answer.status, 400);
        assert!(unit.request.is_none());
    }

    #[test]
    fn only_the_declared_dated_policy_is_accepted() {
        let schema: Value = serde_json::from_slice(SETTINGS_SCHEMA).unwrap();
        let validator = jsonschema::validator_for(&schema).unwrap();
        for policy in [
            Value::Null,
            Value::from(POLICY),
            Value::from("latest"),
            Value::Bool(true),
        ] {
            let settings = serde_json::json!({
                "providers":{"claude":{"protocol":"anthropic","anthropic_execution_policy":policy}},
                "models":{"entry":{"provider":"claude"}}
            });
            let valid = policy.is_null() || policy == POLICY;
            assert_eq!(Shaping::from_settings(&settings).is_ok(), valid);
            assert_eq!(validator.is_valid(&settings), valid);
        }
    }

    #[test]
    fn execution_response_buffer_cannot_outgrow_the_configured_limit() {
        let (mut unit, _) = start(&serde_json::to_vec(&exempt_request()).unwrap());
        unit.execution.as_mut().unwrap().cap = 16;
        let done = reply(&mut unit, &response(), true, true, false);
        assert_eq!(done.verdict, VERDICT_COUNT_REFUSED);
        assert!(done.audits.iter().all(|a| a.1 != b"llm.execution.exempt"));
    }

    #[test]
    fn live_ordinary_text_accounting_still_completes_without_exemption() {
        let (mut unit, outgoing) =
            start(&serde_json::to_vec(&request(serde_json::json!([]))).unwrap());
        assert!(outgoing.request.is_some());
        let mut response: Value = serde_json::from_slice(&response()).unwrap();
        response.as_object_mut().unwrap().remove("container");
        response["usage"]["server_tool_use"]
            .as_object_mut()
            .unwrap()
            .remove("code_execution_requests");
        let done = reply(
            &mut unit,
            &serde_json::to_vec(&response).unwrap(),
            true,
            true,
            false,
        );
        assert_eq!(done.verdict, VERDICT_OK);
        assert_known_counts(&done);
        assert!(done.audits.iter().all(|a| a.1 != b"llm.execution.exempt"));
    }

    #[test]
    fn live_unexpected_execution_is_refused_without_erasing_known_tokens() {
        let (mut unit, outgoing) =
            start(&serde_json::to_vec(&request(serde_json::json!([]))).unwrap());
        assert!(outgoing.request.is_some());
        let done = reply(&mut unit, &response(), true, true, false);
        assert_eq!(done.verdict, VERDICT_COUNT_REFUSED);
        assert_known_counts(&done);
        assert!(done.audits.iter().all(|a| a.1 != b"llm.execution.exempt"));
    }

    #[test]
    fn unexpected_stream_execution_usage_cannot_bypass_the_request_guard() {
        let mut body = request(serde_json::json!([]));
        body["stream"] = true.into();
        let (mut unit, outgoing) = start(&serde_json::to_vec(&body).unwrap());
        assert!(outgoing.request.is_some());
        let frames = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_test1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-pinned\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":7,\"output_tokens\":0,\"server_tool_use\":{\"code_execution_requests\":1}}}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":3,\"server_tool_use\":{\"code_execution_requests\":1}}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"
        );
        let done = far_end(
            &DoorHost(None),
            &mut unit,
            &PieceIn {
                last: true,
                cut: false,
                status: Some(200),
                fields: vec![(b"content-type", b"text/event-stream")],
                bytes: frames.as_bytes(),
                member: b"entry",
                pool: b"",
                passthrough: false,
            },
        );
        assert_eq!(done.verdict, VERDICT_COUNT_REFUSED);
        assert_known_counts(&done);
    }

    #[test]
    fn live_exemption_never_accepts_ambiguous_wrong_model_or_cut_evidence() {
        let original = String::from_utf8(response()).unwrap();
        for body in [
            original.replace(
                "\"input_tokens\":7",
                "\"input_tokens\":1,\"input_tokens\":7",
            ),
            original.replace("claude-pinned", "another-model"),
            original.replace(
                "\"code_execution_requests\":1",
                "\"code_execution_requests\":true",
            ),
            original.replace(
                "\"code_execution_requests\":1",
                "\"code_execution_requests\":null",
            ),
            original.replace(
                "\"web_search_requests\":2",
                "\"web_search_requests\":\"unknown\"",
            ),
        ] {
            let (mut unit, _) = start(&serde_json::to_vec(&exempt_request()).unwrap());
            let done = reply(&mut unit, body.as_bytes(), true, true, false);
            assert_eq!(done.verdict, VERDICT_COUNT_REFUSED);
            assert_known_counts(&done);
        }
        let (mut unit, _) = start(&serde_json::to_vec(&exempt_request()).unwrap());
        let done = reply(&mut unit, &response(), true, true, true);
        assert_eq!(done.verdict, VERDICT_COUNT_REFUSED);
    }

    #[test]
    fn frozen_outbound_tools_cannot_be_forged_with_duplicate_or_client_flags() {
        for body in [
            br#"{"model":"entry","messages":[],"tools":[],"tools":[{"type":"web_fetch_20260209","name":"web_fetch"}]}"#.as_slice(),
            br#"{"model":"entry","messages":[],"tools":[{"type":"web_fetch_20260209","name":"web_fetch","name":"pretend"}]}"#.as_slice(),
            br#"{"model":"entry","messages":[],"execution_exempt":true,"tools":[{"type":"code_execution_20260521","name":"code_execution"}]}"#.as_slice(),
        ] {
            let (_, answer) = start(body);
            assert_eq!(answer.status, 400);
            assert!(!answer.to_far_end);
        }
    }
}
