//! The LLM plane: what bytes mean, for six dialects of one protocol, served through its door
//! ([`plane_door::door`]) over the dialect codecs ([`codec`]).

#![forbid(unsafe_code)]
#![deny(missing_docs)]

#[allow(missing_docs)]
pub mod codec;
pub mod dialect;
pub mod exchange;
pub mod meta;
pub mod migrate;
pub mod options;
pub mod plane_door;
pub mod refusal;

/// THE PLANE'S DECLARED METADATA (`declares.json`): the `declares` section its signed tarball carries
/// and the one the composition root reads for its linked door (`linked-declares`), one file for both
/// — the transport scheme it needs and every code its catalog holds
/// ([`codec::diagnostics::DIAGNOSTICS`]; a test holds the two equal), so the host's catalogue learns
/// this plane's codes from its door's declaration whichever way the plane came in.
pub const DECLARES: &str = include_str!("../declares.json");

#[cfg(test)]
#[path = "../tests/support/codec_diagnostics.rs"]
mod codec_diagnostics;
