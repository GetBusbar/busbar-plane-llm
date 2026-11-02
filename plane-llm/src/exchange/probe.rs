//! A health probe's far-end request, sans I/O: the smallest request a far end answers with its health

use busbar_contract::protocol::{APPLICATION_JSON, EGRESS_UA_DEFAULT};

use super::attempt::{default_head_fields, wire_and_canonical_path, with_option_query, FarRequest};
use super::decl;
use super::shaping::Lane;

/// The probe body the far end's dialect answers for `wire_model` (empty when it has none).
#[must_use]
pub fn probe_body(
    host: &dyn busbar_contract::codec::CodecHost,
    dialect: &str,
    wire_model: &str,
) -> Vec<u8> {
    decl(dialect)
        .and_then(|d| d.dialect())
        .map(|dc| dc.probe_body(host, wire_model))
        .unwrap_or_default()
}

/// The path a probe of `lane` is sent to, before encoding: the provider's fixed path, else the

#[must_use]
pub fn probe_path(lane: &Lane) -> String {
    lane.path.clone().unwrap_or_else(|| {
        decl(lane.dialect)
            .and_then(|d| d.dialect())
            .map(|dc| dc.upstream_path_for_stream(lane.wire_model(), false))
            .unwrap_or_default()
    })
}

/// THE PROBE REQUEST for `lane`; `None` for a lane under a path base, which is not probed.
#[must_use]
pub fn request(host: &dyn busbar_contract::codec::CodecHost, lane: &Lane) -> Option<FarRequest> {
    if lane.path_base.is_some() {
        return None;
    }

    let path = match &lane.path {
        None if lane.invoke => super::invoke::path(lane.wire_model(), false),
        _ => probe_path(lane),
    };
    let body = probe_body(host, lane.dialect, lane.wire_model());
    let body = if lane.invoke {
        super::invoke::body(&body, &[])?
    } else {
        body
    };
    let (target, _canonical) = wire_and_canonical_path(&path);
    let target = with_option_query(lane, &target);
    let user_agent = decl(lane.dialect).map_or(EGRESS_UA_DEFAULT, |d| d.egress_user_agent);

    let (mut fields, forced) = default_head_fields(
        lane,
        [
            (
                "content-type".to_string(),
                APPLICATION_JSON.as_bytes().to_vec(),
            ),
            ("user-agent".to_string(), user_agent.as_bytes().to_vec()),
            ("accept".to_string(), APPLICATION_JSON.as_bytes().to_vec()),
        ],
    );
    fields.extend(forced);
    Some(FarRequest {
        verb: "POST",
        target,
        fields,
        body,
        pristine: false,
        dropped_controls: Vec::new(),
    })
}
