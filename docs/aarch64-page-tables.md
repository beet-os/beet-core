# AArch64 Page Tables in BeetOS

This document describes the AArch64 virtual memory system as implemented in BeetOS. It covers the page table format, virtual address layout, boot sequence, runtime operations, and key invariants.

**Source files:**
- `xous/kernel/src/arch/aarch64/mem.rs` — page table structures and operations
- `xous/kernel/src/arch/aarch64/start.S` — bootstrap page tables and MMU enable
- `xous/kernel/src/arch/aarch64/asm.rs` — TLB maintenance and cache operations
- `xous/kernel/src/mem.rs` — memory manager (physical page allocation)
- `beetos/src/lib.rs` — constants (PAGE_SIZE, memory map addresses)

## Overview

BeetOS uses ARMv8-A translation tables with a **16KB granule** (required by Apple Silicon). The hardware provides a split virtual address space via two independent Translation Table Base Registers:

- **TTBR0_EL1** — lower half (user space), switched on every context switch
- **TTBR1_EL1** — upper half (kernel space), set once at boot, never changed

The kernel is NOT mapped in any user page table. User processes live entirely in TTBR0; the kernel lives entirely in TTBR1.

## Page Size: 16KB

All page tables use 16KB pages (`beetos::PAGE_SIZE = 16384`). This is a hard requirement — Apple Silicon only supports 4KB and 16KB granules, and BeetOS standardizes on 16KB for all platforms (including QEMU virt). Never use hardcoded `4096` or `0x1000`; always use `beetos::PAGE_SIZE`.

## 3-Level Page Table Walk

With `TCR_EL1.T0SZ = T1SZ = 17` (47-bit VA per half) and 16KB granule, the hardware walks a **3-level** table: L1 → L2 → L3.

### Virtual Address Breakdown

```
  63        47 46        36 35        25 24       14 13        0
 ┌──────────┬─────────────┬─────────────┬───────────┬──────────┐
 │ TTBR sel │  L1 index   │  L2 index   │ L3 index  │  offset  │
 │ (0/FFFF) │ (11 bits)   │ (11 bits)   │ (11 bits) │ (14 bits)│
 └──────────┴─────────────┴─────────────┴───────────┴──────────┘
              2048 entries  2048 entries  2048 entries  16KB page
```

- Bits `[63:47]` — TTBR selector: all-zeros selects TTBR0, all-ones selects TTBR1
- Bits `[46:36]` — L1 index (11 bits → 2048 entries, each covering **64 GiB**)
- Bits `[35:25]` — L2 index (11 bits → 2048 entries, each covering **32 MiB**)
- Bits `[24:14]` — L3 index (11 bits → 2048 entries, each covering **16 KiB**)
- Bits `[13:0]`  — page offset (14 bits → 16384 bytes)

Each table is one 16KB page containing 2048 8-byte entries.

### Index Extraction (from `mem.rs`)

```rust
const fn l1_index(va: usize) -> usize {
    (va >> (PAGE_SHIFT + 2 * TABLE_INDEX_BITS)) & (TABLE_ENTRIES - 1)  // bits [46:36]
}
const fn l2_index(va: usize) -> usize {
    (va >> (PAGE_SHIFT + TABLE_INDEX_BITS)) & (TABLE_ENTRIES - 1)       // bits [35:25]
}
const fn l3_index(va: usize) -> usize {
    (va >> PAGE_SHIFT) & (TABLE_ENTRIES - 1)                            // bits [24:14]
}
```

Where `PAGE_SHIFT = 14` and `TABLE_INDEX_BITS = 11`.

## Page Table Entry (PTE) Format

### L1/L2 Table Descriptor

Points to the next-level table:

```
  63          48 47          14 13  2 1 0
 ┌──────────────┬──────────────┬─────┬───┐
 │   ignored    │ Next-level   │ ign │1 1│
 │              │ table PA     │     │   │
 └──────────────┴──────────────┴─────┴───┘
```

Bits `[1:0] = 0b11` (Valid + Table).

### L3 Page Descriptor

Points to the final physical page:

```
  63  55 54 53 52         14         2 1 0
 ┌─────┬──┬──┬──┬────────────────────┬───┐
 │     │UX│PX│  │  Output Address    │   │
 │     │N │N │  │  (PA bits [47:14]) │1 1│
 └─────┴──┴──┴──┴────────────────────┴───┘
```

### PTE Bit Fields

| Bit(s) | Name     | Description |
|--------|----------|-------------|
| 0      | Valid    | Entry is active (hardware walker uses it) |
| 1      | Table/Page | Must be 1 for table descriptors (L1/L2) and page descriptors (L3) |
| 4:2    | AttrIndx | Indexes into MAIR_EL1 to select memory type (see below) |
| 7:6    | AP       | Access Permissions (see table below) |
| 9:8    | SH       | Shareability: `0b11` = Inner Shareable (for SMP cache coherency) |
| 10     | AF       | Access Flag: must be 1 (no lazy AF in BeetOS) |
| 11     | nG       | Non-Global: 1 for user pages (per-ASID TLB tagging) |
| 47:14  | Address  | Physical page address, 16KB-aligned |
| 53     | PXN      | Privileged Execute-Never (blocks EL1 execution) |
| 54     | UXN      | User Execute-Never (blocks EL0 execution) |

### Access Permissions (AP field, bits [7:6])

| AP   | EL1 (kernel) | EL0 (user) | Constant       |
|------|-------------|------------|----------------|
| 0b00 | Read/Write  | No access  | `PTE_AP_RW_EL1` |
| 0b01 | Read/Write  | Read/Write | `PTE_AP_RW_ALL` |
| 0b10 | Read-only   | No access  | `PTE_AP_RO_EL1` |
| 0b11 | Read-only   | Read-only  | `PTE_AP_RO_ALL` |

### Memory Attributes (MAIR_EL1)

Three memory types are configured via `MAIR_EL1 = 0x00_00_00_00_00_44_FF_00`:

| AttrIndx | MAIR byte | Type                  | Use                        | Constant            |
|----------|-----------|-----------------------|----------------------------|---------------------|
| 0        | `0x00`    | Device-nGnRnE         | MMIO registers             | `PTE_ATTR_DEVICE`   |
| 1        | `0xFF`    | Normal WB, RA+WA      | RAM (code, data, stacks)   | `PTE_ATTR_NORMAL`   |
| 2        | `0x44`    | Normal Non-Cacheable   | DMA buffers, shared memory | `PTE_ATTR_NORMAL_NC`|

- **Device-nGnRnE**: No gathering, no reordering, no early write acknowledgement. Strongest ordering for MMIO.
- **Normal WB, RA+WA**: Inner/Outer Write-Back with Read-Allocate and Write-Allocate. Standard for all RAM.
- **Normal NC**: Inner/Outer Non-Cacheable. For DMA buffers where cache coherency is managed manually.

## W^X Enforcement

BeetOS enforces **W^X** (Write XOR Execute) at the page table level. No page is ever mapped as both writable and executable simultaneously:

| Xous flags | User page result                    | Kernel page result     |
|-----------|-------------------------------------|------------------------|
| W only    | AP=RW_ALL, UXN+PXN set              | AP=RW_EL1              |
| X only    | AP=RO_ALL, PXN set (user can exec)  | UXN set (kernel exec)  |
| W + X     | **Defaults to W only** (UXN+PXN set)| Not used               |
| Neither   | AP=RO_ALL, UXN+PXN set              | AP=RO_EL1, UXN+PXN    |

This is enforced in `flags_to_pte()` in `mem.rs`.

## Virtual Address Space Layout

### Lower Half — TTBR0 (Per-Process User Space)

```
0x0000_7000_0000_0000  ┌─────────────────────────────┐ ← USER_AREA_END
                       │ Thread context (kernel-only) │
0x0000_6FF0_0000_0000  ├─────────────────────────────┤
                       │ User stack (1MB)             │
0x0000_6FE0_0000_0000  ├─────────────────────────────┤
                       │ User IRQ stack               │
                       │ ...                          │
0x0000_5800_0000_0000  ├─────────────────────────────┤
                       │ Kernel arg table (PID1 only) │
0x0000_5000_0000_0000  ├─────────────────────────────┤
                       │ Raw ELF temp loading area    │
0x0000_4000_0000_0000  ├─────────────────────────────┤
                       │ Memory mirror (CoW/shared)   │
0x0000_3000_0000_0000  ├─────────────────────────────┤
                       │ mmap area                    │
0x0000_0001_0000       ├─────────────────────────────┤
                       │ ASLR region (code + data)    │
0x0000_0000_0000_0000  ├─────────────────────────────┤
                       │ Null guard (64KB, unmapped)  │
                       └─────────────────────────────┘
```

Each process gets its own L1 table. The kernel is NOT mapped here.

### Upper Half — TTBR1 (Kernel Space, Permanent)

```
0xFFFF_FFFF_FFFF_FFFF  ┌─────────────────────────────┐
                       │ Exception stack (128KB)      │
0xFFFF_FFFF_FFFF_0000  ├─────────────────────────────┤
                       │ IRQ stack (64KB)             │
0xFFFF_FFFF_FFFE_4000  ├─────────────────────────────┤
                       │ Kernel stack (256KB)         │
0xFFFF_FFFF_FFF8_0000  ├─────────────────────────────┤
                       │ Kernel code (up to 2MB)      │
0xFFFF_FFFF_FFD0_0000  ├─────────────────────────────┤
                       │ ...                          │
                       │ Allocation tracker bitmaps   │
0xFFFF_FF80_0000_8000  ├─────────────────────────────┤
                       │ ...                          │
                       │ Physical RAM linear map      │
0xFFFF_8000_0000_0000  └─────────────────────────────┘ ← KERNEL_VIRT_BASE
```

The kernel uses a **linear map**: `kernel_VA = PA + 0xFFFF_8000_0000_0000`. This lets the kernel access any physical address (RAM or MMIO) by adding the offset. The `beetos::phys_to_virt()` and `beetos::virt_to_phys()` functions perform this translation.

## ASID (Address Space ID)

Each process is assigned an ASID equal to its PID (8-bit, stored in TTBR0_EL1 bits [63:48]). User pages are marked non-global (`nG = 1`), so the TLB tags them with the ASID. This avoids a full TLB flush on context switches — only TTBR0 and CONTEXTIDR_EL1 are updated:

```rust
pub fn activate(self) {
    let ttbr0_val = self.ttbr0 as u64 | ((self.pid as u64) << 48);
    // msr ttbr0_el1, ttbr0_val
    // msr contextidr_el1, pid
    // isb
}
```

Kernel pages (via TTBR1) are global (`nG = 0`) and shared across all processes.

## Boot Sequence

### Step 1: `_start` (PA, MMU off)

Entry at physical address. The bootloader (m1n1 or QEMU) passes the FDT pointer in x0. The code:
1. Drops from EL2 to EL1 if needed (RPi5 firmware starts at EL2)
2. Masks all exceptions
3. Enables FP/SIMD (CPACR_EL1)
4. Saves FDT pointer in x19
5. Sets up a small 4KB boot stack (in `.boot.bss` at PA)

### Step 2: `_create_boot_page_tables` (PA, MMU off)

Creates three 16KB tables in `.boot.bss`:

```
  TTBR0 L1 (identity)          TTBR1 L1 (kernel)
  ┌─────────────────┐          ┌─────────────────┐
  │ [0] → L2 (PA)   │          │ [0] → L2 (PA)   │  ← same L2 table!
  │ [1..2047] = 0   │          │ [1..2047] = 0   │
  └─────────────────┘          └─────────────────┘

  Shared L2 table (64 entries used):
  ┌──────────────────────────────────────────────────────────┐
  │ [0..31]  → Device blocks: PA 0x0..0x3FFF_FFFF  (1GB MMIO)│
  │ [32..63] → Normal blocks: PA 0x4000_0000..0x7FFF_FFFF    │
  │ [64..2047] = 0 (unmapped)                      (1GB RAM) │
  └──────────────────────────────────────────────────────────┘
```

Uses **L2 block descriptors** (32 MiB each, no L3 needed for boot). Both TTBR0 and TTBR1 share the same L2 table — this creates:

- **Identity map (TTBR0):** VA `0x4000_0000` → PA `0x4000_0000` (same address)
- **Kernel map (TTBR1):** VA `0xFFFF_8000_4000_0000` → PA `0x4000_0000`

The identity map lets the CPU continue fetching after MMU enable (PC is still at PA).

### Step 3: `_enable_mmu_boot` (PA, MMU off → on)

Configures system registers in order:

1. **MAIR_EL1** — memory attribute encodings (must match `PTE_ATTR_*` constants)
2. **TCR_EL1** — translation control: 47-bit VA, 16KB granule, 48-bit PA, ISH, WB caching for walks
3. **TTBR0_EL1** — identity map L1 (temporary)
4. **TTBR1_EL1** — kernel map L1 (permanent)
5. **TLBI VMALLE1IS** — flush all TLB entries
6. **SCTLR_EL1** — enable MMU (M=1), data cache (C=1), instruction cache (I=1), stack alignment check (SA=1)

### Step 4: `_high_va_entry` (high VA, through TTBR1)

After MMU enable, jumps to the kernel's high VA entry point:
1. Sets SP to `_stack_top` (256KB kernel stack at high VA)
2. Sets `VBAR_EL1` to exception vector table at high VA
3. Clears `.bss`
4. Calls `_start_rust(fdt_ptr)` — Rust entry point

From this point, all kernel code runs through TTBR1. TTBR0 is free for user process page tables.

## Runtime Page Table Operations

### `MemoryMapping` struct

```rust
pub struct MemoryMapping {
    ttbr0: usize,       // Physical address of L1 page table
    pid: usize,         // PID / ASID
    aslr_slide: usize,  // ASLR offset
}
```

### Creating a Process

`MemoryMapping::allocate()` creates a new L1 table:
1. Allocates a 16KB physical page from the memory manager
2. Zeroes it via the TTBR1 linear map (`phys_to_virt`)
3. L1 starts empty — pages mapped on demand

### Mapping a Page

`map_page()` performs a 3-level walk, allocating intermediate tables as needed:

```
map_page(phys, virt, flags, user):
  1. l1_table = phys_to_virt(self.ttbr0)     // Access L1 via TTBR1
  2. l2_table = ensure_table(l1_table, l1_index(va))  // Allocate L2 if missing
  3. l3_table = ensure_table(l2_table, l2_index(va))  // Allocate L3 if missing
  4. l3_table[l3_index(va)] = (phys & ADDR_MASK) | flags_to_pte(flags, user)
  5. flush_tlb_entry(va)
```

`ensure_table()` checks if a table descriptor exists at the given index. If not, it allocates a new 16KB page, zeroes it, and writes a table descriptor (`PA | VALID | TABLE`).

All page tables are accessed through the **TTBR1 linear map** (`phys_to_virt()`), so the kernel can manipulate any process's tables without switching TTBR0.

### Unmapping a Page

`unmap_page()` walks L1 → L2 → L3 and zeroes the L3 entry, then calls `flush_tlb_entry()`.

### Virtual-to-Physical Translation

`virt_to_phys()` performs a software page table walk: reads L1, L2, L3 entries (checking Valid bit at each level), extracts the physical address from L3, and adds the page offset.

## TLB Invalidation

After modifying any PTE, the stale TLB entry must be invalidated. The correct barrier sequence is:

```
1. DSB ISHST     — ensure PTE store is visible to hardware walker
2. TLBI ...      — invalidate the stale entry
3. DSB ISH       — wait for invalidation to complete on all cores
4. ISB           — synchronize instruction stream
```

Three invalidation primitives (all Inner-Shareable for SMP):

| Function          | TLBI instruction  | Scope                          | Use case                |
|-------------------|-------------------|--------------------------------|-------------------------|
| `flush_tlb_entry` | `VAALE1IS`        | Single VA, all ASIDs           | After map/unmap         |
| `flush_tlb_asid`  | `ASIDE1IS`        | All entries for one ASID       | Process destruction     |
| `flush_tlb_all`   | `VMALLE1IS`       | Entire TLB                     | Boot, bulk changes      |

Note: `VAALE1IS` takes the VA shifted right by 12 bits (fixed ISA encoding, not by PAGE_SHIFT).

## TCR_EL1 Configuration

The Translation Control Register controls the page table walk parameters:

| Field    | Bits    | Value  | Meaning |
|----------|---------|--------|---------|
| T0SZ     | [5:0]   | 17     | 47-bit VA for TTBR0 |
| IRGN0    | [9:8]   | 0b01   | Inner WB RA+WA for TTBR0 walks |
| ORGN0    | [11:10] | 0b01   | Outer WB RA+WA for TTBR0 walks |
| SH0      | [13:12] | 0b11   | Inner Shareable for TTBR0 walks |
| TG0      | [15:14] | 0b10   | 16KB granule for TTBR0 |
| T1SZ     | [21:16] | 17     | 47-bit VA for TTBR1 |
| IRGN1    | [25:24] | 0b01   | Inner WB RA+WA for TTBR1 walks |
| ORGN1    | [27:26] | 0b01   | Outer WB RA+WA for TTBR1 walks |
| SH1      | [29:28] | 0b11   | Inner Shareable for TTBR1 walks |
| TG1      | [31:30] | 0b01   | 16KB granule for TTBR1 |
| IPS      | [34:32] | 0b101  | 48-bit physical address space |

## Cache Operations

AArch64 caches are PIPT (Physically Indexed, Physically Tagged) but cache maintenance instructions operate on virtual addresses. Apple M1 uses 64-byte cache lines.

| Function   | Instruction | Operation                          | Use case |
|------------|-------------|------------------------------------|----------|
| `dc_cvac`  | DC CVAC     | Clean (write back dirty line)      | Make data visible to other observers |
| `dc_civac` | DC CIVAC    | Clean + Invalidate (write back, discard) | Transfer buffer ownership to DMA |
| `dc_ivac`  | DC IVAC     | Invalidate (discard, no write-back)| Before DMA read (discard stale data) |

All cache operations in `flush_cache()` are aligned to 64-byte cache line boundaries.

## Critical Invariants

### SP_EL1 Must Always Point to Kernel Stack

SP_EL1 must ALWAYS point into the kernel's `.stack` section (256KB). Never allocate a page from the user pool and use it as a kernel stack. After `eret`, the CPU uses SP_EL1 for all EL1 exceptions (IRQ, SVC, data abort). If SP_EL1 points to a small allocated page, the exception handler will overflow into adjacent physical pages — causing silent memory corruption.

### W^X Always

No page is ever both writable and executable. If both flags are requested, the page defaults to writable-only.

### No Hardcoded Addresses

All MMIO addresses come from the FDT. Use `beetos::PAGE_SIZE` instead of `4096` or `16384` literals.

### Kernel Accesses Page Tables via TTBR1

Page tables are physical pages. The kernel reads/writes them through the TTBR1 linear map (`phys_to_virt(pa)`). This works regardless of TTBR0, so the kernel can modify any process's tables without a context switch.
