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
    export_test_source(
        "tests/root_support/mod.rs",
        "__busbar_common_tests",
        "common_test_source.rs",
    )?;
    Ok(())
}

/// Export source-only test support to the composition root without a runtime dependency.
/// The caller includes OUT_DIR/root_test_source.rs from the owner library's crate root.
fn export_test_source(relative: &str, macro_name: &str, output_name: &str) -> std::io::Result<()> {
    use std::path::PathBuf;
    let manifest = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("manifest dir"))
        .canonicalize()?;
    let source = manifest.join(relative);
    if source.canonicalize()? != source {
        return Err(std::io::Error::other(
            "test source must be the owner's own declared path, without symlink escapes",
        ));
    }
    let package = std::env::var("CARGO_PKG_NAME").expect("package name");
    let output = PathBuf::from(std::env::var_os("OUT_DIR").expect("output dir"));
    std::fs::write(
        output.join(output_name),
        format!(
            "#[doc(hidden)]\n#[macro_export]\nmacro_rules! {macro_name} {{\n    \
             ($module:ident, {package:?}) => {{\n        \
             #[path = {source:?}]\n        pub(crate) mod $module;\n    }};\n}}\n"
        ),
    )?;
    println!("cargo::rerun-if-changed={}", source.display());
    println!(
        "cargo::rerun-if-changed={}",
        manifest.join("Cargo.toml").display()
    );
    Ok(())
}
