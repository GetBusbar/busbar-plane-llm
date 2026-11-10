//! Provider evidence adapters for host-owned observation settlement.
//!
//! These adapters do not issue paid requests or store ownership in process memory. Native
//! serving must first durably bind the original host scope and freeze provider configuration;
//! publishing this module alone does not enable Interactions or hosted-tool billing at the door.

pub mod hosted;
pub mod interactions;
pub mod lifecycle;
