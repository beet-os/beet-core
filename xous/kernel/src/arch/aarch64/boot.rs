// SPDX-FileCopyrightText: 2024 BeetOS contributors
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Early boot: FDT parsing, MemoryManager initialization.
//!
//! Called from `_start_rust` AFTER the assembly boot code has already:
//!   1. Created bootstrap page tables (TTBR0 identity map + TTBR1 kernel map)
//!   2. Enabled the MMU
//!   3. Jumped to high VA (TTBR1 space)
//!
//! At this point the kernel is running at high VA through TTBR1.
//! All physical memory is accessible via `beetos::phys_to_virt(pa)`.
//!
//! # Boot sequence (this module)
//!
//! 1. Parse FDT (at `phys_to_virt(fdt_pa)`) to discover RAM base + size
//! 2. Allocate page tracker after kernel `_end` (high VA, bump allocator)
//! 3. Initialize MemoryManager with the page tracker
//!
//! The assembly boot code (start.S) handles page table creation and MMU enable.

use beetos::PAGE_SIZE;

// ============================================================================
// Linker symbols (at high VA after relink)
// ============================================================================

extern "C" {
    static _end: u8;
}

/// Get the kernel's end address (high VA, first byte after kernel image + stack).
/// Runtime allocations (page tracker, etc.) start here.
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
/// `fdt_ptr` must point to a valid FDT blob (may be at high VA via phys_to_virt).
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
// Bump allocator (works in high VA space)
// ============================================================================

/// Bump allocator for bootstrap page allocation (before MemoryManager exists).
/// Works in high VA space — allocations are accessible through TTBR1.
struct BumpAllocator {
    next: usize,
}

impl BumpAllocator {
    /// Create a new bump allocator starting at the given high VA.
    fn new(start: usize) -> Self {
        Self { next: Self::align_up(start) }
    }

    fn align_up(addr: usize) -> usize {
        (addr + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
    }

    /// Allocate one zeroed 16KB page. Returns the high VA of the page.
    #[allow(dead_code)]
    fn alloc_page(&mut self) -> usize {
        let page = self.next;
        self.next += PAGE_SIZE;
        unsafe { core::ptr::write_bytes(page as *mut u8, 0, PAGE_SIZE) };
        page
    }

    /// Current high-water mark (high VA).
    fn current(&self) -> usize {
        self.next
    }
}

// ============================================================================
// Boot info and initialization
// ============================================================================

/// Information about the bootstrap memory layout, returned to the caller
/// so the MemoryManager can be initialized.
#[allow(dead_code)]
pub struct BootInfo {
    /// RAM base address (PA, from FDT).
    pub ram_base: usize,
    /// RAM size in bytes (from FDT, capped to beetos::RAM_SIZE).
    pub ram_size: usize,
    /// High VA of the page tracker (allocations array).
    pub page_tracker_base: usize,
    /// Size of page tracker in bytes.
    pub page_tracker_size: usize,
    /// First free high VA after all bootstrap allocations.
    pub bootstrap_end: usize,
}

/// Initialize memory management after MMU is enabled.
///
/// Parses FDT for RAM info, allocates the page tracker, and prepares
/// BootInfo for MemoryManager initialization.
///
/// # Safety
///
/// Must be called exactly once, early in boot, after MMU is on and
/// the kernel is running at high VA. `fdt_phys` is the physical address
/// of the FDT blob (from the bootloader).
pub unsafe fn init_memory(fdt_phys: *const u8) -> BootInfo {
    // 1. Discover RAM from FDT (accessed at high VA)
    let (ram_base, ram_size_raw) = if !fdt_phys.is_null() {
        let fdt_va = beetos::phys_to_virt(fdt_phys as usize) as *const u8;
        parse_fdt_ram(fdt_va)
            .map(|r| (r.base, r.size))
            .unwrap_or((beetos::PLAINTEXT_DRAM_BASE, beetos::RAM_SIZE))
    } else {
        (beetos::PLAINTEXT_DRAM_BASE, beetos::RAM_SIZE)
    };

    // Cap to compile-time max (bitmap size is fixed)
    let ram_size = ram_size_raw.min(beetos::RAM_SIZE);

    // 2. Set up bump allocator after kernel _end (high VA)
    let mut bump = BumpAllocator::new(kernel_end());

    // 3. Allocate page tracker (Option<PID> = 2 bytes per page)
    let num_pages = ram_size / PAGE_SIZE;
    let page_tracker_size = num_pages * 2; // sizeof(Option<PID>) = 2
    let page_tracker_base = bump.current();
    // Advance bump past page tracker (round up to page boundary)
    bump.next = BumpAllocator::align_up(page_tracker_base + page_tracker_size);
    // Zero the page tracker (at high VA, through TTBR1)
    core::ptr::write_bytes(page_tracker_base as *mut u8, 0, bump.current() - page_tracker_base);

    let bootstrap_end = bump.current();

    BootInfo {
        ram_base,
        ram_size,
        page_tracker_base,
        page_tracker_size,
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
/// Must be called after `init_memory` returns.
pub unsafe fn init_memory_manager(info: &BootInfo) {
    use xous::PID;

    let ram_base = info.ram_base;
    let num_pages = info.ram_size / PAGE_SIZE;
    let pid1 = PID::new(1).unwrap();

    // Build the allocations slice from the page tracker region (at high VA)
    let alloc_ptr = info.page_tracker_base as *mut Option<PID>;
    let allocations = core::slice::from_raw_parts_mut(alloc_ptr, num_pages);

    // Mark all pages as free initially
    allocations.fill(None);

    // Mark all pages from ram_base to bootstrap_end as owned by PID 1.
    // This covers: FDT, boot code (.text.boot), boot page tables (.boot.bss),
    // kernel image (.text, .rodata, .data), BSS, stack, and page tracker.
    let bootstrap_phys_end = beetos::virt_to_phys(info.bootstrap_end);
    let first_used_page = 0; // FDT is at ram_base
    let last_used_page = (bootstrap_phys_end - ram_base + PAGE_SIZE - 1) / PAGE_SIZE;
    for page in first_used_page..last_used_page.min(num_pages) {
        allocations[page] = Some(pid1);
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
static FS_ELF: &[u8] = include_bytes!(
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/aarch64-unknown-none/debug/fs.stripped")
);
/// Log server: forwards stdout/stderr/panic IPC messages to the UART.
/// Must be launched before any std process that calls println!.
static LOG_ELF: &[u8] = include_bytes!(
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/aarch64-unknown-none/debug/log.stripped")
);
/// hello-std: compiled with Rust std (aarch64-unknown-beetos), stripped to same dir by xtask.
#[cfg(not(feature = "test-mode"))]
static HELLO_STD_ELF: &[u8] = include_bytes!(
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/aarch64-unknown-none/debug/hello-std.stripped")
);
/// beetos-test: self-test binary for `cargo xtask test`.
#[cfg(feature = "test-mode")]
static TEST_ELF: &[u8] = include_bytes!(
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/aarch64-unknown-none/debug/beetos-test.stripped")
);
/// Embedded binary table: name → ELF bytes.
/// The kernel holds these via include_bytes! — no filesystem needed.
/// Used by SpawnByName syscall to create processes by name.
#[cfg(not(feature = "test-mode"))]
static BINARY_TABLE: &[(&str, &[u8])] = &[
    ("log", LOG_ELF),
    ("idle", HELLO_ELF),
    ("hello-std", HELLO_STD_ELF),
    ("hello-nostd", HELLO_ELF),
    ("shell", SHELL_ELF),
    ("procman", PROCMAN_ELF),
    ("fs", FS_ELF),
];

#[cfg(feature = "test-mode")]
static BINARY_TABLE: &[(&str, &[u8])] = &[
    ("log", LOG_ELF),
    ("idle", HELLO_ELF),
    ("beetos-test", TEST_ELF),
    ("hello-nostd", HELLO_ELF),
    ("shell", SHELL_ELF),
    ("procman", PROCMAN_ELF),
    ("fs", FS_ELF),
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

const INTERNAL_SERVICES: &[&str] = &["log", "idle", "shell", "procman", "fs", "beetos-test"];

/// Return the nth user-spawnable program name (skipping internal services).
/// Returns None when index is out of range.
pub fn get_user_binary_at(index: usize) -> Option<&'static str> {
    let mut user_idx = 0;

    for &(name, _) in BINARY_TABLE {
        if !INTERNAL_SERVICES.contains(&name) {
            if user_idx == index {
                return Some(name);
            }
            user_idx += 1;
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

/// Virtual address where disk data is mapped read-only in user processes.
/// Placed well after UART VA to avoid collisions.
pub const DISK_DATA_VA: usize = 0x0000_0010_0200_0000;

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

    // PID 2: log server — must be first so println! works in all std processes.
    let log_pid = PID::new(2).unwrap();
    create_elf_process(log_pid, LOG_ELF, b"log");

    // PID 3: idle placeholder
    let idle_pid = PID::new(3).unwrap();
    create_elf_process(idle_pid, HELLO_ELF, b"idle");

    // PID 4: process manager
    let procman_pid = PID::new(4).unwrap();
    create_elf_process(procman_pid, PROCMAN_ELF, b"procman");

    // PID 5: shell
    let shell_pid = PID::new(5).unwrap();
    create_elf_process(shell_pid, SHELL_ELF, b"shell");

    // PID 6: filesystem service
    let fs_pid = PID::new(6).unwrap();
    create_elf_process(fs_pid, FS_ELF, b"fs");

    // PID 7: beetos-test in test-mode only (hello-std is spawnable from the shell).
    #[cfg(feature = "test-mode")]
    let app_pid = PID::new(7).unwrap();
    #[cfg(feature = "test-mode")]
    create_elf_process(app_pid, TEST_ELF, b"beetos-test");

    // Map UART MMIO into log, procman, shell, fs (and beetos-test in test-mode).
    #[cfg(feature = "platform-qemu-virt")]
    {
        #[cfg(not(feature = "test-mode"))]
        let uart_pids: &[PID] = &[log_pid, procman_pid, shell_pid, fs_pid];
        #[cfg(feature = "test-mode")]
        let uart_pids: &[PID] = &[log_pid, procman_pid, shell_pid, fs_pid, app_pid];
        crate::services::SystemServices::with_mut(|ss| {
            crate::mem::MemoryManager::with_mut(|mm| {
                for &pid in uart_pids {
                    let process = ss.process_mut(pid).expect("process for UART map");
                    process.mapping.map_page(
                        mm,
                        UART_PHYS,
                        SHELL_UART_VA as *mut usize,
                        xous::MemoryFlags::W | xous::MemoryFlags::DEV,
                        true,
                    ).expect("map UART");
                }
            });
        });
    }

    // Read disk data from virtio-blk and map into the fs service process.
    #[cfg(feature = "platform-qemu-virt")]
    let (disk_va, disk_size) = {
        use crate::platform::qemu_virt::blk;
        if blk::is_available() {
            let capacity = blk::capacity();
            let disk_bytes = (capacity as usize) * blk::SECTOR_SIZE;
            let disk_pages = (disk_bytes + beetos::PAGE_SIZE - 1) / beetos::PAGE_SIZE;

            if disk_pages > 0 && disk_pages <= 256 {
                let kernel_pid = xous::PID::new(1).unwrap();
                let mut disk_phys_pages = [0usize; 256];
                let mut ok = true;

                crate::mem::MemoryManager::with_mut(|mm| {
                    for i in 0..disk_pages {
                        match mm.alloc_range(1, kernel_pid) {
                            Ok((pa, _)) => {
                                let kva = beetos::phys_to_virt(pa);
                                core::ptr::write_bytes(kva as *mut u8, 0, beetos::PAGE_SIZE);
                                disk_phys_pages[i] = pa;
                            }
                            Err(_) => { ok = false; }
                        }
                    }
                });

                if ok {
                    let sectors_per_page = beetos::PAGE_SIZE / blk::SECTOR_SIZE;
                    let mut read_ok = true;

                    for i in 0..disk_pages {
                        let pa = disk_phys_pages[i];
                        let kva = beetos::phys_to_virt(pa);
                        let lba = (i * sectors_per_page) as u64;
                        let remaining = disk_bytes - i * beetos::PAGE_SIZE;
                        let read_len = core::cmp::min(remaining, beetos::PAGE_SIZE);
                        let read_sectors = (read_len + blk::SECTOR_SIZE - 1) / blk::SECTOR_SIZE;
                        let buf = core::slice::from_raw_parts_mut(
                            kva as *mut u8,
                            read_sectors * blk::SECTOR_SIZE,
                        );
                        if blk::read_sectors(lba, buf).is_err() {
                            read_ok = false;
                            break;
                        }
                    }

                    if read_ok {
                        // Map disk pages read-only into the fs service's address space
                        crate::services::SystemServices::with_mut(|ss| {
                            crate::mem::MemoryManager::with_mut(|mm| {
                                let process = ss.process_mut(fs_pid).expect("fs process");
                                for i in 0..disk_pages {
                                    let va = DISK_DATA_VA + i * beetos::PAGE_SIZE;
                                    process.mapping.map_page(
                                        mm,
                                        disk_phys_pages[i],
                                        va as *mut usize,
                                        xous::MemoryFlags::empty(),
                                        true,
                                    ).ok();
                                }
                            });
                        });

                        crate::platform::qemu_virt::uart::puts("Disk: mapped into fs service\n");
                        (DISK_DATA_VA, disk_bytes)
                    } else {
                        crate::platform::qemu_virt::uart::puts("Disk: read failed\n");
                        (0, 0)
                    }
                } else { (0, 0) }
            } else { (0, 0) }
        } else { (0, 0) }
    };

    #[cfg(not(feature = "platform-qemu-virt"))]
    let (disk_va, disk_size) = (0usize, 0usize);

    // Pass boot parameters via registers:
    //   log/procman/shell/hello: x0 = UART VA
    //   fs service:              x0 = UART VA, x1 = disk VA, x2 = disk size
    {
        let idx = log_pid.get() as usize - 1;
        super::process::set_thread_arg0(idx, SHELL_UART_VA);
    }
    {
        let idx = procman_pid.get() as usize - 1;
        super::process::set_thread_arg0(idx, SHELL_UART_VA);
    }
    {
        let idx = shell_pid.get() as usize - 1;
        super::process::set_thread_arg0(idx, SHELL_UART_VA);
    }
    {
        let idx = fs_pid.get() as usize - 1;
        super::process::set_thread_args(idx, SHELL_UART_VA, disk_va, disk_size);
    }
    #[cfg(feature = "test-mode")]
    {
        let idx = app_pid.get() as usize - 1;
        super::process::set_thread_arg0(idx, SHELL_UART_VA);
    }

    #[cfg(feature = "platform-qemu-virt")]
    crate::platform::qemu_virt::uart::puts("EL0: launching shell...\n");

    // Build a context frame on the real kernel stack for the initial ERET.
    //
    // _resume_context sets SP = frame_ptr, then restore_context does
    // `add sp, sp, #816; eret`. After eret, SP_EL1 = frame_ptr + 816.
    // All subsequent exception handlers (SVC, IRQ) use SP_EL1 as their stack.
    //
    // CRITICAL: We must use the real kernel stack (_stack_top), NOT a small
    // allocated page. The SVC/IRQ handlers need substantial stack space for
    // nested function calls (SpawnByName → create_process → load_elf → etc.).
    // Using a 16KB page would cause stack overflow into adjacent physical
    // pages, corrupting user process memory.
    //
    // Place the context frame at _stack_top - 816 so that after eret,
    // SP_EL1 = _stack_top. This gives the full 256KB .stack section.
    // kernel_end() = _end = _stack_top (linker script places _end right after .stack).
    // This is the top of the 256KB kernel stack at high VA.
    // Place the context frame BELOW the current SP to avoid overlapping
    // with live stack data. After _resume_context does `add sp, sp, #816; eret`,
    // SP_EL1 = frame_ptr + 816. Subsequent SVC/IRQ handlers grow down from there.
    //
    // We leave 4KB of headroom below current SP for load_context_from_table's
    // own stack usage, then place our 816-byte context frame.
    const FRAME_SIZE: usize = 816;
    let current_sp: usize;
    unsafe { core::arch::asm!("mov {}, sp", out(reg) current_sp, options(nomem, nostack)) };
    let frame_ptr = (current_sp - 4096 - FRAME_SIZE) & !0xF; // 16-byte aligned

    // Set shell as current and activate its address space
    super::process::set_current_pid(shell_pid);
    crate::services::SystemServices::with_mut(|ss| {
        ss.process(shell_pid).expect("shell not registered").activate();
    });

    // Load the process's saved context (PC, SP, SPSR set by setup_process)
    let frame = frame_ptr as *mut super::process::Thread;
    let proc = super::process::Process::current();
    let tid = proc.current_tid();
    unsafe { proc.load_context_from_table(tid, frame) };

    // ERET to EL0 — shell process runs.
    // After restore_context: SP_EL1 = frame_ptr + 816 = _stack_top
    unsafe { super::asm::_resume_context(frame_ptr as *const u8) }
}
