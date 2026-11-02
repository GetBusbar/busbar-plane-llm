//! The LLM plane: what bytes mean, for six dialects of one protocol, served through its door

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

pub const DECLARES: &str = include_str!("../declares.json");
