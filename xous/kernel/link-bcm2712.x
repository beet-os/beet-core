/* BeetOS Raspberry Pi 5 (BCM2712) linker script.
 *
 * RPi5 boot convention: firmware loads kernel8.img at physical 0x80000.
 * RAM starts at 0x0; the first 512KB is reserved by the firmware.
 *
 * MMU is OFF during early boot — all addresses are physical.
 * start.S drops from EL2 to EL1 if needed, then calls _start_rust.
 */

ENTRY(_start)

MEMORY
{
    /* 8MB region starting at 0x80000 (RPi kernel load address) */
    RAM (rwx) : ORIGIN = 0x00080000, LENGTH = 8M
}

SECTIONS
{
    /* Boot code first — RPi firmware jumps here */
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

    /* Kernel stack — 256KB, grows downward. */
    .stack (NOLOAD) : ALIGN(16)
    {
        _stack_bottom = .;
        . += 256K;
        _stack_top = .;
    } > RAM

    _end = .;

    /DISCARD/ :
    {
        *(.comment)
        *(.note*)
        *(.eh_frame*)
    }
}
