// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Common constants for BeetOS — memory map, page size, addresses.
//!
//! This crate is the BeetOS equivalent of KeyOS's `keyos` crate.
//! All addresses are for AArch64 with 16KB pages (Apple Silicon).

#![no_std]

/// Apple Silicon uses 16KB pages.
pub const PAGE_SIZE: usize = 16384;

/// Log2 of PAGE_SIZE (14 for 16KB).
pub const PAGE_SHIFT: usize = 14;

// ======================== User-accessible addresses (EL0) ========================
//
// AArch64 with 4-level page tables and 16KB granule gives us 47-bit VA space.
// TTBR0_EL1 covers the lower half: 0x0000_0000_0000 .. 0x0000_7FFF_FFFF_FFFF
// TTBR1_EL1 covers the upper half: 0xFFFF_8000_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF

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
pub const STACK_PAGE_COUNT: usize = 64;
pub const USER_STACK_TOP_GUARD: usize = USER_STACK_BOTTOM - PAGE_SIZE * (STACK_PAGE_COUNT + 1);

/// End of user-accessible virtual address space.
pub const USER_AREA_END: usize = 0x0000_7000_0000_0000;

// ======================== Per-process kernel-accessible addresses ========================

/// Thread context area (kernel-only, per-process).
pub const THREAD_CONTEXT_AREA: usize = 0x0000_7000_0000_0000;

// ======================== Global kernel-accessible addresses (TTBR1 / upper half) ========================

/// Start of kernel virtual address space (upper half).
pub const KERNEL_VIRT_BASE: usize = 0xFFFF_8000_0000_0000;

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

/// Apple M1 MacBook Air has 8GB or 16GB of unified memory.
/// Actual size is discovered from FDT at boot time.
/// This is a conservative default for the allocation tracker.
pub const RAM_SIZE_DEFAULT: usize = 8 * 1024 * 1024 * 1024; // 8 GiB

/// RAM size (alias for compatibility with kernel code).
pub const RAM_SIZE: usize = RAM_SIZE_DEFAULT;

/// Number of pages in default RAM configuration.
pub const RAM_PAGES: usize = RAM_SIZE / PAGE_SIZE;

// ======================== Physical RAM layout ========================
//
// On Apple Silicon, there is no encrypted DRAM concept like ATSAMA5D2.
// We provide these constants as no-ops for compatibility with the copied
// kernel code (mem.rs, process.rs). The actual RAM base comes from FDT.

/// Physical RAM base (default, actual from FDT).
pub const PLAINTEXT_DRAM_BASE: usize = 0x0_8000_0000; // 2 GiB (typical M1 DRAM start)

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
