fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();

    if target.starts_with("aarch64") && (target.contains("none") || target.contains("beetos")) {
        println!("cargo:rustc-cfg=beetos");
    }
    println!("cargo:rustc-check-cfg=cfg(beetos)");

    println!("cargo:rerun-if-changed=build.rs");
}
