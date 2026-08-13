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

    /* Each section gets its own page, so no page ever needs two different
     * permissions at once. Without this a segment boundary can land mid-page
     * and the loader has to choose which of two answers to be wrong about.
     *
     * The fixed 0x8000 data address that used to live here is gone. The kernel
     * reads an ELF now, and an ELF states permissions per segment -- so the
     * boundary is a fact carried in the file rather than a constant two
     * unrelated files had to agree on.
     */
    . = ALIGN(4096);
    .rodata : { *(.rodata .rodata.*) }

    . = ALIGN(4096);
    .data   : { *(.sdata .sdata.* .data .data.*) }
    .bss    : { *(.sbss .sbss.* .bss .bss.*) }

    /DISCARD/ : { *(.eh_frame) *(.note .note.*) *(.comment) }
}
