// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The plane's stated refusal reasons name the reasons they say they name.

use busbar_contract::abi::plane::reason_of;
use busbar_plane_llm::refusal::reason;

#[test]
fn every_stated_reason_code_is_the_reason_it_is_named_for() {
    for (code, spelling) in [
        (reason::UNAUTHENTICATED, "unauthenticated"),
        (reason::OVER_BUDGET, "over_budget"),
        (reason::DESTINATION_UNREACHABLE, "destination_unreachable"),
        (reason::GROUP_FROZEN, "group_frozen"),
        (reason::NO_RATE, "no_rate"),
        (reason::NO_DESTINATION, "no_destination"),
        (reason::SCHEME_NOT_DECLARED, "scheme_not_declared"),
        (reason::CHALLENGE_EXHAUSTED, "challenge_exhausted"),
        (reason::CURSOR_BUDGET, "cursor_budget"),
        (reason::CREDENTIAL_BUDGET, "credential_budget"),
    ] {
        assert_eq!(
            reason_of(code).map(|r| r.as_str()),
            Some(spelling),
            "code {code}"
        );
    }
}

/// THE BEDROCK QUOTA REFUSAL WEARS THE STATUS THE PLANE DECLARES (`codec/bedrock/refusals.json`,
/// OWNER 2026-10-27): the stated row for bedrock `over_budget` is the declared table's status, so the
/// 400 `ServiceQuotaExceededException` 1.5.5 answered is a 400 again, not the class default 429 under
/// the restored exception. RED before: no row, so the kernel's 429.
#[test]
fn the_bedrock_over_budget_status_is_the_declared_refusals() {
    use busbar_plane_llm::dialect::DIALECTS;
    use busbar_plane_llm::refusal::REFUSAL_STATUSES;
    let doc: serde_json::Value =
        serde_json::from_str(busbar_plane_llm::codec::bedrock::REFUSALS_JSON).expect("parses");
    let declared = doc["refusals"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|r| {
            r["outcomes"]
                .as_array()
                .is_some_and(|o| o.iter().any(|o| o == "over_budget_total"))
        })
        .expect("the over-budget refusal is declared");
    let want = u32::try_from(declared["status"].as_u64().expect("a status")).expect("fits");
    let bedrock = u32::try_from(
        DIALECTS
            .iter()
            .position(|d| d.name == "bedrock")
            .expect("bedrock"),
    )
    .expect("fits");
    let stated = REFUSAL_STATUSES
        .iter()
        .find(|r| r.dialect == bedrock && r.reason == reason::OVER_BUDGET)
        .map(|r| r.status);
    assert_eq!(stated, Some(want));
}
