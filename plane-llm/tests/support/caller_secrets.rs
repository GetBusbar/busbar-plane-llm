// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

use super::*;

#[test]
fn retained_caller_values_have_zeroizing_ownership_and_borrow_without_copying() {
    fn requires_wipe_on_drop(_: &impl zeroize::ZeroizeOnDrop) {}

    let shaping = Shaping::from_settings(&serde_json::json!({})).unwrap();
    let mut unit = UnitState::new(Arc::new(shaping));
    unit.caller.push((
        b"authorization".to_vec(),
        zeroize::Zeroizing::new(b"Bearer private".to_vec()),
    ));
    requires_wipe_on_drop(&unit.caller[0].1);
    let fields = unit.caller_fields();
    assert_eq!(fields[0].1, b"Bearer private");
    assert_eq!(fields[0].1.as_ptr(), unit.caller[0].1.as_ptr());
}
