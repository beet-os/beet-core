# BeetOS — Implementation Plan

> _Rooted in Rust. Bare-metal to the root._

A secure, minimal OS for AArch64, built on the Xous microkernel (cherry-picked from KeyOS). Multi-platform: runs on QEMU virt, Apple Silicon, and any AArch64 board. Microkernel architecture with all drivers in userspace. Early milestones run entirely from RAM (no disk needed).

**Primary target:** QEMU `virt` machine (development & CI), then Raspberry Pi 5 (BCM2712) and MacBook Air M1 (j313, Apple SoC T8103)
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
├── bcm2712/         ← Raspberry Pi 5 (GICv3, RP1 UART, ARM timer) — SECOND target
├── apple_t8103/     ← Apple M1 (AIC, m1n1, SPI keyboard, ANS NVMe) — THIRD target
└── rpi4/            ← Raspberry Pi 4 (future)
```

QEMU virt is the **first hardware platform** because:
- Any contributor can test AArch64 code without owning specific hardware
- QEMU virt has well-documented, standard hardware (GIC, PL011 UART, virtio) — much simpler than proprietary controllers
- Faster iteration, ideal for CI
- `cargo xtask qemu` is the dream command for CI and contributors

**Raspberry Pi 5 is the second hardware platform** because:
- BCM2712 (Cortex-A76) supports 16KB pages natively — same as Apple M1, no special casing needed
- GICv3 is the same interrupt controller architecture as QEMU virt — driver is largely reusable
- ARM generic timer: identical ISA to QEMU virt and Apple M1 — zero new work
- Real hardware is available (owned)
- Hardware is documented; Linux kernel has BCM2712 and RP1 drivers as reference

The Apple M1 platform becomes the third target, after RPi5 proves the arch layer works on real silicon.

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

- [x] Clone sources: `git clone https://github.com/Foundation-Devices/KeyOS.git /tmp/KeyOS`
- [x] Clone sources: `git clone https://github.com/betrusted-io/xous-core.git /tmp/xous-core`
- [x] Create beetos repo structure
- [x] **Copy kernel core files:**
  - [x] `cp /tmp/KeyOS/xous/kernel/src/{main,syscall,server,services,mem,process,scheduler,irq,args,io,macros,test}.rs → xous/kernel/src/`
  - [x] `cp /tmp/KeyOS/xous/kernel/Cargo.toml → xous/kernel/Cargo.toml`
  - [x] `cp /tmp/KeyOS/xous/kernel/link.x → xous/kernel/link.x`
- [x] **Copy hosted arch** (for host testing):
  - [x] `cp -r /tmp/KeyOS/xous/kernel/src/arch/hosted/ → xous/kernel/src/arch/hosted/`
  - [x] `cp /tmp/KeyOS/xous/kernel/src/arch/mod.rs → xous/kernel/src/arch/mod.rs`
- [x] **Copy debug module:**
  - [x] `cp -r /tmp/KeyOS/xous/kernel/src/debug/ → xous/kernel/src/debug/`
- [x] **Copy xous-rs whole directory, remove arch/arm/:**
  - [x] `cp -r /tmp/KeyOS/xous/xous-rs/ → xous/xous-rs/`
  - [x] `rm -rf xous/xous-rs/src/arch/arm/`
- [x] **Copy xous-ipc:**
  - [x] `cp -r /tmp/KeyOS/xous/ipc/ → xous/ipc/`
- [x] **Copy core service APIs:**
  - [x] `cp -r /tmp/KeyOS/xous/api/ → xous/api/`
- [x] **Copy core services:**
  - [x] `cp -r /tmp/KeyOS/xous/{log,names,ticktimer,trng}/ → xous/`
- [x] **Copy loader:**
  - [x] `cp -r /tmp/KeyOS/loader/ → loader/`
- [x] **Create beetos/ constants crate** inspired by `KeyOS/keyos/src/lib.rs`:
  - [x] `PAGE_SIZE: usize = 16384` (Apple Silicon 16KB pages!)
  - [x] AArch64 memory map constants (ASLR range, kernel load offset, stack addresses)
  - [ ] Audit ALL copied files for hardcoded `4096` / `0x1000` and replace with `beetos::PAGE_SIZE`
- [x] **Create stub `arch/aarch64/mod.rs`** with empty impls of the arch trait (enough to compile)
- [x] **Create stub `platform/apple_t8103/mod.rs`** with empty impls
- [x] **Create workspace Cargo.toml** referencing all crates
- [x] **Fix all references:** `keyos::` → `beetos::`, `cfg(keyos)` → `cfg(beetos)`, remove `atsama5d2` imports
- [x] **Create xtask/** with basic `check` command
- [x] `cargo check` compiles in hosted mode

### Tests

- [x] `cargo check` succeeds (hosted mode, no cross-compile)
- [x] `cargo test` runs Xous kernel tests in hosted mode
- [x] `grep -r "atsama5d2\|sama5\|keyos::" xous/` returns zero matches (1 comment reference to rust-keyos repo URL remains — acceptable)

### Definition of Done

Platform-agnostic Xous code lives in beetos repo. Compiles in hosted mode. Tests pass. Zero KeyOS hardware references remain.

**Status: DONE** (sauf audit 4096→PAGE_SIZE restant, à finir pendant M1)

---

## Milestone 1 — AArch64 Arch Port

**Goal:** Implement `arch/aarch64/`. Kernel cross-compiles for `aarch64-unknown-none`.

No custom Rust toolchain needed — we use the standard `aarch64-unknown-none` target with `no_std` + `alloc`.

### Tasks

- [x] **asm.S** (299 LOC): Exception vectors (16 entries), context save/restore (816-byte frame), svc entry, idle (wfe)
- [x] **start.S** (54 LOC): Boot entry, FP/SIMD enable, VBAR setup, BSS clear, jump to Rust
- [x] **mem.rs** (520 LOC): 4-level page tables, 16KB granule, TTBR0/TTBR1 split, MAIR, W^X enforcement
- [x] **process.rs** (458 LOC): Context switch, ASID management (64 processes × 32 threads), eret to EL0
- [x] **irq.rs** (147 LOC): Generic IRQ dispatch (platform provides the interrupt controller) — QEMU path complete, Apple deferred to M3
- [x] **elf.rs** (267 LOC): ELF64 loader with ASLR, PIE relocation, W^X enforcement
- [x] **panic.rs** (33 LOC), **backtrace.rs** (51 LOC), **mod.rs** (71 LOC), **rand.rs** (60 LOC), **syscall.rs** (34 LOC)
- [x] Adapt `xous-rs/src/arch/aarch64/` (494 LOC): svc wrappers, thread primitives, IPC types
- [x] Fix `mem.rs` (kernel) for 64-bit pointers
- [x] `cargo build --target aarch64-unknown-none -p beetos-kernel` compiles (Rust compilation passes; linker symbols need linker script from M2)

### Tests

- [ ] Unit test: page table entries, 4-level walk, ASID allocator, ELF64 parser (mem.rs has tests, need to wire into `cargo test`)
- [x] Hosted mode tests still pass

### Definition of Done

Kernel cross-compiles for `aarch64-unknown-none`. All arch functions implemented. No hardware boot yet.

**Status: DONE** (Rust compilation passes. Linker script and unit test wiring deferred to M2.)

---

## Milestone 2 — QEMU virt Platform & First Boot

**Goal:** `platform/qemu_virt/`, boot on QEMU `virt` machine, "BeetOS v0.1.0" on UART.

QEMU virt is the first hardware platform — standard, well-documented, and anyone can run it.

### Tasks

- [x] **platform/qemu_virt/mod.rs**: Platform init, default MMIO addresses (UART0, GICD, GICR), shutdown via WFI
- [x] **platform/qemu_virt/gic.rs**: ARM GICv3 interrupt controller driver
  - [x] Distributor (GICD) init: enable, configure SPIs
  - [x] Redistributor (GICR) init: wake, configure PPIs/SGIs
  - [x] CPU interface (ICC system registers): enable, set PMR, acknowledge/EOI
  - [x] IRQ enable/disable/claim/complete for kernel IRQ dispatch
- [x] **platform/qemu_virt/uart.rs**: PL011 UART driver
  - [x] Polled output for early boot (`putc`, `puts`)
  - [x] IRQ-driven receive (for shell input)
  - [x] fmt::Write trait implementation
- [x] **platform/qemu_virt/timer.rs**: ARM generic timer (CNTP, EL1 physical timer)
  - [x] Read CNTFRQ_EL0 for frequency
  - [x] Set CNTP_TVAL_EL0 for periodic tick (100 Hz)
  - [x] Timer IRQ handler with rearming
- [ ] **Adapt loader** for flat binary / ELF load (no m1n1 payload format needed) — _not needed for QEMU: `-kernel` flag loads directly_
- [x] **cargo xtask qemu**: Launch QEMU with correct args:
  - [x] `-machine virt,gic-version=3 -cpu cortex-a72 -m 512M -nographic`
  - [x] `-kernel` pointing to the built kernel binary
  - [x] UART output to terminal stdout
- [x] **Linker script** for QEMU virt memory layout (RAM at 0x4008_0000, 8MB region)
- [x] `cargo xtask build` compiles kernel with linker script successfully

### Tests

- [x] UART shows "BeetOS v0.1.0" in QEMU terminal
- [x] Timer ticks (timer initialized, periodic IRQ armed)
- [x] GIC handles timer IRQ correctly
- [ ] Name server and ticktimer server running (deferred — requires full Xous process infrastructure)
- [x] `cargo xtask qemu` works end-to-end (build + boot + UART output + shell prompt)

### Definition of Done

BeetOS boots on QEMU `virt`. Xous microkernel operational. Any developer can run `cargo xtask qemu` — no hardware needed.

**Status: DONE** — Kernel boots, UART output works, GIC/timer initialized, interactive shell with ramfs operational.

---

## Milestone 3 — Raspberry Pi 5 Platform & Hardware Boot

**Goal:** `platform/bcm2712/`, boot on real Raspberry Pi 5, "BeetOS v0.1.0" on UART. First real silicon. The `arch/aarch64/` layer is proven on QEMU — this milestone adds the BCM2712 platform code.

**Why RPi5 before Apple M1:**
- GICv3 already implemented in `platform/qemu_virt/gic.rs` — largely reusable
- ARM generic timer: identical ISA, zero new work
- BCM2712 supports 16KB pages natively — no special casing, same as the rest of BeetOS
- Hardware available; no proprietary boot chain (no m1n1 needed)

### What's shared with QEMU virt (zero new code)

- `arch/aarch64/` entirely — page tables, exception vectors, context switch
- ARM generic timer driver — same CNTP registers, same IRQ routing through GIC
- GICv3 distributor + CPU interface — same architecture, different MMIO base from FDT

### What's new

- **`platform/bcm2712/mod.rs`**: Platform init, FDT parsing (RAM base/size, GIC base, UART base, timer IRQ)
- **`platform/bcm2712/gic.rs`**: Thin wrapper around the QEMU virt GIC driver with BCM2712-specific redistributor layout (GIC-600 topology may differ slightly)
- **`platform/bcm2712/uart.rs`**: RP1 UART driver
  - RP1 is a separate I/O chip (PCIe-attached); UART is PL011-compatible at the register level but at a different address and requires RP1 PCIe init first
  - Reference: Linux `drivers/tty/serial/amba-pl011.c` + RPi5 device tree
- **`platform/bcm2712/timer.rs`**: ARM generic timer (reuse QEMU virt timer, different IRQ number from FDT)
- **Boot chain**: RPi5 bootloader (start.elf / config.txt) loads kernel8.img at 0x8_0000. `start.S` already handles this. Add `cargo xtask rpi5` to produce `kernel8.img`.

### Tasks

- [ ] **`platform/bcm2712/mod.rs`**: FDT parsing, call GIC/UART/timer init
- [ ] **`platform/bcm2712/gic.rs`**: GICv3 init adapted for BCM2712 redistributor count
- [ ] **`platform/bcm2712/uart.rs`**: RP1 UART (PL011-compatible registers, new base address)
- [ ] **`platform/bcm2712/timer.rs`**: ARM generic timer, IRQ number from FDT
- [ ] **`cargo xtask rpi5`**: Build `kernel8.img`, copy to SD card (or TFTP)
- [ ] **Linker script** for BCM2712 memory layout (RAM at 0x0, kernel loaded at 0x8_0000)
- [ ] Boot on real Raspberry Pi 5

### Tests

- [ ] UART shows "BeetOS v0.1.0" on RPi5 serial console
- [ ] Timer ticks (periodic IRQ via GICv3)
- [ ] GICv3 handles interrupts correctly
- [ ] Shell prompt appears

### Definition of Done

BeetOS boots on real Raspberry Pi 5. `cargo xtask rpi5` produces a bootable image. Same arch layer as QEMU — only `platform/bcm2712/` is new.

---

## Milestone 3b — Apple M1 Platform & Hardware Boot

**Goal:** `platform/apple_t8103/`, boot via m1n1, "BeetOS v0.1.0" on screen. After RPi5 proves the multi-platform architecture on open silicon, this milestone adds Apple-specific platform code.

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

### Definition of Done

BeetOS boots on real Apple M1 hardware. Same kernel binary (modulo platform selection), same arch layer proven on QEMU and RPi5.

---

## Milestone 4 — Keyboard, Shell & RAM Filesystem

**Goal:** Type on keyboard, interact with shell, read/write files in memory. First Xous userspace services.

All services and apps use `no_std` + `alloc` — gives us `Vec`, `String`, `Box`, `BTreeMap`. No custom toolchain needed. Services communicate via Xous IPC.

### Tasks

_Note: M4 was implemented early as a kernel-mode shell (not as Xous userspace services) to provide immediate interactivity. The Xous IPC-based service architecture is deferred to when we have full process infrastructure._

- [x] QEMU: PL011 UART input (IRQ-driven via GIC, character dispatch to shell)
- [ ] `api/keyboard/` + `os/keyboard/` — Xous IPC service (future: when process infra is ready)
  - [ ] Apple M1: SPI HID keyboard driver
- [x] QEMU: PL011 UART output (direct `uart::putc`)
- [ ] `api/console/` + `os/console/` — Xous IPC service (future)
  - [ ] Apple M1: framebuffer server
- [x] `xous/kernel/src/shell/ramfs.rs` (313 LOC) — RAM filesystem:
  - [x] `BTreeMap<String, Vec<u8>>` as backing store
  - [x] Operations: create, read, write, delete, list directory
  - [x] Hierarchical paths
  - [ ] Per-process namespace (future: when processes exist)
  - [x] Everything lost on reboot (by design)
- [x] `xous/kernel/src/shell/mod.rs` (439 LOC) — bsh shell
- [x] Built-ins: help, echo, info, mem, reboot
- [x] File commands: `write <path> <content>`, `cat <path>`, `ls [path]`, `rm <path>`, `mkdir <path>`

### Definition of Done

Interactive shell with in-memory filesystem. You can create, read, list, and delete files. ~~Multiple Xous services communicating via IPC.~~ This is a real OS.

### Tests

- [x] 20 ramfs unit tests (create, read, write, overwrite, delete, mkdir, list, stats, errors, edge cases)
- [x] 9 shell unit tests (command execution, character processing, backspace, Ctrl-C, unknown commands)
- [x] All 29 tests pass in hosted mode (`cargo test -p beetos-kernel`)
- [x] Shell operational on QEMU with UART I/O

### API Crates

- [x] `api/keyboard/` (`beetos-api-keyboard`): KeyEvent types, KeyboardOp IPC opcodes
- [x] `api/console/` (`beetos-api-console`): ConsoleOp IPC opcodes

**Status: DONE** — Shell, ramfs, and API type crates complete. 29 new tests. Xous IPC service migration deferred until process infrastructure is ready.

---

## Milestone 5 — Process Lifecycle (Spawn / Exit / Wait)

**Goal:** A process can spawn another, wait for it to finish, and get its exit code. The shell can launch programs: `bsh> hello` → new process prints "Hello, BeetOS!" → exits → shell shows prompt.

**Architecture:** Microkernel-proper. A **Process Manager** (`procman`) service in userspace handles lifecycle policy. The kernel provides the mechanism (syscalls). The shell talks to procman via Xous IPC, never directly to the kernel for spawn/wait.

```
bsh> hello
  ↓
Shell → BlockingScalar(SpawnAndWait, "hello") → ProcMan (IPC)
  ↓
ProcMan → SVC SpawnByName("hello") → Kernel
  ↓
Kernel: looks up "hello" in binary table
        creates process (ELF load, page tables, stack, UART mapping)
        returns PID
  ↓
ProcMan → SVC WaitProcess(pid) → Kernel
  ↓
Kernel: parks procman thread (WaitProcess { pid })
        schedules hello process
  ↓
Hello runs at EL0: writes "Hello, BeetOS!" to UART, calls TerminateProcess(0)
  ↓
Kernel: cleans up hello (pages, servers, connections)
        wakes procman thread → exit_code=0
  ↓
ProcMan → ReturnScalar(exit_code) → Shell (IPC return)
  ↓
Shell: prints "[exited: 0]", shows bsh> prompt
```

### Syscalls

Two new syscalls on top of existing Xous ones:

| Syscall | Number | Signature | Description |
|---------|--------|-----------|-------------|
| `SpawnByName` | 57 | `(name_ptr, name_len) → PID` | Kernel looks up name in embedded binary table, creates process (ELF load + stack + UART mapping), returns PID. |
| `WaitProcess` | 58 | `(pid) → exit_code` | Blocks until target process calls `TerminateProcess`. Returns exit code. |

Existing syscalls used as-is:
- `TerminateProcess(exit_code)` = 22 — already has full cleanup (pages, servers, connections, reparent children)
- `GetProcessId` = 33 — used by hello to print its PID
- `CreateServerWithAddress`, `ReceiveMessage`, `SendMessage`, `Connect`, `ReturnScalar1` — all existing Xous IPC

### New kernel infrastructure

#### 1. Binary registry (`boot.rs`)

```rust
/// Embedded binary table: name → ELF bytes.
/// The kernel holds these via include_bytes! — no filesystem needed.
static BINARY_TABLE: &[(&str, &[u8])] = &[
    ("hello", HELLO_ELF),
    ("shell", SHELL_ELF),
];
```

#### 2. SpawnByName handler (`syscall.rs`, `boot.rs`)

The kernel:
1. Copies the name from user VA (validated, max 32 bytes)
2. Looks up `BINARY_TABLE` by name
3. Calls existing `create_elf_process` pipeline (ELF load → stack alloc → page table setup)
4. Maps UART MMIO into the new process (same as shell gets it today)
5. Passes UART VA via x0 (same mechanism as shell)
6. Grants syscall permissions
7. Returns `Result::ProcessID(pid)`

Key: the new process is created in `Ready` state but NOT immediately scheduled. The caller (procman) decides what to do next (typically WaitProcess).

#### 3. WaitProcess handler (`syscall.rs`, `services.rs`)

New `ThreadState::WaitProcess { pid: PID }` variant added to the existing enum.

When `WaitProcess(target_pid)` is called:
- If target PID doesn't exist or already exited: return immediately with exit code
- Otherwise: set caller thread to `ThreadState::WaitProcess { pid: target_pid }`, yield

When `terminate_current_process(exit_code)` runs:
- Existing cleanup (servers, connections, memory, pages) stays unchanged
- **New**: scan all threads in all processes for `WaitProcess { pid: dying_pid }`, wake them with `Result::Scalar1(exit_code)`

#### 4. UART mapping for spawned processes

`SpawnByName` automatically maps the UART MMIO page into every new process at `SHELL_UART_VA`. This is a boot-time policy — all processes can write to the console. Future milestones can restrict this.

### New crates

#### `api/procman/` — Process Manager API

```rust
#![no_std]

pub const PROCMAN_SID: [u32; 4] = [0x5052_4F43, 0x4D41_4E00, 0, 0]; // "PROCMAN\0"

#[repr(usize)]
pub enum ProcManOp {
    /// Spawn a process by name and wait for it to exit.
    /// BlockingScalar: arg1-arg4 = name bytes packed as 4×usize (max 32 bytes).
    /// Returns: Scalar1(exit_code) on success.
    SpawnAndWait = 0,
    /// Spawn a process by name, return immediately with PID.
    /// Scalar: arg1-arg4 = name bytes packed as 4×usize.
    /// Returns: Scalar1(pid) on success.
    Spawn = 1,
    /// Wait for a process to exit.
    /// BlockingScalar: arg1 = pid.
    /// Returns: Scalar1(exit_code).
    Wait = 2,
}

/// Pack a process name (up to 32 bytes) into 4 usize values for Scalar messages.
pub fn pack_name(name: &str) -> [usize; 4] { ... }
/// Unpack a name from 4 usize values.
pub fn unpack_name(args: &[usize; 4]) -> &str { ... }
```

#### `os/procman/` — Process Manager Service

Userspace process that:
1. Creates server with `PROCMAN_SID`
2. Loops on `ReceiveMessage`:
   - `SpawnAndWait(name)`: calls `SpawnByName` syscall, then `WaitProcess(pid)`, returns exit code to sender
   - `Spawn(name)`: calls `SpawnByName`, returns PID to sender
   - `Wait(pid)`: calls `WaitProcess`, returns exit code to sender
3. Gets UART mapped (same as shell) for potential debug output

The procman is launched at boot alongside idle and shell.

### Updated apps

#### `apps/hello/` — rewritten

```rust
#[no_mangle]
pub extern "C" fn _start() -> ! {
    // x0 = UART VA (set by kernel)
    let uart_base = read_x0();
    uart_puts(uart_base, "Hello, BeetOS!\n");
    uart_puts(uart_base, "I am PID ");
    // GetProcessId syscall
    uart_puts(uart_base, &pid_to_str());
    uart_puts(uart_base, ", running at EL0.\n");
    xous::terminate_process(0);  // clean exit
}
```

#### `apps/shell/` — updated command dispatch

```rust
match cmd {
    "help" | "echo" | "info" | ... => /* builtins */,
    unknown_cmd => {
        // Try to spawn via procman
        let cid = connect_to_procman();  // lazy connect, blocks until procman ready
        let name_packed = pack_name(unknown_cmd);
        match xous::send_message(cid, Message::new_blocking_scalar(
            ProcManOp::SpawnAndWait as usize,
            name_packed[0], name_packed[1], name_packed[2], name_packed[3],
        )) {
            Ok(Result::Scalar1(exit_code)) => {
                write!(UartWriter, "[exited: {}]\n", exit_code);
            }
            Err(e) => {
                write!(UartWriter, "bsh: {}: not found\n", unknown_cmd);
            }
        }
    }
}
```

### Boot sequence (updated)

```
Kernel boots → MMU → MemoryManager → SystemServices
  ↓
launch_first_process():
  1. create_elf_process(PID 2, HELLO_ELF, "idle")      — idle loop
  2. create_elf_process(PID 3, PROCMAN_ELF, "procman")  — process manager
  3. create_elf_process(PID 4, SHELL_ELF, "shell")      — interactive shell
  4. Map UART into procman (PID 3) and shell (PID 4)
  5. ERET into shell (PID 4)
  ↓
Scheduler runs:
  - Shell prints banner, creates CONSOLE server, connects to PROCMAN (blocks until procman ready)
  - ProcMan creates PROCMAN server → shell's connect unblocks
  - Shell enters ReceiveMessage loop
  - Idle yields in a loop
  ↓
User types "hello":
  - UART IRQ → kernel sends char to shell via CONSOLE IPC
  - Shell receives chars, parses "hello", sends SpawnAndWait to procman
  - ProcMan calls SpawnByName("hello") → kernel creates PID 5
  - ProcMan calls WaitProcess(5) → blocks
  - Hello runs, prints, calls TerminateProcess(0)
  - Kernel wakes procman, procman returns exit_code to shell
  - Shell prints "[exited: 0]", shows prompt
```

### Implementation order

1. **Kernel: SpawnByName syscall + binary table** — mechanism for creating processes by name
2. **Kernel: WaitProcess syscall + ThreadState** — mechanism for waiting on process exit
3. **Kernel: TerminateProcess wakeup** — wake WaitProcess waiters when process dies
4. **api/procman/** — IPC types and name packing helpers
5. **os/procman/** — process manager service binary
6. **apps/hello/** — rewrite to print and exit
7. **apps/shell/** — add procman connect + unknown-command spawn
8. **boot.rs** — launch procman alongside idle and shell
9. **xtask** — build procman ELF, embed in kernel

### Tests

- [x] Hosted mode: `cargo test` still passes (all 43 existing tests)
- [x] QEMU: `bsh> hello` prints "Hello, BeetOS!" and returns to prompt
- [x] QEMU: `bsh> hello` shows correct PID (e.g., PID 5)
- [x] QEMU: `bsh> hello` then `bsh> hello` shows PID 6 (new process each time)
- [x] QEMU: unknown command shows "not found" error
- [x] QEMU: process cleanup works (no memory leak across many spawns)

### What this does NOT include (deferred)

- No dynamic ELF loading from filesystem (binaries are embedded at compile time)
- No fork/exec semantics (spawn only)
- No signal delivery (TerminateProcess is self-termination only; TerminatePid exists but is restricted)
- No per-process UART permission control (all processes get UART for now)
- No procman access control (any process can spawn anything)

**Status: DONE** — SpawnByName (57) + WaitProcess (58) implemented in kernel. Procman service running at boot. Shell spawns external processes via procman IPC. `hello`, `hello-std`, `coreutils` all launch and return to prompt cleanly.

---

## Milestone 6 — Block Storage (virtio-blk on QEMU)

**Status: DONE** — virtio-blk driver, tar read-only filesystem, `api/storage` BlockDevice trait, shell `disk`/`ls /disk/`/`cat /disk/` commands all operational on QEMU.

---

## Milestone 6 (archived spec) — Block Storage (virtio-blk on QEMU)

**Goal:** Read/write a virtual block device via virtio-blk on QEMU. Simple read-only filesystem. Foundation for persistent storage on any platform.

**Strategy:** Implement virtio MMIO transport + virtio-blk driver in the kernel (platform code), expose a block device API, add a simple tar-based read-only filesystem. The block API is platform-agnostic — future platforms (RPi5, Apple M1) add their own storage backends (SD card, NVMe) behind the same API.

### Architecture

```
bsh> cat /disk/hello.txt
  ↓
Shell → ramfs lookup fails → disk filesystem lookup
  ↓
DiskFs (tar reader) → BlockDev::read_sector(lba)
  ↓
virtio-blk driver → virtqueue request → QEMU virtio-blk backend
  ↓
QEMU reads from disk.img file on host
```

### virtio MMIO Transport (QEMU virt)

QEMU virt exposes virtio devices at MMIO region `0x0A00_0000`, each transport occupying `0x200` bytes. IRQs start at SPI 16 (GIC IRQ 48). The kernel discovers virtio-blk by scanning transports for `DeviceID == 2` (block device).

**Key virtio MMIO registers (spec v1.2, section 4.2):**

| Offset | Name | R/W | Description |
|--------|------|-----|-------------|
| 0x000 | MagicValue | R | Must be `0x74726976` ("virt") |
| 0x004 | Version | R | Must be `2` (modern) |
| 0x008 | DeviceID | R | `2` = block device |
| 0x010 | DeviceFeatures | R | Feature bits (selected by DeviceFeaturesSel) |
| 0x020 | DriverFeatures | W | Feature bits accepted by driver |
| 0x030 | QueueSel | W | Select virtqueue index |
| 0x034 | QueueNumMax | R | Max queue size |
| 0x038 | QueueNum | W | Queue size (power of 2) |
| 0x044 | QueueReady | W | `1` to mark queue ready |
| 0x050 | QueueNotify | W | Write queue index to notify device |
| 0x060 | InterruptStatus | R | Bit 0 = used buffer, bit 1 = config change |
| 0x064 | InterruptACK | W | Write to acknowledge interrupt |
| 0x070 | Status | R/W | Device status (ACKNOWLEDGE, DRIVER, FEATURES_OK, DRIVER_OK) |
| 0x080 | QueueDescLow/High | W | Physical address of descriptor table |
| 0x090 | QueueDriverLow/High | W | Physical address of available ring |
| 0x0A0 | QueueDeviceLow/High | W | Physical address of used ring |

**Virtqueue layout (physically contiguous DMA memory):**
- Descriptor table: 16 bytes × queue_size
- Available ring: 6 + 2 × queue_size bytes
- Used ring: 6 + 8 × queue_size bytes

**Block request format:**
```rust
#[repr(C)]
struct VirtioBlkReq {
    type_: u32,      // 0 = read, 1 = write
    reserved: u32,
    sector: u64,     // LBA (512-byte sectors)
}
// Followed by data buffer (512+ bytes), then 1-byte status
```

### New Files

#### 1. `xous/kernel/src/platform/qemu_virt/virtio.rs` — virtio MMIO transport

Generic virtio MMIO transport: device discovery, feature negotiation, virtqueue setup.
- `VirtioMmio` struct: base address, register read/write
- `Virtqueue` struct: descriptor table, available ring, used ring, free list
- Device init sequence (spec §3.1): reset → acknowledge → driver → features → features_ok → driver_ok
- Queue notification and interrupt handling

#### 2. `xous/kernel/src/platform/qemu_virt/blk.rs` — virtio-blk driver

Block device driver on top of virtio MMIO transport.
- `init()`: scan MMIO region for DeviceID=2, negotiate features, set up requestq (queue 0)
- `read_sectors(lba: u64, count: u32, buf: &mut [u8])`: submit read request, wait for completion
- `write_sectors(lba: u64, count: u32, buf: &[u8])`: submit write request (for future use)
- `capacity() -> u64`: read device config for total sectors
- Synchronous (polling) for now — IRQ-driven later

#### 3. `api/storage/` — Block storage API crate

```rust
#![no_std]
pub const BLOCK_SIZE: usize = 512;

pub trait BlockDevice {
    fn read_sectors(&self, lba: u64, buf: &mut [u8]) -> Result<(), BlockError>;
    fn write_sectors(&self, lba: u64, buf: &[u8]) -> Result<(), BlockError>;
    fn capacity_sectors(&self) -> u64;
}

pub enum BlockError {
    IoError,
    OutOfRange,
    NotReady,
}
```

#### 4. Filesystem: tar-based read-only rootfs

Simple tar reader (POSIX ustar format) that reads from the block device:
- Parse 512-byte tar headers (filename, size, type)
- `ls(path)` → list entries
- `cat(path)` → read file contents
- Mount at `/disk/` in the shell
- No write support (read-only)

The tar image is created on the host (`tar cf disk.tar hello.txt readme.txt`) and passed to QEMU via `-drive file=disk.tar,format=raw,if=virtio`.

#### 5. Shell integration

- New commands: `disk` (show block device info), update `ls`/`cat` to try `/disk/` paths
- Or: `mount` command that registers the disk filesystem

### QEMU Launch Update

```bash
qemu-system-aarch64 \
    -machine virt,gic-version=3 \
    -cpu neoverse-n1 \
    -m 512M \
    -nographic \
    -kernel beetos-kernel \
    -drive file=disk.tar,format=raw,if=virtio   # NEW
```

### Implementation Order

1. **`virtio.rs`** — MMIO transport: register access, device discovery, virtqueue alloc/setup
2. **`blk.rs`** — virtio-blk: init, synchronous read_sectors (polling mode)
3. **`api/storage/`** — BlockDevice trait, BlockError
4. **xtask** — update QEMU launch with `-drive` flag, add `disk.tar` generation
5. **tar reader** — parse ustar headers, read file data from block device
6. **Shell** — `disk` command, `/disk/` path support in ls/cat
7. **Tests** — hosted mode unit tests for tar parser, QEMU integration test

### Memory Allocation for Virtqueues

Virtqueues need physically contiguous DMA-accessible memory. On BeetOS:
- Allocate pages from `MemoryManager::alloc_range()` (PID 1 = kernel)
- Convert to kernel VA via `beetos::phys_to_virt(pa)` for CPU access
- Pass physical addresses to virtio device registers
- Queue size = 16 entries (small, ~1 page total for desc + avail + used)

### IRQ Handling

virtio-blk uses SPI 16 (first virtio transport = GIC IRQ 48). For the initial implementation:
- **Polling mode**: after submitting a request, spin-wait on the used ring
- **Future**: add IRQ handler in `handle_irq()` match arm, wake blocked thread

### Tests

- [ ] Hosted mode: tar parser unit tests (header parsing, file listing, file reading)
- [ ] Hosted mode: virtqueue data structure tests (descriptor chains, ring operations)
- [ ] QEMU: `cargo xtask qemu` with disk.tar shows block device initialized
- [ ] QEMU: `bsh> disk` shows capacity and device info
- [ ] QEMU: `bsh> ls /disk/` lists files from tar image
- [ ] QEMU: `bsh> cat /disk/hello.txt` reads file content

### What this does NOT include (deferred)

- No write filesystem (tar is read-only)
- No verified boot / ed25519 (deferred to when we have real rootfs)
- No partition table parsing (raw tar image, no GPT/MBR)
- No DMA cache management (QEMU is cache-coherent)
- No async/IRQ-driven I/O (polling only for now)

### Definition of Done

`cargo xtask qemu` boots with a disk image. Shell can list and read files from the virtio-blk device. Block storage API is platform-agnostic.

---

## Milestone 7 — Network

**Goal:** TCP/IP via USB-C Ethernet. Remote shell access.

### Tasks

- [ ] `os/usb/` — xHCI USB-C driver (minimal, for Ethernet dongle)
- [ ] `api/net/` — network API
- [ ] smoltcp integration as Xous service
- [ ] SSH or raw TCP shell
- [ ] Shell commands: `ifconfig`, `ping`

---

## Milestone 8 — Encrypted Storage & WiFi

- [ ] NVMe write support
- [ ] Encrypted data partition (AES-256-GCM)
- [ ] `os/wifi/` — Broadcom BCM4378 in sandboxed process
- [ ] Shell commands: `wifi scan`, `wifi connect`

---

## Milestone 9 — Full std Support (optional, when needed)

**Goal:** Fork the Rust compiler to add `aarch64-unknown-beetos` target. Services can use full `std`.

### Tasks

- [x] Fork rust → `beet-os/rust` (separate repo, merged to main)
- [x] Add `aarch64-unknown-beetos` target spec
- [x] Adapt libstd Xous backend for AArch64 syscall ABI + 64-bit pointers (TLS_MEMORY_SIZE=16KB, dlmalloc beetos branch in library/Cargo.toml)
- [x] Build libstd: `python3 x.py build --stage 1 library/std`
- [x] `hello-std`: Box, String, Vec, format!, HashMap all work on QEMU

**Status: DONE** — `aarch64-unknown-beetos` target in beet-os/rust (main). hello-std validated on QEMU.

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

- M10: Trackpad, DCP (display coprocessor)
- M11: GPU (AGX) — reference Asahi Lina's Rust DRM driver
- M12: Desktop environment (Wayland compositor or COSMIC port)
- M13: A/B updates, OTA distribution
- M14: M2/M3/M4 support (new platform/ modules + device trees)
- Future: Raspberry Pi 4 platform (platform/rpi4/), Ampere Altra, etc.

---

## Key Difference: 16KB Pages

Apple Silicon uses 16KB pages. Xous/KeyOS assume 4KB. This is the most pervasive change across the codebase. Every `4096`, `0x1000`, `PAGE_SHIFT = 12` must be audited and fixed.

---

## Licensing Note

Audit each cherry-picked file. Xous kernel = MIT OR Apache-2.0. Some KeyOS modifications = GPL-3.0-or-later. BeetOS new code = MIT OR Apache-2.0. GPL files need careful handling.
