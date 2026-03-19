/* BeetOS QEMU virt linker script.
 *
 * Two-region layout for TTBR1 kernel mapping:
 *
 *   BOOT region (PA): Entry code that runs with MMU off. Contains _start,
 *     boot page table setup, MMU enable, and the trampoline to high VA.
 *     Linked and loaded at physical address.
 *
 *   KERNEL region (high VA): All kernel code, data, BSS, and stack.
 *     Linked at high virtual address (TTBR1 space), loaded at physical
 *     address (AT> RAM). After MMU enable, the boot trampoline jumps
 *     here and all subsequent kernel execution uses TTBR1.
 *
 * Physical layout (QEMU virt, RAM at 0x4000_0000):
 *   PA 0x4000_0000:  FDT (placed by QEMU)
 *   PA 0x4008_0000:  .text.boot (entry code, ~2KB)
 *   PA 0x4008_4000:  .boot.bss (page tables + boot stack, 52KB)
 *   PA 0x400A_0000:  .text.vectors, .text, .rodata, .data (kernel image)
 *   PA after data :  .bss (zeroed by assembly at high VA)
 *   PA after bss  :  .stack (kernel stack, 256KB)
 *   PA after stack:  free for runtime allocations (page tracker, etc.)
 *
 * Virtual layout:
 *   VA 0x4008_0000:            .text.boot (PA, used only during boot)
 *   VA 0xFFFF_8000_400A_0000:  .text.vectors (high VA, through TTBR1)
 *   VA 0xFFFF_8000_400A_xxxx:  .text, .rodata, .data, .bss, .stack
 */

ENTRY(_start)

/* Physical address where QEMU loads the kernel */
PHYS_BASE = 0x40080000;

/* TTBR1 linear map offset: high_VA = PA + KERNEL_VA_OFFSET */
KERNEL_VA_OFFSET = 0xFFFF800000000000;

/* Size reserved for boot code + boot page tables + boot stack */
BOOT_SIZE = 128K;

MEMORY
{
    /* Boot code at physical address (runs with MMU off) */
    BOOT (rwx) : ORIGIN = PHYS_BASE, LENGTH = BOOT_SIZE

    /* Physical RAM for load addresses (LMA) of kernel sections */
    RAM (rwx) : ORIGIN = PHYS_BASE + BOOT_SIZE, LENGTH = 8M - BOOT_SIZE

    /* Kernel virtual addresses (VMA) — TTBR1 space */
    KERNEL (rwx) : ORIGIN = PHYS_BASE + BOOT_SIZE + KERNEL_VA_OFFSET, LENGTH = 8M - BOOT_SIZE
}

SECTIONS
{
    /* === BOOT region: runs at physical address with MMU off === */

    .text.boot : ALIGN(16)
    {
        KEEP(*(.text.boot))
        /* Literal pool for ldr =<high_va_symbol> */
        . = ALIGN(8);
    } > BOOT

    /* Pre-allocated boot page tables and boot stack (NOLOAD).
     * Zeroed by boot assembly before use. */
    .boot.bss (NOLOAD) : ALIGN(16384)
    {
        *(.boot.bss)
    } > BOOT

    /* Export physical base for Rust code */
    _kernel_phys_base = PHYS_BASE;

    /* === KERNEL region: linked at high VA, loaded at PA === */

    /* Exception vectors must be 2KB-aligned (0x800) for VBAR_EL1 */
    .text.vectors : ALIGN(0x800)
    {
        _vectors = .;
        KEEP(*(.text.vectors))
    } > KERNEL AT> RAM

    .text : ALIGN(16)
    {
        *(.text .text.*)
    } > KERNEL AT> RAM

    .rodata : ALIGN(16)
    {
        *(.rodata .rodata.*)
    } > KERNEL AT> RAM

    /* Data must be 16KB-aligned for page table isolation */
    .data : ALIGN(0x4000)
    {
        _sdata = .;
        *(.data .data.*)
        . = ALIGN(8);
        _edata = .;
    } > KERNEL AT> RAM

    .bss (NOLOAD) : ALIGN(0x4000)
    {
        _sbss = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        _ebss = .;
    } > KERNEL

    /* Kernel stack — 256KB, grows downward.
     * Needs to be large enough for debug-mode ProcessImpl construction
     * (ProcessImpl = 32 × Thread × 840 bytes ≈ 26KB on stack). */
    .stack (NOLOAD) : ALIGN(16)
    {
        _stack_bottom = .;
        . += 256K;
        _stack_top = .;
    } > KERNEL

    _end = .;

    /* Discard sections we don't need */
    /DISCARD/ :
    {
        *(.comment)
        *(.note*)
        *(.eh_frame*)
    }
}
