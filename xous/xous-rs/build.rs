fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();

    if target.starts_with("aarch64") && (target.contains("none") || target.contains("beetos")) {
        // Set the `beetos` cfg flag for bare-metal AArch64
        println!("cargo:rustc-cfg=beetos");
    }

    println!("cargo:rerun-if-changed=build.rs");
}
