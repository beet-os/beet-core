use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("check") => check()?,
        Some("build") => build(&args[1..])?,
        Some("qemu") => qemu(&args[1..])?,
        Some("rpi5") => rpi5()?,
        Some("test") => test()?,
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
            println!("  test               Build and run self-test suite on QEMU (CI)");
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

/// Find the stage1 rustc from the Rust fork (for building std-based apps).
fn find_stage1_rustc(root: &std::path::Path) -> Option<PathBuf> {
    let rust_root = root.parent()?.join("rust");
    for host in &["aarch64-apple-darwin", "x86_64-unknown-linux-gnu", "x86_64-apple-darwin"] {
        let rustc = rust_root.join(format!("build/{host}/stage1/bin/rustc"));
        if rustc.exists() {
            return Some(rustc);
        }
    }
    None
}

/// Write a shell-script RUSTC_WRAPPER that dispatches to stage1 for the
/// aarch64-unknown-beetos target and to the system rustc for everything else
/// (notably build scripts, which compile for the host and need host std).
///
/// Using RUSTC_WRAPPER instead of RUSTC keeps build-script compilation on the
/// host rustc while still routing target crate compilation through stage1.
/// Target-specific flags (sysroot, linker script) are passed separately via
/// CARGO_TARGET_AARCH64_UNKNOWN_BEETOS_RUSTFLAGS, which Cargo does not forward
/// to build scripts.
fn write_rustc_wrapper(stage1_rustc: &std::path::Path) -> anyhow::Result<PathBuf> {
    let wrapper = std::env::temp_dir().join("beetos-rustc-wrapper");
    let script = format!(
        "#!/bin/sh\nORIGINAL_RUSTC=\"$1\"\nshift\ncase \"$*\" in\n  *aarch64-unknown-beetos*)\n    exec '{}' \"$@\" ;;\n  *)\n    exec \"$ORIGINAL_RUSTC\" \"$@\" ;;\nesac\n",
        stage1_rustc.display()
    );
    std::fs::write(&wrapper, &script)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(wrapper)
}

/// Find the rust-lld binary for linking aarch64-unknown-beetos binaries.
///
/// The stage1 compiler directory often lacks rust-lld; it lives in stage0 or
/// stage0-sysroot instead. Returns the directory containing rust-lld so the
/// caller can prepend it to PATH.
fn find_rust_lld_dir(root: &std::path::Path) -> Option<PathBuf> {
    let rust_root = root.parent()?.join("rust");
    for host in &["aarch64-apple-darwin", "x86_64-unknown-linux-gnu", "x86_64-apple-darwin"] {
        for stage in &["stage1", "stage0-sysroot", "stage0"] {
            let lld = rust_root.join(format!("build/{host}/{stage}/lib/rustlib/{host}/bin/rust-lld"));
            if lld.exists() {
                return lld.parent().map(|p| p.to_path_buf());
            }
        }
    }
    None
}

/// Build userspace binaries (apps/) for aarch64-unknown-none.
fn build_apps(root: &std::path::Path) -> anyhow::Result<()> {
    let user_linker = root.join("apps/link-user.x");
    let linker_arg = format!("-Clink-arg=-T{}", user_linker.display());

    let ws_target = root.join("target");
    let target_dir = ws_target.join("aarch64-unknown-none/debug");

    // Build all app/service crates (excluded from workspace, so use --manifest-path)
    for app in &["hello", "shell", "procman", "fs", "log"] {
        println!("Building app: {app}");
        // procman, fs, and log live in os/, everything else in apps/
        let manifest = if *app == "procman" || *app == "fs" || *app == "log" {
            root.join(format!("os/{app}/Cargo.toml"))
        } else {
            root.join(format!("apps/{app}/Cargo.toml"))
        };
        let status = Command::new("cargo")
            .args([
                "build",
                "--manifest-path",
                manifest.to_str().expect("non-UTF8 path"),
                "--target-dir",
                ws_target.to_str().expect("non-UTF8 path"),
                "--target",
                "aarch64-unknown-none",
            ])
            .env("RUSTFLAGS", format!("{linker_arg} -Ccodegen-units=1"))
            .status()?;
        anyhow::ensure!(status.success(), "building app '{app}' failed");

        // Strip debug info to keep the embedded ELF small
        let elf = target_dir.join(app);
        let stripped = target_dir.join(format!("{app}.stripped"));
        strip_binary(&elf, &stripped)?;

        let size = std::fs::metadata(&stripped)?.len();
        println!("  {app}.stripped: {size} bytes");
    }

    // Build hello-std with the custom stage1 rustc (aarch64-unknown-beetos target)
    if let Some(stage1_rustc) = find_stage1_rustc(root) {
        build_std_app(root, &stage1_rustc, &user_linker, &ws_target)?;
    } else {
        println!("  [skip] hello-std: stage1 rustc not found (build ../rust first)");
    }

    Ok(())
}

/// Build hello-std using the custom stage1 rustc with aarch64-unknown-beetos target.
fn build_std_app(
    root: &std::path::Path,
    stage1_rustc: &std::path::Path,
    user_linker: &std::path::Path,
    ws_target: &std::path::Path,
) -> anyhow::Result<()> {
    println!("Building app: hello-std (with std, aarch64-unknown-beetos)");
    let manifest = root.join("apps/hello-std/Cargo.toml");
    let linker_arg = format!("-Clink-arg=-T{}", user_linker.display());

    // The stage1 sysroot is the parent of bin/rustc (i.e. the stage1 directory)
    let sysroot = stage1_rustc
        .parent().expect("no parent for rustc")
        .parent().expect("no grandparent for rustc");
    let sysroot_arg = format!("--sysroot={}", sysroot.display());

    // RUSTC_WRAPPER dispatches beetos-target builds to stage1 while keeping
    // build-script compilation on the system rustc (which has host std).
    // Target-specific flags go in CARGO_TARGET_AARCH64_UNKNOWN_BEETOS_RUSTFLAGS
    // so build scripts never see the beetos sysroot or linker script.
    let wrapper = write_rustc_wrapper(stage1_rustc)?;
    let target_rustflags = format!("{sysroot_arg} {linker_arg} -Ccodegen-units=1");

    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "--manifest-path",
        manifest.to_str().expect("non-UTF8 path"),
        "--target-dir",
        ws_target.to_str().expect("non-UTF8 path"),
        "--target",
        "aarch64-unknown-beetos",
    ])
    .env("RUSTC_WRAPPER", &wrapper)
    .env("CARGO_TARGET_AARCH64_UNKNOWN_BEETOS_RUSTFLAGS", &target_rustflags);

    // rust-lld may only exist in stage0; prepend its directory to PATH.
    if let Some(lld_dir) = find_rust_lld_dir(root) {
        let path = format!("{}:{}", lld_dir.display(), env::var("PATH").unwrap_or_default());
        cmd.env("PATH", path);
    }

    let status = cmd.status()?;
    let _ = std::fs::remove_file(&wrapper);
    anyhow::ensure!(status.success(), "building app 'hello-std' failed");

    // Strip and copy to the no_std target dir so include_bytes! can find it
    let std_target_dir = ws_target.join("aarch64-unknown-beetos/debug");
    let nostd_target_dir = ws_target.join("aarch64-unknown-none/debug");
    let elf = std_target_dir.join("hello-std");
    let stripped = nostd_target_dir.join("hello-std.stripped");
    strip_binary(&elf, &stripped)?;

    let size = std::fs::metadata(&stripped)?.len();
    println!("  hello-std.stripped: {size} bytes");
    Ok(())
}

/// Strip debug info from an ELF binary.
fn strip_binary(elf: &std::path::Path, stripped: &std::path::Path) -> anyhow::Result<()> {
    let status = Command::new("llvm-strip")
        .args(["--strip-debug", "-o"])
        .arg(stripped)
        .arg(elf)
        .status()
        .or_else(|_| {
            Command::new("rust-objcopy")
                .args(["--strip-debug"])
                .arg(elf)
                .arg(stripped)
                .status()
        })?;
    anyhow::ensure!(status.success(), "stripping {:?} failed", elf.file_name());
    Ok(())
}

fn build(args: &[String]) -> anyhow::Result<()> {
    let platform = parse_platform(args);
    let root = workspace_root();

    // Build userspace apps first (kernel embeds them via include_bytes!)
    build_apps(&root)?;

    let feature = match platform.as_str() {
        "qemu-virt" => "platform-qemu-virt",
        "bcm2712" => "platform-bcm2712",
        "apple-t8103" => "platform-apple-t8103",
        other => anyhow::bail!("unknown platform: {other}"),
    };

    let linker_script = match platform.as_str() {
        "qemu-virt" => root.join("xous/kernel/link-qemu-virt.x"),
        "bcm2712" => root.join("xous/kernel/link-bcm2712.x"),
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

/// Build a kernel8.img for Raspberry Pi 5 (BCM2712).
///
/// kernel8.img is a raw binary (ELF stripped to flat binary) placed in the
/// root of the SD card. The RPi5 firmware loads it at physical 0x80000.
///
/// Usage:
///   cargo xtask rpi5
///   # Then copy kernel8.img to the SD card root
fn rpi5() -> anyhow::Result<()> {
    let args = vec!["--platform".to_string(), "bcm2712".to_string()];
    build(&args)?;

    let root = workspace_root();
    let elf = root.join("target/aarch64-unknown-none/debug/beetos-kernel");
    let img = root.join("kernel8.img");

    anyhow::ensure!(elf.exists(), "kernel ELF not found at {}", elf.display());

    // Convert ELF to flat binary with llvm-objcopy or rust-objcopy
    let status = Command::new("llvm-objcopy")
        .args(["-O", "binary"])
        .arg(&elf)
        .arg(&img)
        .status()
        .or_else(|_| {
            Command::new("rust-objcopy")
                .args(["-O", "binary"])
                .arg(&elf)
                .arg(&img)
                .status()
        })?;

    anyhow::ensure!(status.success(), "objcopy failed");

    let size = std::fs::metadata(&img)?.len();
    println!("Built: {} ({} bytes)", img.display(), size);
    println!();
    println!("Copy kernel8.img to the root of your RPi5 SD card:");
    println!("  cp {} /Volumes/bootfs/kernel8.img", img.display());

    Ok(())
}

/// Create a test disk image (tar archive) for virtio-blk testing.
fn create_test_disk(root: &std::path::Path) -> anyhow::Result<PathBuf> {
    let disk_dir = root.join("target/disk");
    let disk_img = root.join("target/disk.img");

    // Create test files
    std::fs::create_dir_all(&disk_dir)?;
    std::fs::write(disk_dir.join("hello.txt"), "Hello from virtio-blk!\n")?;
    std::fs::write(disk_dir.join("readme.txt"), "BeetOS test disk image.\nThis file is stored on a virtual block device.\n")?;
    std::fs::write(disk_dir.join("numbers.txt"), "1\n2\n3\n4\n5\n")?;

    // Create tar archive (COPYFILE_DISABLE prevents macOS ._ resource fork files)
    let status = Command::new("tar")
        .env("COPYFILE_DISABLE", "1")
        .args(["cf", disk_img.to_str().expect("non-UTF8 path"),
               "-C", disk_dir.to_str().expect("non-UTF8 path"),
               "hello.txt", "readme.txt", "numbers.txt"])
        .status()?;
    anyhow::ensure!(status.success(), "tar creation failed");

    let size = std::fs::metadata(&disk_img)?.len();
    println!("Disk image: {} ({} bytes)", disk_img.display(), size);
    Ok(disk_img)
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

    // Create test disk image
    let disk_img = create_test_disk(&root)?;

    println!();
    println!("Launching QEMU...");
    println!("  Press Ctrl-A X to exit QEMU");
    println!();

    let mut qemu_args = vec![
        "-machine".to_string(), "virt,gic-version=3".to_string(),
        "-cpu".to_string(), "neoverse-n1".to_string(),
        "-m".to_string(), "2G".to_string(),
        "-nographic".to_string(),
        "-kernel".to_string(), kernel.to_str().expect("non-UTF8 path").to_string(),
    ];

    // Add virtio-blk disk if image exists
    if disk_img.exists() {
        qemu_args.extend_from_slice(&[
            "-drive".to_string(),
            format!("file={},format=raw,if=none,id=disk0", disk_img.display()),
            "-device".to_string(),
            "virtio-blk-device,drive=disk0".to_string(),
        ]);
    }

    let status = Command::new("qemu-system-aarch64")
        .args(&qemu_args)
        .status()?;

    if !status.success() {
        anyhow::bail!("QEMU exited with status: {status}");
    }

    Ok(())
}

/// Build the beetos-test binary using the custom stage1 rustc.
fn build_test_app(
    root: &std::path::Path,
    stage1_rustc: &std::path::Path,
    user_linker: &std::path::Path,
    ws_target: &std::path::Path,
) -> anyhow::Result<()> {
    println!("Building app: beetos-test (with std, aarch64-unknown-beetos)");
    let manifest = root.join("apps/beetos-test/Cargo.toml");
    let linker_arg = format!("-Clink-arg=-T{}", user_linker.display());

    let sysroot = stage1_rustc
        .parent().expect("no parent for rustc")
        .parent().expect("no grandparent for rustc");
    let sysroot_arg = format!("--sysroot={}", sysroot.display());

    let wrapper = write_rustc_wrapper(stage1_rustc)?;
    let target_rustflags = format!("{sysroot_arg} {linker_arg} -Ccodegen-units=1");

    let mut cmd = Command::new("cargo");
    cmd.args([
        "build",
        "--manifest-path",
        manifest.to_str().expect("non-UTF8 path"),
        "--target-dir",
        ws_target.to_str().expect("non-UTF8 path"),
        "--target",
        "aarch64-unknown-beetos",
    ])
    .env("RUSTC_WRAPPER", &wrapper)
    .env("CARGO_TARGET_AARCH64_UNKNOWN_BEETOS_RUSTFLAGS", &target_rustflags);

    // rust-lld may only exist in stage0; prepend its directory to PATH.
    if let Some(lld_dir) = find_rust_lld_dir(root) {
        let path = format!("{}:{}", lld_dir.display(), env::var("PATH").unwrap_or_default());
        cmd.env("PATH", path);
    }

    let status = cmd.status()?;
    let _ = std::fs::remove_file(&wrapper);
    anyhow::ensure!(status.success(), "building app 'beetos-test' failed");

    // Strip and copy to the no_std target dir so include_bytes! can find it
    let std_target_dir = ws_target.join("aarch64-unknown-beetos/debug");
    let nostd_target_dir = ws_target.join("aarch64-unknown-none/debug");
    let elf = std_target_dir.join("beetos-test");
    let stripped = nostd_target_dir.join("beetos-test.stripped");
    strip_binary(&elf, &stripped)?;

    let size = std::fs::metadata(&stripped)?.len();
    println!("  beetos-test.stripped: {size} bytes");
    Ok(())
}

/// Build the kernel with platform-qemu-virt + test-mode features.
fn build_test_kernel(root: &std::path::Path) -> anyhow::Result<()> {
    let linker_script = root.join("xous/kernel/link-qemu-virt.x");
    let linker_arg = format!("-Clink-arg=-T{}", linker_script.display());

    println!("Building test kernel (platform-qemu-virt + test-mode)...");
    let status = Command::new("cargo")
        .args([
            "build",
            "--package",
            "beetos-kernel",
            "--target",
            "aarch64-unknown-none",
            "--features",
            "platform-qemu-virt,test-mode",
        ])
        .env("RUSTFLAGS", format!("{linker_arg} -Ccodegen-units=1"))
        .status()?;
    anyhow::ensure!(status.success(), "test kernel build failed");
    Ok(())
}

/// Build the test binary + kernel, launch QEMU with piped stdout, parse results.
///
/// Exits with code 0 if all tests pass, 1 if any fail or the timeout is reached.
fn test() -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    let root = workspace_root();
    let user_linker = root.join("apps/link-user.x");
    let ws_target = root.join("target");

    // Build beetos-test first so include_bytes! in the kernel can find it.
    let stage1_rustc = find_stage1_rustc(&root)
        .ok_or_else(|| anyhow::anyhow!("stage1 rustc not found — build ../rust first"))?;
    build_test_app(&root, &stage1_rustc, &user_linker, &ws_target)?;

    // Build all other apps (kernel embeds them all).
    build_apps(&root)?;

    // Build the kernel with test-mode enabled.
    build_test_kernel(&root)?;

    let kernel = root.join("target/aarch64-unknown-none/debug/beetos-kernel");
    anyhow::ensure!(kernel.exists(), "kernel binary not found at {}", kernel.display());

    let disk_img = create_test_disk(&root)?;

    println!();
    println!("Launching QEMU for tests (timeout: 60s)...");
    println!();

    let mut qemu_args = vec![
        "-machine".to_string(), "virt,gic-version=3".to_string(),
        "-cpu".to_string(), "neoverse-n1".to_string(),
        "-m".to_string(), "2G".to_string(),
        "-nographic".to_string(),
        "-kernel".to_string(), kernel.to_str().expect("non-UTF8 path").to_string(),
    ];

    if disk_img.exists() {
        qemu_args.extend_from_slice(&[
            "-drive".to_string(),
            format!("file={},format=raw,if=none,id=disk0", disk_img.display()),
            "-device".to_string(),
            "virtio-blk-device,drive=disk0".to_string(),
        ]);
    }

    let mut child = Command::new("qemu-system-aarch64")
        .args(&qemu_args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;

    let stdout = child.stdout.take().expect("piped stdout missing");
    let (tx, rx) = std::sync::mpsc::channel::<String>();

    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().flatten() {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let mut all_passed = false;
    let mut some_failed = false;

    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                println!("  {line}");
                if line.contains("ALL TESTS PASSED") {
                    all_passed = true;
                    break;
                }
                if line.contains("SOME TESTS FAILED") {
                    some_failed = true;
                    break;
                }
            }
            Err(_) => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    println!();
    if all_passed {
        println!("Result: ALL TESTS PASSED");
        Ok(())
    } else if some_failed {
        anyhow::bail!("Result: SOME TESTS FAILED");
    } else {
        anyhow::bail!("Result: TIMEOUT — test sentinel not seen within 60s");
    }
}
