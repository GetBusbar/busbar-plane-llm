//! Stream only after original provider ID publication; hold terminal bytes through clean EOF and
//! final observation settlement. Each input chunk is ingested once across host PENDING retries.

use super::*;
use crate::observations::native_stream::{
    NativeStream, ProfileVerdict, StreamCompletion, StreamFrame, StreamIssue, StreamMode,
};

pub(super) struct State {
    parser: Option<NativeStream>,
    prelude: Vec<u8>,
    terminal_wire: Vec<u8>,
    chunk: Option<Chunk>,
    pub(super) head_delivered: bool,
    checked_type: bool,
}
struct Chunk {
    output: Vec<u8>,
    created_id: Option<String>,
    bind_started: bool,
    completion: Option<StreamCompletion>,
    final_started: bool,
    last: bool,
}
impl State {
    pub(super) fn new(mode: StreamMode, id: Option<String>) -> Result<Self, &'static str> {
        Ok(Self {
            parser: Some(
                NativeStream::new(mode, id).map_err(|_| "invalid native stream identity")?,
            ),
            prelude: Vec::new(),
            terminal_wire: Vec::new(),
            chunk: None,
            head_delivered: false,
            checked_type: false,
        })
    }
    fn add(&mut self, chunk: &mut Chunk, frames: Vec<StreamFrame>) -> Result<(), &'static str> {
        for frame in frames {
            if matches!(
                frame.issue,
                Some(
                    StreamIssue::IdentityMismatch
                        | StreamIssue::ModelMismatch
                        | StreamIssue::InvalidUtf8
                        | StreamIssue::MalformedEvent
                        | StreamIssue::SizeLimit
                        | StreamIssue::StateLimit
                )
            ) {
                return Err("native stream contains invalid identity or event evidence");
            }
            if let Some(id) = frame.created_id {
                if chunk.created_id.as_ref().is_some_and(|old| old != &id) {
                    return Err("native stream changed identity");
                }
                chunk.created_id = Some(id);
            }
            if frame.terminal {
                append(&mut self.terminal_wire, &frame.wire)?;
            } else {
                append(&mut chunk.output, &frame.wire)?;
            }
        }
        Ok(())
    }
    pub(super) fn error(&mut self, message: &str) -> Answer {
        let mut bytes = b"event: error\ndata: ".to_vec();
        bytes.extend(serde_json::to_vec(&serde_json::json!({"event_type":"error","error":{"code":"internal_error","message":message}})).unwrap_or_default());
        bytes.extend(b"\n\n");
        Answer {
            bytes,
            verdict: VERDICT_HARD,
            done: true,
            audits: unresolved(),
            ..Answer::default()
        }
    }
}
fn append(target: &mut Vec<u8>, bytes: &[u8]) -> Result<(), &'static str> {
    if target.len().saturating_add(bytes.len()) > MAX_RESPONSE {
        return Err("native stream retention limit exceeded");
    }
    target.extend(bytes);
    Ok(())
}
fn unresolved() -> Vec<(u32, Vec<u8>, Vec<u8>)> {
    vec![(
        AUDIT_DEGRADED,
        b"interaction.usage_unresolved".to_vec(),
        b"stream does not establish supported complete final usage evidence".to_vec(),
    )]
}

pub(super) fn receive(
    native: &mut Native,
    input: &Lent<'_, OnPieceIn>,
) -> Result<PieceResult, &'static str> {
    let mut state = native.stream.take().ok_or("missing native stream state")?;
    let result = receive_inner(native, &mut state, input);
    native.stream = Some(state);
    result
}
fn receive_inner(
    native: &mut Native,
    state: &mut State,
    input: &Lent<'_, OnPieceIn>,
) -> Result<PieceResult, &'static str> {
    if input.flags() & (PIECE_CUT | busbar_contract::abi::plane::PIECE_REFUSED) != 0 {
        if let Some(session) = native.session.as_mut() {
            let _ = session.incomplete_stream();
        }
        return Err("native stream interrupted; original accounting remains unresolved");
    }
    if state.chunk.is_none() {
        let trailers = input.flags() & PIECE_FIELDS != 0;
        if input.flags() & PIECE_HAS_STATUS != 0 && !trailers {
            native.response_status =
                u16::try_from(input.status_code()).map_err(|_| "invalid native stream status")?;
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
        if !state.checked_type {
            let event_stream = native.response_fields.iter().any(|(name, value)| {
                name.eq_ignore_ascii_case(b"content-type")
                    && std::str::from_utf8(value).ok().is_some_and(|v| {
                        v.split(';')
                            .next()
                            .unwrap_or("")
                            .trim()
                            .eq_ignore_ascii_case("text/event-stream")
                    })
            });
            if !event_stream {
                return Err("native stream did not return event-stream framing");
            }
            state.checked_type = true;
        }
        let mut chunk = Chunk {
            output: Vec::new(),
            created_id: None,
            bind_started: false,
            completion: None,
            final_started: false,
            last: input.flags() & PIECE_LAST != 0,
        };
        if !trailers {
            let frames = state
                .parser
                .as_mut()
                .ok_or("native stream already ended")?
                .push(input.bytes().bytes())
                .map_err(|_| "native stream exceeds bounded framing")?;
            state.add(&mut chunk, frames)?;
        }
        if chunk.last {
            let mut completion = state
                .parser
                .take()
                .ok_or("native stream already ended")?
                .finish();
            state.add(&mut chunk, std::mem::take(&mut completion.frames))?;
            append(&mut state.terminal_wire, &completion.tail)?;
            chunk.completion = Some(completion);
        }
        state.chunk = Some(chunk);
    }
    let chunk = state.chunk.as_mut().ok_or("missing stream chunk")?;
    let session = native.session.as_mut().ok_or("missing native session")?;
    let extension = input.head().extensions().bytes();
    if !chunk.bind_started {
        if let Some(id) = &chunk.created_id {
            session.bind_provider_id(id, extension)?;
        }
        chunk.bind_started = true;
    }
    if let Step::Pending(bytes) = session.poll(extension)? {
        return Ok(PieceResult::Pending(bytes));
    }
    if chunk.last && !chunk.final_started {
        let completion = chunk
            .completion
            .as_ref()
            .ok_or("missing stream completion")?;
        if let Some(terminal) = &completion.terminal {
            let profile=serde_json::to_vec(&serde_json::json!({"version":1,"verdict":completion.profile,"diagnostics":completion.diagnostics})).map_err(|_|"invalid stream profile evidence")?;
            session.response_with_profile(
                native.response_status,
                &terminal.interaction,
                false,
                matches!(completion.profile, ProfileVerdict::Supported(_)),
                &profile,
                extension,
            )?;
        } else {
            session.incomplete_stream()?;
        }
        chunk.final_started = true;
    }
    if let Step::Pending(bytes) = session.poll(extension)? {
        return Ok(PieceResult::Pending(bytes));
    }
    let mut chunk = state.chunk.take().ok_or("missing stream chunk")?;
    let bound = session.run().and_then(|r| r.provider_id()).is_some();
    if !bound && !chunk.last {
        append(&mut state.prelude, &chunk.output)?;
        return Ok(PieceResult::Answer(Answer::default()));
    }
    if !bound {
        return Err("native stream ended without attributable provider identity");
    }
    if !state.prelude.is_empty() {
        let mut output = std::mem::take(&mut state.prelude);
        append(&mut output, &chunk.output)?;
        chunk.output = output;
    }
    if chunk.last {
        append(&mut chunk.output, &state.terminal_wire)?;
        state.terminal_wire.clear();
    }
    let missing_terminal = chunk.last
        && chunk
            .completion
            .as_ref()
            .is_some_and(|c| c.terminal.is_none());
    if missing_terminal {
        chunk.output.extend(
            state
                .error("native stream ended without authoritative completion")
                .bytes,
        );
    }
    if chunk.output.is_empty() && !chunk.last {
        return Ok(PieceResult::Answer(Answer::default()));
    }
    let first = !state.head_delivered;
    state.head_delivered = true;
    Ok(PieceResult::Answer(Answer {
        status: if first {
            u32::from(native.response_status)
        } else {
            0
        },
        fields: if first {
            std::mem::take(&mut native.response_fields)
        } else {
            Vec::new()
        },
        bytes: chunk.output,
        verdict: if missing_terminal {
            VERDICT_HARD
        } else if chunk.last {
            VERDICT_OK
        } else {
            VERDICT_NONE
        },
        done: chunk.last,
        audits: if chunk.last && session.unresolved_final() {
            unresolved()
        } else {
            Vec::new()
        },
        ..Answer::default()
    }))
}

#[cfg(test)]
#[allow(unsafe_code)]
#[path = "../../../tests/support/native_stream_door.rs"]
mod tests;
