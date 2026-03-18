# BeetOS — Implementation Plan

> _Rooted in Rust. Bare-metal to the root._

A secure, minimal OS for Apple Silicon MacBook Air M1.
Built on the Xous microkernel (cherry-picked from KeyOS), with all platform code rewritten for AArch64 / Apple Silicon. Microkernel architecture with all drivers in userspace. Early milestones run entirely from RAM (no disk needed).

**Target:** MacBook Air M1 (j313, Apple SoC T8103)
**Language:** 100% Rust — no_std + alloc (no custom toolchain needed). Full std via custom Rust toolchain is a future milestone.
**Rust target:** `aarch64-unknown-none` (standard, no fork required)
**License:** MIT OR Apache-2.0 (Xous kernel code) + GPL-3.0 (KeyOS-derived code) — check per-file

---

## Strategy: Cherry-Pick from KeyOS

KeyOS (by Foundation Devices) is an ARM port of the Xous microkernel, built for the Passport hardware wallet (SAMA5D28 / Cortex-A5). We cherry-pick the platform-agnostic kernel core and rewrite only the hardware layer for AArch64 / Apple M1.

### What we COPY from KeyOS (platform-agnostic, ~7500 LOC):

**Kernel core** (~4500 LOC):

- `xous/kernel/src/syscall.rs` (757 lines) — syscall dispatch table
- `xous/kernel/src/services.rs` (1048 lines) — built-in services
- `xous/kernel/src/server.rs` (882 lines) — server registry
- `xous/kernel/src/mem.rs` (900 lines) — memory manager (needs 32→64-bit adaptation)
- `xous/kernel/src/process.rs` (379 lines) — process abstraction
- `xous/kernel/src/scheduler.rs` (244 lines) — scheduler
- `xous/kernel/src/irq.rs` (149 lines) — IRQ routing to userspace
- `xous/kernel/src/main.rs` (95 lines) — kernel entry
- `xous/kernel/src/args.rs`, `io.rs`, `macros.rs` — support code

**xous-rs** (~3000 LOC) — userspace syscall library:

- `xous/xous-rs/src/syscall.rs` (2027 lines) — safe syscall wrappers
- `xous/xous-rs/src/definitions.rs` + submodules — message types, flags
- `xous/xous-rs/src/lib.rs`, `carton.rs`, `string.rs`, `stringbuffer.rs`, `process.rs`

**IPC, core service APIs, core services, loader.**

### What we REWRITE (~5000 LOC):

- `kernel/src/arch/aarch64/` — replacing `arch/arm/` (3198 lines)
- `kernel/src/platform/apple_t8103/` — replacing `platform/atsama5d2/` (1130 lines)
- `xous-rs/src/arch/aarch64/` — replacing `arch/arm/` (670 lines)
- All `os/` services — Apple Silicon drivers from scratch

### What we DELETE from KeyOS:

- `arch/arm/`, `platform/atsama5d2/`
- `cryptoauthlib/`, `imports/atsama5d27/`
- `slint-keyos-platform/`, all `apps/gui-app-*`, `ui/`
- `boot/at91bootstrap/`, `boot/keyos-boot/`
- All `os/` services (wrong hardware)

---

## Repository Structure

```
beetos/
├── Cargo.toml
├── CLAUDE.md
├── rust-toolchain.toml
├── .cargo/config.toml
│
├── xous/                       ← cherry-picked from KeyOS/xous/
│   ├── kernel/
│   │   └── src/
│   │       ├── main.rs, syscall.rs, server.rs    ← COPIED
│   │       ├── services.rs, mem.rs, process.rs   ← COPIED (mem.rs: adapt 32→64)
│   │       ├── scheduler.rs, irq.rs, args.rs     ← COPIED
│   │       ├── arch/
│   │       │   ├── aarch64/    ← NEW (replacing arm/)
│   │       │   └── hosted/     ← COPIED (for host testing)
│   │       └── platform/
│   │           └── apple_t8103/ ← NEW (replacing atsama5d2/)
│   ├── xous-rs/                ← COPIED + new arch/aarch64/
│   ├── ipc/                    ← COPIED as-is
│   ├── api/{log,names,ticktimer}/ ← COPIED as-is
│   ├── log/                    ← COPIED, adapt output
│   ├── names/                  ← COPIED as-is
│   ├── ticktimer/              ← COPIED, rewrite platform/
│   └── trng/                   ← COPIED, rewrite platform/
│
├── beetos/                     ← constants crate (~KeyOS keyos/)
│   └── src/lib.rs              ← memory map, PAGE_SIZE=16384, addresses
│
├── api/{console,keyboard,storage,net}/  ← BeetOS service APIs
├── os/{console,keyboard,nvme,dart,wifi,usb}/  ← BeetOS drivers
├── apps/shell/                 ← bsh (no_std + alloc, full std later)
├── loader/                     ← COPIED from KeyOS, adapted for m1n1
├── boot/m1n1/                  ← git submodule → AsahiLinux/m1n1
├── xtask/                      ← build system
├── dts/apple-j313.dts          ← vendored from Asahi
├── test-apps/
└── .claude/plan.md
```

---

## Milestone 0 — Cherry-Pick & Workspace Setup

**Goal:** Copy all Xous platform-agnostic code from KeyOS. Workspace compiles in hosted mode. No hardware.

### Tasks

- [ ] Clone sources: `git clone https://github.com/Foundation-Devices/KeyOS.git /tmp/KeyOS`
- [ ] Clone sources: `git clone https://github.com/betrusted-io/xous-core.git /tmp/xous-core`
- [ ] Create beetos repo structure
- [ ] **Copy kernel core files:**
  - [ ] `cp /tmp/KeyOS/xous/kernel/src/{main,syscall,server,services,mem,process,scheduler,irq,args,io,macros,test}.rs → xous/kernel/src/`
  - [ ] `cp /tmp/KeyOS/xous/kernel/Cargo.toml → xous/kernel/Cargo.toml`
  - [ ] `cp /tmp/KeyOS/xous/kernel/link.x → xous/kernel/link.x`
- [ ] **Copy hosted arch** (for host testing):
  - [ ] `cp -r /tmp/KeyOS/xous/kernel/src/arch/hosted/ → xous/kernel/src/arch/hosted/`
  - [ ] `cp /tmp/KeyOS/xous/kernel/src/arch/mod.rs → xous/kernel/src/arch/mod.rs`
- [ ] **Copy debug module:**
  - [ ] `cp -r /tmp/KeyOS/xous/kernel/src/debug/ → xous/kernel/src/debug/`
- [ ] **Copy xous-rs whole directory, remove arch/arm/:**
  - [ ] `cp -r /tmp/KeyOS/xous/xous-rs/ → xous/xous-rs/`
  - [ ] `rm -rf xous/xous-rs/src/arch/arm/`
- [ ] **Copy xous-ipc:**
  - [ ] `cp -r /tmp/KeyOS/xous/ipc/ → xous/ipc/`
- [ ] **Copy core service APIs:**
  - [ ] `cp -r /tmp/KeyOS/xous/api/ → xous/api/`
- [ ] **Copy core services:**
  - [ ] `cp -r /tmp/KeyOS/xous/{log,names,ticktimer,trng}/ → xous/`
- [ ] **Copy loader:**
  - [ ] `cp -r /tmp/KeyOS/loader/ → loader/`
- [ ] **Create beetos/ constants crate** inspired by `KeyOS/keyos/src/lib.rs`:
  - [ ] `PAGE_SIZE: usize = 16384` (Apple Silicon 16KB pages!)
  - [ ] AArch64 memory map constants (ASLR range, kernel load offset, stack addresses)
  - [ ] Audit ALL copied files for hardcoded `4096` / `0x1000` and replace with `beetos::PAGE_SIZE`
- [ ] **Create stub `arch/aarch64/mod.rs`** with empty impls of the arch trait (enough to compile)
- [ ] **Create stub `platform/apple_t8103/mod.rs`** with empty impls
- [ ] **Create workspace Cargo.toml** referencing all crates
- [ ] **Fix all references:** `keyos::` → `beetos::`, `cfg(keyos)` → `cfg(beetos)`, remove `atsama5d2` imports
- [ ] **Create xtask/** with basic `check` command
- [ ] `cargo check` compiles in hosted mode

### Tests

- [ ] `cargo check` succeeds (hosted mode, no cross-compile)
- [ ] `cargo test` runs Xous kernel tests in hosted mode
- [ ] `grep -r "atsama5d2\|sama5\|keyos::" xous/` returns zero matches

### Definition of Done

Platform-agnostic Xous code lives in beetos repo. Compiles in hosted mode. Tests pass. Zero KeyOS hardware references remain.

---

## Milestone 1 — AArch64 Arch Port

**Goal:** Implement `arch/aarch64/`. Kernel cross-compiles for `aarch64-unknown-none`.

No custom Rust toolchain needed — we use the standard `aarch64-unknown-none` target with `no_std` + `alloc`.

### Tasks

- [ ] **asm.S**: Exception vectors, context save/restore, svc entry, idle (wfe)
- [ ] **mem.rs**: 4-level page tables, 16KB granule, TTBR0/TTBR1 split, MAIR
- [ ] **process.rs**: Context switch, ASID management (up to 65536), eret to EL0
- [ ] **irq.rs**: Apple AIC interrupt dispatch
- [ ] **elf.rs**: ELF64 loader
- [ ] **panic.rs**, **backtrace.rs**, **mod.rs**
- [ ] Adapt `xous-rs/src/arch/aarch64/`: svc wrappers, thread primitives
- [ ] Fix `mem.rs` (kernel) for 64-bit pointers
- [ ] `cargo build --target aarch64-unknown-none -p xous-kernel` compiles

### Tests

- [ ] Unit test: page table entries, 4-level walk, ASID allocator, ELF64 parser
- [ ] Hosted mode tests still pass

### Definition of Done

Kernel cross-compiles for `aarch64-unknown-none`. All arch functions implemented. No hardware boot yet.

---

## Milestone 2 — Platform Port & First Boot

**Goal:** `platform/apple_t8103/`, boot via m1n1, "BeetOS" on screen.

### Tasks

- [ ] FDT parsing (RAM, framebuffer, AIC base)
- [ ] Apple AIC driver (reference: Asahi `irq-apple-aic.c`)
- [ ] ARM generic timer
- [ ] Framebuffer console (SimpleFB from m1n1)
- [ ] Adapt loader for m1n1 payload format
- [ ] `cargo xtask image` + `cargo xtask run`
- [ ] Boot on real MBA M1

### Tests

- [ ] Screen shows "BeetOS v0.1.0"
- [ ] Timer ticks
- [ ] Name server and ticktimer server running

### Definition of Done

BeetOS boots on real hardware. Xous microkernel operational.

---

## Milestone 3 — Keyboard, Shell & RAM Filesystem

**Goal:** Type on keyboard, interact with shell, read/write files in memory. First Xous userspace services.

All services and apps use `no_std` + `alloc` — gives us `Vec`, `String`, `Box`, `BTreeMap`. No custom toolchain needed. Services communicate via Xous IPC.

### Tasks

- [ ] `api/keyboard/` + `os/keyboard/` (Apple SPI HID)
- [ ] `api/console/` + `os/console/` (framebuffer server)
- [ ] `api/storage/` + `os/ramfs/` — RAM filesystem service:
  - [ ] `BTreeMap<String, Vec<u8>>` as backing store
  - [ ] Operations: create, read, write, delete, list directory
  - [ ] Hierarchical paths (`/tmp/foo/bar.txt`)
  - [ ] Per-process namespace possible (future: isolation)
  - [ ] Everything lost on reboot (by design — NVMe persistence comes in M4)
- [ ] `apps/shell/` — bsh, using `alloc` collections (`Vec`, `String`, `BTreeMap` for command dispatch)
- [ ] Built-ins: help, echo, info, mem, reboot
- [ ] File commands: `write <path> <content>`, `cat <path>`, `ls [path]`, `rm <path>`, `mkdir <path>`

### Definition of Done

Interactive shell with in-memory filesystem. You can create, read, list, and delete files. Multiple Xous services communicating via IPC. This is a real OS.

---

## Milestone 4 — NVMe Storage

**Goal:** Read the built-in SSD. Mount read-only filesystem. Verified boot.

### Tasks

- [ ] `os/dart/` — Apple DART (IOMMU) driver
- [ ] `os/nvme/` — Apple ANS NVMe driver (reference: Asahi `nvme-apple.c`)
- [ ] `api/storage/` — block storage API
- [ ] Read-only filesystem (tar or simple custom format)
- [ ] Verified boot: ed25519 signature check on rootfs image
- [ ] Shell commands: `ls`, `cat`

### Definition of Done

Kernel reads the SSD. Rootfs mounted read-only with signature verification.

---

## Milestone 5 — Network

**Goal:** TCP/IP via USB-C Ethernet. Remote shell access.

### Tasks

- [ ] `os/usb/` — xHCI USB-C driver (minimal, for Ethernet dongle)
- [ ] `api/net/` — network API
- [ ] smoltcp integration as Xous service
- [ ] SSH or raw TCP shell
- [ ] Shell commands: `ifconfig`, `ping`

---

## Milestone 6 — Encrypted Storage & WiFi

- [ ] NVMe write support
- [ ] Encrypted data partition (AES-256-GCM)
- [ ] `os/wifi/` — Broadcom BCM4378 in sandboxed process
- [ ] Shell commands: `wifi scan`, `wifi connect`

---

## Milestone 7 — Full std Support (optional, when needed)

**Goal:** Fork the Rust compiler to add `aarch64-unknown-xous-elf` target. Services can use full `std`.

This milestone is deferred until we actually need `std` features that `alloc` doesn't provide (e.g. `std::net::TcpStream`, `std::thread::spawn`, `std::fs`). Until then, `no_std` + `alloc` covers 90% of needs.

### Tasks

- [ ] Fork `Foundation-Devices/rust-keyos` → `beetos/rust` (separate repo)
- [ ] Study how KeyOS added `armv7a-unknown-xous-elf` target
- [ ] Add `aarch64-unknown-xous-elf` target spec (base on `aarch64-unknown-none`, set `"os": "xous"`)
- [ ] Adapt libstd Xous backend for AArch64 syscall ABI + 64-bit pointers
- [ ] Build libstd rlibs: `./x.py build --target aarch64-unknown-xous-elf library/std`
- [ ] Package + publish as GitHub Release
- [ ] `cargo xtask install-toolchain` downloads and installs the rlibs into sysroot
- [ ] Switch services from `#![no_std] extern crate alloc;` to normal `std`

### What this unlocks

- `std::collections::HashMap` (we already have `BTreeMap` via `alloc`, but `HashMap` needs `std`)
- `std::thread::spawn` (kernel-backed threads via Xous syscalls)
- `std::net::TcpStream` (kernel-backed networking via Xous syscalls)
- `std::fs::File` (kernel-backed filesystem via Xous syscalls)
- `std::time::Instant` (kernel-backed timer via Xous syscalls)

### Rust fork chain

```
rust-lang/rust
  → xous-os/rust                     (adds Xous RISC-V target)
    → Foundation-Devices/rust-keyos  (adds Xous ARM target)
      → beetos/rust                  (adds Xous AArch64 target)
```

`beetos/rust` is a **separate repo** (it's a fork of the entire Rust compiler). The BeetOS monorepo only needs the pre-built rlibs, installed via `cargo xtask install-toolchain`.

### Maintenance

Pinned to a specific nightly. Bump when needed (not at every Rust release). The rebase is mechanical — Claude Code can handle it.

### Definition of Done

`rustc --target aarch64-unknown-xous-elf` compiles std Rust. Services use `use std::*`.

---

## Future Milestones

- M8: Trackpad, DCP (display coprocessor)
- M9: GPU (AGX) — reference Asahi Lina's Rust DRM driver
- M10: Desktop environment (Wayland compositor or COSMIC port)
- M11: A/B updates, OTA distribution
- M12: M2/M3/M4 support (new platform/ modules + device trees)

---

## Key Difference: 16KB Pages

Apple Silicon uses 16KB pages. Xous/KeyOS assume 4KB. This is the most pervasive change across the codebase. Every `4096`, `0x1000`, `PAGE_SHIFT = 12` must be audited and fixed.

---

## Licensing Note

Audit each cherry-picked file. Xous kernel = MIT OR Apache-2.0. Some KeyOS modifications = GPL-3.0-or-later. BeetOS new code = MIT OR Apache-2.0. GPL files need careful handling.
