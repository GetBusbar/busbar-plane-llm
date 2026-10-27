//! The codec hosts the integration suites hand the exchange and the codecs.

// Each test binary includes this module and uses the host it needs.
#![allow(dead_code)]

/// The OS CSPRNG and the system clock in whole seconds, read the way the kernel's host reads them.
pub struct TestHost;

impl busbar_contract::codec::CodecHost for TestHost {
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

/// Constant entropy (a minted id renders the same twice) and no clock.
pub struct SevensHost;

impl busbar_contract::codec::CodecHost for SevensHost {
    fn fill_entropy(&self, out: &mut [u8]) -> bool {
        out.fill(7);
        true
    }

    fn wall_clock_now(&self) -> Option<u64> {
        None
    }
}
