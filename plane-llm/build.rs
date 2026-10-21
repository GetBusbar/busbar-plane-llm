// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE ROOT CELLS' AND THE DRIVER PROOFS' ADDRESSES (this crate's `links` row): the composition root
//! compiles this plane's own root cells and its driver proofs from the paths printed here, reading
//! them as `DEP_BUSBAR_DOOR_CELLS_<CRATE>_CELLS` and `DEP_BUSBAR_DOOR_CELLS_<CRATE>_DRIVER` (the same
//! for a path dependency and a pinned one). Nothing is generated and nothing is linked.

use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo::rerun-if-changed=build.rs");
    // THE ROOT CELLS' ADDRESS (this crate's `links` row): the composition root compiles this
    // plane's own root cells from here, reading it as `DEP_BUSBAR_DOOR_CELLS_<CRATE>_CELLS`.
    // busbar's `door-cells-rig:cells-path` gate holds this to its one canonical statement.
    println!(
        "cargo::metadata=cells={}",
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?)
            .join("tests/root/mod.rs")
            .display()
    );
    // THE DRIVER PROOFS' ADDRESS (the same `links` row): the composition root's `hook_parity_driver`
    // suite compiles `tests/on_driver/mod.rs` from this directory, and its door cells read
    // `words.txt` there, reading it as `DEP_BUSBAR_DOOR_CELLS_<CRATE>_DRIVER` (its
    // `[package.metadata.busbar.driver-proofs]` row names this crate). This crate's own `cargo test`
    // never builds that directory: the proofs need the kernel and the loader only the root links.
    println!(
        "cargo::metadata=driver={}",
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?)
            .join("tests/on_driver")
            .display()
    );
    Ok(())
}
