//! Provider evidence adapters for host-owned observation settlement.
//!
//! Native Interactions serving uses these adapters behind explicit model opt-in and durable
//! host scope/record barriers. Hosted-tool adapters remain evidence APIs pending runtime wiring;
//! no ownership or paid-create recovery is reconstructed from process memory alone.

pub mod crossing;
pub mod hosted;
pub mod interactions;
pub mod lifecycle;
pub mod lookup;
pub mod native_request;
pub mod native_stream;
pub mod profiles;
pub mod session;
