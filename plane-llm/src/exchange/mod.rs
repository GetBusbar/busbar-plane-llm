//! THE PLANE'S EXCHANGE WITH THE KERNEL, sans I/O: what `arrive` reads off an arrival, what an
//! attempt sends to the far end and what the far end's answer becomes for the caller. Every
//! function here is pure over bytes the kernel hands in; the door adapts them to the plane ABI.

pub mod arrive;
pub mod attempt;
pub mod catalog;
pub mod invoke;
pub mod multipart;
pub mod probe;
pub mod project;
pub mod refuse;
pub mod reply;
pub mod shaping;
pub mod webhook;

use busbar_contract::codec::OperationHandler;

/// The declaration of the dialect named `name`: the plane's one lookup ([`crate::codec::decl_of`]).
pub(crate) use crate::codec::decl_of as decl;

/// The operation handler that serves `arrived`: its dialect's handler for its operation.
#[must_use]
pub fn handler_of(arrived: &arrive::Arrived) -> Option<&'static dyn OperationHandler> {
    decl(arrived.dialect)
        .and_then(|d| d.handler)
        .and_then(|rh| rh.operation_handler(arrived.operation))
}
