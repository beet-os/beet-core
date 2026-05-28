use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();

    match args.first().map(|s| s.as_str()) {
        Some("check") => check()?,
        Some("build") => build(&args[1..])?,
        Some("qemu") => qemu(&args[1..])?,
        Some("qemu-smoke") => qemu_smoke()?,
        Some("qemu-smoke-nodisk") => qemu_smoke_nodisk()?,
        Some("qemu-screenshot") => qemu_screenshot(&args[1..])?,
        Some("qemu-animation") => qemu_animation(&args[1..])?,
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
            println!("  qemu [--terminal]  Build and run on QEMU virt (--terminal = headless, no FB)");
            println!("  qemu-smoke         Boot QEMU and verify expected progress markers (CI)");
            println!("  qemu-smoke-nodisk  Boot QEMU *without* a disk image — verifies graceful degradation");
            println!("  qemu-screenshot [--wait SECS] [--out PATH]");
            println!("                     Boot QEMU and capture the framebuffer (default: 6s, target/beetos-fb.png)");
            println!("  qemu-animation [--frames N] [--interval SECS] [--out PATH]");
            println!("                     Capture N frames at given interval and stitch into an animated GIF");
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
    for app in &["hello", "shell", "procman", "fs", "log", "block"] {
        println!("Building app: {app}");
        // procman, fs, log, and block live in os/, everything else in apps/
        let manifest = if matches!(*app, "procman" | "fs" | "log" | "block") {
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

/// Convert the ELF kernel to a flat binary image alongside it.
///
/// QEMU's `-kernel <elf>` jumps straight to the ELF entry point and does
/// NOT run the Linux-style boot trampoline that puts the FDT physical
/// address in x0 — so an ELF-booted kernel sees `x0 = 0` and falls back
/// to compiled-in MMIO defaults instead of parsing the FDT. Booting via
/// the flat image triggers the Image-format path and gives us x0 = FDT,
/// matching the contract m1n1 and the RPi5 firmware use on real hardware.
fn elf_to_image(elf: &std::path::Path) -> anyhow::Result<PathBuf> {
    let img = elf.with_extension("img");
    let status = Command::new("llvm-objcopy")
        .args(["-O", "binary"])
        .arg(elf)
        .arg(&img)
        .status()
        .or_else(|_| {
            Command::new("rust-objcopy")
                .args(["-O", "binary"])
                .arg(elf)
                .arg(&img)
                .status()
        })?;
    anyhow::ensure!(status.success(), "objcopy failed");
    Ok(img)
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
    // --terminal flips off the FB stack: no ramfb, no input devices, no
    // QEMU display window. UART → stdio just like the early BeetOS
    // milestones; the kernel sees fb::init() fail and skips the entire
    // GUI compose loop so this is also the lightest-CPU way to boot.
    let terminal_only = args.iter().any(|a| a == "--terminal" || a == "--no-gui");

    // Build for qemu-virt first (strip our own flags before forwarding).
    let forward: Vec<String> = args.iter()
        .filter(|a| a.as_str() != "--terminal" && a.as_str() != "--no-gui")
        .cloned()
        .collect();
    build(&{
        let mut a = vec!["--platform".to_string(), "qemu-virt".to_string()];
        a.extend_from_slice(&forward);
        a
    })?;

    let root = workspace_root();
    let kernel_elf = root.join("target/aarch64-unknown-none/debug/beetos-kernel");

    anyhow::ensure!(
        kernel_elf.exists(),
        "kernel binary not found at {}",
        kernel_elf.display()
    );

    // Hand QEMU the flat image so x0 = FDT phys when entering _start
    // (see elf_to_image for the why).
    let kernel = elf_to_image(&kernel_elf)?;

    // Create test disk image
    let disk_img = create_test_disk(&root)?;

    println!();
    if terminal_only {
        println!("Launching QEMU (terminal mode — no FB/GUI)...");
    } else {
        println!("Launching QEMU...");
    }
    println!("  Press Ctrl-A X to exit QEMU");
    println!();

    let mut qemu_args = vec![
        "-machine".to_string(), "virt,gic-version=3".to_string(),
        "-cpu".to_string(), "neoverse-n1".to_string(),
        "-m".to_string(), "2G".to_string(),
        "-serial".to_string(), "stdio".to_string(),
        "-monitor".to_string(), "none".to_string(),
    ];
    if !terminal_only {
        qemu_args.extend_from_slice(&[
            "-device".to_string(), "ramfb".to_string(),
            "-device".to_string(), "virtio-keyboard-device".to_string(),
            "-device".to_string(), "virtio-tablet-device".to_string(),
        ]);
    } else {
        // Headless: no display, no FB, no pointer.
        qemu_args.extend_from_slice(&[
            "-display".to_string(), "none".to_string(),
        ]);
    }
    qemu_args.extend_from_slice(&[
        "-kernel".to_string(), kernel.to_str().expect("non-UTF8 path").to_string(),
    ]);

    // Add virtio-blk disk if image exists
    if disk_img.exists() {
        qemu_args.extend_from_slice(&[
            "-drive".to_string(),
            format!("file={},format=raw,if=none,id=disk0", disk_img.display()),
            "-device".to_string(),
            "virtio-blk-device,drive=disk0".to_string(),
        ]);
    }

    // Add virtio-net (user-mode networking; QEMU assigns 10.0.2.15 via DHCP)
    qemu_args.extend_from_slice(&[
        "-netdev".to_string(),
        "user,id=net0".to_string(),
        "-device".to_string(),
        "virtio-net-device,netdev=net0".to_string(),
    ]);

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

/// Boot the qemu-virt kernel, wait for the FB to stabilize, then capture
/// it via QEMU's QMP `screendump` and convert PPM → PNG. Saves the result
/// to the path given by `--out` (default `target/beetos-fb.png`). The
/// `--wait` flag controls how many seconds to let the kernel run before
/// snapping (default 6).
///
/// Requires: `qemu-system-aarch64`, `socat`, and ImageMagick `convert`
/// (or `magick`).
fn qemu_screenshot(args: &[String]) -> anyhow::Result<()> {
    let root = workspace_root();

    let mut wait_secs: u64 = 6;
    let mut out_path = root.join("target/beetos-fb.png");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--wait" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow::anyhow!("--wait needs a value"))?;
                wait_secs = v.parse()?;
                i += 2;
            }
            "--out" => {
                let v = args.get(i + 1).ok_or_else(|| anyhow::anyhow!("--out needs a value"))?;
                out_path = PathBuf::from(v);
                i += 2;
            }
            other => anyhow::bail!("unknown flag: {other}"),
        }
    }

    // Same dummy-stub trick as qemu_smoke so we don't need stage1 rustc.
    let nostd_target = root.join("target/aarch64-unknown-none/debug");
    std::fs::create_dir_all(&nostd_target)?;
    let hello_std = nostd_target.join("hello-std.stripped");
    if !hello_std.exists() {
        std::fs::write(&hello_std, b"\x7fELF\x02\x01\x01")?;
    }

    build(&["--platform".to_string(), "qemu-virt".to_string()])?;
    let kernel_elf = root.join("target/aarch64-unknown-none/debug/beetos-kernel");
    let kernel = elf_to_image(&kernel_elf)?;

    let qmp_sock = root.join("target/qemu-screenshot.qmp");
    let serial_log = root.join("target/qemu-screenshot-serial.log");
    let ppm_path = root.join("target/beetos-fb.ppm");
    let _ = std::fs::remove_file(&qmp_sock);
    let _ = std::fs::remove_file(&serial_log);
    let _ = std::fs::remove_file(&ppm_path);

    println!();
    println!("Launching QEMU for screenshot (wait: {}s)...", wait_secs);

    let mut child = Command::new("qemu-system-aarch64")
        .args([
            "-machine", "virt,gic-version=3",
            "-cpu", "neoverse-n1",
            "-m", "2G",
            "-display", "none",
            "-device", "ramfb",
            "-device", "virtio-keyboard-device",
            "-device", "virtio-tablet-device",
            "-chardev", &format!("file,id=c0,path={}", serial_log.display()),
            "-serial", "chardev:c0",
            "-qmp", &format!("unix:{},server,nowait", qmp_sock.display()),
            "-kernel", kernel.to_str().expect("non-UTF8 path"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    // Give QEMU a moment to open the QMP socket before we connect.
    std::thread::sleep(std::time::Duration::from_millis(500));
    // Let the kernel run long enough for the boot output to render.
    std::thread::sleep(std::time::Duration::from_secs(wait_secs));

    let qmp_script = format!(
        "{}\n{}\n{}\n",
        r#"{"execute":"qmp_capabilities"}"#,
        format!(r#"{{"execute":"screendump","arguments":{{"filename":"{}"}}}}"#, ppm_path.display()),
        r#"{"execute":"quit"}"#,
    );
    // socat itself often exits non-zero because `quit` makes QMP close the
    // socket on us — that's fine, the real success signal is whether the
    // PPM file shows up below.
    let _ = Command::new("socat")
        .arg("-")
        .arg(format!("UNIX-CONNECT:{}", qmp_sock.display()))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut c| {
            use std::io::Write;
            if let Some(mut stdin) = c.stdin.take() {
                let _ = stdin.write_all(qmp_script.as_bytes());
                let _ = stdin.flush();
                drop(stdin);
            }
            c.wait()
        });

    let _ = child.wait();

    anyhow::ensure!(
        ppm_path.exists(),
        "QMP screendump did not produce {}",
        ppm_path.display()
    );

    // PPM → PNG. Try `magick` (IM7) first, then `convert` (IM6).
    let conv_status = Command::new("magick")
        .arg(&ppm_path)
        .arg(&out_path)
        .status()
        .or_else(|_| {
            Command::new("convert")
                .arg(&ppm_path)
                .arg(&out_path)
                .status()
        })?;
    anyhow::ensure!(conv_status.success(), "PPM → PNG conversion failed");

    let size = std::fs::metadata(&out_path)?.len();
    println!();
    println!("Screenshot saved: {} ({} bytes)", out_path.display(), size);
    Ok(())
}

/// Capture a sequence of N framebuffer snapshots over time and stitch
/// them into an animated GIF. Useful for showing off animations
/// (clock, spinner, Mandelbrot zoom, Game of Life, Snake) in a single
/// asset.
///
/// Implementation: boot the qemu-virt kernel once, hold a QMP socket
/// open, issue successive `screendump` commands sleeping between each,
/// then convert the resulting PPMs to a single GIF via ImageMagick.
///
/// Flags:
///   --frames N        — number of frames (default 8)
///   --interval SECS   — seconds between captures (default 1)
///   --wait SECS       — initial settle delay (default 5)
///   --out PATH        — output GIF path (default target/beetos-anim.gif)
fn qemu_animation(args: &[String]) -> anyhow::Result<()> {
    let root = workspace_root();
    let mut frames: u32 = 8;
    let mut interval: f64 = 1.0;
    let mut wait_secs: u64 = 5;
    let mut out_path = root.join("target/beetos-anim.gif");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--frames"   => { frames = args[i+1].parse()?; i += 2; }
            "--interval" => { interval = args[i+1].parse()?; i += 2; }
            "--wait"     => { wait_secs = args[i+1].parse()?; i += 2; }
            "--out"      => { out_path = PathBuf::from(&args[i+1]); i += 2; }
            o => anyhow::bail!("unknown flag: {o}"),
        }
    }

    // Same dummy-stub trick as qemu_screenshot so we don't need stage1 rustc.
    let nostd_target = root.join("target/aarch64-unknown-none/debug");
    std::fs::create_dir_all(&nostd_target)?;
    let hello_std = nostd_target.join("hello-std.stripped");
    if !hello_std.exists() {
        std::fs::write(&hello_std, b"\x7fELF\x02\x01\x01")?;
    }

    build(&["--platform".to_string(), "qemu-virt".to_string()])?;
    let kernel_elf = root.join("target/aarch64-unknown-none/debug/beetos-kernel");
    let kernel = elf_to_image(&kernel_elf)?;

    let qmp_sock = root.join("target/qemu-animation.qmp");
    let serial_log = root.join("target/qemu-animation-serial.log");
    let _ = std::fs::remove_file(&qmp_sock);
    let _ = std::fs::remove_file(&serial_log);
    let frames_dir = root.join("target/qemu-animation-frames");
    let _ = std::fs::remove_dir_all(&frames_dir);
    std::fs::create_dir_all(&frames_dir)?;

    println!();
    println!("Launching QEMU for animation (frames: {frames}, interval: {interval}s, wait: {wait_secs}s)...");

    let mut child = Command::new("qemu-system-aarch64")
        .args([
            "-machine", "virt,gic-version=3",
            "-cpu", "neoverse-n1",
            "-m", "2G",
            "-display", "none",
            "-device", "ramfb",
            "-device", "virtio-keyboard-device",
            "-device", "virtio-tablet-device",
            "-chardev", &format!("file,id=c0,path={}", serial_log.display()),
            "-serial", "chardev:c0",
            "-qmp", &format!("unix:{},server,nowait", qmp_sock.display()),
            "-kernel", kernel.to_str().expect("non-UTF8 path"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    // Wait for QEMU + initial boot.
    std::thread::sleep(std::time::Duration::from_millis(500));
    std::thread::sleep(std::time::Duration::from_secs(wait_secs));

    // Build the QMP script: handshake, N screendumps separated by `interval`, then quit.
    let mut script = String::from(r#"{"execute":"qmp_capabilities"}"#);
    script.push('\n');
    for f in 0..frames {
        let ppm = frames_dir.join(format!("frame-{:03}.ppm", f));
        script.push_str(&format!(
            r#"{{"execute":"screendump","arguments":{{"filename":"{}"}}}}"#,
            ppm.display(),
        ));
        script.push('\n');
    }
    script.push_str(r#"{"execute":"quit"}"#);
    script.push('\n');

    // Pipe the script into socat, but pace it so QEMU has time to actually
    // render between screendumps. We could do that with separate socat calls;
    // simpler is to interleave sleeps via a small shell loop.  Spawn one
    // process per command instead.
    use std::io::Write;
    for line in script.lines() {
        let mut proc = Command::new("socat")
            .arg("-")
            .arg(format!("UNIX-CONNECT:{}", qmp_sock.display()))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        if let Some(mut stdin) = proc.stdin.take() {
            let _ = stdin.write_all(line.as_bytes());
            let _ = stdin.write_all(b"\n");
            let _ = stdin.flush();
            drop(stdin);
        }
        let _ = proc.wait();
        if line.contains("screendump") {
            std::thread::sleep(std::time::Duration::from_millis((interval * 1000.0) as u64));
        }
    }

    let _ = child.wait();

    // Convert all PPMs to a single animated GIF via ImageMagick.
    let delay_centisec = (interval * 100.0).round() as u64;
    let pattern = frames_dir.join("frame-*.ppm");
    let conv = Command::new("magick")
        .args(["-delay", &delay_centisec.to_string(), "-loop", "0"])
        .arg(&pattern)
        .arg(&out_path)
        .status()
        .or_else(|_| {
            Command::new("convert")
                .args(["-delay", &delay_centisec.to_string(), "-loop", "0"])
                .arg(&pattern)
                .arg(&out_path)
                .status()
        })?;
    anyhow::ensure!(conv.success(), "PPM -> GIF conversion failed");

    let size = std::fs::metadata(&out_path)?.len();
    println!();
    println!("Animation saved: {} ({} bytes, {} frames @ {}s interval)",
        out_path.display(), size, frames, interval);
    Ok(())
}

/// Boot the qemu-virt kernel and verify expected progress markers appear in
/// the serial log. Lightweight CI smoke test — does not need the stage1 rustc
/// or any beetos-test sentinel; just confirms the kernel makes it through
/// platform init, MMU bring-up, ELF loading, the first preemption switch,
/// and ends up at the shell prompt.
///
/// Exits with code 0 if every marker is seen within the timeout, 1 otherwise.
fn qemu_smoke() -> anyhow::Result<()> {
    // Markers we expect to see during a healthy boot. If any is missing
    // the smoke test fails — that's enough signal to catch regressions
    // in early init, MMU bring-up, ELF loading, scheduling, or shell
    // launch.
    //
    // Each substring is chosen to appear exactly once during a normal
    // boot (e.g. "Platform: QEMU virt" rather than the bare "BeetOS
    // v0.1.0" banner, which is re-emitted by the shell once it starts).
    let markers: &[&str] = &[
        "Platform: QEMU virt",
        "UART: address from FDT",
        "GIC: initialized (address from FDT)",
        "Timer: initialized",
        "MMU: enabled",
        "EL0: loading shell ELF",
        "Disk: mapped into block service",
        "EL0: launching shell",
        "PREEMPT: timer switched",
        "[fs] started, disk=10240 bytes via IPC",
        "[block] started, disk=",
        "[shell] block self-test: OK",
        "bsh>",
    ];
    run_smoke("qemu-smoke", /*with_disk*/ true, markers)
}

/// Same harness as [`qemu_smoke`] but boots QEMU without a `-drive`,
/// confirming that fs / block / shell degrade cleanly when there is
/// no backing device:
///   * block reports 0 blocks (still starts, still answers IPC)
///   * fs caches 0 bytes via IPC (still starts, still serves ramfs)
///   * shell's self-test detects capacity 0 and skips the read
fn qemu_smoke_nodisk() -> anyhow::Result<()> {
    let markers: &[&str] = &[
        "Platform: QEMU virt",
        "UART: address from FDT",
        "GIC: initialized (address from FDT)",
        "Timer: initialized",
        "MMU: enabled",
        "EL0: loading shell ELF",
        // No "Disk: mapped into block service" — there's no disk to map.
        "EL0: launching shell",
        "PREEMPT: timer switched",
        "[fs] started, disk=0 bytes via IPC",
        "[block] started, disk=0 bytes (0 blocks)",
        "[shell] block self-test: no disk attached (skipped)",
        "bsh>",
    ];
    run_smoke("qemu-smoke-nodisk", /*with_disk*/ false, markers)
}

/// Shared QEMU smoke harness: build kernel, launch QEMU (optionally
/// attaching a virtio-blk disk image), and verify every marker appears
/// in the serial log before the deadline.
fn run_smoke(name: &str, with_disk: bool, markers: &[&str]) -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    let root = workspace_root();

    // hello-std.stripped is embedded via include_bytes! at kernel build time.
    // Without the stage1 rustc we can't build it, but the kernel only spawns
    // it on demand from the shell — a dummy placeholder lets the kernel link
    // and boot. (build_apps() prints the same "[skip] hello-std" notice.)
    let nostd_target = root.join("target/aarch64-unknown-none/debug");
    std::fs::create_dir_all(&nostd_target)?;
    let hello_std = nostd_target.join("hello-std.stripped");
    if !hello_std.exists() {
        std::fs::write(&hello_std, b"\x7fELF\x02\x01\x01")?;
    }

    build(&["--platform".to_string(), "qemu-virt".to_string()])?;

    let kernel_elf = root.join("target/aarch64-unknown-none/debug/beetos-kernel");
    anyhow::ensure!(kernel_elf.exists(), "kernel binary not found at {}", kernel_elf.display());
    let kernel = elf_to_image(&kernel_elf)?;

    let disk_img = if with_disk { Some(create_test_disk(&root)?) } else { None };

    let serial_log = root.join(format!("target/{name}-serial.log"));
    let _ = std::fs::remove_file(&serial_log);

    println!();
    println!("Launching QEMU smoke test [{name}] (timeout: 15s)...");
    println!("  serial log: {}", serial_log.display());
    println!();

    let mut qemu_args = vec![
        "-machine".to_string(), "virt,gic-version=3".to_string(),
        "-cpu".to_string(), "neoverse-n1".to_string(),
        "-m".to_string(), "2G".to_string(),
        "-display".to_string(), "none".to_string(),
        "-device".to_string(), "ramfb".to_string(),
        "-chardev".to_string(),
        format!("file,id=c0,path={}", serial_log.display()),
        "-serial".to_string(), "chardev:c0".to_string(),
        "-kernel".to_string(), kernel.to_str().expect("non-UTF8 path").to_string(),
    ];

    if let Some(ref img) = disk_img {
        qemu_args.extend_from_slice(&[
            "-drive".to_string(),
            format!("file={},format=raw,if=none,id=disk0", img.display()),
            "-device".to_string(),
            "virtio-blk-device,drive=disk0".to_string(),
        ]);
    }

    let mut child = Command::new("qemu-system-aarch64")
        .args(&qemu_args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    // Markers are matched as a *set* — every marker must appear in the
    // log within the deadline, but the order between two unrelated
    // services (fs / block / shell self-test) is up to the scheduler
    // and would otherwise be a flake source.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let mut pending: Vec<&str> = markers.to_vec();
    let mut log_pos: u64 = 0;

    while std::time::Instant::now() < deadline && !pending.is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(200));

        let Ok(file) = std::fs::File::open(&serial_log) else { continue };
        let metadata = file.metadata()?;
        if metadata.len() <= log_pos {
            continue;
        }
        let mut reader = BufReader::new(file);
        use std::io::Seek;
        reader.seek(std::io::SeekFrom::Start(log_pos))?;
        for line in reader.lines().flatten() {
            println!("  {line}");
            pending.retain(|m| {
                if line.contains(m) {
                    println!("    [ok] marker matched: {m}");
                    false
                } else {
                    true
                }
            });
        }
        log_pos = metadata.len();
    }

    let _ = child.kill();
    let _ = child.wait();

    println!();
    if pending.is_empty() {
        println!("Result: SMOKE TEST PASSED [{name}] ({} markers seen)", markers.len());
        Ok(())
    } else {
        anyhow::bail!(
            "Result: SMOKE TEST FAILED [{name}] — {} of {} markers missing: {:?}",
            pending.len(),
            markers.len(),
            pending,
        );
    }
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

    let kernel_elf = root.join("target/aarch64-unknown-none/debug/beetos-kernel");
    anyhow::ensure!(kernel_elf.exists(), "kernel binary not found at {}", kernel_elf.display());
    let kernel = elf_to_image(&kernel_elf)?;

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

    // Add virtio-net (user-mode networking)
    qemu_args.extend_from_slice(&[
        "-netdev".to_string(),
        "user,id=net0".to_string(),
        "-device".to_string(),
        "virtio-net-device,netdev=net0".to_string(),
    ]);

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
