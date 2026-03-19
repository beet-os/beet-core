// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Early boot: FDT parsing, MMU enable, MemoryManager initialization.
//!
//! Called from `_start_rust` before anything else that needs virtual memory.
//! At this point the MMU is OFF — all addresses are physical.
//!
//! # Boot sequence
//!
//! 1. Parse FDT (from QEMU or m1n1) to discover RAM base + size
//! 2. Place page tracker + bootstrap page tables after kernel `_end`
//! 3. Build identity-map page tables (VA = PA) using L2 block descriptors
//! 4. Enable MMU (MAIR, TCR, TTBR0, SCTLR)
//! 5. Initialize MemoryManager with the page tracker
//!
//! After this, the kernel continues running at physical addresses (identity
//! mapped via TTBR0). User processes will get their own TTBR0 with kernel
//! pages mapped as EL1-only (inaccessible from EL0).

use core::arch::asm;

use beetos::PAGE_SIZE;

use super::mem::{MAIR_VALUE, TCR_VALUE};

// ============================================================================
// Linker symbols
// ============================================================================

extern "C" {
    static _start: u8;
    static _end: u8;
}

/// Get the kernel's physical start address.
fn kernel_start() -> usize {
    unsafe { &_start as *const u8 as usize }
}

/// Get the kernel's physical end address (first byte after kernel image + stack).
fn kernel_end() -> usize {
    unsafe { &_end as *const u8 as usize }
}

// ============================================================================
// Minimal FDT parser
// ============================================================================

/// RAM region discovered from FDT.
pub struct RamRegion {
    pub base: usize,
    pub size: usize,
}

// FDT tokens
const FDT_MAGIC: u32 = 0xD00D_FEED;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// Read a big-endian u32 from a raw pointer.
unsafe fn be32(ptr: *const u8) -> u32 {
    u32::from_be_bytes([*ptr, *ptr.add(1), *ptr.add(2), *ptr.add(3)])
}

/// Read a big-endian u64 from a raw pointer.
unsafe fn be64(ptr: *const u8) -> u64 {
    u64::from_be_bytes([
        *ptr,
        *ptr.add(1),
        *ptr.add(2),
        *ptr.add(3),
        *ptr.add(4),
        *ptr.add(5),
        *ptr.add(6),
        *ptr.add(7),
    ])
}

/// Compare a null-terminated C string at `ptr` with a Rust byte slice.
unsafe fn cstr_starts_with(ptr: *const u8, prefix: &[u8]) -> bool {
    for (i, &b) in prefix.iter().enumerate() {
        if *ptr.add(i) != b {
            return false;
        }
    }
    true
}

/// Parse the FDT to find the first /memory node's `reg` property.
///
/// Assumes `#address-cells = 2` and `#size-cells = 2` (standard for QEMU virt).
///
/// # Safety
///
/// `fdt_ptr` must point to a valid FDT blob.
pub unsafe fn parse_fdt_ram(fdt_ptr: *const u8) -> Option<RamRegion> {
    let magic = be32(fdt_ptr);
    if magic != FDT_MAGIC {
        return None;
    }

    let off_dt_struct = be32(fdt_ptr.add(8)) as usize;
    let off_dt_strings = be32(fdt_ptr.add(12)) as usize;

    let struct_base = fdt_ptr.add(off_dt_struct);
    let strings_base = fdt_ptr.add(off_dt_strings);

    let mut pos: usize = 0;
    let mut depth: usize = 0;
    let mut in_memory_node = false;

    loop {
        let token = be32(struct_base.add(pos));
        pos += 4;

        match token {
            FDT_BEGIN_NODE => {
                depth += 1;
                // Node name is a null-terminated string, 4-byte aligned
                let name_ptr = struct_base.add(pos);
                let mut name_len = 0;
                while *struct_base.add(pos + name_len) != 0 {
                    name_len += 1;
                }
                pos += name_len + 1; // skip null terminator
                pos = (pos + 3) & !3; // align to 4

                // /memory or /memory@XXXXXXXX at depth 2 (root is depth 1)
                if depth == 2 && cstr_starts_with(name_ptr, b"memory") {
                    in_memory_node = true;
                }
            }
            FDT_END_NODE => {
                if in_memory_node && depth == 2 {
                    in_memory_node = false;
                }
                depth -= 1;
            }
            FDT_PROP => {
                let len = be32(struct_base.add(pos)) as usize;
                let nameoff = be32(struct_base.add(pos + 4)) as usize;
                pos += 8; // skip len + nameoff

                if in_memory_node && len >= 16 {
                    let prop_name = strings_base.add(nameoff);
                    if cstr_starts_with(prop_name, b"reg\0") {
                        let val = struct_base.add(pos);
                        let base = be64(val) as usize;
                        let size = be64(val.add(8)) as usize;
                        return Some(RamRegion { base, size });
                    }
                }

                pos += len;
                pos = (pos + 3) & !3; // align to 4
            }
            FDT_NOP => {}
            FDT_END => break,
            _ => break,
        }
    }

    None
}

// ============================================================================
// Bootstrap page tables (identity map with L2 blocks)
// ============================================================================

// L2 block = 32 MiB with 16KB granule
const L2_BLOCK_SIZE: usize = 32 * 1024 * 1024;
const L2_BLOCK_ADDR_MASK: u64 = 0x0000_FFFF_FE00_0000; // bits [47:25]

// Descriptor bits
const DESC_VALID: u64 = 1 << 0;
const DESC_TABLE: u64 = 1 << 1; // L1/L2 table descriptor: bits[1:0] = 0b11
const DESC_BLOCK: u64 = DESC_VALID; // L2 block descriptor: bits[1:0] = 0b01 (valid, NOT table)

// Page table entry attributes (same definitions as mem.rs, duplicated to avoid
// coupling boot code to the full MemoryMapping infrastructure)
const ATTR_IDX_DEVICE: u64 = 0 << 2; // MAIR index 0: Device-nGnRnE
const ATTR_IDX_NORMAL: u64 = 1 << 2; // MAIR index 1: Normal WB cacheable
const ATTR_AF: u64 = 1 << 10; // Access flag
const ATTR_SH_ISH: u64 = 0b11 << 8; // Inner-shareable
const ATTR_AP_RW_EL1: u64 = 0b00 << 6; // Read/write at EL1 only
const ATTR_UXN: u64 = 1 << 54; // User execute-never
const ATTR_PXN: u64 = 1 << 53; // Privileged execute-never

/// Normal memory block attributes: RWX at EL1, no user access.
/// The entire RAM identity map is kernel-only. We use L2 blocks (32MB)
/// so we can't separate text/data permissions — that requires L3 pages.
/// PXN=0 allows EL1 execution. UXN=1 blocks EL0 execution.
const BLOCK_NORMAL_RWX: u64 =
    DESC_BLOCK | ATTR_IDX_NORMAL | ATTR_AF | ATTR_SH_ISH | ATTR_AP_RW_EL1 | ATTR_UXN;

/// Device memory block attributes: RW at EL1, no user access, no execute, no cache.
const BLOCK_DEVICE_RW: u64 =
    DESC_BLOCK | ATTR_IDX_DEVICE | ATTR_AF | ATTR_SH_ISH | ATTR_AP_RW_EL1 | ATTR_UXN | ATTR_PXN;

/// Number of L1 entries (16KB / 8 bytes = 2048).
const TABLE_ENTRIES: usize = 2048;

/// Bump allocator for bootstrap page allocation (before MemoryManager exists).
struct BumpAllocator {
    next: usize,
}

impl BumpAllocator {
    /// Create a new bump allocator starting at the given physical address.
    fn new(start: usize) -> Self {
        Self { next: Self::align_up(start) }
    }

    fn align_up(addr: usize) -> usize {
        (addr + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
    }

    /// Allocate one zeroed 16KB page.
    fn alloc_page(&mut self) -> usize {
        let page = self.next;
        self.next += PAGE_SIZE;
        unsafe { core::ptr::write_bytes(page as *mut u8, 0, PAGE_SIZE) };
        page
    }

    /// Current high-water mark.
    fn current(&self) -> usize {
        self.next
    }
}

/// Information about the bootstrap memory layout, returned to the caller
/// so the MemoryManager can be initialized.
#[allow(dead_code)]
pub struct BootInfo {
    /// RAM base address (from FDT).
    pub ram_base: usize,
    /// RAM size in bytes (from FDT, capped to beetos::RAM_SIZE).
    pub ram_size: usize,
    /// Physical address of the page tracker (allocations array).
    pub page_tracker_base: usize,
    /// Size of page tracker in bytes.
    pub page_tracker_size: usize,
    /// Physical address of the L1 page table (set as TTBR0_EL1).
    pub ttbr0: usize,
    /// First free address after all bootstrap allocations.
    pub bootstrap_end: usize,
}

/// Set up identity-map page tables and enable the MMU.
///
/// # Safety
///
/// Must be called exactly once, early in boot, with MMU off.
/// `fdt_ptr` must point to a valid FDT blob or be null (uses defaults).
pub unsafe fn enable_mmu(fdt_ptr: *const u8) -> BootInfo {
    // 1. Discover RAM from FDT (or use platform defaults)
    let (ram_base, ram_size_raw) = if !fdt_ptr.is_null() {
        parse_fdt_ram(fdt_ptr)
            .map(|r| (r.base, r.size))
            .unwrap_or((beetos::PLAINTEXT_DRAM_BASE, beetos::RAM_SIZE))
    } else {
        (beetos::PLAINTEXT_DRAM_BASE, beetos::RAM_SIZE)
    };

    // Cap to compile-time max (bitmap size is fixed)
    let ram_size = ram_size_raw.min(beetos::RAM_SIZE);

    // Verify kernel is within RAM
    let k_start = kernel_start();
    let k_end = kernel_end();
    assert!(k_start >= ram_base && k_end <= ram_base + ram_size,
        "kernel not within discovered RAM");

    // 2. Set up bump allocator after kernel _end
    let mut bump = BumpAllocator::new(k_end);

    // 3. Allocate page tracker (Option<PID> = 2 bytes per page)
    let num_pages = ram_size / PAGE_SIZE;
    let page_tracker_size = num_pages * 2; // sizeof(Option<PID>) = 2
    let page_tracker_base = bump.current();
    // Advance bump past page tracker (round up to page boundary)
    bump.next = BumpAllocator::align_up(page_tracker_base + page_tracker_size);
    // Zero the page tracker
    core::ptr::write_bytes(page_tracker_base as *mut u8, 0, bump.current() - page_tracker_base);

    // 4. Allocate page tables: 1 L1 + N L2 tables
    let l1_page = bump.alloc_page();
    let l1_table = l1_page as *mut u64;

    // We need L2 tables for MMIO and RAM. Each L1 entry covers 64 GiB.
    //
    // QEMU virt: MMIO (0..0x4000_0000) + RAM (0x4000_0000+) — both in L1[0].
    // Apple M1: RAM at 0x8_0000_0000 — still in L1[0] (0..64GiB).
    // BCM2712 (RPi5): RAM at 0x0, peripherals at ~66GiB → needs L1[0] + L1[1].
    let l2_page = bump.alloc_page();
    let l2_table = l2_page as *mut u64;

    // Wire L1[0] → L2 table (covers VA 0..64 GiB)
    let l1_desc = (l2_page as u64 & super::mem::PTE_ADDR_MASK) | DESC_VALID | DESC_TABLE;
    core::ptr::write_volatile(l1_table.add(0), l1_desc);

    // 5. Map MMIO region below RAM as device memory (L2 blocks).
    // QEMU virt: 0..0x4000_0000 (GIC + UART below RAM).
    // BCM2712: RAM starts at 0x0, so this range is empty (no-op).
    if ram_base > 0 {
        let mmio_blocks = ram_base / L2_BLOCK_SIZE;

        for i in 0..mmio_blocks {
            let phys = i * L2_BLOCK_SIZE;
            let l2_idx = phys / L2_BLOCK_SIZE;
            let desc = (phys as u64 & L2_BLOCK_ADDR_MASK) | BLOCK_DEVICE_RW;
            core::ptr::write_volatile(l2_table.add(l2_idx), desc);
        }
    }

    // 6. Map RAM as normal memory (L2 blocks).
    let ram_blocks = (ram_size + L2_BLOCK_SIZE - 1) / L2_BLOCK_SIZE;

    for i in 0..ram_blocks {
        let phys = ram_base + i * L2_BLOCK_SIZE;
        let l2_idx = phys / L2_BLOCK_SIZE;

        if l2_idx < TABLE_ENTRIES {
            let desc = (phys as u64 & L2_BLOCK_ADDR_MASK) | BLOCK_NORMAL_RWX;
            core::ptr::write_volatile(l2_table.add(l2_idx), desc);
        }
    }

    // 6b. BCM2712: map high peripherals (>64GiB) in L1[1].
    // UART0 at 0x107D001000 and GIC at 0x107FFF9000 are both above 64GiB,
    // in L1[1] (covers 64GiB..128GiB). We map two 32MB blocks covering them.
    #[cfg(feature = "platform-bcm2712")]
    {
        // L1[1] covers physical 0x10_0000_0000..0x20_0000_0000 (64GiB..128GiB).
        const L1_1_BASE: usize = 64 * 1024 * 1024 * 1024; // 64GiB
        let l2_hi_page = bump.alloc_page();
        let l2_hi_table = l2_hi_page as *mut u64;
        let l1_hi_desc = (l2_hi_page as u64 & super::mem::PTE_ADDR_MASK) | DESC_VALID | DESC_TABLE;
        core::ptr::write_volatile(l1_table.add(1), l1_hi_desc);

        // Map blocks 62..=63 within L1[1]'s L2 table:
        //   block 62: 0x107C000000..0x107DFFFFFF (covers UART at 0x107D001000)
        //   block 63: 0x107E000000..0x107FFFFFFF (covers GIC  at 0x107FFF9000)
        for block in 62usize..=63 {
            let phys = L1_1_BASE + block * L2_BLOCK_SIZE;
            let desc = (phys as u64 & L2_BLOCK_ADDR_MASK) | BLOCK_DEVICE_RW;
            core::ptr::write_volatile(l2_hi_table.add(block), desc);
        }
    }

    // 7. Set up MMU system registers
    // MAIR_EL1: memory attribute indirection register
    asm!("msr mair_el1, {}", in(reg) MAIR_VALUE, options(nomem, nostack));

    // TCR_EL1: translation control register
    asm!("msr tcr_el1, {}", in(reg) TCR_VALUE, options(nomem, nostack));

    // TTBR0_EL1: user/kernel page table base (identity map for now)
    // ASID = 0 in upper bits (kernel uses ASID 0)
    asm!("msr ttbr0_el1, {}", in(reg) l1_page as u64, options(nomem, nostack));

    // TTBR1_EL1: kernel upper-half page table (empty for now — we'll use it
    // later when the kernel is re-linked at upper VA). Set to same L1 to avoid
    // faults on accidental upper-half accesses.
    asm!("msr ttbr1_el1, {}", in(reg) l1_page as u64, options(nomem, nostack));

    // Ensure all writes to page tables are visible before enabling MMU
    asm!("dsb ish", options(nomem, nostack));
    asm!("isb", options(nomem, nostack));

    // Invalidate all TLB entries
    asm!("tlbi vmalle1is", options(nomem, nostack));
    asm!("dsb ish", options(nomem, nostack));
    asm!("isb", options(nomem, nostack));

    // 8. Enable MMU
    // Set SCTLR_EL1 explicitly instead of read-modify-write, to avoid
    // inheriting unwanted bits from the reset value (which is IMPDEF).
    let sctlr: u64 = (1 << 0)   // M: enable MMU
                    | (1 << 2)   // C: enable data cache
                    | (1 << 3)   // SA: SP alignment check
                    | (1 << 12)  // I: enable instruction cache
                    | (1 << 23)  // SPAN: PAN not set on exception entry (disables PAN)
                    | (1 << 26)  // UCI: allow EL0 cache maintenance
                    ; // WXN (bit 19) = 0: don't enforce W→XN for EL1
                      // EE (bit 25) = 0: little-endian at EL1
    asm!(
        "msr sctlr_el1, {}",
        "isb",
        in(reg) sctlr,
        options(nomem, nostack),
    );

    // MMU is now ON. Identity mapping means VA = PA for everything we mapped.
    // The kernel continues running at the same physical addresses.

    let bootstrap_end = bump.current();

    BootInfo {
        ram_base,
        ram_size,
        page_tracker_base,
        page_tracker_size,
        ttbr0: l1_page,
        bootstrap_end,
    }
}

/// Initialize the MemoryManager after MMU is enabled.
///
/// Marks kernel + bootstrap pages as owned by PID 1 and all remaining
/// RAM pages as free.
///
/// # Safety
///
/// Must be called after `enable_mmu` returns.
pub unsafe fn init_memory_manager(info: &BootInfo) {
    use xous::PID;

    let ram_base = info.ram_base;
    let num_pages = info.ram_size / PAGE_SIZE;
    let pid1 = PID::new(1).unwrap();

    // Build the allocations slice from the page tracker region
    let alloc_ptr = info.page_tracker_base as *mut Option<PID>;
    let allocations = core::slice::from_raw_parts_mut(alloc_ptr, num_pages);

    // Mark all pages as free initially
    allocations.fill(None);

    // Mark kernel + bootstrap pages as owned by PID 1
    let k_start_page = (kernel_start() - ram_base) / PAGE_SIZE;
    let bootstrap_end_page = (BumpAllocator::align_up(info.bootstrap_end) - ram_base) / PAGE_SIZE;
    for page in k_start_page..bootstrap_end_page {
        if page < num_pages {
            allocations[page] = Some(pid1);
        }
    }

    // Also mark the FDT region as used (first 512KB before kernel = 32 pages)
    let fdt_start_page = 0; // FDT is at ram_base
    let fdt_end_page = (kernel_start() - ram_base) / PAGE_SIZE;
    for page in fdt_start_page..fdt_end_page {
        if page < num_pages {
            allocations[page] = Some(pid1);
        }
    }

    // Initialize the MemoryManager with this page tracker
    crate::mem::MemoryManager::with_mut(|mm| {
        mm.init_from_bootstrap(allocations, num_pages);
    });
}

// ============================================================================
// ELF-based process launch
// ============================================================================

/// Embedded userspace ELF binaries (built by `cargo xtask build`).
#[allow(dead_code)]
static HELLO_ELF: &[u8] = include_bytes!(
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/aarch64-unknown-none/debug/hello.stripped")
);
static SHELL_ELF: &[u8] = include_bytes!(
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/aarch64-unknown-none/debug/shell.stripped")
);
static PROCMAN_ELF: &[u8] = include_bytes!(
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/aarch64-unknown-none/debug/procman.stripped")
);

/// Embedded binary table: name → ELF bytes.
/// The kernel holds these via include_bytes! — no filesystem needed.
/// Used by SpawnByName syscall to create processes by name.
static BINARY_TABLE: &[(&str, &[u8])] = &[
    ("idle", HELLO_ELF),
    ("hello", HELLO_ELF),
    ("shell", SHELL_ELF),
    ("procman", PROCMAN_ELF),
];

/// Look up a binary by name in the embedded binary table.
pub fn lookup_binary(name: &str) -> Option<&'static [u8]> {
    for &(entry_name, elf_bytes) in BINARY_TABLE {
        if entry_name == name {
            return Some(elf_bytes);
        }
    }
    None
}

/// UART MMIO physical address on QEMU virt.
/// The shell process gets this mapped into its address space for direct output.
#[cfg(feature = "platform-qemu-virt")]
const UART_PHYS: usize = 0x0900_0000;

/// Virtual address where UART is mapped in user processes.
/// Must be in L1[1]+ (same as user code) to avoid conflict with kernel L1[0].
pub const SHELL_UART_VA: usize = 0x0000_0010_0100_0000; // L1[1], well after code

/// Create a user process from an ELF binary using the kernel's standard
/// load_elf → allocate stack → setup_process pipeline.
///
/// # Safety
///
/// Must be called after MMU, MemoryManager, and SystemServices are initialized.
unsafe fn create_elf_process(
    pid: xous::PID,
    elf_bytes: &[u8],
    name: &[u8],
) {
    use xous::MemoryAddress;

    let elf_range = xous::MemoryRange::new(
        elf_bytes.as_ptr() as usize,
        elf_bytes.len(),
    ).expect("elf range");

    let init = xous::ProcessInit {
        elf: elf_range,
        name_addr: MemoryAddress::new(name.as_ptr() as usize).expect("name addr"),
        app_id: [pid.get() as u32, 0, 0, 0].into(),
    };

    crate::services::SystemServices::with_mut(|ss| {
        ss.create_process(init).expect("create_process failed");
    });

    // Grant all syscall permissions
    crate::services::SystemServices::with_mut(|ss| {
        let process = ss.process_mut(pid).expect("process not found");
        process.set_syscall_permissions(u64::MAX);
    });
}

/// Launch user processes in EL0.
///
/// Creates the shell process from the embedded ELF binary, maps UART MMIO
/// into its address space for direct output, and ERets into it.
///
/// # Safety
///
/// Must be called after MMU, MemoryManager, and SystemServices (init_pid1) are initialized.
/// Does not return — enters EL0 via ERET.
pub unsafe fn launch_first_process(_boot_info: &BootInfo) -> ! {
    use xous::PID;

    #[cfg(feature = "platform-qemu-virt")]
    {
        use core::fmt::Write;
        let _ = write!(
            crate::platform::qemu_virt::uart::UartWriter,
            "EL0: loading shell ELF ({} bytes)...\n",
            SHELL_ELF.len(),
        );
    }

    // Create the idle process first (PID 2) — just WFE loops, absorbs CPU
    // when no other process is ready.
    let idle_pid = PID::new(2).unwrap();
    create_elf_process(idle_pid, HELLO_ELF, b"idle");

    // Create the procman process (PID 3) — process lifecycle manager.
    let procman_pid = PID::new(3).unwrap();
    create_elf_process(procman_pid, PROCMAN_ELF, b"procman");

    // Create the shell process (PID 4)
    let shell_pid = PID::new(4).unwrap();
    create_elf_process(shell_pid, SHELL_ELF, b"shell");

    // Map UART MMIO into procman and shell address spaces for direct output.
    #[cfg(feature = "platform-qemu-virt")]
    {
        crate::services::SystemServices::with_mut(|ss| {
            crate::mem::MemoryManager::with_mut(|mm| {
                // Map UART into procman
                let process = ss.process_mut(procman_pid).expect("procman process");
                process.mapping.map_page(
                    mm,
                    UART_PHYS,
                    SHELL_UART_VA as *mut usize,
                    xous::MemoryFlags::W | xous::MemoryFlags::DEV,
                    true,
                ).expect("map UART into procman");

                // Map UART into shell
                let process = ss.process_mut(shell_pid).expect("shell process");
                process.mapping.map_page(
                    mm,
                    UART_PHYS,
                    SHELL_UART_VA as *mut usize,
                    xous::MemoryFlags::W | xous::MemoryFlags::DEV,
                    true,
                ).expect("map UART into shell");
            });
        });
    }

    // Pass the UART VA to procman and shell via x0 register.
    {
        let idx = procman_pid.get() as usize - 1;
        super::process::set_thread_arg0(idx, SHELL_UART_VA);
    }
    {
        let idx = shell_pid.get() as usize - 1;
        super::process::set_thread_arg0(idx, SHELL_UART_VA);
    }

    #[cfg(feature = "platform-qemu-virt")]
    crate::platform::qemu_virt::uart::puts("EL0: launching shell...\n");

    // Build a context frame for the initial ERET.
    // Start with the shell process (PID 4). Idle (PID 2) and procman (PID 3)
    // are in the scheduler queue and will run when the shell blocks.
    let kstack_phys = crate::mem::MemoryManager::with_mut(|mm| {
        mm.alloc_range(1, PID::new(1).unwrap()).expect("alloc kstack").0
    });
    core::ptr::write_bytes(kstack_phys as *mut u8, 0, PAGE_SIZE);

    // Set shell as current and activate its address space
    super::process::set_current_pid(shell_pid);
    crate::services::SystemServices::with_mut(|ss| {
        ss.process(shell_pid).expect("shell not registered").activate();
    });

    // Load the process's saved context (PC, SP, SPSR set by setup_process)
    let frame = kstack_phys as *mut super::process::Thread;
    let proc = super::process::Process::current();
    let tid = proc.current_tid();
    proc.load_context_from_table(tid, frame);

    // ERET to EL0 — shell process runs
    super::asm::_resume_context(kstack_phys as *const u8)
}
