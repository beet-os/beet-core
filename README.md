# BeetOS

> _Rooted in Rust. Bare-metal to the root._

A secure, minimal OS for AArch64, built on the [Xous microkernel](https://github.com/betrusted-io/xous-core) (cherry-picked from [KeyOS](https://github.com/Foundation-Devices/KeyOS)). 100% Rust — no C, no custom toolchain, no forks. Runs on QEMU virt today; Raspberry Pi 5 and Apple M1 next.

```
cargo xtask qemu   # boots a full OS in your terminal, no hardware needed
```

---

## What is this?

BeetOS is a microkernel OS where every driver runs in userspace. The kernel handles only memory, scheduling, and IPC. Everything else — filesystem, input, networking — is a userspace service communicating via Xous message passing.

**Architecture highlights:**

- **Xous microkernel** — battle-tested IPC and process model from a production hardware wallet OS
- **AArch64-generic arch layer** — the same `arch/aarch64/` code runs on QEMU, Raspberry Pi 5, and Apple M1. Zero hardware-specific code in `arch/`
- **Platform modules** — adding a new board = new `platform/<name>/` module, no kernel rewrite
- **16KB pages** — native granule for Apple Silicon; used everywhere for consistency
- **W^X enforced** — no page is ever writable and executable simultaneously
- **no_std + alloc** — kernel, drivers, and apps compile with standard `rustup`, no custom toolchain
- **Allocation-free GUI stack** — `beetos::gfx` software rasterizer with a wgpu-shaped `Surface` API, `beetos::gui` window manager + widgets + animation, interactive demos (Snake, Game of Life, Calculator, Notes, Mandelbrot). Today CPU-rendered; designed to swap in a real wgpu backend on top of native GPU drivers (Asahi for M1, v3d for RPi5) once M11 lands. See [`docs/gui.md`](docs/gui.md) and [`docs/wgpu-roadmap.md`](docs/wgpu-roadmap.md).

---

## Quick start

```bash
# Prerequisites: Rust (stable), qemu-system-aarch64

rustup target add aarch64-unknown-none
cargo xtask qemu
```

This builds the kernel, all services, and all apps, then launches QEMU. You get an interactive shell on UART in your terminal:

```
bsh> help
bsh> ls
bsh> cat /etc/motd
bsh> echo hello world
```

---

## Repository layout

```
beet-core/
├── xous/
│   ├── kernel/          ← Xous microkernel (cherry-picked from KeyOS, adapted for AArch64)
│   │   └── src/
│   │       ├── arch/aarch64/        ← AArch64 port: page tables, exception vectors, context switch
│   │       ├── arch/hosted/         ← Hosted mode (development on macOS/Linux, no hardware)
│   │       └── platform/
│   │           ├── qemu_virt/       ← QEMU virt: GICv3, PL011 UART, virtio-blk/net
│   │           ├── bcm2712/         ← Raspberry Pi 5: GICv3, PL011, SDHCI, mailbox
│   │           └── apple_t8103/     ← Apple M1: AIC, m1n1 (in progress)
│   └── xous-rs/         ← Userspace syscall library
│
├── beetos/              ← Shared constants: PAGE_SIZE, memory map, addresses
├── api/                 ← Service API crates (IPC types, opcodes, client stubs)
│   ├── fs/              ← Filesystem IPC protocol
│   ├── block/           ← Block-device IPC protocol
│   ├── storage/         ← BlockDevice trait + errors
│   ├── cryptblock/      ← AES-256-GCM encrypted area over api/block
│   ├── net/             ← Userspace TCP sockets (TcpListener/TcpStream)
│   ├── console/         ← Console IPC protocol
│   ├── procman/         ← Process manager IPC protocol
│   └── keyboard/        ← Keyboard IPC protocol
│
├── os/                  ← Service implementations (userspace drivers)
│   ├── fs/              ← Filesystem service (ramfs + tar disk + encrypted /data)
│   ├── block/           ← Block service (disk reads/writes, flush via kernel)
│   ├── console/         ← Console output service (UART + TCP fan-out)
│   └── procman/         ← Process manager service
│
├── apps/
│   ├── shell/           ← bsh interactive shell (EL0 userspace)
│   ├── coreutils/       ← Small userland utilities
│   ├── hello/           ← Hello world (spawn/exit demo)
│   └── hello-std/       ← std-on-BeetOS demo (needs the beet-os/rust fork)
│
├── loader/              ← Bootloader (loads kernel + services into RAM)
└── xtask/               ← Build system (cargo xtask build/qemu/image/run)
```

---

## Build commands

| Command             | What it does                                                 |
| ------------------- | ------------------------------------------------------------ |
| `cargo test`        | Run all tests in hosted mode (no hardware, no cross-compile) |
| `cargo run`         | Run the OS in hosted mode (kernel as a normal process)       |
| `cargo xtask build` | Cross-compile everything for `aarch64-unknown-none`          |
| `cargo xtask qemu`  | Build + launch QEMU virt                                     |
| `cargo xtask qemu-smoke`      | Boot QEMU and verify 13 progress markers (CI)      |
| `cargo xtask qemu-smoke-nodisk` | Boot without a disk — verifies graceful degradation |
| `cargo xtask qemu-smoke-net`  | DHCP + TCP remote console + `ifconfig`/`ping` (CI) |
| `cargo xtask qemu-smoke-net-userspace` | `api/net` sockets: listen + connect (CI)  |
| `cargo xtask qemu-smoke-crypt` | Two-boot encrypted-storage test (format/seal/reboot/decrypt) |
| `cargo xtask qemu-bench`      | Kernel micro-benchmark perf gate (deterministic icount) |
| `cargo xtask qemu-screenshot` | Capture the framebuffer via QMP screendump → PNG   |
| `cargo xtask qemu-animation`  | Capture N FB frames over time → animated GIF       |
| `cargo xtask rpi5`  | Build `kernel8.img` for the Raspberry Pi 5 SD card           |
| `cargo xtask image` | Build m1n1 + loader + kernel payload (Apple M1)              |
| `cargo xtask run`   | Push to Apple M1 via m1n1 USB proxy                          |

---

## Platforms

| Platform           | Status     | Notes                                       |
| ------------------ | ---------- | ------------------------------------------- |
| **QEMU virt**      | ✅ Working | GICv3, PL011, virtio-blk, virtio-net        |
| **Raspberry Pi 5** | 🧩 Code complete | BCM2712; `cargo xtask rpi5` builds `kernel8.img`; physical-hardware validation pending (`docs/rpi5.md`) |
| **Apple M1**       | 🔜 In progress | T8103, AIC, m1n1 boot chain             |

---

## What's running

When you `cargo xtask qemu`, the following processes start:

| Process   | EL  | Description                                                 |
| --------- | --- | ----------------------------------------------------------- |
| Kernel    | EL1 | Xous microkernel: IPC, scheduler, memory manager            |
| `log`     | EL0 | Log sink                                                    |
| `procman` | EL0 | Process lifecycle: spawn, wait, exit                        |
| `shell`   | EL0 | Interactive shell (UART, TCP remote console, framebuffer)   |
| `fs`      | EL0 | Filesystem: ramfs (r/w) + tar disk (r/o) + encrypted `/data` (AES-256-GCM, r/w) |
| `block`   | EL0 | Block service: disk reads/writes, flush via kernel syscall  |
| `console` | EL0 | Console output fan-out: UART + TCP remote console           |

The shell communicates with `fs` and `procman` via Xous blocking IPC. The kernel routes UART IRQ characters to the shell via the console server.

A GUI desktop runs in parallel on the ramfb framebuffer with seven live windows: System Info, boot log, Game of Life, Snake, Mandelbrot fractal, Calculator, and Notes. The window manager (`beetos::gui::WindowManager`) lives kernel-side for now; the timer IRQ only *requests* a repaint at 10 Hz and the actual compose runs from the kernel idle loop with IRQs unmasked (see `docs/gui.md`). Keyboard input goes through the WM first (Tab cycles focus, Shift+arrows move the cursor, Ctrl+arrows drag the focused window, Shift+Enter clicks).

---

## Architecture: why Xous?

Xous is a production microkernel from [Betrusted](https://betrusted.io/) / [Foundation Devices](https://foundationdevices.com/), running on their Passport hardware wallet. It has a clean, message-passing IPC model and is proven in real-world security products.

We cherry-pick the platform-agnostic core (~7500 LOC) and replace the ARM Cortex-M hardware layer with our AArch64 implementation. The original code is not touched — we build on top of it.

Key design decisions inherited from Xous:

- **Capability-based IPC** — processes communicate via server IDs, not shared memory or file descriptors
- **Blocking message passing** — the sender blocks until the server processes the message (no polling, no callbacks)
- **No shared mutable state between processes** — enforced by the kernel's memory model

---

## Development workflow

**Hosted mode first.** All kernel logic, IPC, services, and the shell work as normal host processes. No hardware needed.

```bash
cargo test        # 200+ unit tests: ramfs, shell, syscalls, IPC, crypto, net
cargo run         # full OS as a host process, instant feedback
```

**QEMU next.** Tests real AArch64 code: page tables, exception vectors, GIC, context switch.

```bash
cargo xtask qemu  # any developer, any machine, no hardware
```

**Hardware last.** RPi5 and Apple M1 for platform-specific drivers.

---

## Code provenance

| Path                                                                 | Origin                              |
| -------------------------------------------------------------------- | ----------------------------------- |
| `xous/kernel/src/{syscall,server,services,mem,process,scheduler}.rs` | KeyOS (copied)                      |
| `xous/xous-rs/`                                                      | KeyOS (copied + AArch64 arch added) |
| `xous/kernel/src/arch/aarch64/`                                      | BeetOS (new)                        |
| `xous/kernel/src/platform/qemu_virt/`                                | BeetOS (new)                        |
| `xous/kernel/src/platform/bcm2712/`                                  | BeetOS (new)                        |
| `xous/kernel/src/platform/apple_t8103/`                              | BeetOS (new)                        |
| `api/`, `os/`, `apps/`, `beetos/`                                    | BeetOS (new)                        |

---

## License

MIT OR Apache-2.0 for BeetOS-original code. Copied Xous/KeyOS files retain their original license (MIT OR Apache-2.0 or GPL-3.0 per file). See `SPDX-FileCopyrightText` headers.
