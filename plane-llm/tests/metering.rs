//! The plane's meter classes are the door's billable classes, by their 1.5.5 names.

// A list read is `unsafe` while its pointer and length fields are pub, and safe once they are
// private: the same `unsafe { x.list() }` compiles on both.
#![allow(unused_unsafe)]

use busbar_plane_llm::LlmPlane;

/// ONE DECLARATION, ONE SOURCE (`$` meter classes: "Money is ledger x ratecard, so there
/// must be ONE declaration and one source. The door's billable classes, which are what the
/// dispatcher checks, are the source; MeterClassDecl derives from them").
///
/// Every class the plane's `MeterClassDecl` list names is the door tail's billable class at the same
/// index, in the same family, and each is 1.5.5's own name for the tier, byte for byte: the
/// `rate_card:` tiers `input_utok`/`output_utok`/`cache_read_utok`/`cache_write_utok`, the ledger's
/// `TierTokens { input, output, cache_read, cache_write }` and the spend metrics' `tier` label. RED
/// while the list spelled its own `tokens_in`/`tokens_out`, names no card prices and no tier carries.
#[test]
fn the_meter_classes_are_the_doors_billable_classes_by_their_1_5_5_names() {
    use busbar_contract::plane::PlaneMeta;
    use busbar_plane_llm::plane_door::{TAIL, TOKEN_CLASSES};
    let declared: Vec<(&str, &str)> = <LlmPlane as PlaneMeta>::METER_CLASSES
        .iter()
        .map(|c| (c.key.as_str(), c.family))
        .collect();
    let stated = unsafe { TAIL.billable_classes() };
    let door: Vec<(&str, &str)> = stated[..declared.len()]
        .iter()
        .map(|b| {
            // SAFETY: each `AbiStr` names a `'static` string of `len` bytes.
            let (c, f) = unsafe {
                (
                    std::slice::from_raw_parts(b.class.ptr(), b.class.len()),
                    std::slice::from_raw_parts(b.family.ptr(), b.family.len()),
                )
            };
            (
                std::str::from_utf8(c).expect("a class is UTF-8"),
                std::str::from_utf8(f).expect("a family is UTF-8"),
            )
        })
        .collect();
    assert_eq!(
        declared, door,
        "the meter classes and the door's billable classes are one list"
    );
    assert_eq!(
        declared,
        vec![
            ("input", "token"),
            ("output", "token"),
            ("cache_read", "token"),
            ("cache_write", "token"),
        ],
        "1.5.5's tier names, byte for byte"
    );
    assert_eq!(TOKEN_CLASSES.len(), declared.len());
}
