/* Linker script for user programs.
 *
 * 0x1000 rather than 0: leaving the first page unmapped means a null pointer
 * dereference faults instead of quietly reading whatever is at address zero.
 * That is the entire reason every Unix does the same thing.
 *
 * This is in the LOW half of the address space -- the 256 GB the kernel handed
 * back when it relocated to the higher half and dropped the identity map. A
 * user program and the kernel can now both exist without ever colliding,
 * because they live in ranges the hardware keeps separate.
 */

OUTPUT_ARCH(riscv)
ENTRY(_start)

SECTIONS
{
    . = 0x1000;

    .text : {
        KEEP(*(.text.start))    /* _start must be first: the kernel jumps here */
        *(.text .text.*)
    }

    .rodata : { *(.rodata .rodata.*) }
    .data   : { *(.sdata .sdata.* .data .data.*) }
    /* .bss occupies no file space, so nothing that must exist at runtime may
     * live here -- the kernel copies the raw image and does not know to zero
     * anything beyond it. The stack comes from the kernel. */
    .bss    : { *(.sbss .sbss.* .bss .bss.*) }

    /DISCARD/ : { *(.eh_frame) *(.note .note.*) *(.comment) }
}
