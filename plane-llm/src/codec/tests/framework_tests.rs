// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The framework items' own proofs, moved with them: the duration rendering is byte-identical

use super::{duration_seconds_to_wire, head_field, ImageOutput};
use busbar_contract::Count;

/// A 64-bit xorshift, so the sweep draws from integers and the proof of exactness does not itself
struct Xorshift(u64);

impl Xorshift {
    fn step(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

/// The exact decimal spelling of a micro-second count, built by integer arithmetic.
fn decimal_text(micros: i128) -> String {
    Count::from_micros(micros).to_decimal_string()
}

/// THE BYTE-IDENTITY CLAIM, SWEPT.
#[test]
fn the_wire_rendering_is_the_same_double_the_text_used_to_parse_to() {
    let mut checked = 0usize;
    let check = |micros: i128| {
        let text = decimal_text(micros);
        let rendered = duration_seconds_to_wire(Count::from_micros(micros));
        let parsed: f64 = text
            .parse()
            .expect("a decimal this crate wrote is parseable");
        assert_eq!(
            rendered.to_bits(),
            parsed.to_bits(),
            "`{text}` renders to a different double than it parses to"
        );

        assert_eq!(
            serde_json::to_string(&serde_json::json!({ "seconds": rendered }))
                .expect("a finite number serializes"),
            serde_json::to_string(&serde_json::json!({ "seconds": parsed }))
                .expect("a finite number serializes"),
        );
    };

    for micros in 0..10_000i128 {
        check(micros);
        checked += 1;
    }

    let mut rng = Xorshift(0xDEAD_BEEF_CAFE_F00D);
    for _ in 0..100_000 {
        check(i128::from(rng.step() % 3_600_000_000));
        checked += 1;
    }

    for whole in 0..1_000i128 {
        for delta in -1..=1i128 {
            let micros = whole * 1_000_000 + delta;
            if micros >= 0 {
                check(micros);
                checked += 1;
            }
        }
    }
    assert!(checked > 100_000, "only {checked} values swept");
}

#[test]
fn image_output_is_additive_b64_and_url_coexist() {
    let img = ImageOutput {
        b64: Some("iVBORw0KGgo=".into()),
        url: Some("https://example/img.png".into()),
        ..Default::default()
    };

    let b64_only = ImageOutput {
        b64: img.b64.clone(),
        ..Default::default()
    };
    let url_only = ImageOutput {
        url: img.url.clone(),
        ..Default::default()
    };
    assert_ne!(b64_only, url_only);
    assert_ne!(img, b64_only);
    assert_ne!(img, url_only);
}

/// A claims predicate reads a head field by name in any case, and the first field whose value is
#[test]
fn a_head_field_is_the_first_legal_value_under_its_name_in_any_case() {
    let fields: &[(&[u8], &[u8])] = &[
        (b"Content-Type", b"application/json"),
        (b"Authorization", b"bad\nvalue"),
        (b"AUTHORIZATION", b"AWS4-HMAC-SHA256 Credential=x"),
        (b"authorization", b"Bearer second"),
        (b"x-tabbed", b"a\tb"),
    ];
    assert_eq!(
        head_field(fields, "authorization"),
        Some(&b"AWS4-HMAC-SHA256 Credential=x"[..])
    );
    assert_eq!(
        head_field(fields, "content-type"),
        Some(&b"application/json"[..])
    );
    assert_eq!(head_field(fields, "x-tabbed"), Some(&b"a\tb"[..]));
    assert_eq!(head_field(fields, "x-api-key"), None);
    let deleted: &[(&[u8], &[u8])] = &[(b"x-del", b"a\x7fb")];
    assert_eq!(head_field(deleted, "x-del"), None);
}
