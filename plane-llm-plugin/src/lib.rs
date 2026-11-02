// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The `llm` plane as a droppable busbar plugin: the `cdylib` a signed tarball carries

#![deny(unsafe_code)]

pub use busbar_plane_llm::*;

/// The exported door: the macro's `#[no_mangle]` symbol is the one exemption.
#[allow(unsafe_code)]
mod exported {
    busbar_contract::export_door!(busbar_plane_llm::plane_door::door);
}
