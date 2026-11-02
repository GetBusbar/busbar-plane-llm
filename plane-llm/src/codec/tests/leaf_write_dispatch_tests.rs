// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! Leaf-op write dispatchers fail LOUDLY on an unknown egress protocol, rather than emitting a

use crate::codec::ir::embeddings::EmbeddingsReq;
use crate::codec::leaf_codec::embeddings_write_request;

#[test]
#[should_panic(expected = "leaf write: unknown egress protocol")]
fn unknown_egress_protocol_panics() {
    let _ = embeddings_write_request("no-such-protocol", &EmbeddingsReq::default());
}
