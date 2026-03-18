/* BeetOS QEMU virt linker script.
 *
 * Kernel loaded directly by QEMU (-kernel) into RAM.
 * QEMU virt RAM starts at 0x4000_0000.
 * We load at 0x4008_0000 to leave space for FDT at RAM base.
 *
 * MMU is OFF during early boot — all addresses are physical.
 * Once MMU is enabled, the kernel will be re-mapped to upper VA space.
 */

ENTRY(_start)

MEMORY
{
    /* 8MB region starting at 0x4008_0000 */
    RAM (rwx) : ORIGIN = 0x40080000, LENGTH = 8M
}

SECTIONS
{
    /* Boot code first — this is where QEMU starts execution */
    .text.boot : ALIGN(16)
    {
        KEEP(*(.text.boot))
    } > RAM

    /* Exception vectors must be 2KB-aligned (0x800) for VBAR_EL1 */
    .text.vectors : ALIGN(0x800)
    {
        _vectors = .;
        KEEP(*(.text.vectors))
    } > RAM

    .text : ALIGN(16)
    {
        *(.text .text.*)
    } > RAM

    .rodata : ALIGN(16)
    {
        *(.rodata .rodata.*)
    } > RAM

    .data : ALIGN(16)
    {
        _sdata = .;
        *(.data .data.*)
        . = ALIGN(8);
        _edata = .;
    } > RAM

    .bss (NOLOAD) : ALIGN(16)
    {
        _sbss = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        _ebss = .;
    } > RAM

    /* Kernel stack — 64KB, grows downward */
    .stack (NOLOAD) : ALIGN(16)
    {
        _stack_bottom = .;
        . += 64K;
        _stack_top = .;
    } > RAM

    _end = .;

    /* Discard sections we don't need */
    /DISCARD/ :
    {
        *(.comment)
        *(.note*)
        *(.eh_frame*)
    }
}
