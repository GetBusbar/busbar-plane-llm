#![no_main]
//! Fuzz the ingress decode of an untrusted embeddings request body.
//!
//! Each provider dialect's production `read_embeddings_request` reader (its registration's
//! embeddings row) — the code path that turns bytes off the wire into typed IR.
//! The invariant under test: **arbitrary bytes must never panic the reader.** A malformed body is
//! a `Result::Err` (`IngressReject`), never a crash — so libFuzzer only fails on a genuine panic,
//! abort, OOM, or timeout, i.e. a real robustness defect in the parse surface.

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // Steer coverage across every dialect that has an embeddings reader, using the first byte so
    // the mutator can explore each parser; the remainder is the untrusted body.
    let proto = match data[0] % 4 {
        0 => "openai",
        1 => "cohere",
        2 => "bedrock",
        _ => "gemini",
    };
    let body = &data[1..];

    // content_type is part of the ingress contract; fuzz JSON and a non-JSON value so the reader's
    // content-type branch is exercised too.
    let content_type = if data[0] & 0x80 != 0 {
        "application/json"
    } else {
        "application/octet-stream"
    };

    // The dialect's declared embeddings reader, as its registration row names it.
    let reader = busbar_plane_llm::codec::proto_codec::entry_of(proto)
        .and_then(|d| d.leaf.embeddings.as_ref())
        .map(|c| c.read_request);
    if let Some(read) = reader {
        let _ = read(body, content_type);
    }
});
