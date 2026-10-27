// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THIS CRATE'S TEST HOST (#83a SD-3): the host services a shipped binary's host hands the codec,
//! provided for this crate's own test binary by the test itself — so the codec's tests prove the
//! codec's own seam and never reach the host crate. Nothing is armed behind the contract: it holds
//! no port. Test-only by construction (`#[cfg(test)]` at its one `mod` line).
//!
//! - ENTROPY and WALL CLOCK: [`HOST`], the [`CodecHost`] every codec call in this crate's tests is
//!   handed — the OS CSPRNG (the same source the kernel hands) and the system clock in whole
//!   seconds, as the kernel reads it.
//! - TRANSLATE CAP: none armed — the contract's documented 32 MiB default applies, which is the
//!   host's own default for an unconfigured limit.
//! - USAGE-TAP FAULTS: none to arm. A tap returns its fault as data (row 155); what the kernel does
//!   with it (the `busbar_billing_tap_decode_fail_total` series and the warn-once set) is pinned in
//!   the kernel's suite.
//! - CLASSIFICATION: [`classified`] reads the class the host's classifier places a raw upstream error
//!   in off the shared fixture `testing/plane-copies/upstream-classes.json`, which the host's suite
//!   holds its classifier to.

use busbar_contract::codec::CodecHost;
use busbar_contract::upstream::{CanonicalSignal, RawUpstreamError, StatusClass};

/// THE TEST HOST'S [`CodecHost`]: the OS CSPRNG and the system clock in whole seconds — the services
/// the kernel hands a codec, read the way the kernel reads them.
#[derive(Clone, Copy, Debug, Default)]
pub struct TestHost;

impl CodecHost for TestHost {
    fn fill_entropy(&self, out: &mut [u8]) -> bool {
        getrandom::fill(out).is_ok()
    }

    fn wall_clock_now(&self) -> Option<u64> {
        Some(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        )
    }
}

/// The handle every codec call in this crate's tests is handed.
pub const HOST: &dyn CodecHost = &TestHost;

/// The class the host's classifier places `raw` in with an EMPTY operator `error_map`, read off the
/// shared fixture. A raw shape the fixture does not list fails the test that produced it: a dialect
/// whose error reader starts producing a new shape must add it there, where the host's suite checks
/// it too.
pub fn classified(raw: &RawUpstreamError) -> CanonicalSignal {
    let path =
        std::path::PathBuf::from(std::env::var_os("BUSBAR_CHECKOUT").expect(
            "BUSBAR_CHECKOUT is unset: busbar's tree at .busbar-ref (plugin CI exports it)",
        ))
        .join("testing/plane-copies/upstream-classes.json");
    let doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).expect("fixture readable"))
            .expect("fixture is JSON");
    let row = doc["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|r| {
            r["http_status"] == u64::from(raw.http_status)
                && r["provider_code"].as_str() == raw.provider_code.as_deref()
                && r["structured_type"].as_str() == raw.structured_type.as_deref()
                && r["retry_after_secs"].as_u64() == raw.retry_after_secs
        })
        .unwrap_or_else(|| panic!("{raw:?} is not in testing/plane-copies/upstream-classes.json"));
    CanonicalSignal {
        class: StatusClass::parse(row["class"].as_str().expect("class")).expect("a class token"),
        provider_signal: raw.provider_code.clone(),
        retry_after: raw.retry_after_secs,
    }
}

/// Every `ContentItem`'s screenable text, in order — the exact strings a `prompt: ro` gate is shown.
pub fn screened(items: &[busbar_contract::ir::facts::ContentItem<'_>]) -> Vec<String> {
    items
        .iter()
        .map(|i| i.screenable_text().into_owned())
        .collect()
}

/// The stop reason of a non-stream response read from its RAW BYTES the way the same-protocol
/// relay reads it: the dialect's stop-reason field, scanned, its token mapped.
pub(crate) fn raw_stop_reason(
    reader: &dyn crate::codec::proto_codec::ProtocolReader,
    body: &[u8],
) -> Option<crate::codec::ir::IrStopReason> {
    let mut scan = crate::codec::usage_tail::StopKeyScanner::new(reader.stop_reason_key()?)?;
    scan.feed(body);
    scan.token()
        .and_then(|token| reader.stop_reason_of_token(token))
}
