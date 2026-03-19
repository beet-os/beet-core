use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("check") => check()?,
        Some("build") => build(&args[1..])?,
        Some("qemu") => qemu(&args[1..])?,
        Some(cmd) => anyhow::bail!("unknown command: {cmd}"),
        None => {
            println!("BeetOS xtask build system");
            println!();
            println!("Usage: cargo xtask <command>");
            println!();
            println!("Commands:");
            println!("  check              Check all workspace crates (hosted mode)");
            println!("  build [--platform]  Cross-compile for aarch64-unknown-none");
            println!("  qemu               Build and run on QEMU virt");
            println!();
            println!("Platforms:");
            println!("  qemu-virt          QEMU virt machine (default)");
            println!("  apple-t8103        Apple M1 (MacBook Air)");
        }
    }

    Ok(())
}

fn check() -> anyhow::Result<()> {
    println!("Checking workspace (hosted mode)...");
    let status = Command::new("cargo")
        .args(["check", "--workspace"])
        .status()?;
    anyhow::ensure!(status.success(), "cargo check failed");
    Ok(())
}

/// Get the workspace root directory.
fn workspace_root() -> PathBuf {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version=1", "--no-deps"])
        .output()
        .expect("failed to run cargo metadata");
    let metadata: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("failed to parse cargo metadata");
    PathBuf::from(metadata["workspace_root"].as_str().expect("no workspace_root"))
}

/// Parse --platform flag, defaulting to qemu-virt.
fn parse_platform(args: &[String]) -> String {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--platform" {
            if let Some(p) = args.get(i + 1) {
                return p.clone();
            }
        }
        if let Some(p) = arg.strip_prefix("--platform=") {
            return p.to_string();
        }
    }
    "qemu-virt".to_string()
}

/// Build userspace binaries (apps/) for aarch64-unknown-none.
fn build_apps(root: &std::path::Path) -> anyhow::Result<()> {
    let user_linker = root.join("apps/link-user.x");
    let linker_arg = format!("-Clink-arg=-T{}", user_linker.display());

    let target_dir = root.join("target/aarch64-unknown-none/debug");

    // Build all app crates
    for app in &["hello"] {
        println!("Building app: {app}");
        let status = Command::new("cargo")
            .args([
                "build",
                "--package",
                app,
                "--target",
                "aarch64-unknown-none",
            ])
            .env("RUSTFLAGS", format!("{linker_arg} -Ccodegen-units=1"))
            .status()?;
        anyhow::ensure!(status.success(), "building app '{app}' failed");

        // Strip debug info to keep the embedded ELF small
        let elf = target_dir.join(app);
        let stripped = target_dir.join(format!("{app}.stripped"));
        let status = Command::new("llvm-strip")
            .args(["--strip-debug", "-o"])
            .arg(&stripped)
            .arg(&elf)
            .status()
            .or_else(|_| {
                // Fallback to rust-objcopy from cargo-binutils
                Command::new("rust-objcopy")
                    .args(["--strip-debug"])
                    .arg(&elf)
                    .arg(&stripped)
                    .status()
            })?;
        anyhow::ensure!(status.success(), "stripping app '{app}' failed");

        let size = std::fs::metadata(&stripped)?.len();
        println!("  {app}.stripped: {size} bytes");
    }

    Ok(())
}

fn build(args: &[String]) -> anyhow::Result<()> {
    let platform = parse_platform(args);
    let root = workspace_root();

    // Build userspace apps first (kernel embeds them via include_bytes!)
    build_apps(&root)?;

    let feature = match platform.as_str() {
        "qemu-virt" => "platform-qemu-virt",
        "apple-t8103" => "platform-apple-t8103",
        other => anyhow::bail!("unknown platform: {other}"),
    };

    let linker_script = match platform.as_str() {
        "qemu-virt" => root.join("xous/kernel/link-qemu-virt.x"),
        "apple-t8103" => root.join("xous/kernel/link-aarch64.x"),
        _ => unreachable!(),
    };

    println!("Building BeetOS kernel for platform: {platform}");
    println!("Linker script: {}", linker_script.display());

    let linker_arg = format!("-Clink-arg=-T{}", linker_script.display());

    let status = Command::new("cargo")
        .args([
            "build",
            "--package",
            "beetos-kernel",
            "--target",
            "aarch64-unknown-none",
            "--features",
            feature,
        ])
        .env("RUSTFLAGS", format!("{linker_arg} -Ccodegen-units=1"))
        .status()?;

    anyhow::ensure!(status.success(), "cargo build failed");

    let binary = root
        .join("target/aarch64-unknown-none/debug/beetos-kernel");
    println!("Built: {}", binary.display());

    Ok(())
}

fn qemu(args: &[String]) -> anyhow::Result<()> {
    // Build for qemu-virt first
    build(&{
        let mut a = vec!["--platform".to_string(), "qemu-virt".to_string()];
        a.extend_from_slice(args);
        a
    })?;

    let root = workspace_root();
    let kernel = root.join("target/aarch64-unknown-none/debug/beetos-kernel");

    anyhow::ensure!(
        kernel.exists(),
        "kernel binary not found at {}",
        kernel.display()
    );

    println!();
    println!("Launching QEMU...");
    println!("  Press Ctrl-A X to exit QEMU");
    println!();

    let status = Command::new("qemu-system-aarch64")
        .args([
            "-machine", "virt,gic-version=3",
            "-cpu", "neoverse-n1",  // Supports 16KB granule (required by BeetOS)
            "-m", "512M",
            "-nographic",
            "-kernel", kernel.to_str().expect("non-UTF8 path"),
        ])
        .status()?;

    if !status.success() {
        anyhow::bail!("QEMU exited with status: {status}");
    }

    Ok(())
}
