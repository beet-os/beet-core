# BeetOS — Implementation Plan

> _Rooted in Rust. Bare-metal to the root._

A secure, minimal OS for AArch64, built on the Xous microkernel (cherry-picked from KeyOS). Multi-platform: runs on QEMU virt, Apple Silicon, and any AArch64 board. Microkernel architecture with all drivers in userspace. Early milestones run entirely from RAM (no disk needed).

**Primary target:** QEMU `virt` machine (development & CI), then MacBook Air M1 (j313, Apple SoC T8103)
**Language:** 100% Rust — no_std + alloc (no custom toolchain needed). Full std via custom Rust toolchain is a future milestone.
**Rust target:** `aarch64-unknown-none` (standard, no fork required)
**License:** MIT OR Apache-2.0 (Xous kernel code) + GPL-3.0 (KeyOS-derived code) — check per-file

---

## Strategy: Cherry-Pick from KeyOS

KeyOS (by Foundation Devices) is an ARM port of the Xous microkernel, built for the Passport hardware wallet (SAMA5D28 / Cortex-A5). We cherry-pick the platform-agnostic kernel core and rewrite only the hardware layer for AArch64.

### Multi-Platform Strategy

The `arch/aarch64/` code is **generic AArch64** — page tables, exception vectors, context switch, ASID, eret. It's the same ISA on Apple M1, Ampere Altra, Raspberry Pi 4, AWS Graviton, QEMU virt. Zero hardware-specific code belongs in `arch/`.

All hardware-specific code lives in `platform/`. Adding a new platform = new platform module, no kernel rewrite:

```
xous/kernel/src/platform/
├── qemu_virt/       ← QEMU virt machine (GIC, PL011 UART, virtio) — FIRST target
├── apple_t8103/     ← Apple M1 (AIC, m1n1, SPI keyboard, ANS NVMe) — SECOND target
└── rpi4/            ← Raspberry Pi 4 (future)
```

QEMU virt is the **first hardware platform** because:
- Any contributor can test AArch64 code without owning a Mac
- QEMU virt has well-documented, standard hardware (GIC, PL011 UART, virtio) — much simpler than Apple's custom controllers
- Faster iteration than m1n1 USB proxy
- `cargo xtask qemu` is the dream command for CI and contributors
- The Apple M1 platform becomes the second target, after QEMU proves the arch layer works

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
- `kernel/src/platform/qemu_virt/` — QEMU virt machine (GIC, PL011, virtio)
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
│   │           ├── qemu_virt/   ← NEW (QEMU virt: GIC, PL011, virtio)
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
- [ ] **irq.rs**: Generic IRQ dispatch (platform provides the interrupt controller)
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

## Milestone 2 — QEMU virt Platform & First Boot

**Goal:** `platform/qemu_virt/`, boot on QEMU `virt` machine, "BeetOS v0.1.0" on UART.

QEMU virt is the first hardware platform — standard, well-documented, and anyone can run it.

### Tasks

- [ ] **platform/qemu_virt/mod.rs**: Platform init, FDT parsing (RAM base/size, GIC base, UART base, timer IRQ)
- [ ] **platform/qemu_virt/gic.rs**: ARM GICv3 interrupt controller driver
  - [ ] Distributor (GICD) init: enable, configure SPIs
  - [ ] Redistributor (GICR) init: wake, configure PPIs/SGIs
  - [ ] CPU interface (ICC system registers): enable, set PMR, acknowledge/EOI
  - [ ] IRQ enable/disable/claim/complete for kernel IRQ dispatch
- [ ] **platform/qemu_virt/uart.rs**: PL011 UART driver (reference: ARM PL011 TRM)
  - [ ] Polled output for early boot (`putc`, `puts`)
  - [ ] IRQ-driven receive (for later shell input)
  - [ ] Integrate as `log` backend
- [ ] **platform/qemu_virt/timer.rs**: ARM generic timer (CNTP, EL1 physical timer)
  - [ ] Read CNTFRQ_EL0 for frequency
  - [ ] Set CNTP_TVAL_EL0 for periodic tick
  - [ ] Timer IRQ handler (PPI 30)
- [ ] **Adapt loader** for flat binary / ELF load (no m1n1 payload format needed)
- [ ] **cargo xtask qemu**: Launch QEMU with correct args:
  - [ ] `-machine virt -cpu cortex-a72 -m 512M -nographic`
  - [ ] `-kernel` pointing to the built kernel binary
  - [ ] UART output to terminal stdout
- [ ] **Linker script** for QEMU virt memory layout (RAM at 0x4000_0000)

### Tests

- [ ] UART shows "BeetOS v0.1.0" in QEMU terminal
- [ ] Timer ticks (visible via log output)
- [ ] GIC handles timer IRQ correctly
- [ ] Name server and ticktimer server running
- [ ] `cargo xtask qemu` works end-to-end

### Definition of Done

BeetOS boots on QEMU `virt`. Xous microkernel operational. Any developer can run `cargo xtask qemu` — no hardware needed.

---

## Milestone 3 — Apple M1 Platform & Hardware Boot

**Goal:** `platform/apple_t8103/`, boot via m1n1, "BeetOS v0.1.0" on screen. The `arch/aarch64/` layer is already proven on QEMU — this milestone only adds the Apple-specific platform code.

### Tasks

- [ ] **platform/apple_t8103/mod.rs**: Platform init, FDT parsing (RAM, framebuffer, AIC base from m1n1-provided FDT)
- [ ] **platform/apple_t8103/aic.rs**: Apple AIC interrupt controller driver (reference: Asahi `irq-apple-aic.c`)
  - [ ] AICv2 event register layout
  - [ ] IRQ mask/unmask/ack
  - [ ] IPI support (for future SMP)
- [ ] **platform/apple_t8103/timer.rs**: ARM generic timer (same ISA as QEMU, but AIC routes the IRQ differently)
- [ ] **platform/apple_t8103/fb.rs**: Framebuffer console (SimpleFB from m1n1)
  - [ ] Parse framebuffer address/stride/format from FDT
  - [ ] Simple font rendering for boot console
- [ ] **Adapt loader** for m1n1 payload format
- [ ] **cargo xtask image**: Build m1n1 + loader + kernel + services payload
- [ ] **cargo xtask run**: Push to MBA M1 via m1n1 USB proxy
- [ ] Boot on real MBA M1

### Tests

- [ ] Screen shows "BeetOS v0.1.0"
- [ ] Timer ticks
- [ ] AIC handles interrupts correctly
- [ ] Name server and ticktimer server running

### Definition of Done

BeetOS boots on real Apple M1 hardware. Same kernel binary (modulo platform selection), same arch layer that was proven on QEMU.

---

## Milestone 4 — Keyboard, Shell & RAM Filesystem

**Goal:** Type on keyboard, interact with shell, read/write files in memory. First Xous userspace services.

All services and apps use `no_std` + `alloc` — gives us `Vec`, `String`, `Box`, `BTreeMap`. No custom toolchain needed. Services communicate via Xous IPC.

### Tasks

- [ ] `api/keyboard/` + `os/keyboard/` — platform-abstracted input:
  - [ ] QEMU: PL011 UART input (already have the driver from M2)
  - [ ] Apple M1: SPI HID keyboard driver
- [ ] `api/console/` + `os/console/` — platform-abstracted console:
  - [ ] QEMU: PL011 UART output
  - [ ] Apple M1: framebuffer server
- [ ] `api/storage/` + `os/ramfs/` — RAM filesystem service:
  - [ ] `BTreeMap<String, Vec<u8>>` as backing store
  - [ ] Operations: create, read, write, delete, list directory
  - [ ] Hierarchical paths (`/tmp/foo/bar.txt`)
  - [ ] Per-process namespace possible (future: isolation)
  - [ ] Everything lost on reboot (by design — NVMe persistence comes in M5)
- [ ] `apps/shell/` — bsh, using `alloc` collections (`Vec`, `String`, `BTreeMap` for command dispatch)
- [ ] Built-ins: help, echo, info, mem, reboot
- [ ] File commands: `write <path> <content>`, `cat <path>`, `ls [path]`, `rm <path>`, `mkdir <path>`

### Definition of Done

Interactive shell with in-memory filesystem. You can create, read, list, and delete files. Multiple Xous services communicating via IPC. This is a real OS.

---

## Milestone 5 — NVMe Storage

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

## Milestone 6 — Network

**Goal:** TCP/IP via USB-C Ethernet. Remote shell access.

### Tasks

- [ ] `os/usb/` — xHCI USB-C driver (minimal, for Ethernet dongle)
- [ ] `api/net/` — network API
- [ ] smoltcp integration as Xous service
- [ ] SSH or raw TCP shell
- [ ] Shell commands: `ifconfig`, `ping`

---

## Milestone 7 — Encrypted Storage & WiFi

- [ ] NVMe write support
- [ ] Encrypted data partition (AES-256-GCM)
- [ ] `os/wifi/` — Broadcom BCM4378 in sandboxed process
- [ ] Shell commands: `wifi scan`, `wifi connect`

---

## Milestone 8 — Full std Support (optional, when needed)

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

- M9: Trackpad, DCP (display coprocessor)
- M10: GPU (AGX) — reference Asahi Lina's Rust DRM driver
- M11: Desktop environment (Wayland compositor or COSMIC port)
- M12: A/B updates, OTA distribution
- M13: M2/M3/M4 support (new platform/ modules + device trees)
- Future: Raspberry Pi 4 platform (platform/rpi4/), Ampere Altra, etc.

---

## Key Difference: 16KB Pages

Apple Silicon uses 16KB pages. Xous/KeyOS assume 4KB. This is the most pervasive change across the codebase. Every `4096`, `0x1000`, `PAGE_SHIFT = 12` must be audited and fixed.

---

## Licensing Note

Audit each cherry-picked file. Xous kernel = MIT OR Apache-2.0. Some KeyOS modifications = GPL-3.0-or-later. BeetOS new code = MIT OR Apache-2.0. GPL files need careful handling.
