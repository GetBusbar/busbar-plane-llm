// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! The `llm` plane as a droppable busbar plugin: the `cdylib` a signed tarball carries
//! (`kind: plane`). It holds one line: the logic crate's door (`busbar_plane_llm::door`, the one a
//! busbar build links) exported as this image's ONE symbol, `busbar_plugin_door`, with
//! `busbar_contract::export_door!`, so the library carries exactly the door a busbar build links and
//! is no second path to the logic crate's Rust items (`BUSBAR-1.6.0.md` l.3560: "The thin cdylib
//! crate holds one line").
//!
//! This crate is `deny`, not `forbid`: the export macro's `#[unsafe(no_mangle)]` is the one
//! reviewed exemption (a `forbid` cannot be lifted for it). No other `unsafe` exists here.

#![deny(unsafe_code)]

/// The exported door: the macro's `#[no_mangle]` symbol is the one exemption.
#[allow(unsafe_code)]
mod exported {
    busbar_contract::export_door!(busbar_plane_llm::door);
}
