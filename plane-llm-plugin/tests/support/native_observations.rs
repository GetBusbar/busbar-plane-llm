//! Native tier quantities pass through the shipped door and real loader callbacks.
use super::*;
use busbar_contract::abi::{mechanism::extensions, sdk::recovery};
use std::collections::BTreeMap;

pub(super) fn exercise(dropped: bool) -> Vec<(String, String)> {
    let host = Arc::new(native_observation_host::NativeHost::default());
    let d = Arc::new(Dispatcher::with_services(
        DispatchConfig::default(),
        host.clone(),
        busbar_contract::services::test_caller_binding(),
    ));
    let p = load(dropped, &d);
    let settings=serde_json::to_vec(&serde_json::json!({"providers":{"google":{"protocol":"gemini"}},"models":{"native":{"provider":"google","upstream_model":"gemini-pinned"}}})).unwrap();
    let owned=serde_json::to_vec(&serde_json::json!({"interactions":{"models":{"native":{"account_namespace":"account-a","pricing_modes":["flex","priority"]}}}})).unwrap();
    // Native settings already contain the complete consumed provider/model sections.
    let mut frame: Frame<PlaneOpenIn, PlaneOpenOut> = Frame::new(input(), output());
    frame.input.open.generation = 1;
    frame.input.open.settings = json(&settings);
    frame.input.owned = json(&owned);
    let opened = p.open(&mut frame).0;
    assert_eq!(opened.outcome, Outcome::Ready, "{}", called(&opened));
    p.ready(&d, OP_DEADLINE).unwrap();
    let mut answers = Vec::new();
    for (i, tier) in ["flex", "priority"].into_iter().enumerate() {
        let id = 90 + i as u64;
        let unit = Unit::mint(&d, 65536);
        let request = serde_json::to_vec(
            &serde_json::json!({"model":"native","input":"hi","service_tier":tier}),
        )
        .unwrap();
        let arrival = Arrival {
            unit: id,
            claim: 0,
            method: b"POST".to_vec(),
            target: b"/v1beta/interactions".to_vec(),
            fields: vec![],
            route: true,
        };
        let arrived = arrive(&p, &arrival, &request);
        assert!(arrived.starts_with("Ready"), "{arrived}");
        let own = Carried {
            member: b"native".to_vec(),
            pool: None,
            caller_ref: Some(vec![b'a'; 64]),
        };
        let before = host.state.lock().unwrap().publications;
        let sent = piece(&p, &d, &unit, attempt(id, 0), &own);
        assert_eq!(
            sent.called.outcome,
            Outcome::Ready,
            "{}",
            sent.line(Piece::session_line)
        );
        assert_ne!(sent.frame.as_ref().unwrap().out.flags & EMIT_TO_FAR_END, 0);
        assert!(
            host.state.lock().unwrap().publications > before,
            "dispatch follows original intent publication"
        );
        let body=serde_json::to_vec(&serde_json::json!({"id":format!("interaction-{tier}"),"model":"gemini-pinned","service_tier":tier,"status":"completed","steps":[],"usage":{"total_input_tokens":100,"total_cached_tokens":20,"total_output_tokens":30,"total_thought_tokens":40,"total_tool_use_tokens":0,"total_tokens":170,"input_tokens_by_modality":[{"modality":"text","tokens":100}]}})).unwrap();
        let head = vec![("content-type".into(), "application/json".into())];
        let read = piece(
            &p,
            &d,
            &unit,
            far_end(id, 0, PIECE_HAS_STATUS | PIECE_LAST, &body, (200, &head)),
            &own,
        );
        assert_eq!(
            read.called.outcome,
            Outcome::Ready,
            "{}",
            read.line(Piece::session_line)
        );
        assert_eq!(read.frame.as_ref().unwrap().out.reply_status, 200);
        let ordinary = read.piece.units(&read.frame.as_ref().unwrap().out);
        assert!(
            ordinary.iter().all(|row| row.starts_with("19:")),
            "native token observations must not also charge ordinary rows: {ordinary:?}"
        );
        let state = host.state.lock().unwrap();
        assert!(
            state.closed.contains(&id),
            "success requires final scope receipt"
        );
        let mut totals = BTreeMap::<u32, u64>::new();
        for observation in state
            .observations
            .iter()
            .filter(|o| o.scope == [id as u8; 32])
        {
            for (class, count) in &observation.counts {
                *totals.entry(*class).or_default() += count.whole_u64().unwrap();
            }
        }
        let first = 22 + 3 * i as u32;
        assert_eq!(
            totals,
            BTreeMap::from([(first, 80), (first + 1, 70), (first + 2, 20)])
        );
        let measured: Vec<_> = totals
            .into_iter()
            .map(|(class, count)| format!("{class}:{count}:true"))
            .collect();
        answers.push((
            format!("native {tier} observations"),
            format!("observation_units={measured:?}"),
        ));
        drop(state);
        unit.end(&d);
    }
    assert_eq!(close(&p).outcome, Outcome::Ready);
    answers
}

fn piece(p: &Plugin<Plane>, d: &Dispatcher, unit: &Unit, g: Given<'_>, own: &Carried) -> Pieced {
    let (piece, mut frame) = Piece::lent(unit.reply_cap, g, (CAP, CAP), own);
    // This fixture host has installed original-owner work recovery; the actual loader supplies
    // the separate conditional/observation capabilities and resumes each receipt callback.
    let capability = extensions::encode(&[(recovery::CAPABILITY, &[1])]);
    frame.input.head.extensions = octets(&capability);
    let done = submit_on(
        p,
        d,
        unit.ticket,
        slot::ON_PIECE,
        frame,
        (DeadlineClass::Stream, deadline()),
        Arc::clone(&piece) as Lent,
        Duration::ZERO,
    );
    Pieced {
        called: answered(&done),
        short: done.short,
        piece,
        frame: done.frame,
    }
}
