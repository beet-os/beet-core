/* BeetOS AArch64 kernel linker script.
 *
 * Memory layout for the kernel, loaded by the BeetOS loader into upper
 * virtual address space (TTBR1). All addresses from beetos constants crate.
 *
 * The kernel runs at EL1 with 16KB pages (Apple Silicon granule).
 */

ENTRY(_start)

MEMORY
{
    /* Kernel code + rodata + data + bss.
     * KERNEL_LOAD_OFFSET = 0xFFFF_FFFF_FFD0_0000
     * NUM_KERNEL_PAGES_MAX = 128, so max size = 128 * 16KB = 2MB */
    KERNEL (rwx) : ORIGIN = 0xFFFFFFFFFFD00000, LENGTH = 2M
}

SECTIONS
{
    /* Exception vectors must be 2KB-aligned (0x800) */
    .text.vectors ORIGIN(KERNEL) : ALIGN(0x800)
    {
        KEEP(*(.text.vectors))
    } > KERNEL

    .text : ALIGN(16)
    {
        *(.text .text.*)
    } > KERNEL

    .rodata : ALIGN(16)
    {
        *(.rodata .rodata.*)
    } > KERNEL

    /* Data must be 16KB-aligned for page table isolation */
    .data : ALIGN(0x4000)
    {
        _sdata = .;
        *(.data .data.*)
        . = ALIGN(8);
        _edata = .;
    } > KERNEL

    .bss (NOLOAD) : ALIGN(0x4000)
    {
        _sbss = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        _ebss = .;
    } > KERNEL

    _end = .;

    /* Discard debug sections for size */
    /DISCARD/ :
    {
        *(.comment)
        *(.note*)
        *(.eh_frame*)
    }
}
