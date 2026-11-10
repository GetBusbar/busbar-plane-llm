// SPDX-License-Identifier: Apache-2.0
// Copyright (C) 2026 Busbar Inc and contributors

//! THE ROOT CELLS' AND THE DRIVER PROOFS' ADDRESSES (this crate's `links` row): the composition root
//! compiles this plane's own root cells and its driver proofs from the paths printed here, reading
//! them as `DEP_BUSBAR_DOOR_CELLS_<CRATE>_CELLS` and `DEP_BUSBAR_DOOR_CELLS_<CRATE>_DRIVER` (the same
//! for a path dependency and a pinned one).

use std::error::Error;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=tests/blackbox");

    println!(
        "cargo::metadata=cells={}",
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?)
            .join("tests/root/mod.rs")
            .display()
    );

    println!(
        "cargo::metadata=driver={}",
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?)
            .join("tests/on_driver")
            .display()
    );
    println!(
        "cargo::metadata=blackbox={}",
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR")?)
            .join("tests/blackbox")
            .display()
    );
    export_context_source()?;
    Ok(())
}

fn export_context_source() -> std::io::Result<()> {
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"))
        .canonicalize()?;
    let source = manifest.join("tests/root_context/inbound.rs");
    if source.canonicalize()? != source {
        return Err(std::io::Error::other(
            "test source must remain in its owner",
        ));
    }
    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("output dir"));
    std::fs::write(output.join("context_test_source.rs"), format!(
        "#[doc(hidden)]\n#[macro_export]\nmacro_rules! __busbar_context_tests {{\n    (inbound, \"busbar-plane-llm\") => {{ include!({source:?}); }};\n}}\n"
    ))?;
    println!("cargo::rerun-if-changed={}", source.display());
    Ok(())
}
