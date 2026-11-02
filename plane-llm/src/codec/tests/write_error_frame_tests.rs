// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The mid-stream error seam, across every declared writer.

use super::*;
use crate::codec::proto_codec::protocol_for;
use busbar_contract::upstream::{CanonicalSignal, StatusClass};

fn a_mid_stream_error() -> CanonicalSignal {
    CanonicalSignal {
        class: StatusClass::ServerError,
        provider_signal: Some("mid-stream transport failure".to_string()),
        retry_after: None,
    }
}

#[test]
fn every_declared_writer_frames_a_stream_error_in_band() {
    let err = a_mid_stream_error();
    let mut checked = 0usize;
    for decl in DECLS {
        if decl.codec.is_none() {
            continue;
        }
        let proto = protocol_for(decl.name).expect("a codec protocol resolves its writer");
        assert!(
            proto
                .writer()
                .write_error_frame(crate::codec::test_host::HOST, &err)
                .is_some(),
            "{}: write_error_frame returned None — a mid-stream error would fall back to core's \
             dialect-free frame instead of this protocol's native stream-error shape",
            decl.name
        );
        checked += 1;
    }

    assert_eq!(
        checked, 6,
        "all six codec dialects must be swept for a native stream-error frame; swept {checked}"
    );
}
