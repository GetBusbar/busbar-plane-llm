//! Native Interactions serving. Provider semantics stay in the LLM plane; ownership and prices
//! cross only the generic authenticated-route, conditional-record and observation mechanisms.

mod stream;

use super::*;
use crate::observations::{
    crossing::Crossing,
    interactions::{self, Binding, Create, Operation},
    lifecycle::Partition,
    lookup::{Found, Lookup},
    native_request::{self, NativeConfig},
    session::{Session, Step},
};
use std::task::Poll;

pub(super) const SECTION: &str = "interactions";
const MAX_RESPONSE: usize = 1024 * 1024;

pub(super) struct Native {
    operation: Operation,
    create: Option<Create>,
    body: Vec<u8>,
    lookup: Option<Lookup>,
    found: Option<Found>,
    session: Option<Session>,
    head_sent: bool,
    response: Vec<u8>,
    response_fields: Vec<(Vec<u8>, Vec<u8>)>,
    response_status: u16,
    response_queued: bool,
    streaming: bool,
    stream: Option<stream::State>,
}

pub(super) fn config(owned: &[u8], shaping: &Shaping) -> Result<NativeConfig, String> {
    let all: Value = if owned.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice(owned).map_err(|e| format!("interactions: {e}"))?
    };
    let config = match all.get(SECTION) {
        None | Some(Value::Null) => NativeConfig::default(),
        Some(value) => serde_json::from_value::<NativeConfig>(value.clone())
            .map_err(|e| format!("interactions: {e}"))?,
    };
    config.validate(shaping).map_err(str::to_owned)?;
    Ok(config)
}

pub(super) fn claims(enabled: bool) -> Vec<ClaimSpec> {
    if !enabled {
        return Vec::new();
    }
    [
        ("POST", "/v1beta/interactions", CLAIM_EXACT),
        ("GET", "/v1beta/interactions/", 0),
        ("POST", "/v1beta/interactions/", 0),
        ("DELETE", "/v1beta/interactions/", 0),
    ]
    .into_iter()
    .map(|(verb, path, flags)| {
        let mut c = ClaimSpec::new(verb, path, TRANSPORT, flags);
        c.refusal_dialect = u16::try_from(dialect_index("gemini")).unwrap_or(0);
        c
    })
    .collect()
}

pub(super) fn recognizes(target: &str) -> bool {
    let path = target.split_once('?').map_or(target, |(path, _)| path);
    path == "/v1beta/interactions" || path.starts_with("/v1beta/interactions/")
}

pub(super) fn arrive(
    door: &LlmDoor,
    unit: &mut UnitState,
    method: &str,
    target: &str,
    body: &[u8],
    out: &mut Out<'_, ArriveOut>,
) -> Result<(), &'static str> {
    if door.native.models.is_empty() {
        return Err("native interactions are not enabled");
    }
    let operation = interactions::operation(method, target)?;
    let create = if operation == Operation::Create {
        let create = interactions::create(body)?;
        // Continuation ownership is checked in the authenticated route callback before admission.
        let lane = unit
            .shaping
            .lane(&create.model)
            .ok_or("native model is not configured")?;
        let config = door
            .native
            .models
            .get(&create.model)
            .ok_or("native model is not enabled")?;
        if create.previous_interaction_id.is_some() {
            out.route(busbar_contract::abi::plane::ROUTE_AUTHENTICATED, "");
        } else {
            native_request::prepare_create(lane, config, &create, body, None)?;
            out.route(ROUTE_DIRECT, &create.model);
        }
        Some(create)
    } else {
        if !body.is_empty() {
            return Err("stored interaction operation does not accept a body");
        }
        out.route(busbar_contract::abi::plane::ROUTE_AUTHENTICATED, "");
        None
    };
    let streaming = create.as_ref().is_some_and(|c| c.stream)
        || matches!(operation, Operation::Get { stream: true, .. });
    if streaming {
        out.stream();
    }
    out.once();
    out.set(|o| &o.dialect, dialect_index("gemini"));
    out.set(
        |o| &o.op_class,
        op_class_of(busbar_contract::operation::OpVerb::CHAT),
    );
    out.set(|o| &o.principal_need, PRINCIPAL_REQUIRED);
    unit.native = Some(Native {
        operation,
        create,
        body: body.to_vec(),
        lookup: None,
        found: None,
        session: None,
        head_sent: false,
        response: Vec::new(),
        response_fields: Vec::new(),
        response_status: 0,
        response_queued: false,
        streaming,
        stream: None,
    });
    Ok(())
}

pub(super) fn authenticated(
    door: &LlmDoor,
    unit: &mut UnitState,
    input: &Lent<'_, busbar_contract::abi::plane::AuthenticatedRouteIn>,
) -> Poll<Result<String, &'static str>> {
    let Some(native) = unit.native.as_mut() else {
        return Poll::Ready(Err("missing native request"));
    };
    let caller = match std::str::from_utf8(input.caller_ref().bytes()) {
        Ok(c) => c,
        Err(_) => return Poll::Ready(Err("invalid authenticated identity")),
    };
    let id = match &native.operation {
        Operation::Get { id, .. } | Operation::Cancel { id } | Operation::Delete { id } => id,
        Operation::Create => match native
            .create
            .as_ref()
            .and_then(|c| c.previous_interaction_id.as_ref())
        {
            Some(id) => id,
            None => return Poll::Ready(Err("missing continuation ID")),
        },
    };
    if native.lookup.is_none() {
        native.lookup = match Lookup::new(caller, id, input.head().ticket()) {
            Ok(l) => Some(l),
            Err(_) => return Poll::Ready(Err("invalid native lookup")),
        };
    }
    let Some(services) = &door.services else {
        return Poll::Ready(Err("record lookup unavailable"));
    };
    let found = match native
        .lookup
        .as_mut()
        .unwrap()
        .poll(services, input.head().ticket(), caller)
    {
        Poll::Pending => return Poll::Pending,
        Poll::Ready(Err(_)) => {
            return Poll::Ready(Err("interaction is not attributable to this caller"))
        }
        Poll::Ready(Ok(found)) => found,
    };
    let entry = found.run.binding().route_entry.clone();
    let Some(lane) = unit.shaping.lane(&entry) else {
        return Poll::Ready(Err("original model is not configured"));
    };
    let Some(config) = door.native.models.get(&entry) else {
        return Poll::Ready(Err("original native model is not enabled"));
    };
    let prepared = if let Some(create) = &native.create {
        if create.model != entry {
            return Poll::Ready(Err("continuation must use original route"));
        }
        native_request::prepare_create(
            lane,
            config,
            create,
            &native.body,
            Some(native_request::Continuation {
                id,
                binding: found.run.binding(),
            }),
        )
    } else {
        native_request::prepare_stored(lane, config, found.run.binding(), &native.operation)
    };
    if let Err(error) = prepared {
        return Poll::Ready(Err(error));
    }
    native.found = Some(found);
    Poll::Ready(Ok(entry))
}

pub(super) enum PieceResult {
    Pending(Vec<u8>),
    Answer(Answer),
}

pub(super) fn piece(
    door: &LlmDoor,
    unit: &mut UnitState,
    input: &Lent<'_, OnPieceIn>,
) -> Result<PieceResult, &'static str> {
    let mut native = unit.native.take().ok_or("missing native request")?;
    let result = piece_inner(door, unit, &mut native, input);
    let result = match result {
        Err(message) if native.stream.as_ref().is_some_and(|s| s.head_delivered) => {
            if let Some(session) = native.session.as_mut() {
                let _ = session.incomplete_stream();
            }
            Ok(PieceResult::Answer(
                native.stream.as_mut().unwrap().error(message),
            ))
        }
        other => other,
    };
    unit.native = Some(native);
    result
}
fn piece_inner(
    door: &LlmDoor,
    unit: &mut UnitState,
    native: &mut Native,
    input: &Lent<'_, OnPieceIn>,
) -> Result<PieceResult, &'static str> {
    let extension = input.head().extensions().bytes();
    match input.from() {
        FROM_KERNEL => {
            if native.head_sent {
                return Err("native dispatch is not retryable");
            }
            if native.session.is_none() {
                Crossing::supported(extension)?;
                let member = std::str::from_utf8(input.member().bytes())
                    .map_err(|_| "invalid selected native member")?;
                let lane = unit
                    .shaping
                    .lane(member)
                    .ok_or("selected native model missing")?;
                let config = door
                    .native
                    .models
                    .get(member)
                    .ok_or("selected native model not enabled")?;
                let (mut request, session) = if let Some(create) = &native.create {
                    let predecessor = native.found.as_ref().map(|f| native_request::Continuation {
                        id: f.run.provider_id().unwrap_or(""),
                        binding: f.run.binding(),
                    });
                    let request = native_request::prepare_create(
                        lane,
                        config,
                        create,
                        &native.body,
                        predecessor,
                    )?;
                    let caller = std::str::from_utf8(input.caller_ref().bytes())
                        .map_err(|_| "invalid host principal")?;
                    let partition = Partition::new(caller, &config.account_namespace)?;
                    let mut random = [0; 32];
                    door.services
                        .as_ref()
                        .ok_or("host random unavailable")?
                        .random_fill(
                            CompletionHandle {
                                ticket: input.head().ticket(),
                                seq: 90,
                                ..CompletionHandle::ZERO
                            },
                            &mut random,
                        )
                        .map_err(|_| "host random unavailable")?;
                    let binding = Binding {
                        scope: [0; 32],
                        account_namespace: config.account_namespace.clone(),
                        route_entry: member.into(),
                        model: lane.wire_model().into(),
                        pricing_mode: create.pricing_mode.clone(),
                        usage_schema: "gemini-interactions-2026-11-10".into(),
                        input_class: 0,
                        cache_read_class: 2,
                        output_class: 1,
                        tool_use_class: None,
                        priced_classes: vec![0, 1, 2],
                    };
                    (
                        request,
                        Session::create(partition, hex::encode(random), binding, extension)?,
                    )
                } else {
                    let found = native
                        .found
                        .take()
                        .ok_or("missing authenticated original run")?;
                    let request = native_request::prepare_stored(
                        lane,
                        config,
                        found.run.binding(),
                        &native.operation,
                    )?;
                    (request, Session::stored(found, extension)?)
                };
                request.fields =
                    attempt::native_forward_fields(lane, request.fields, &unit.caller_fields());
                unit.request = Some(request);
                unit.member = member.into();
                native.session = Some(session);
            }
            match native.session.as_mut().unwrap().poll(extension)? {
                Step::Pending(bytes) => return Ok(PieceResult::Pending(bytes)),
                Step::Ready => native.session.as_mut().unwrap().dispatch()?,
            }
            if native.streaming {
                let mode = if matches!(
                    &native.operation,
                    Operation::Get {
                        last_event_id: Some(_),
                        ..
                    }
                ) {
                    crate::observations::native_stream::StreamMode::Replay
                } else {
                    crate::observations::native_stream::StreamMode::Fresh
                };
                let id = native
                    .session
                    .as_ref()
                    .and_then(|s| s.run())
                    .and_then(|r| r.provider_id())
                    .map(str::to_owned);
                native.stream = Some(stream::State::new(mode, id)?);
            }
            native.head_sent = true;
            let request = unit
                .request
                .as_ref()
                .ok_or("missing native upstream request")?;
            Ok(PieceResult::Answer(Answer {
                to_far_end: true,
                request: Some((
                    request.verb.as_bytes().to_vec(),
                    request.target.as_bytes().to_vec(),
                )),
                fields: request
                    .fields
                    .iter()
                    .map(|(k, v)| (k.as_bytes().to_vec(), v.clone()))
                    .collect(),
                ..Answer::default()
            }))
        }
        FROM_CALLER => Ok(PieceResult::Answer(Answer {
            to_far_end: true,
            bytes: unit
                .request
                .as_mut()
                .map(|r| std::mem::take(&mut r.body))
                .unwrap_or_default(),
            ..Answer::default()
        })),
        FROM_FAR_END => {
            if native.streaming {
                if input.flags() & PIECE_HAS_STATUS != 0 && input.flags() & PIECE_FIELDS == 0 {
                    native.response_status = u16::try_from(input.status_code())
                        .map_err(|_| "invalid native stream status")?;
                }
                if (200..300).contains(&native.response_status) {
                    return stream::receive(native, input);
                }
                if native.response_status == 0 {
                    return Err("native stream has no upstream status");
                }
                native.streaming = false; // Preserve a provider's ordinary JSON error response.
            }
            if !native.response_queued {
                if input.flags() & busbar_contract::abi::plane::PIECE_REFUSED != 0 {
                    return Err("native upstream dispatch refused; accounting remains unresolved");
                }
                if input.flags() & PIECE_CUT != 0 {
                    return Err(
                        "native upstream response interrupted; create outcome may be unresolved",
                    );
                }
                let trailers = input.flags() & PIECE_FIELDS != 0;
                if !trailers {
                    if input.flags() & PIECE_HAS_STATUS != 0 {
                        native.response_status = u16::try_from(input.status_code())
                            .map_err(|_| "invalid native response status")?;
                        native.response_fields = input
                            .head_fields()
                            .iter()
                            .filter_map(|field| {
                                let name = field.name().bytes();
                                (!name.eq_ignore_ascii_case(b"content-length")
                                    && !name.eq_ignore_ascii_case(b"transfer-encoding")
                                    && !name.eq_ignore_ascii_case(b"connection"))
                                .then(|| (name.to_vec(), field.value().bytes().to_vec()))
                            })
                            .collect();
                    }
                    let bytes = input.bytes().bytes();
                    if native.response.len().saturating_add(bytes.len()) > MAX_RESPONSE {
                        return Err(
                            "native response exceeds buffered limit; accounting remains unresolved",
                        );
                    }
                    native.response.extend(bytes);
                }
                if input.flags() & PIECE_LAST == 0 {
                    return Ok(PieceResult::Answer(Answer::default()));
                }
                native.response_queued = true;
                if !(100..600).contains(&native.response_status) {
                    return Err("native upstream response has no valid status");
                }
                let profile = native_request::final_text_profile(&native.response).is_ok();
                native
                    .session
                    .as_mut()
                    .ok_or("missing native session")?
                    .response(
                        native.response_status,
                        &native.response,
                        matches!(native.operation, Operation::Delete { .. }),
                        profile,
                        extension,
                    )?;
            }
            match native
                .session
                .as_mut()
                .ok_or("missing native session")?
                .poll(extension)?
            {
                Step::Pending(bytes) => Ok(PieceResult::Pending(bytes)),
                Step::Ready => Ok(PieceResult::Answer(Answer {
                    status: u32::from(native.response_status),
                    fields: std::mem::take(&mut native.response_fields),
                    bytes: std::mem::take(&mut native.response),
                    verdict: VERDICT_OK,
                    done: true,
                    audits: native.session.as_ref().filter(|s| s.unresolved_final()).map(|_| vec![(
                        AUDIT_DEGRADED,
                        b"interaction.usage_unresolved".to_vec(),
                        b"terminal provider response lacks supported complete usage evidence".to_vec(),
                    )]).unwrap_or_default(),
                    ..Answer::default()
                })),
            }
        }
        _ => Err("unexpected native piece"),
    }
}

pub(super) fn failure(message: &str) -> Answer {
    Answer {
        status: 503,
        fields: vec![(b"content-type".to_vec(), b"application/json".to_vec())],
        bytes: serde_json::to_vec(
            &serde_json::json!({"error":{"code":503,"status":"UNAVAILABLE","message":message}}),
        )
        .unwrap_or_default(),
        verdict: VERDICT_HARD,
        done: true,
        ..Answer::default()
    }
}
