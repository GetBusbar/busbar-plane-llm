//! Actual linked/drop response metering for appended ordinary classes.
use super::*;
use serde_json::json;

pub(super) fn exercise(
    p: &Plugin<Plane>,
    d: &Arc<Dispatcher>,
    answers: &mut Vec<(String, String)>,
) {
    let cases = [
        (
            "claude",
            "/v1/messages",
            json!({"model":"claude","messages":[{"role":"user","content":"run"}],"max_tokens":64}),
            json!({"id":"msg_count","type":"message","role":"assistant","model":"claude","content":[],"stop_reason":"end_turn","usage":{"input_tokens":2,"output_tokens":1,"server_tool_use":{"code_execution_requests":3}}}),
            vec![(28, 3)],
        ),
        (
            "response-model",
            "/v1/responses",
            json!({"model":"response-model","input":"draw"}),
            json!({"id":"resp_parts","object":"response","status":"completed","output":[
           {"id":"img","type":"image_generation_call","status":"completed","result":"payload"},
           {"id":"code","type":"code_interpreter_call","status":"completed","container_id":"cntr_1","code":"1+1","outputs":[]},
           {"id":"shell","type":"shell_call","status":"completed","environment":{"type":"container_reference","container_id":"cntr_1"},"action":{"commands":["true"]}}
         ],"usage":{"input_tokens":20,"output_tokens":2,"input_tokens_details":{"cached_tokens":0,"cache_write_tokens":7,"cache_write_tokens_details":{"ttl_30m_tokens":3,"ttl_12h_tokens":4}}},
           "tool_usage":{"image_gen":{"input_tokens":30,"input_tokens_details":{"text_tokens":10,"image_tokens":20},"cached_tokens":7,"cached_tokens_details":{"cached_text_tokens":3,"cached_image_tokens":4},"output_tokens":17,"output_tokens_details":{"text_tokens":5,"image_tokens":12}}}}),
            vec![
                (20, 3),
                (21, 4),
                (29, 7),
                (30, 3),
                (32, 16),
                (33, 4),
                (38, 5),
                (39, 12),
                (41, 1),
                (42, 1),
                (43, 1),
            ],
        ),
        (
            "response-model",
            "/v1/responses",
            json!({"model":"response-model","input":"draw"}),
            json!({"id":"resp_unknown","object":"response","status":"completed","output":[],"tool_usage":{"image_gen":{"input_tokens_details":{"text_tokens":11,"image_tokens":22},"output_tokens":7}}}),
            vec![(31, 11), (34, 22), (40, 7)],
        ),
        (
            "response-model",
            "/v1/responses",
            json!({"model":"response-model","input":"draw"}),
            json!({"id":"resp_scalar","object":"response","status":"completed","output":[],"tool_usage":{"image_gen":{"input_tokens":30,"cached_tokens":8}}}),
            vec![(35, 22), (36, 8)],
        ),
        (
            "response-model",
            "/v1/responses",
            json!({"model":"response-model","input":"draw"}),
            json!({"id":"resp_scalar_unknown","object":"response","status":"completed","output":[],"tool_usage":{"image_gen":{"input_tokens":9}}}),
            vec![(37, 9)],
        ),
    ];
    for (i, (member, target, request, response, expected)) in cases.into_iter().enumerate() {
        let id = 80 + i as u64;
        let unit = Unit::mint(d, 65536);
        let arrival = Arrival {
            unit: id,
            claim: 0,
            method: b"POST".to_vec(),
            target: target.as_bytes().to_vec(),
            fields: vec![],
            route: true,
        };
        let read = arrive(p, &arrival, &serde_json::to_vec(&request).unwrap());
        assert!(read.starts_with("Ready"), "{read}");
        let own = Carried {
            member: member.as_bytes().to_vec(),
            pool: None,
            caller_ref: None,
        };
        let sent = on_piece(p, d, &unit, attempt(id, 0), (CAP, CAP), &own);
        assert_eq!(sent.called.outcome, Outcome::Ready);
        let body = serde_json::to_vec(&response).unwrap();
        let fields = vec![("content-type".into(), "application/json".into())];
        let reply = on_piece(
            p,
            d,
            &unit,
            far_end(id, 0, PIECE_LAST | PIECE_HAS_STATUS, &body, (200, &fields)),
            (CAP, CAP),
            &own,
        );
        assert_eq!(reply.called.outcome, Outcome::Ready);
        let line = reply.line(Piece::session_line);
        for (class, amount) in expected {
            assert!(
                line.contains(&format!("{class}:{amount}:true")),
                "class {class}: {line}"
            );
        }
        answers.push((format!("raw measured response {i}"), line));
        unit.end(d);
    }
}
