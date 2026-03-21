fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();

    if target.starts_with("aarch64") && (target.contains("none") || target.contains("beetos")) {
        // Set the `beetos` cfg flag for bare-metal AArch64
        println!("cargo:rustc-cfg=beetos");
    }

    // Declare "beetos" as a known target_os value so rustc doesn't warn on
    // cfg(target_os = "beetos") when compiling for other targets.
    println!("cargo:rustc-check-cfg=cfg(target_os, values(\"beetos\"))");

    println!("cargo:rerun-if-changed=build.rs");
}
