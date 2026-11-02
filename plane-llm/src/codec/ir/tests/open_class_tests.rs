// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Open usage classes and their tail declarations.

use super::*;

/// Every class is named exactly once: two spellings of one class, or one class under two names,

#[test]
fn every_open_class_is_named_once() {
    let mut names: Vec<&str> = OPEN_CLASSES.iter().map(|(c, _)| *c).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), OPEN_CLASSES.len(), "{OPEN_CLASSES:?}");
    for (class, family) in OPEN_CLASSES {
        assert!(!class.is_empty() && !family.is_empty(), "{class}/{family}");
        assert!(
            !busbar_contract::records::RESERVED_UNITS.contains(class),
            "{class} is a reserved token tier, never an open class"
        );
    }
}

/// No open class is in the token family: a token-family class counts toward a `tokens:` cap, and

#[test]
fn no_open_class_counts_toward_a_token_cap() {
    for (class, family) in OPEN_CLASSES {
        assert_ne!(
            *family,
            busbar_contract::plane::TOKEN_FAMILY,
            "{class} would count toward a tokens: cap"
        );
    }
}

/// Every guardrail count lands in a declared open class, and no two counts share one (AWS prices

#[test]
fn every_guardrail_count_has_its_own_declared_class() {
    let declared: Vec<&str> = OPEN_CLASSES.iter().map(|(c, _)| *c).collect();
    let mut classes: Vec<&str> = GUARDRAIL_COUNT_CLASSES.iter().map(|(_, c)| *c).collect();
    for class in &classes {
        assert!(declared.contains(class), "{class} is not declared");
    }
    classes.sort_unstable();
    classes.dedup();
    assert_eq!(classes.len(), GUARDRAIL_COUNT_CLASSES.len());
}

/// A zero is no hit; a repeat adds (saturating).
#[test]
fn add_skips_a_zero_and_sums_a_repeat() {
    let mut m = std::collections::BTreeMap::new();
    add(&mut m, IMAGES_CLASS, 0);
    assert!(m.is_empty());
    add(&mut m, IMAGES_CLASS, 2);
    add(&mut m, IMAGES_CLASS, 3);
    add(&mut m, AUDIO_MS_CLASS, u64::MAX);
    add(&mut m, AUDIO_MS_CLASS, 1);
    assert_eq!(m.get(IMAGES_CLASS), Some(&5));
    assert_eq!(m.get(AUDIO_MS_CLASS), Some(&u64::MAX));
}
