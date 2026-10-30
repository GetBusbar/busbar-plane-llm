// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The two member peeks the dialects' same-protocol relays read over `busbar_contract::spans`
//! (the SDK's byte-level object scanner and splices, which this plane's body edits use directly).

use busbar_contract::spans::{scan, RawMember};

/// Whether the object `b` opens on has a member for which `hit(member, value bytes)` holds,
/// allocating nothing; `true` when `b` cannot be read as an object, so a caller that skips work on
/// `false` skips only what it has read.
pub fn any_member(b: &[u8], mut hit: impl FnMut(&RawMember<'_>, &[u8]) -> bool) -> bool {
    let mut found = false;
    scan(b, 0, |m| {
        found = found || hit(&m, &b[m.value_start..m.value_end])
    })
    .is_none_or(|_| found)
}

/// The bytes of the array value `v` from its first element on; `None` when `v` is not an array or
/// holds no element.
#[must_use]
pub fn first_element(v: &[u8]) -> Option<&[u8]> {
    if v.first() != Some(&b'[') {
        return None;
    }
    let mut i = 1;
    while i < v.len() && v[i].is_ascii_whitespace() {
        i += 1;
    }
    v.get(i).is_some_and(|&c| c != b']').then(|| &v[i..])
}
