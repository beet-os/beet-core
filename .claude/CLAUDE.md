# CLAUDE.md — BeetOS

## What is this project?

BeetOS is a secure, minimal OS for AArch64, built on the **Xous microkernel** cherry-picked from [KeyOS](https://github.com/Foundation-Devices/KeyOS) (Foundation Devices' hardware wallet OS). We keep Xous's platform-agnostic kernel core (~7500 LOC) and rewrite only the hardware layer for AArch64 (~5000 LOC).

BeetOS is **multi-platform**: the `arch/aarch64/` code is generic AArch64 (page tables, exception vectors, context switch, ASID, eret — same ISA everywhere). All hardware-specific code lives in `platform/` modules. Adding a new platform = new platform module, no kernel rewrite.

**Supported platforms:**
- **QEMU virt** — primary development & CI target (GIC, PL011 UART, virtio)
- **Apple M1** (MacBook Air j313, T8103) — real hardware target (AIC, m1n1, SPI keyboard, ANS NVMe)
- **Raspberry Pi 4** — future

The boot chain on Apple M1 hardware is: iBoot (Apple firmware) → m1n1 (Asahi bootloader) → BeetOS loader → BeetOS kernel + services.

For development, the **hosted mode** runs the entire OS as a normal process on your laptop — no hardware needed. The **QEMU virt** platform allows testing real AArch64 code without owning specific hardware.

## Origin of the code

The `xous/` subtree is cherry-picked from KeyOS, NOT written from scratch. Key files:

| File                                    | Origin | Status                           |
| --------------------------------------- | ------ | -------------------------------- |
| `xous/kernel/src/syscall.rs`            | KeyOS  | Copied as-is                     |
| `xous/kernel/src/server.rs`             | KeyOS  | Copied as-is                     |
| `xous/kernel/src/services.rs`           | KeyOS  | Copied as-is                     |
| `xous/kernel/src/process.rs`            | KeyOS  | Copied as-is                     |
| `xous/kernel/src/scheduler.rs`          | KeyOS  | Copied as-is                     |
| `xous/kernel/src/mem.rs`                | KeyOS  | Copied, adapted for 64-bit       |
| `xous/kernel/src/arch/hosted/`          | KeyOS  | Copied as-is (for dev/test)      |
| `xous/kernel/src/arch/aarch64/`         | —      | **NEW** (our AArch64 port, generic — no platform-specific code) |
| `xous/kernel/src/platform/qemu_virt/`   | —      | **NEW** (QEMU virt: GIC, PL011, virtio) |
| `xous/kernel/src/platform/apple_t8103/` | —      | **NEW** (Apple M1: AIC, m1n1, SPI, ANS) |
| `xous/xous-rs/`                         | KeyOS  | Copied + new `arch/aarch64/`     |
| `xous/ipc/`                             | KeyOS  | Copied as-is                     |
| `xous/{log,names,ticktimer,trng}/`      | KeyOS  | Copied, platform/ rewritten      |
| `api/`, `os/`, `apps/`                  | —      | **NEW** (BeetOS services & apps) |

When modifying copied Xous code, understand the existing design before changing it. These files have been battle-tested on real hardware wallets.

## Workspace Layout

```
xous/           ← Xous microkernel (cherry-picked from KeyOS)
  kernel/       ← the microkernel: syscalls, IPC, scheduler, memory
    src/arch/aarch64/   ← our AArch64 port (generic, no platform-specific code)
    src/arch/hosted/    ← hosted mode for dev/test (from KeyOS)
    src/platform/qemu_virt/    ← QEMU virt platform (GIC, PL011, virtio)
    src/platform/apple_t8103/  ← Apple M1 platform (AIC, m1n1, SPI, ANS)
  xous-rs/      ← userspace syscall library (like libc for Xous)
  ipc/          ← shared IPC types
  api/          ← core service APIs (log, names, ticktimer)
  log/, names/, ticktimer/, trng/  ← core service implementations
beetos/         ← constants crate (PAGE_SIZE, memory map, addresses)
api/            ← BeetOS service APIs (console, keyboard, storage, net)
os/             ← BeetOS service implementations / drivers
apps/           ← user applications (shell)
loader/         ← loads kernel + services into RAM
boot/m1n1/      ← git submodule (Asahi bootloader)
xtask/          ← build system (runs on host)
```

The `api/` vs `os/` split follows KeyOS/Xous pattern: `api/keyboard` defines the types and client stubs, `os/keyboard` is the actual driver. Any process can depend on an api crate without pulling in the driver.

## How to Develop (hosted mode)

**Primary development workflow — no hardware needed:**

```bash
cargo run                  # runs Xous kernel in hosted mode
cargo test                 # runs kernel + service unit tests on host
```

In hosted mode:

- The Xous kernel runs as a normal host process
- Xous "processes" are host threads
- Xous IPC goes through TCP sockets on localhost
- Memory allocation uses the host allocator
- No MMU, no interrupts, no cross-compilation

This tests all kernel logic: IPC, scheduling, syscall dispatch, service registration, the shell, the ramfs — everything except hardware-specific code.

## How to Build for Hardware

### QEMU virt (primary hardware target — no special hardware needed)

```bash
cargo xtask build          # cross-compile for aarch64-unknown-none
cargo xtask qemu           # launch QEMU virt with the kernel (UART output to terminal)
```

Requires: `qemu-system-aarch64` installed on host.

### Apple M1 (real hardware)

```bash
cargo xtask build          # cross-compile for aarch64-unknown-none
cargo xtask image          # build m1n1 + loader + kernel + services payload
cargo xtask run            # push to MBA M1 via m1n1 USB proxy (~7 sec cycle)
```

Requires: MBA M1 with m1n1 installed, USB-C cable to host, Python 3 + m1n1 proxyclient.

## Compilation Model

**No custom Rust toolchain required.** Everything compiles with standard `rustup`:

- **Kernel, loader**: `#![no_std]`, target `aarch64-unknown-none`
- **Services (`os/`), apps (`apps/`)**: `#![no_std]` + `extern crate alloc` — gives `Vec`, `String`, `Box`, `BTreeMap`
- **xtask**: normal `std` Rust, runs on host
- **Hosted mode**: everything compiles for host target with `std`

Full `std` support (via custom Rust toolchain fork) is planned as M7, optional, for when we need `std::net`, `std::thread`, `std::fs` in services. Until then, `alloc` covers 90% of needs.

## Implementation Rules

1. **Follow the plan.** See `.claude/plan.md`. Implement milestones in order. Do not skip ahead.
2. **Develop in hosted mode first.** Get the logic working with `cargo run` / `cargo test` before touching hardware.
3. **no_std + alloc** for kernel, services, and apps. No `std` except in `xtask/` and hosted mode.
4. **No C code.** No `cc` crate. No `.c` files. Only assembly allowed is `asm.S` for exception vectors and context switch.
5. **No `unwrap()` in kernel.** Use explicit error handling. Panic must be intentional and informative.
6. **`unsafe` only in `xous/kernel/src/arch/`** and MMIO register wrappers. All driver and service logic must be safe Rust.
7. **W^X always.** No page is ever writable AND executable simultaneously.
8. **Tests at every milestone.** Hosted mode tests first, hardware tests second.
9. **16KB pages.** Apple Silicon uses 16KB pages, not 4KB. Audit all `4096` / `0x1000` / `PAGE_SHIFT = 12` constants. Use `beetos::PAGE_SIZE` everywhere.

## Key Conventions

- **cfg flags**: `cfg(beetos)` for real hardware, `cfg(not(beetos))` for hosted mode. Replaces KeyOS's `cfg(keyos)`.
- **Entry point on hardware**: `kmain(fdt_ptr: *const u8)` — receives FDT from m1n1 in x0.
- **All hardware addresses from FDT.** No hardcoded MMIO addresses in driver code.
- **Kernel runs at EL1.** Userspace at EL0.
- **MMIO access**: `core::ptr::read_volatile` / `write_volatile` wrapped in safe abstractions.
- **Logging**: `log` crate macros (`info!`, `debug!`, `warn!`, `error!`). Backend: framebuffer on hardware, println on hosted.
- **Service naming**: services register with the name server (Xous pattern). Use `api/` crate for the client, `os/` crate for the server.
- **IPC**: Xous message passing. Borrow/MutableBorrow/Move semantics. See `xous/xous-rs/src/syscall.rs` for the API.

## Error Handling

- `xous/kernel/`: follows existing Xous error patterns. Do not change without good reason.
- `os/` services: each defines its own error type.
- `xtask/`: `anyhow` is fine, it runs on the host.

## Reference Code

**For Xous kernel internals** — consult the original Xous documentation:

- `https://github.com/betrusted-io/xous-core` — original Xous (RISC-V)
- `https://github.com/Foundation-Devices/KeyOS` — KeyOS (ARM port we cherry-pick from)
- KeyOS `xous/kernel/src/arch/arm/` — reference for our `arch/aarch64/` port

**For QEMU virt platform** — standard ARM hardware:

- **GICv3**: ARM GIC Architecture Specification (IHI 0069), Linux `drivers/irqchip/irq-gic-v3.c`
- **PL011 UART**: ARM PL011 Technical Reference Manual, Linux `drivers/tty/serial/amba-pl011.c`
- **virtio**: virtio spec v1.2, Linux `drivers/virtio/`
- **QEMU virt machine**: QEMU source `hw/arm/virt.c` for memory map and device layout

**For Apple Silicon hardware** — consult the Asahi Linux kernel tree:

- **AIC**: `drivers/irqchip/irq-apple-aic.c`
- **SPI keyboard**: `drivers/input/keyboard/apple-spi.c` + `drivers/spi/spi-apple.c`
- **NVMe (ANS)**: `drivers/nvme/host/apple.c`
- **DART**: `drivers/iommu/apple-dart.c`
- **WiFi (BCM)**: `drivers/net/wireless/broadcom/brcm80211/`

These are C files. Reimplement the protocol in Rust — do not translate C line-by-line. Understand the hardware interaction, then write idiomatic Rust.

## Testing Strategy

1. **Hosted mode** (`cargo test`, `cargo run`): Primary. Tests all kernel logic, IPC, services, shell, ramfs. No hardware needed. 80% of development happens here.
2. **QEMU virt** (`cargo xtask qemu`): Tests real AArch64 arch code (page tables, exceptions, context switch) on standard hardware (GIC, PL011). Any developer can run this. Ideal for CI.
3. **Apple M1 hardware** (via m1n1 USB proxy): Tests Apple platform code, real drivers. 7-second cycle. Requires physical hardware.
4. **Conditional compilation**: `#[cfg(beetos)]` for hardware paths, `#[cfg(not(beetos))]` for hosted paths. `#[cfg(test)]` for unit tests.

## Git Workflow

- `main` branch is always functional in hosted mode (from M0 onwards), bootable on QEMU (from M2 onwards), and bootable on Apple M1 (from M3 onwards).
- Feature branches per milestone: `m0-cherry-pick`, `m1-aarch64`, `m2-first-boot`, `m3-shell`, etc.
- Squash merge to main when milestone passes all tests.
- Tag releases: `v0.1.0` (M0), `v0.2.0` (M1), etc.

## Things to Avoid

- No `unwrap()` in library/kernel code. Use proper error propagation.
- No `println!` in kernel/services. Use `log::{info, debug, warn, error}`.
- No `unsafe` outside of `arch/` and MMIO wrappers.
- No C dependencies. Ever.
- No hardcoded hardware addresses. Everything from FDT.
- No `4096` literals. Use `beetos::PAGE_SIZE` (16384).
- Do not modify copied Xous kernel code without understanding why it was written that way.
- Do not implement milestones out of order.
