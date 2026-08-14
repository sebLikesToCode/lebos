/* Where the kernel goes in memory.
 *
 * OpenSBI occupies the low 2 MiB of RAM and jumps to 0x8020_0000, so that is
 * where the first instruction must be. Nothing chose that number -- the
 * firmware did, and this file exists to agree with it.
 *
 * Deliberately simple. The reference kernel links high and relocates itself at
 * milestone 6c; that split makes ~435 absolute addresses in .rodata point at
 * an address space that does not exist yet, which is a genuine problem worth
 * meeting on its own rather than on day one.
 */

OUTPUT_ARCH(riscv)
ENTRY(_start)

SECTIONS
{
    . = 0x80200000;

    /* Each region is padded to a whole number of 4096-byte pages, and its
     * boundaries exported, so the kernel can give each one different
     * permissions. A page can only have ONE set of permissions, so a boundary
     * landing mid-page would force a choice between "code is writable" and
     * "data is executable" -- both wrong. ALIGN(4096) removes the choice.
     *
     * These symbols have no value, only an address. Read them in Rust with
     * core::ptr::addr_of!, never by dereferencing. */

    __text_start = .;
    /* .text.entry FIRST, so _start lands exactly at 0x80200000. If any other
     * code got there first, OpenSBI would jump into the middle of it. */
    .text : {
        KEEP(*(.text.entry))
        *(.text .text.*)
    }
    . = ALIGN(4096);
    __text_end = .;

    __rodata_start = .;
    .rodata : { *(.rodata .rodata.*) }
    . = ALIGN(4096);
    __rodata_end = .;

    __data_start = .;
    .data   : { *(.sdata .sdata.* .data .data.*) }

    /* .bss occupies NO space in the file -- it is a promise that some zeroed
     * bytes will exist, not a record of them. entry.S has to zero it, which is
     * why these two symbols are exported. */
    . = ALIGN(8);
    __bss_start = .;
    .bss : { *(.sbss .sbss.* .bss .bss.*) }
    . = ALIGN(8);
    __bss_end = .;

    /* The boot stack. 64 KiB, and it grows DOWNWARD from __stack_top. */
    . = ALIGN(16);
    __stack_bottom = .;
    . += 64 * 1024;
    __stack_top = .;

    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ : { *(.eh_frame) *(.note .note.*) *(.comment) }
}
