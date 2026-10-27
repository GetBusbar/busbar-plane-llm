//! The claims are the detection ladder, in the ladder's own order.
//!
//! The ladder is the thing that decides which dialect a request is, and getting its ORDER wrong is
//! not a compile error and not a crash — it is a request quietly read as the wrong dialect. So the
//! order is asserted, rung by rung, and every rung is asserted to route what it claims to route.

use busbar_contract::grammar::Selector;
use busbar_contract::plane::PlaneMeta;
use busbar_plane_llm::claims::{CLAIMS, LADDER};
use busbar_plane_llm::LlmPlane;

/// The ladder has fourteen rungs, numbered without a gap.
#[test]
fn the_ladder_has_fourteen_rungs_in_ascending_order() {
    let mut seen: Vec<u16> = LADDER.iter().map(|c| c.rung).collect();
    assert!(
        seen.windows(2).all(|w| w[0] <= w[1]),
        "the claims are not in rung order: {seen:?}"
    );
    seen.dedup();
    assert_eq!(
        seen,
        (1..=14).collect::<Vec<u16>>(),
        "the rungs are not one through fourteen without a gap"
    );
}

/// What the plane declares is the ladder, one claim per entry, in the same order.
#[test]
fn the_declared_claims_are_the_ladder() {
    assert_eq!(<LlmPlane as PlaneMeta>::CLAIMS.len(), LADDER.len());
    for (declared, entry) in <LlmPlane as PlaneMeta>::CLAIMS.iter().zip(LADDER) {
        assert_eq!(*declared, entry.claim);
    }
    assert_eq!(CLAIMS.len(), LADDER.len());
}

/// Every claim is made against the one transport this plane claims, under one scheme.
#[test]
fn every_claim_names_one_transport_and_one_scheme() {
    let first = LADDER[0].claim;
    for entry in LADDER {
        assert_eq!(entry.claim.transport, first.transport);
        assert_eq!(entry.claim.scheme, first.scheme);
        assert_eq!(
            entry.claim.scheme_alternatives, first.scheme_alternatives,
            "a claim narrows to a different alternative set than its siblings"
        );
        assert!(
            entry.claim.idempotency.is_none(),
            "an idempotency rule is claim configuration, not a plane declaration"
        );
    }
}

/// The selector forms this plane uses are the five the ladder needs and no others.
///
/// A form the transport cannot evaluate is refused at boot, so the set is worth pinning: a sixth
/// form appearing here is a claim that may not be evaluable where it is claimed.
#[test]
fn the_ladder_uses_only_the_forms_it_needs() {
    for entry in LADDER {
        assert!(
            matches!(
                entry.claim.selector,
                Selector::HeaderPresent(_)
                    | Selector::HeaderPrefix(_, _)
                    | Selector::PathContains(_)
                    | Selector::PathSuffix(_)
                    | Selector::PathPattern(_)
            ),
            "rung {} uses a selector form the ladder does not need: {:?}",
            entry.rung,
            entry.claim.selector
        );
    }
}

/// The two dialects that carry the model in the request target say so, and the four that carry it
/// in the body say that.
///
/// The location is the value's ACTUAL place. It used to be a body pointer for all six, which was
/// only true because the arrival path had copied a target-carried model into the body first; a
/// location that describes where something was put rather than where it is is a location that goes
/// wrong the moment the putting stops.
#[test]
fn the_two_target_carried_dialects_name_a_path_segment() {
    use busbar_contract::grammar::{ArrivalLocation, Location};
    use busbar_plane_llm::dialect::{dialect, DIALECTS};

    for name in ["gemini", "bedrock"] {
        let d = dialect(name).expect("the dialect is in the table");
        assert_eq!(
            d.model_location,
            Location::Arrival(ArrivalLocation::PathSegment(0)),
            "{name} does not name the path segment its model is in"
        );
    }
    for name in ["anthropic", "openai", "responses", "cohere"] {
        let d = dialect(name).expect("the dialect is in the table");
        assert_eq!(
            d.model_location,
            Location::Arrival(ArrivalLocation::FirstFrameJsonPointer("/model")),
            "{name} does not name the body member its model is in"
        );
    }

    // Exactly two of the six, so a seventh dialect added on either side is a visible change here.
    let carried = DIALECTS
        .iter()
        .filter(|d| {
            matches!(
                d.model_location,
                Location::Arrival(ArrivalLocation::PathSegment(_))
            )
        })
        .count();
    assert_eq!(carried, 2);
}
