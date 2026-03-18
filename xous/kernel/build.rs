// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();

    if target.starts_with("aarch64") && target.contains("none") {
        // Select linker script based on platform feature.
        // The xtask build system passes the linker script via RUSTFLAGS,
        // so we only set the default here for direct cargo build invocations.
        // Don't add a linker script from build.rs — xtask handles it via RUSTFLAGS.
        // This avoids conflicts when xtask passes a platform-specific linker script.

        // Set the `beetos` cfg flag for bare-metal AArch64
        println!("cargo:rustc-cfg=beetos");
    }

    // Rerun if linker scripts change
    println!("cargo:rerun-if-changed=link-aarch64.x");
    println!("cargo:rerun-if-changed=link-qemu-virt.x");
    println!("cargo:rerun-if-changed=build.rs");
}
