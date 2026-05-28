# Raspberry Pi 5 (BCM2712) on BeetOS

This is the bring-up plan and status for the `bcm2712` platform.
QEMU's `raspi5` machine doesn't exist (the newest aarch64 raspi
machine QEMU 8.2 ships is `raspi3b`, BCM2837 — no GIC, different
MMIO layout), so end-to-end testing requires real RPi5 hardware.

## What works today

  - `cargo xtask rpi5` produces `kernel8.img` (raw flat binary,
    aarch64) ready to drop on an SD card boot partition.
  - Kernel-side bcm2712 platform module: PL011 UART driver (polled
    and IRQ-driven), GIC-v3 init shared with QEMU virt, ARM Generic
    Timer 100 Hz tick.
  - **FDT discovery**: `bcm2712::init()` now reads the FDT physical
    address passed by the firmware in x0, parses it with the same
    `arch::aarch64::boot::parse_fdt_mmio` that qemu_virt uses, and
    looks up the UART (`arm,pl011`) and GIC (`arm,gic-v3`) base
    addresses. Compiled-in defaults are the fallback when the FDT
    is missing or doesn't advertise those nodes.
  - Linker script `link-bcm2712.x` places `.boot.bss` adjacent to
    `.text.boot` so `_create_boot_page_tables`'s `adr` instructions
    stay in the ±1 MB relocation range — previously this failed to
    link.

## How to deploy on real hardware

```bash
cargo xtask rpi5
# Then on the host:
cp kernel8.img /Volumes/bootfs/kernel8.img
```

Add this to `config.txt` on the boot partition:

```ini
arm_64bit=1
kernel=kernel8.img
# Skip the firmware's framebuffer setup if you only want UART output
disable_overscan=1
```

With a USB-TTL serial cable on GPIO 14/15 (UART0), you should see the
boot banner on 115200 8N1.

## What's missing for "RPi5 fully usable"

The order matches `rpi5.md` in the project root.

### Prerequisite: PCIe + RP1

Everything below Tier 1 on a real RPi5 sits behind the **RP1**
southbridge, which talks to BCM2712 over PCIe Gen 2 x4. Without a
PCIe host controller driver and the RP1 init sequence, the kernel
sees zero of the GPIO / USB / Ethernet / second-stage UART
peripherals. This is the biggest single chunk of work — the Linux
`pcie-brcmstb` driver is the reference, plus Raspberry Pi's open
`rp1` driver tree.

### Tier 1 (minimal usable boot)

  - **Framebuffer (HDMI)** — bootloader (`start4.elf`) initialises
    HDMI and writes the FB descriptor into the Mailbox property
    interface (BCM2835 mbox, addr `0x107C013880` on BCM2712).
    BeetOS just needs to read the address back and treat it as a
    regular pixel buffer. Should be reusable with the existing
    `beetos::gfx` / `beetos::gui` stack once the FB pointer is in
    hand.
  - **UART (RP1)** — once PCIe + RP1 land, the standard GPIO 14/15
    UART becomes available too. Until then, the kernel can talk
    over the BCM2712 mini-UART that we initialise at boot.
  - **SD card (BCM2712 SD0)** — direct on the SoC, not via RP1. The
    Linux `bcm2835-sdhost` driver is the reference; ~1500 LoC of C
    that maps cleanly onto the same virtio-blk request flow we
    already have in the qemu_virt path.

### Tier 2 (interactive)

  - GPIO 40-pin header (via RP1) — gates SPI / UART2 / I2C / PWM
    / I2S
  - USB 2 + USB 3 (via RP1)
  - Gigabit Ethernet (via RP1)

### Tier 3 (advanced)

  - MIPI CSI / DSI (camera, official RPi display)
  - PCIe Gen 2 x1 external (NVMe slot)
  - PWM fan control, 12-bit ADC

### Tier 4 (complete)

  - VideoCore VII GPU — accel for the `wgpu_compat` backend
    described in [`wgpu-roadmap.md`](wgpu-roadmap.md). The Mesa
    `v3d` driver is the reference, plus the kernel-side Linux v3d
    driver — a Rust port is the BeetOS path.
  - Hardware H.264 / HEVC decode
  - Bluetooth + WiFi (separate Broadcom chip over SDIO)

## Testing without real hardware

QEMU 8.2 in Debian/Ubuntu ships `raspi3b` (BCM2837) but **not**
`raspi4b` (BCM2711, which would be the closest emulated cousin of
BCM2712 because both have GIC). Options if you want a QEMU-only
loop:

1. Build QEMU from source with the `raspi4b` machine enabled and
   point `xtask` at it. The patches for raspi4b in upstream are
   tagged through QEMU 7+; the Debian builds disable it as
   experimental.
2. Use `raspi3b`. This needs a BCM2836-style interrupt controller
   in BeetOS (no GIC, peripherals on the ARM_LOCAL block). Closer
   to bcm2711 than bcm2712 in spirit; not great preparation for
   real RPi5 work.
3. Keep developing the platform stack with `cargo xtask rpi5`
   (compiles cleanly, FDT-aware, identical kernel image structure
   to a real-hardware deploy) and flash to a real RPi5 to test.
   Iteration is slow (SD card swap) but architecturally honest.

This is the strategy DISPLAY.md and rpi5.md endorse — the
"framebuffer suffices for the next milestones" path so we don't
need an emulator to make progress on the GUI side.

## Next milestone-level steps

1. **Mailbox driver** (~200 LoC) — read the firmware-initialised
   FB descriptor, expose it through `beetos::gui::Surface`.
2. **PCIe controller driver** (`brcm,bcm2712-pcie`) — the gate to
   all RP1 work. This is the months-of-work item.
3. **RP1 chip driver** — large but mostly mailbox-style register
   pokes once PCIe is up.
4. **BCM2712 SDHCI driver** — independent of PCIe/RP1; gives us
   storage on the SoC's native SD controller.
