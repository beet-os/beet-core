// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Common constants for BeetOS — memory map, page size, addresses.
//!
//! This crate is the BeetOS equivalent of KeyOS's `keyos` crate.
//! All addresses are for AArch64 with 16KB pages (Apple Silicon).
//!
//! # AArch64 Address Space
//!
//! With `TCR_EL1.T0SZ = T1SZ = 17`, AArch64 gives us a 47-bit VA space
//! split into two halves by the hardware:
//!
//! - **TTBR0 (lower half):** `0x0000_0000_0000_0000 .. 0x0000_7FFF_FFFF_FFFF`
//!   Per-process user space. Switched on every context switch.
//!
//! - **TTBR1 (upper half):** `0xFFFF_8000_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF`
//!   Kernel space. Set once at boot, never changed. The kernel uses a
//!   **linear map**: `kernel_VA = PA + KERNEL_VA_OFFSET`.
//!
//! Addresses between the two halves (bit 47 set but upper bits not all-ones)
//! generate a translation fault — this is the "VA hole" that separates user
//! and kernel space.
//!
//! # Page Table Geometry (16KB granule)
//!
//! Each 16KB page table holds 2048 entries (8 bytes each). The 47-bit VA
//! is decoded as: L1[11 bits] → L2[11 bits] → L3[11 bits] → offset[14 bits].
//! See `xous/kernel/src/arch/aarch64/mem.rs` for the full page table
//! documentation including PTE format, MAIR configuration, and W^X enforcement.

#![no_std]

#[cfg(feature = "fb")]
pub mod font;
#[cfg(feature = "fb")]
pub mod fb_console;

/// AArch64 translation granule — set by the `page-4k` / `page-16k` / `page-64k` feature.
/// Exactly one feature must be enabled. Default: `page-16k` (Apple Silicon).
#[cfg(feature = "page-4k")]
pub const PAGE_SIZE: usize = 4096;
#[cfg(feature = "page-16k")]
pub const PAGE_SIZE: usize = 16384;
#[cfg(feature = "page-64k")]
pub const PAGE_SIZE: usize = 65536;

/// Log2 of PAGE_SIZE (12 / 14 / 16 for 4KB / 16KB / 64KB).
#[cfg(feature = "page-4k")]
pub const PAGE_SHIFT: usize = 12;
#[cfg(feature = "page-16k")]
pub const PAGE_SHIFT: usize = 14;
#[cfg(feature = "page-64k")]
pub const PAGE_SHIFT: usize = 16;

// ======================== User-accessible addresses (EL0, TTBR0) ========================
//
// All addresses in this section are in the lower VA half (bit 47 = 0), mapped
// through TTBR0_EL1 which is per-process. Page table entries have nG=1 (non-global)
// so TLB entries are tagged with the process ASID.
//
// Layout (low to high):
//   0x0000_0000_0000_0000  Null guard page (unmapped, catches null derefs)
//   0x0000_0001_0000       ASLR region start (code, heap, data)
//   0x0000_3000_0000_0000  mmap area
//   0x0000_4000_0000_0000  Memory mirror (CoW, shared memory)
//   0x0000_5000_0000_0000  Raw ELF temp area
//   0x0000_5800_0000_0000  Kernel argument table (PID1 only)
//   0x0000_6FE0_0000_0000  User IRQ stack
//   0x0000_6FF0_0000_0000  User stack
//   0x0000_7000_0000_0000  End of user area (thread context above, kernel-only)

/// Start of ASLR range for user processes.
pub const ASLR_START: usize = 0x0000_0001_0000;

/// End of ASLR range for user processes.
pub const ASLR_END: usize = 0x0000_3000_0000_0000;

/// Virtual address for mmap-style memory allocations.
pub const MMAP_AREA_VIRT: usize = 0x0000_3000_0000_0000;
pub const MMAP_AREA_VIRT_END: usize = 0x0000_4000_0000_0000;

/// Memory mirror area (used for copy-on-write and shared memory).
pub const MEMORY_MIRROR_AREA_VIRT: usize = 0x0000_4000_0000_0000;

/// Temporary address for loading raw ELF binaries.
pub const RAW_ELF_TEMPORARY_ADDRESS: usize = 0x0000_5000_0000_0000;

/// Kernel argument table (only mapped to PID1).
pub const KERNEL_ARGUMENT_OFFSET: usize = 0x0000_5800_0000_0000;

/// User IRQ stack.
pub const USER_IRQ_STACK_BOTTOM: usize = 0x0000_6FE0_0000_0000;
pub const USER_IRQ_STACK_PAGE_COUNT: usize = 3;

/// User stack.
pub const USER_STACK_BOTTOM: usize = 0x0000_6FF0_0000_0000;
pub const STACK_PAGE_COUNT: usize = 256; // 4MB — std init + dlmalloc need >1MB
pub const USER_STACK_TOP_GUARD: usize = USER_STACK_BOTTOM - PAGE_SIZE * (STACK_PAGE_COUNT + 1);

/// End of user-accessible virtual address space.
pub const USER_AREA_END: usize = 0x0000_7000_0000_0000;

// ======================== Per-process kernel-accessible addresses ========================

/// Thread context area (kernel-only, per-process).
pub const THREAD_CONTEXT_AREA: usize = 0x0000_7000_0000_0000;

// ======================== Global kernel-accessible addresses (TTBR1 / upper half) ========================
//
// All addresses in this section are in the upper VA half (bits [63:47] all ones),
// mapped through TTBR1_EL1 which is set once at boot and never changed.
// Page table entries are global (nG=0) — TLB entries survive context switches.
//
// The kernel uses a SIMPLE LINEAR MAP: kernel_VA = PA + 0xFFFF_8000_0000_0000.
// This means the kernel can access any physical address (RAM or MMIO) by adding
// the offset. No per-address page table setup is needed — the boot code maps the
// entire physical address range at this offset using L2 block descriptors.
//
// Layout (low to high within upper half):
//   0xFFFF_8000_0000_0000  Linear map base (PA 0x0 maps here)
//   0xFFFF_A000_0000_0000  Physical RAM identity region
//   0xFFFF_FF80_0000_8000  Allocation tracker bitmaps
//   0xFFFF_FFFF_FFD0_0000  Kernel code (.text, .rodata, .data)
//   0xFFFF_FFFF_FFF8_0000  Kernel stack (256KB)
//   0xFFFF_FFFF_FFFE_4000  IRQ stack (64KB)
//   0xFFFF_FFFF_FFFF_0000  Exception stack (128KB)

/// Start of kernel virtual address space (upper half).
pub const KERNEL_VIRT_BASE: usize = 0xFFFF_8000_0000_0000;

/// Offset between physical addresses and kernel virtual addresses.
///
/// The kernel uses a **linear map**: `kernel_VA = PA + KERNEL_VA_OFFSET`.
/// TTBR1 maps all physical RAM and MMIO at this offset so the kernel
/// can access any physical address without going through TTBR0.
///
/// This is the fundamental mechanism by which the kernel manipulates user
/// page tables: it reads/writes page table pages at their physical address
/// + this offset, which goes through TTBR1 regardless of TTBR0 state.
///
/// TTBR0 is reserved for user process page tables and changes on every
/// context switch. TTBR1 (kernel) never changes.
pub const KERNEL_VA_OFFSET: usize = KERNEL_VIRT_BASE; // 0xFFFF_8000_0000_0000

/// Convert a physical address to its kernel virtual address (TTBR1 linear map).
///
/// Example: `phys_to_virt(0x4000_0000)` → `0xFFFF_8000_4000_0000`
///
/// The returned VA goes through TTBR1, which maps all physical memory.
/// This works regardless of the current TTBR0 (user process) setting.
#[inline]
pub const fn phys_to_virt(pa: usize) -> usize {
    pa.wrapping_add(KERNEL_VA_OFFSET)
}

/// Convert a kernel virtual address back to its physical address.
///
/// Only valid for addresses in the TTBR1 linear map range.
/// Example: `virt_to_phys(0xFFFF_8000_4000_0000)` → `0x4000_0000`
#[inline]
pub const fn virt_to_phys(va: usize) -> usize {
    va.wrapping_sub(KERNEL_VA_OFFSET)
}

/// Physical RAM identity-mapped into kernel space.
pub const MAPPED_PHYSICAL_RAM: usize = 0xFFFF_A000_0000_0000;

/// Allocation tracker.
pub const ALLOCATION_TRACKER_OFFSET: usize = 0xFFFF_FF80_0000_8000;
pub const ALLOCATION_TRACKER_PAGES_MAX: usize = 32;

/// Kernel code load address.
pub const KERNEL_LOAD_OFFSET: usize = 0xFFFF_FFFF_FFD0_0000;
pub const NUM_KERNEL_PAGES_MAX: usize = 128;

/// Kernel stack.
pub const KERNEL_STACK_BOTTOM: usize = 0xFFFF_FFFF_FFF8_0000;
pub const KERNEL_STACK_PAGE_COUNT: usize = 16;
pub const KERNEL_STACK_TOP_GUARD: usize = KERNEL_STACK_BOTTOM - PAGE_SIZE * (KERNEL_STACK_PAGE_COUNT + 1);

/// IRQ stack.
pub const IRQ_STACK_BOTTOM: usize = 0xFFFF_FFFF_FFFE_4000;
pub const IRQ_STACK_PAGE_COUNT: usize = 4;
pub const IRQ_STACK_TOP_GUARD: usize = IRQ_STACK_BOTTOM - PAGE_SIZE * (IRQ_STACK_PAGE_COUNT + 1);

/// Aliases for compatibility with copied kernel code (services.rs).
pub const KERNEL_IRQ_HANDLER_STACK_BOTTOM: usize = IRQ_STACK_BOTTOM;
pub const KERNEL_IRQ_HANDLER_STACK_PAGE_COUNT: usize = IRQ_STACK_PAGE_COUNT;

/// Exception stack.
pub const EXCEPTION_STACK_BOTTOM: usize = 0xFFFF_FFFF_FFFF_0000;
pub const EXCEPTION_STACK_PAGE_COUNT: usize = 8;
pub const EXCEPTION_STACK_TOP_GUARD: usize =
    EXCEPTION_STACK_BOTTOM - PAGE_SIZE * (EXCEPTION_STACK_PAGE_COUNT + 1);

// ======================== Physical addresses (from FDT at runtime) ========================
//
// NOTE: Unlike KeyOS, BeetOS does NOT hardcode peripheral physical addresses.
// All MMIO base addresses come from the Flattened Device Tree (FDT) passed by m1n1.
// The constants below are for RAM layout only.

// ======================== RAM size and base — platform-specific ========================
//
// These constants define the physical RAM layout. The actual RAM size is
// discovered from FDT at boot time, but the allocation tracker bitmaps
// are sized at compile time, so we need a max-RAM-size constant.

/// Maximum RAM size for this platform.
/// On QEMU virt: 1 GiB max (default -m 512M, max practical ~1G).
/// On Apple M1: 8 GiB (MacBook Air) or 16 GiB.
#[cfg(feature = "platform-qemu-virt")]
pub const RAM_SIZE: usize = 1 * 1024 * 1024 * 1024; // 1 GiB

#[cfg(feature = "platform-bcm2712")]
pub const RAM_SIZE: usize = 8 * 1024 * 1024 * 1024; // 8 GiB (RPi5 max)

#[cfg(feature = "platform-apple-t8103")]
pub const RAM_SIZE: usize = 8 * 1024 * 1024 * 1024; // 8 GiB

#[cfg(not(any(feature = "platform-qemu-virt", feature = "platform-bcm2712", feature = "platform-apple-t8103")))]
pub const RAM_SIZE: usize = 1 * 1024 * 1024 * 1024; // 1 GiB default

/// Number of pages in max RAM configuration.
pub const RAM_PAGES: usize = RAM_SIZE / PAGE_SIZE;

// ======================== Physical RAM layout ========================
//
// On Apple Silicon, there is no encrypted DRAM concept like ATSAMA5D2.
// We provide these constants as no-ops for compatibility with the copied
// kernel code (mem.rs, process.rs). The actual RAM base comes from FDT.

/// Physical RAM base address.
/// QEMU virt: 0x4000_0000 (1 GiB mark — from hw/arm/virt.c).
/// Apple M1: 0x8_0000_0000 (34 GiB mark — above I/O MMIO aperture).
#[cfg(feature = "platform-qemu-virt")]
pub const PLAINTEXT_DRAM_BASE: usize = 0x4000_0000;

// RPi5 (BCM2712): RAM starts at physical 0x0.
// The firmware reserves the first ~512KB for its own use; the kernel loads
// at 0x80000 (RPi convention). FDT describes the usable ranges at runtime.
#[cfg(feature = "platform-bcm2712")]
pub const PLAINTEXT_DRAM_BASE: usize = 0x0000_0000;

#[cfg(feature = "platform-apple-t8103")]
pub const PLAINTEXT_DRAM_BASE: usize = 0x8_0000_0000;

#[cfg(not(any(feature = "platform-qemu-virt", feature = "platform-bcm2712", feature = "platform-apple-t8103")))]
pub const PLAINTEXT_DRAM_BASE: usize = 0x4000_0000; // default to QEMU

/// End of physical RAM.
pub const PLAINTEXT_DRAM_END: usize = PLAINTEXT_DRAM_BASE + RAM_SIZE;

/// On Apple Silicon there is no hardware encrypted DRAM region.
/// These aliases exist for compatibility with copied kernel code.
pub const ENCRYPTED_DRAM_BASE: usize = PLAINTEXT_DRAM_BASE;
pub const ENCRYPTED_DRAM_END: usize = PLAINTEXT_DRAM_END;

/// Convert to "encrypted" physical address (no-op on Apple Silicon).
pub fn to_encrypted_phys_addr(addr: usize) -> usize { addr }

/// Convert to plaintext physical address (no-op on Apple Silicon).
pub fn to_plaintext_phys_addr(addr: usize) -> usize { addr }

/// Check if address is in the "encrypted" region (always same as plaintext on M1).
pub fn is_address_encrypted(addr: usize) -> bool {
    (PLAINTEXT_DRAM_BASE..PLAINTEXT_DRAM_END).contains(&addr)
}

/// Check if address is in physical DRAM.
pub fn is_address_in_plaintext_dram(addr: usize) -> bool {
    (PLAINTEXT_DRAM_BASE..PLAINTEXT_DRAM_END).contains(&addr)
}

// ======================== Loader ========================

/// Loader code address (will be set properly for m1n1 payload).
pub const LOADER_CODE_ADDRESS: usize = PLAINTEXT_DRAM_BASE;

/// Boot splash framebuffer pages.
pub const BOOT_SPLASH_FB: usize = 0x0000_5000_0000_0000;

// ======================== Argv ========================

/// Virtual address where the kernel maps the argv page for spawned processes.
/// The page is mapped read-only and contains null-separated argument strings.
/// x1 = ARGV_PAGE_VA, x2 = total byte length of argv data.
pub const ARGV_PAGE_VA: usize = 0x10_0300_0000;

/// Virtual address at which the kernel maps the UART MMIO page into userspace.
/// Passed to the shell (and other processes) as x0 at process start.
pub const SHELL_UART_VA: usize = 0x10_0100_0000;

/// Virtual address at which the kernel maps the framebuffer into the shell.
/// Passed as x1 at process start. The FB is 4 MiB (256 × 16 KiB pages).
#[cfg(feature = "platform-qemu-virt")]
pub const SHELL_FB_VA: usize = 0x10_0200_0000;

/// Virtual address of the shared framebuffer cursor page.
///
/// Mapped at a fixed VA in every process that writes to the framebuffer
/// (shell, log server, spawned apps). Layout: `[0..4]` = row (u32 LE),
/// `[4..8]` = col (u32 LE). Processes must sync this page before and
/// after FB writes to keep cursor state consistent across processes.
pub const SHARED_CURSOR_VA: usize = 0x10_0240_0000;

/// Maximum size of argv data (fits in one page).
pub const ARGV_MAX_LEN: usize = PAGE_SIZE;
