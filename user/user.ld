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

    /* WRITABLE DATA STARTS AT A FIXED ADDRESS.
     *
     * The kernel receives a flat binary with no section table, so it has no
     * way to discover where read-only code ends and writable data begins --
     * and it must know, because W^X applies to user pages: mapping the whole
     * image r-x makes any global variable fault on first write, and mapping it
     * all rw-x hands a program the ability to rewrite its own code.
     *
     * So the boundary is a constant both sides agree on. Ugly, and deliberately
     * temporary: milestone 19 has to parse a real ELF anyway, which carries a
     * permissions field per segment and deletes this whole arrangement.
     *
     * The gap is padded with zeros by objcopy, which costs image size and
     * nothing else.
     */
    . = 0x8000;
    .data   : { *(.sdata .sdata.* .data .data.*) }
    .bss    : { *(.sbss .sbss.* .bss .bss.*) }
    __bss_end = .;

    ASSERT(ADDR(.data) == 0x8000,
           "code and rodata overran 0x8000 -- raise USER_DATA_BASE in main.rs too")
    ASSERT(__bss_end <= 0xC000,
           "data + bss overran 16 KiB -- raise USER_DATA_SIZE in main.rs too")

    /DISCARD/ : { *(.eh_frame) *(.note .note.*) *(.comment) }
}
