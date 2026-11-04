//! A far end's success answer taken whole: a non-stream answer from a far end of another dialect
//! (written as the caller's own stream when the caller asked for one).

use crate::codec::cells::{OperationHandler, TranslatedResponse};
use crate::codec::protocol::{APPLICATION_JSON, KIND_API_ERROR, KIND_NOT_FOUND, TEXT_EVENT_STREAM};
use busbar_contract::billing::Billing;
use busbar_contract::operation::OpVerb;
use serde_json::Value;

use super::super::attempt::DETAIL_ENDPOINT_UNSUPPORTED_OPERATION;
use super::super::decl;
use super::super::refuse::{render, Rendered};
use super::failure::request_id_field;
use super::wire;
use super::wire::head_field;
use crate::codec::drops;
use crate::codec::translate::{TranslateCodec as _, TranslateRespInput};

fn handler(dialect: &str, operation: OpVerb) -> Option<&'static dyn OperationHandler> {
    decl(dialect)
        .and_then(|d| d.handler)
        .and_then(|rh| rh.operation_handler(operation))
}

/// How a whole answer ended for the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WholeEnd {
    /// The caller reads the answer, in its own dialect.
    Delivered,
    /// The far end reported the generation failed: the caller reads a 502, and the usage the far
    /// end reported stands.
    FailedGeneration,
    /// The caller's dialect serves no such operation: a 404, nothing delivered.
    IngressUnsupported,
    /// The caller's dialect cannot be written from the answer: a 500, nothing delivered.
    NotTranslatable,
    /// The body is over the translation cap: our limit, not the far end's fault; a 500, nothing
    /// delivered.
    OverCap,
    /// The far end's body stopped short of its end: a failed transfer; a 502, nothing delivered.
    Cut,
}

/// A whole answer, as the caller reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct Whole {
    /// How it ended.
    pub end: WholeEnd,
    /// What the far end reported it used: set on a delivery and on a failed generation.
    pub usage: Option<Billing>,
    /// What the caller reads.
    pub answer: Rendered,
    /// Why the far end's dialect refused the body, when it did (for the operator's log).
    pub refused: Option<Refusal>,
    /// Wire paths of the far end's answer that did not cross to the caller's dialect on a delivered
    /// TRANSLATE (design F3 "Drops"), already warned through the one drop path; the host records one
    /// audit row per path (`egress.control_unrepresentable`, `<path> from <dialect>`, degraded). Empty
    /// on a relay and on every end that delivers nothing.
    pub dropped: Vec<String>,
    /// Whether the caller reads a stream synthesized from this whole response.
    pub synthesized: bool,
}

/// The far end's dialect refusing a whole body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The body was opaque (bridged at the byte level) rather than JSON.
    pub opaque: bool,
    /// The codec's own account.
    pub detail: String,
}

/// What of the exchange a whole answer is written for.
#[derive(Clone, Copy, Debug)]
pub struct WholeCtx<'a> {
    /// The caller's dialect.
    pub ingress: &'a str,
    /// The far end's dialect.
    pub egress: &'a str,
    /// The operation.
    pub operation: OpVerb,
    /// The model's configured name (a translated answer that names none reports it).
    pub model: &'a str,
    /// The caller asked for a stream.
    pub wants_stream: bool,
    /// The caller's stream is a JSON array rather than server-sent events.
    pub json_array: bool,
    /// The caller's request, when it was JSON (a dialect whose answer mirrors request members
    /// reads them here).
    pub request: Option<&'a Value>,
    /// Wall-clock seconds, for a translated answer's creation stamp.
    pub now_s: u64,
    /// Milliseconds from the attempt's start, for a dialect whose answer reports its latency.
    pub elapsed_ms: Option<u64>,
    /// The TRANSLATION CAP the plane enforces, the one its settings carry
    /// ([`crate::exchange::shaping::Shaping::translate_cap`]): a writer re-emitting the answer as a
    /// stream bounds what it accumulates there.
    pub translate_cap: usize,
}

/// The caller's error for an answer that could not be relayed.
fn internal(host: &dyn crate::codec::cells::CodecHost, ingress: &str, status: u16) -> Rendered {
    render(
        host,
        ingress,
        status,
        KIND_API_ERROR,
        wire::GENERIC_RESPONSE_ERROR_DETAIL,
        0,
    )
}

/// A delivered answer: the far end's status, the content type, the caller's request id.
fn delivered(
    host: &dyn crate::codec::cells::CodecHost,
    ctx: &WholeCtx<'_>,
    status: u16,
    content_type: &[u8],
    body: Vec<u8>,
) -> Rendered {
    let mut fields = vec![("content-type".to_string(), content_type.to_vec())];
    fields.extend(request_id_field(host, ctx.ingress, &[]));
    Rendered {
        status,
        fields,
        body,
    }
}

fn whole(end: WholeEnd, usage: Option<Billing>, answer: Rendered) -> Whole {
    Whole {
        end,
        usage,
        answer,
        refused: None,
        dropped: Vec::new(),
        synthesized: false,
    }
}

/// The body over the translation cap: our limit, not the far end's fault, and nothing delivered.
#[must_use]
pub fn over_cap(host: &dyn crate::codec::cells::CodecHost, ingress: &str) -> Whole {
    whole(WholeEnd::OverCap, None, internal(host, ingress, 500))
}

/// The far end's body stopped short of its end: a failed transfer, nothing delivered.
#[must_use]
pub fn cut(host: &dyn crate::codec::cells::CodecHost, ingress: &str) -> Whole {
    whole(WholeEnd::Cut, None, internal(host, ingress, 502))
}

/// TRANSLATE A WHOLE ANSWER: the far end's 2xx `status` and its whole body.
#[must_use]
pub fn translate(
    host: &dyn crate::codec::cells::CodecHost,
    ctx: &WholeCtx<'_>,
    status: u16,
    bytes: &[u8],
) -> Whole {
    let egress_op = handler(ctx.egress, ctx.operation);
    let ingress_serves = handler(ctx.ingress, ctx.operation).is_some();
    let body_json = crate::codec::json::parse::<Value>(bytes);
    let mut refused = None;
    if body_json.is_err() {
        match opaque(host, ctx, status, egress_op, ingress_serves, bytes) {
            Ok(Some(w)) => return w,
            Ok(None) => {}
            Err(detail) => {
                refused = Some(Refusal {
                    opaque: true,
                    detail,
                })
            }
        }
    }
    if let (Ok(rv), Some(eh)) = (&body_json, egress_op) {
        if decl(ctx.ingress).is_some_and(|d| d.codec.is_some()) {
            match json(host, ctx, status, eh, ingress_serves, rv) {
                Ok(Some(w)) => return w,
                Ok(None) => {}
                Err(detail) => {
                    refused = Some(Refusal {
                        opaque: false,
                        detail,
                    })
                }
            }
        }
    }
    let recovered_usage = refused
        .as_ref()
        .and(egress_op)
        .and_then(|handler| handler.recover_response_billing(host, bytes));
    Whole {
        refused,
        ..whole(
            WholeEnd::NotTranslatable,
            recovered_usage,
            internal(host, ctx.ingress, 500),
        )
    }
}

/// An opaque body bridged at the byte level; `None` falls through to the JSON path, `Err` is the
/// far end's dialect refusing the body.
fn opaque(
    host: &dyn crate::codec::cells::CodecHost,
    ctx: &WholeCtx<'_>,
    status: u16,
    egress_op: Option<&'static dyn OperationHandler>,
    ingress_serves: bool,
    bytes: &[u8],
) -> Result<Option<Whole>, String> {
    let Some(eh) = egress_op else {
        return Ok(None);
    };
    let (usage, answered) = eh
        .translate_response(
            host,
            TranslateRespInput::Opaque(bytes),
            ingress_serves,
            ctx.ingress,
            ctx.model,
            ctx.now_s,
            false,
            None,
            ctx.request,
            ctx.translate_cap,
        )
        .map_err(|e| format!("{e:?}"))?;
    let wire = match answered {
        TranslatedResponse::Typed(wire) => wire,
        TranslatedResponse::Untranslatable => {
            return Ok(Some(whole(
                WholeEnd::NotTranslatable,
                usage,
                internal(host, ctx.ingress, 500),
            )))
        }
        _ => return Ok(None),
    };
    Ok(Some(whole(
        WholeEnd::Delivered,
        usage,
        delivered(
            host,
            ctx,
            status,
            wire.content_type.as_bytes(),
            wire.bytes.as_slice().to_vec(),
        ),
    )))
}

/// A JSON body translated; `None` when no body could be written, `Err` the far end's dialect
/// refusing the body.
fn json(
    host: &dyn crate::codec::cells::CodecHost,
    ctx: &WholeCtx<'_>,
    status: u16,
    eh: &dyn OperationHandler,
    ingress_serves: bool,
    rv: &Value,
) -> Result<Option<Whole>, String> {
    let read = || json_inner(host, ctx, status, eh, ingress_serves, rv);
    let (answer, dropped) = if ctx.ingress == ctx.egress {
        (read(), Vec::new())
    } else {
        drops::scope(
            drops::Seam {
                direction: drops::Direction::Response,
                ingress: ctx.ingress,
                egress: ctx.egress,
            },
            read,
        )
    };
    let mut answer = answer?;
    if let Some(w) = answer.as_mut() {
        if w.end == WholeEnd::Delivered {
            w.dropped = dropped;
        }
    }
    Ok(answer)
}

fn json_inner(
    host: &dyn crate::codec::cells::CodecHost,
    ctx: &WholeCtx<'_>,
    status: u16,
    eh: &dyn OperationHandler,
    ingress_serves: bool,
    rv: &Value,
) -> Result<Option<Whole>, String> {
    use busbar_contract::ir::handle::IrHandle as _;

    let checked = crate::codec::proto_codec::CheckedEntropy::new(host);
    let decoded = (ctx.operation == OpVerb::CHAT)
        .then(|| {
            crate::codec::proto_codec::with_reader(ctx.egress, |reader| {
                match reader.read_response(&checked, rv) {
                    Ok(ir) => Ok(Ok(ir)),
                    Err(error)
                        if error.provider_signal.as_deref()
                            != Some(crate::codec::framework::SIGNAL_IR_PARSE) =>
                    {
                        Ok(Err(reader.recover_response_usage(rv).map(Billing::Tokens)))
                    }
                    Err(error) => Err(format!("{error:?}")),
                }
            })
        })
        .flatten();
    let (usage, answered) = if let Some(decoded) = decoded {
        let mut ir = match decoded? {
            Ok(ir) => crate::codec::chat_handle::ChatRespHandle(ir, ctx.translate_cap),
            Err(usage) => {
                return Ok(Some(whole(
                    WholeEnd::FailedGeneration,
                    usage,
                    internal(host, ctx.ingress, 502),
                )))
            }
        };
        if ingress_serves && ir.0.stop_reason == Some(crate::codec::ir::IrStopReason::Error) {
            return Ok(Some(whole(
                WholeEnd::FailedGeneration,
                ir.billing(),
                internal(host, ctx.ingress, 502),
            )));
        }
        crate::codec::translate::write_response_ir(
            &checked,
            &mut ir,
            ingress_serves,
            ctx.ingress,
            ctx.model,
            ctx.now_s,
            ctx.wants_stream && !ctx.json_array,
            ctx.elapsed_ms,
            ctx.request,
            ctx.translate_cap,
        )
    } else {
        eh.translate_response(
            &checked,
            TranslateRespInput::Json(rv),
            ingress_serves,
            ctx.ingress,
            ctx.model,
            ctx.now_s,
            ctx.wants_stream && !ctx.json_array,
            ctx.elapsed_ms,
            ctx.request,
            ctx.translate_cap,
        )
        .map_err(|error| format!("{error:?}"))?
    };
    crate::codec::chat_handle::drop_untranslatable_response(ctx.egress, rv);

    if checked.failed() {
        return Ok(Some(whole(
            WholeEnd::NotTranslatable,
            usage,
            internal(host, ctx.ingress, 500),
        )));
    }
    let w = match answered {
        TranslatedResponse::StreamFrames(frames) => {
            let ct = wire::ingress_stream_content_type(ctx.ingress).unwrap_or(TEXT_EVENT_STREAM);
            Whole {
                synthesized: true,
                ..whole(
                    WholeEnd::Delivered,
                    usage,
                    delivered(host, ctx, status, ct.as_bytes(), frames),
                )
            }
        }
        TranslatedResponse::IngressUnsupported => whole(
            WholeEnd::IngressUnsupported,
            None,
            render(
                host,
                ctx.ingress,
                404,
                KIND_NOT_FOUND,
                DETAIL_ENDPOINT_UNSUPPORTED_OPERATION,
                0,
            ),
        ),
        TranslatedResponse::Typed(wire) => whole(
            WholeEnd::Delivered,
            usage,
            delivered(
                host,
                ctx,
                status,
                wire.content_type.as_bytes(),
                wire.bytes.as_slice().to_vec(),
            ),
        ),
        TranslatedResponse::Json(mut translated) => {
            if let Some(dialect) = decl(ctx.ingress).and_then(|d| d.dialect()) {
                dialect.inject_response_metrics(&mut translated, ctx.elapsed_ms);
            }
            let answer = if ctx.json_array && ctx.wants_stream {
                let arr = Value::Array(vec![translated]);
                let body = crate::codec::json::to_vec(&arr)
                    .unwrap_or_else(|_| arr.to_string().into_bytes());
                Rendered {
                    status,
                    fields: head_field("content-type", APPLICATION_JSON.as_bytes())
                        .into_iter()
                        .collect(),
                    body,
                }
            } else {
                let body = crate::codec::json::to_vec(&translated)
                    .unwrap_or_else(|_| translated.to_string().into_bytes());
                delivered(host, ctx, status, APPLICATION_JSON.as_bytes(), body)
            };
            whole(WholeEnd::Delivered, usage, answer)
        }
        TranslatedResponse::Untranslatable => whole(
            WholeEnd::NotTranslatable,
            usage,
            internal(host, ctx.ingress, 500),
        ),
    };
    Ok(Some(w))
}
