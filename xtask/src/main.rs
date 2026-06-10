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
        Some("qemu-smoke-net") => qemu_smoke_net()?,
        Some("qemu-smoke-net-userspace") => qemu_smoke_net_userspace()?,
        Some("qemu-bench") => qemu_bench(&args[1..])?,
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
            println!("  qemu-smoke-net     Boot QEMU with networking, drive the TCP remote console over a host socket");
            println!("  qemu-smoke-net-userspace   Boot QEMU and exercise api/net via shell commands (listen + connect)");
            println!("  qemu-bench [--update] [--tolerance PCT]");
            println!("                     Run kernel micro-benchmarks under deterministic icount and");
            println!("                     compare against xtask/qemu-bench-baseline.txt (CI perf gate)");
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
    for app in &["hello", "shell", "procman", "fs", "log", "block", "console"] {
        println!("Building app: {app}");
        // procman, fs, log, block, console live in os/, everything else in apps/
        let manifest = if matches!(*app, "procman" | "fs" | "log" | "block" | "console") {
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

/// Build a kernel8.img + matching config.txt bundle for Raspberry Pi 5
/// (BCM2712).
///
/// `kernel8.img` is a raw binary (ELF stripped to flat) the Pi firmware
/// loads at physical 0x80000 and jumps to with x0 = FDT PA (the Linux
/// ARM64 boot protocol). `config.txt` is the firmware config file that
/// tells the GPU bootloader to use our kernel and to expose UART0 on
/// the GPIO header for serial console.
///
/// Both files land in `target/rpi5/`. To run on a real Pi 5:
///   cargo xtask rpi5
///   cp target/rpi5/{kernel8.img,config.txt} /Volumes/bootfs/
fn rpi5() -> anyhow::Result<()> {
    let args = vec!["--platform".to_string(), "bcm2712".to_string()];
    build(&args)?;

    let root = workspace_root();
    let elf = root.join("target/aarch64-unknown-none/debug/beetos-kernel");
    let out_dir = root.join("target/rpi5");
    std::fs::create_dir_all(&out_dir)?;
    let img = out_dir.join("kernel8.img");
    let cfg = out_dir.join("config.txt");

    anyhow::ensure!(elf.exists(), "kernel ELF not found at {}", elf.display());

    // ELF → flat binary. Try llvm-objcopy first, fall back to rust-objcopy
    // (the cargo-binutils shim) so this works on stock toolchains too.
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

    // Drop a `kernel8.img`-only `config.txt` next to it. arm_64bit=1 is
    // the default on the Pi 5 but we set it explicitly so a copy-paste
    // onto a Pi 4 SD card behaves the same way. enable_uart=1 routes
    // UART0 (PL011) onto GPIO 14/15 — that's the line our PL011 driver
    // hardcodes via the FDT-discovered base 0x10_7D00_1000.
    std::fs::write(&cfg, "\
# BeetOS Raspberry Pi 5 boot configuration.
# Generated by `cargo xtask rpi5`. Drop this file + kernel8.img on the
# root of the bootfs partition.

arm_64bit=1          # AArch64 boot
kernel=kernel8.img   # our kernel (default name, set explicitly)
enable_uart=1        # expose UART0 on GPIO header (115200-8N1)
# dtoverlay=disable-bt  # uncomment if Bluetooth steals UART0 on your board
")?;

    let size = std::fs::metadata(&img)?.len();
    println!("Built: {} ({} bytes)", img.display(), size);
    println!("Built: {}", cfg.display());
    println!();
    println!("To boot on a real Pi 5:");
    println!("  cp {}/{{kernel8.img,config.txt}} /Volumes/bootfs/", out_dir.display());
    println!("Then connect a USB-TTL adapter to GPIO 14/15 (115200-8N1) and power-cycle.");
    println!("First UART line should read:  BeetOS v0.1.0");

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

/// Boot QEMU with user-mode networking + a hostfwd to the guest's TCP
/// remote console (guest port 2323 → host 127.0.0.1:5555), wait for
/// DHCP to bind, then drive the console over a real TCP socket from the
/// host. This is the M7 end-to-end proof: virtio-net RX/TX, the in-
/// kernel ARP/DHCP/IP stack, and the new TCP server all on the live
/// path — no mocking.
fn qemu_smoke_net() -> anyhow::Result<()> {
    use std::time::{Duration, Instant};

    let root = workspace_root();

    // Same hello-std placeholder dance as run_smoke so the kernel links.
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

    let serial_log = root.join("target/qemu-smoke-net-serial.log");
    let _ = std::fs::remove_file(&serial_log);

    const HOST_PORT: u16 = 5555;
    const GUEST_PORT: u16 = 2323;

    println!();
    println!("Launching QEMU net smoke test (timeout: 20s)...");
    println!("  serial log: {}", serial_log.display());
    println!("  hostfwd: 127.0.0.1:{HOST_PORT} -> guest :{GUEST_PORT}");
    println!();

    let qemu_args = vec![
        "-machine".to_string(), "virt,gic-version=3".to_string(),
        "-cpu".to_string(), "neoverse-n1".to_string(),
        "-m".to_string(), "2G".to_string(),
        "-display".to_string(), "none".to_string(),
        "-device".to_string(), "ramfb".to_string(),
        "-chardev".to_string(),
        format!("file,id=c0,path={}", serial_log.display()),
        "-serial".to_string(), "chardev:c0".to_string(),
        "-netdev".to_string(),
        format!("user,id=net0,hostfwd=tcp:127.0.0.1:{HOST_PORT}-:{GUEST_PORT}"),
        "-device".to_string(), "virtio-net-device,netdev=net0".to_string(),
        "-kernel".to_string(), kernel.to_str().expect("non-UTF8 path").to_string(),
    ];

    let mut child = Command::new("qemu-system-aarch64")
        .args(&qemu_args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    // Phase 1: wait for the guest to bind an IP via DHCP. net_stack
    // prints "virtio-net: IP=..." on the DHCP ACK.
    let result = (|| -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(20);
        let dhcp_marker = "virtio-net: IP=";
        wait_for_marker(&serial_log, dhcp_marker, deadline)?;
        println!("  [ok] DHCP bound (saw '{dhcp_marker}')");

        // Phase 2: open the TCP console. QEMU accepts the host-side
        // connection immediately but the guest only answers once it's
        // listening with an IP — so retry the whole open/read cycle.
        let mut last_err = String::new();
        while Instant::now() < deadline {
            match try_console_session(HOST_PORT) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    last_err = e.to_string();
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
        anyhow::bail!("TCP console never responded correctly: {last_err}");
    })();

    let _ = child.kill();
    let _ = child.wait();

    println!();
    match result {
        Ok(()) => {
            println!("Result: NET SMOKE TEST PASSED");
            Ok(())
        }
        Err(e) => anyhow::bail!("Result: NET SMOKE TEST FAILED — {e}"),
    }
}

/// Userspace-socket smoke test. Boots QEMU, drives the *real* shell
/// over the kernel TCP console to invoke `nettest-listen` (proves
/// passive open from a userspace app), then `nettest-connect` (proves
/// active open through ARP + SYN to a host listener at the slirp
/// gateway, 10.0.2.2).
///
/// Three host-side TCP endpoints involved:
///   - 127.0.0.1:HOST_CONSOLE_PORT → guest :2323 — kernel console.
///     Drives shell input.
///   - 127.0.0.1:HOST_USER_PORT → guest :USER_LISTEN_PORT — the
///     shell's `nettest-listen` opens a userspace TcpListener; we
///     connect there from the host and exchange a byte pattern.
///   - host TcpListener on `HOST_LISTEN_PORT` — `nettest-connect`
///     opens an outbound TcpStream to 10.0.2.2:HOST_LISTEN_PORT (slirp
///     NATs that back to the host); we accept and exchange bytes.
fn qemu_smoke_net_userspace() -> anyhow::Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::time::{Duration, Instant};

    let root = workspace_root();

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

    let serial_log = root.join("target/qemu-smoke-net-userspace-serial.log");
    let _ = std::fs::remove_file(&serial_log);

    const HOST_CONSOLE_PORT: u16 = 5557;
    const HOST_USER_PORT: u16 = 5558;
    const USER_LISTEN_PORT: u16 = 7777;
    const HOST_LISTEN_PORT: u16 = 5559;

    // Start the host listener BEFORE QEMU so the guest's connect()
    // finds it immediately when fired.
    let host_listener = TcpListener::bind(("127.0.0.1", HOST_LISTEN_PORT))?;
    host_listener.set_nonblocking(true)?;

    println!();
    println!("Launching QEMU userspace-net smoke test (timeout: 60s)...");
    println!("  serial log: {}", serial_log.display());
    println!("  hostfwd console: 127.0.0.1:{HOST_CONSOLE_PORT} -> guest :2323");
    println!("  hostfwd user:    127.0.0.1:{HOST_USER_PORT} -> guest :{USER_LISTEN_PORT}");
    println!("  outbound:        host listener on 127.0.0.1:{HOST_LISTEN_PORT} (guest sees 10.0.2.2:{HOST_LISTEN_PORT})");
    println!();

    let qemu_args = vec![
        "-machine".to_string(), "virt,gic-version=3".to_string(),
        "-cpu".to_string(), "neoverse-n1".to_string(),
        "-m".to_string(), "2G".to_string(),
        "-display".to_string(), "none".to_string(),
        "-device".to_string(), "ramfb".to_string(),
        "-chardev".to_string(),
        format!("file,id=c0,path={}", serial_log.display()),
        "-serial".to_string(), "chardev:c0".to_string(),
        "-netdev".to_string(),
        format!(
            "user,id=net0,hostfwd=tcp:127.0.0.1:{HOST_CONSOLE_PORT}-:2323,hostfwd=tcp:127.0.0.1:{HOST_USER_PORT}-:{USER_LISTEN_PORT}"
        ),
        "-device".to_string(), "virtio-net-device,netdev=net0".to_string(),
        "-kernel".to_string(), kernel.to_str().expect("non-UTF8 path").to_string(),
    ];

    let mut child = Command::new("qemu-system-aarch64")
        .args(&qemu_args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let result = (|| -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_secs(60);

        // 1. DHCP.
        wait_for_marker(&serial_log, "virtio-net: IP=", deadline)?;
        println!("  [ok] DHCP bound");

        // 2. Open the console.
        let mut console = retry_connect(HOST_CONSOLE_PORT, deadline)?;
        console.set_read_timeout(Some(Duration::from_millis(200)))?;
        console.set_write_timeout(Some(Duration::from_secs(2)))?;
        // The shell takes a moment to finish booting and emit the
        // first prompt. drain_until_then_quiet keeps reading until it
        // sees "bsh>" (a stable end-of-boot marker), then waits a brief
        // quiet period for the post-prompt motd to land.
        let initial = drain_until(&mut console, "bsh>", Duration::from_secs(15));
        if !initial.contains("bsh>") {
            anyhow::bail!("never saw the shell prompt: {initial:?}");
        }
        // Soak any remaining straggler bytes so subsequent reads only
        // contain the post-command output.
        let _ = drain_quiet(&mut console, Duration::from_millis(300), Duration::from_secs(2));
        println!("  [ok] console connected, shell prompt visible");

        // 3. nettest-listen: ask the shell to start a userspace
        // listener on USER_LISTEN_PORT.
        let cmd = format!("nettest-listen {USER_LISTEN_PORT}\n");
        println!("  [..] writing {} bytes: {cmd:?}", cmd.len());
        console.write_all(cmd.as_bytes())?;
        console.flush()?;
        let reply = drain_until(
            &mut console,
            &format!("listening on port {USER_LISTEN_PORT}"),
            Duration::from_secs(5),
        );
        anyhow::ensure!(
            reply.contains(&format!("listening on port {USER_LISTEN_PORT}")),
            "shell never reported listen, got: {reply:?}"
        );
        println!("  [ok] shell opened userspace listener");

        // 4. Connect from host to the guest's userspace listener and
        // exchange a byte pattern.
        let mut user = retry_connect(HOST_USER_PORT, Instant::now() + Duration::from_secs(10))?;
        user.set_read_timeout(Some(Duration::from_millis(200)))?;
        user.set_write_timeout(Some(Duration::from_secs(2)))?;
        const PATTERN: &[u8] = b"abc123\n";
        user.write_all(PATTERN)?;
        let echoed = drain_quiet(&mut user, Duration::from_millis(500), Duration::from_secs(5));
        anyhow::ensure!(
            echoed.as_bytes().contains(&b'a') && echoed.contains("abc"),
            "userspace listener didn't echo our bytes, got: {echoed:?}"
        );
        println!("  [ok] userspace listener echoed: {echoed:?}");
        // Close — that triggers the shell's loop to wrap up via FIN.
        drop(user);

        // Wait for the shell prompt to come back.
        let _ = drain_quiet(&mut console, Duration::from_millis(800), Duration::from_secs(8));
        println!("  [ok] passive-open path verified");

        // 5. nettest-connect: shell opens an outbound socket to the
        // slirp gateway (10.0.2.2) on HOST_LISTEN_PORT. The host
        // listener (started before QEMU) accepts it.
        console.write_all(
            format!("nettest-connect 10.0.2.2 {HOST_LISTEN_PORT}\n").as_bytes()
        )?;

        // Accept the inbound connection on the host listener.
        let accept_deadline = Instant::now() + Duration::from_secs(15);
        let (mut server, _peer) = loop {
            match host_listener.accept() {
                Ok(s) => break s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() >= accept_deadline {
                        anyhow::bail!("host listener never received connection from guest");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(e) => anyhow::bail!("host accept failed: {e}"),
            }
        };
        println!("  [ok] guest connected to host listener");
        server.set_read_timeout(Some(Duration::from_millis(500)))?;
        server.set_write_timeout(Some(Duration::from_secs(2)))?;

        // Read what the guest sent.
        let mut greeting = [0u8; 64];
        let n = server.read(&mut greeting).unwrap_or(0);
        let greeting_str = String::from_utf8_lossy(&greeting[..n]).to_string();
        anyhow::ensure!(
            greeting_str.contains("hi from BeetOS"),
            "guest greeting missing, got: {greeting_str:?}"
        );
        println!("  [ok] guest sent: {greeting_str:?}");

        // Reply, then close.
        server.write_all(b"hello back\n")?;
        drop(server);

        // The shell's nettest-connect prints `recv: hello back` then
        // closes — should appear on the console socket.
        let reply = drain_until(&mut console, "recv: hello back", Duration::from_secs(8));
        anyhow::ensure!(
            reply.contains("recv: hello back"),
            "shell didn't print recv from host, got: {reply:?}"
        );
        println!("  [ok] active-open path verified");

        Ok(())
    })();

    let _ = child.kill();
    let _ = child.wait();

    println!();
    match result {
        Ok(()) => {
            println!("Result: NET USERSPACE SMOKE TEST PASSED");
            Ok(())
        }
        Err(e) => anyhow::bail!("Result: NET USERSPACE SMOKE TEST FAILED — {e}"),
    }
}

/// Kernel performance gate: boot QEMU with `-icount shift=0,sleep=off`
/// (virtual time advances with the *instruction count*, so in-guest
/// CNTVCT measurements are deterministic across runs and host
/// machines), drive the shell's `bench` command over the TCP console,
/// and compare each result against `xtask/qemu-bench-baseline.txt`.
///
/// `--update` rewrites the baseline; `--tolerance PCT` overrides the
/// default ±30% regression threshold. The `cpu_mix` entry is a pure-
/// CPU calibration point: if it moves, the measurement environment
/// changed (QEMU version, icount config) — not the kernel.
fn qemu_bench(args: &[String]) -> anyhow::Result<()> {
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::time::{Duration, Instant};

    let mut update = false;
    let mut tolerance = 0.30f64;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--update" => update = true,
            "--tolerance" => {
                i += 1;
                tolerance = args
                    .get(i)
                    .and_then(|s| s.parse::<f64>().ok())
                    .map(|p| p / 100.0)
                    .ok_or_else(|| anyhow::anyhow!("--tolerance needs a percentage"))?;
            }
            other => anyhow::bail!("unknown qemu-bench arg: {other}"),
        }
        i += 1;
    }

    let root = workspace_root();

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

    let serial_log = root.join("target/qemu-bench-serial.log");
    let _ = std::fs::remove_file(&serial_log);

    const HOST_CONSOLE_PORT: u16 = 5561;

    println!();
    println!("Launching QEMU kernel benchmarks (icount: deterministic virtual time)...");
    println!("  serial log: {}", serial_log.display());
    println!();

    let qemu_args = vec![
        "-machine".to_string(), "virt,gic-version=3".to_string(),
        "-cpu".to_string(), "neoverse-n1".to_string(),
        "-m".to_string(), "2G".to_string(),
        // Headless on purpose — and *no ramfb*: with a framebuffer
        // present, the kernel recomposes the boot desktop at 10 Hz
        // inside the timer IRQ (~90 ms of virtual time per frame in a
        // debug build!), which lands in measurement windows phase-
        // dependently and inflated affected benchmarks by up to 12×
        // (yield read 43 µs/op when its true cost is 3.5 µs). Without
        // ramfb, fb::is_fb_ready() is false and the whole GUI pipeline
        // is skipped — benches measure the kernel, not the rasterizer.
        "-display".to_string(), "none".to_string(),
        // 1 instruction = 1 virtual ns (shift=0). sleep=off lets idle
        // WFI warp virtual time to the next timer deadline so boot
        // doesn't take minutes of wall time.
        "-icount".to_string(), "shift=0,sleep=off".to_string(),
        "-chardev".to_string(),
        format!("file,id=c0,path={}", serial_log.display()),
        "-serial".to_string(), "chardev:c0".to_string(),
        "-netdev".to_string(),
        format!("user,id=net0,hostfwd=tcp:127.0.0.1:{HOST_CONSOLE_PORT}-:2323"),
        "-device".to_string(), "virtio-net-device,netdev=net0".to_string(),
        "-kernel".to_string(), kernel.to_str().expect("non-UTF8 path").to_string(),
    ];

    let mut child = Command::new("qemu-system-aarch64")
        .args(&qemu_args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let result = (|| -> anyhow::Result<Vec<(String, u64)>> {
        let deadline = Instant::now() + Duration::from_secs(180);

        wait_for_marker(&serial_log, "virtio-net: IP=", deadline)?;

        let mut console = retry_connect(HOST_CONSOLE_PORT, deadline)?;
        console.set_read_timeout(Some(Duration::from_millis(200)))?;
        console.set_write_timeout(Some(Duration::from_secs(2)))?;
        let initial = drain_until(&mut console, "bsh>", Duration::from_secs(90));
        anyhow::ensure!(initial.contains("bsh>"), "never saw the shell prompt: {initial:?}");
        let _ = drain_quiet(&mut console, Duration::from_millis(300), Duration::from_secs(2));

        console.write_all(b"bench\n")?;
        console.flush()?;
        // icount trades wall-clock speed for determinism; the suite is
        // sized to finish well inside this window.
        let out = drain_until(&mut console, "[bench] done", Duration::from_secs(150));
        anyhow::ensure!(out.contains("[bench] done"), "bench never completed, got: {out:?}");

        let mut results = Vec::new();
        for line in out.lines() {
            // "[bench] syscall_null 1042 ns/op n=10000"
            let Some(rest) = line.trim().strip_prefix("[bench] ") else { continue };
            let mut parts = rest.split_whitespace();
            let (Some(name), Some(ns), Some(unit)) = (parts.next(), parts.next(), parts.next())
            else {
                continue;
            };
            if unit != "ns/op" {
                continue;
            }
            if let Ok(ns) = ns.parse::<u64>() {
                results.push((name.to_string(), ns));
            }
        }
        anyhow::ensure!(!results.is_empty(), "no benchmark lines parsed from: {out:?}");
        Ok(results)
    })();

    let _ = child.kill();
    let _ = child.wait();

    let results = result?;

    let baseline_path = root.join("xtask/qemu-bench-baseline.txt");
    let baseline: BTreeMap<String, u64> = std::fs::read_to_string(&baseline_path)
        .ok()
        .map(|text| {
            text.lines()
                .filter_map(|l| {
                    let l = l.trim();
                    if l.is_empty() || l.starts_with('#') {
                        return None;
                    }
                    let mut parts = l.split_whitespace();
                    Some((parts.next()?.to_string(), parts.next()?.parse::<u64>().ok()?))
                })
                .collect()
        })
        .unwrap_or_default();

    if update || baseline.is_empty() {
        let mut text = String::from(
            "# Kernel micro-benchmark baseline (ns/op under QEMU -icount shift=0).\n\
             # Deterministic: 1 ns = 1 instruction. Regenerate with\n\
             #   cargo xtask qemu-bench --update\n",
        );
        println!("  {:<14} {:>12}", "benchmark", "ns/op");
        for (name, ns) in &results {
            println!("  {name:<14} {ns:>12}");
            text.push_str(&format!("{name} {ns}\n"));
        }
        std::fs::write(&baseline_path, text)?;
        println!();
        println!("Result: BASELINE WRITTEN to {}", baseline_path.display());
        return Ok(());
    }

    println!("  {:<14} {:>12} {:>12} {:>9}", "benchmark", "baseline", "now", "delta");
    let mut regressions = Vec::new();
    let mut improvements = Vec::new();
    for (name, now) in &results {
        match baseline.get(name) {
            Some(&base) => {
                let delta = (*now as f64 - base as f64) / base as f64;
                println!(
                    "  {name:<14} {base:>12} {now:>12} {:>+8.1}%",
                    delta * 100.0
                );
                if delta > tolerance {
                    regressions.push(format!("{name}: {base} → {now} ns/op ({:+.1}%)", delta * 100.0));
                } else if delta < -tolerance {
                    improvements.push(name.clone());
                }
            }
            None => {
                println!("  {name:<14} {:>12} {now:>12}       new", "-");
                regressions.push(format!(
                    "{name}: not in baseline — run `cargo xtask qemu-bench --update`"
                ));
            }
        }
    }
    for name in baseline.keys() {
        if !results.iter().any(|(n, _)| n == name) {
            regressions.push(format!(
                "{name}: in baseline but not reported — run `cargo xtask qemu-bench --update`"
            ));
        }
    }

    println!();
    if !improvements.is_empty() {
        println!(
            "Note: {} improved beyond tolerance — consider refreshing the baseline (--update).",
            improvements.join(", ")
        );
    }
    if regressions.is_empty() {
        println!("Result: BENCH PASSED (tolerance ±{:.0}%)", tolerance * 100.0);
        Ok(())
    } else {
        for r in &regressions {
            println!("  REGRESSION: {r}");
        }
        anyhow::bail!("Result: BENCH FAILED — {} regression(s)", regressions.len());
    }
}

fn retry_connect(port: u16, deadline: std::time::Instant) -> anyhow::Result<std::net::TcpStream> {
    use std::time::Duration;
    let mut last = String::new();
    while std::time::Instant::now() < deadline {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(s) => return Ok(s),
            Err(e) => {
                last = e.to_string();
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
    anyhow::bail!("could not connect to 127.0.0.1:{port}: {last}")
}

fn drain_quiet(
    stream: &mut std::net::TcpStream,
    quiet: std::time::Duration,
    overall: std::time::Duration,
) -> String {
    use std::io::Read;
    use std::time::Instant;
    let mut buf = [0u8; 1024];
    let mut acc = String::new();
    let deadline = Instant::now() + overall;
    let mut last_byte_at = Instant::now();
    while Instant::now() < deadline {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                last_byte_at = Instant::now();
            }
            Err(_) => {
                if last_byte_at.elapsed() >= quiet {
                    break;
                }
            }
        }
    }
    acc
}

fn drain_until(
    stream: &mut std::net::TcpStream,
    needle: &str,
    overall: std::time::Duration,
) -> String {
    use std::io::Read;
    use std::time::Instant;
    let mut buf = [0u8; 1024];
    let mut acc = String::new();
    let deadline = Instant::now() + overall;
    while Instant::now() < deadline {
        if acc.contains(needle) {
            break;
        }
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => acc.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(_) => continue,
        }
    }
    acc
}

/// One full remote-console exchange: connect, read the banner, then
/// issue `ip` and `ping` and assert the replies. Returns Ok only if
/// every expected token is seen.
fn try_console_session(host_port: u16) -> anyhow::Result<()> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let mut stream = TcpStream::connect(("127.0.0.1", host_port))?;
    // Short per-read timeout so we can poll for quiescence. Bytes
    // arrive in bursts as the shell does IPC → kernel push → next ACK
    // segment, so we read until 500 ms goes by without new data or 5
    // s of total wall time.
    stream.set_read_timeout(Some(Duration::from_millis(200)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;

    fn drain(stream: &mut TcpStream) -> String {
        use std::time::Instant;
        let mut buf = [0u8; 1024];
        let mut acc = String::new();
        let overall_deadline = Instant::now() + Duration::from_secs(5);
        let mut quiet_since: Option<Instant> = None;
        while Instant::now() < overall_deadline {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                    quiet_since = None;
                }
                Err(_) => {
                    // Read timeout — start (or extend) the quiet timer.
                    let start = quiet_since.get_or_insert_with(Instant::now);
                    if start.elapsed() >= Duration::from_millis(500) {
                        break;
                    }
                }
            }
        }
        acc
    }

    // First read: the kernel banner sent on handshake completion, then
    // anything `os/console` has spooled from the shell since boot
    // (motd, prompt, …). Either order is fine; we only require the
    // banner is in there and the shell has reached a prompt-ish state.
    let initial = drain(&mut stream);
    anyhow::ensure!(
        initial.contains("BeetOS remote console"),
        "missing banner, got: {initial:?}"
    );
    println!("  [ok] banner: {:?}", initial.trim());
    anyhow::ensure!(
        initial.contains("bsh"),
        "no shell prompt in initial drain — shell didn't tap output? got: {initial:?}"
    );
    println!("  [ok] shell prompt visible over TCP");

    // Drive the real shell — `ifconfig` is the most distinctive
    // command (no false-positive match in the prompt/echo) and its
    // reply hits multiple integration points: the shell sends the
    // NetGetInfo syscall, formats the result, taps it through the
    // console output service, the kernel TCP module flushes it to the
    // socket. If we see "10.0.2.15" here, the whole pipe is live.
    stream.write_all(b"ifconfig\n")?;
    let reply = drain(&mut stream);
    anyhow::ensure!(
        reply.contains("10.0.2.15"),
        "ifconfig did not return the DHCP address, got: {reply:?}"
    );
    println!("  [ok] 'ifconfig' -> contains 10.0.2.15");

    // ICMP path: ping the slirp gateway. Exercises echo-request TX
    // (ARP-resolved next hop), slirp's reply, and the echo-reply
    // capture + NetPingPoll syscall. 2 packets keeps the smoke fast.
    stream.write_all(b"ping 10.0.2.2 2\n")?;
    let reply = drain(&mut stream);
    anyhow::ensure!(
        reply.contains("reply from 10.0.2.2"),
        "ping got no reply from the gateway, got: {reply:?}"
    );
    anyhow::ensure!(
        reply.contains("2 sent, 2 received"),
        "ping lost packets on a loopback link, got: {reply:?}"
    );
    println!("  [ok] 'ping 10.0.2.2' -> 2/2 replies");

    Ok(())
}

/// Poll a growing serial-log file until `marker` appears or the deadline
/// passes. Echoes new lines as they arrive for debugging.
fn wait_for_marker(
    serial_log: &std::path::Path,
    marker: &str,
    deadline: std::time::Instant,
) -> anyhow::Result<()> {
    use std::io::{BufRead, BufReader, Seek};

    let mut log_pos: u64 = 0;
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(200));
        let Ok(file) = std::fs::File::open(serial_log) else { continue };
        let len = file.metadata()?.len();
        if len <= log_pos {
            continue;
        }
        let mut reader = BufReader::new(file);
        reader.seek(std::io::SeekFrom::Start(log_pos))?;
        for line in reader.lines().flatten() {
            println!("  {line}");
            if line.contains(marker) {
                return Ok(());
            }
        }
        log_pos = len;
    }
    anyhow::bail!("timed out waiting for serial marker {marker:?}")
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
