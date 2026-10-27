//! The LLM plane: what bytes mean, for six dialects of one protocol, served through its door
//! ([`plane_door::door`]) over the dialect codecs ([`codec`]). It holds no governance, no breaker
//! and no money: it reports counts and locators, and the kernel prices them.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

// `#![deny(missing_docs)]` above is a rule this crate holds itself to; the `codec` module was not
// written under it and is exempt, scoped to exactly this module.
#[allow(missing_docs)]
pub mod codec;
pub mod dialect;
pub mod exchange;
pub mod meta;
pub mod plane_door;
pub mod refusal;

/// THE PLANE'S DECLARED METADATA (`declares.json`): the `declares` section its signed tarball carries
/// and the one the composition root reads for its linked door (`linked-declares`), one file for both
/// — the transport scheme it needs and every code its catalog holds
/// ([`codec::diagnostics::DIAGNOSTICS`]; a test holds the two equal), so the host's catalogue learns
/// this plane's codes from its door's declaration whichever way the plane came in.
pub const DECLARES: &str = include_str!("../declares.json");
