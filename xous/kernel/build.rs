// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();

    if target.starts_with("aarch64") && target.contains("none") {
        // Use our AArch64 linker script for bare-metal targets
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        println!("cargo:rustc-link-arg=-T{}/link-aarch64.x", manifest_dir);

        // Set the `beetos` cfg flag for bare-metal AArch64
        println!("cargo:rustc-cfg=beetos");
    }

    // Rerun if linker script changes
    println!("cargo:rerun-if-changed=link-aarch64.x");
    println!("cargo:rerun-if-changed=build.rs");
}
