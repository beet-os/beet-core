/* BeetOS userspace linker script for aarch64-unknown-none.
 *
 * Static ELF (ET_EXEC) loaded by the kernel's load_elf() into a
 * per-process address space with its own page tables.
 *
 * Code is placed at 0x10_0000_0000 (64 GiB = L1[1] with 16KB granule).
 * This is in a separate L1 index from the kernel identity map (L1[0]),
 * avoiding any conflict with kernel MMIO or RAM mappings.
 *
 * IMPORTANT: Each section must be 16KB-aligned (Apple Silicon page size).
 * Without this, .text and .rodata can share a 16KB page, and load_elf
 * will overwrite the RX mapping with RO, causing instruction aborts.
 *
 * Stack is allocated separately by the kernel at USER_STACK_BOTTOM.
 */

ENTRY(_start)

MEMORY
{
    CODE (rx) : ORIGIN = 0x1000000000, LENGTH = 4M
}

SECTIONS
{
    .text : ALIGN(0x4000)
    {
        *(.text.boot)
        *(.text .text.*)
    } > CODE

    .rodata : ALIGN(0x4000)
    {
        *(.rodata .rodata.*)
    } > CODE

    .data : ALIGN(0x4000)
    {
        *(.data .data.*)
    } > CODE

    .bss : ALIGN(0x4000)
    {
        *(.bss .bss.*)
        *(COMMON)
    } > CODE

    /DISCARD/ :
    {
        *(.comment)
        *(.note*)
        *(.eh_frame*)
    }
}
