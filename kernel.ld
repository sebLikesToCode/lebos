/* Where the kernel goes in memory -- and there are now TWO answers.
 *
 * OpenSBI loads us at 0x80200000 and jumps there with paging off, so that is
 * where the bytes must physically land. But the kernel RUNS in the higher half,
 * and the difference matters for one specific reason:
 *
 *   ~436 absolute addresses are baked into .rodata by the linker.
 *
 * Those are vtables. `println!` goes through core::fmt, which uses dynamic
 * dispatch, which means a table of function pointers -- and a function pointer
 * is eight bytes containing a NUMBER, written by the linker. code-model=medium
 * makes generated CODE position-independent; it does nothing about data the
 * linker writes. So those 436 numbers are whatever address the kernel is LINKED
 * at, forever, no matter where it executes.
 *
 * Link low and printing dies the instant the low half is unmapped. Link high
 * and printing does not work until the high half exists.
 *
 * We choose the second, because it fails at a known moment during boot rather
 * than permanently afterwards. The cost is a rule:
 *
 *   NOTHING MAY PRINT, PANIC, OR DYNAMICALLY DISPATCH BEFORE THE JUMP.
 *
 * VMA (where it runs) is high. LMA (where it loads) is low. `> virt AT> phys`
 * is how a linker script says exactly that.
 *
 * entry.S survives because `la` is PC-relative: it computes "here plus a fixed
 * distance", so symbols resolve to whichever alias is executing, for free.
 */

OUTPUT_ARCH(riscv)
ENTRY(_start)

HIGH_BASE = 0xFFFFFFC000000000;

MEMORY
{
    phys (rwx) : ORIGIN = 0x80200000,             LENGTH = 64M
    virt (rwx) : ORIGIN = 0xFFFFFFC080200000,     LENGTH = 64M
}

SECTIONS
{
    /* Every boundary symbol is defined INSIDE its section. Defined outside,
     * the location counter is not yet in the `virt` region, so the symbol gets
     * a near-zero address and every PC-relative reference to it overflows. */

    .text : {
        __text_start = .;
        /* .text.entry FIRST, so _start lands exactly at 0x80200000. If any
         * other code got there first, OpenSBI would jump into the middle. */
        KEEP(*(.text.entry))
        *(.text .text.*)
        . = ALIGN(4096);
        __text_end = .;
    } > virt AT> phys

    .rodata : {
        __rodata_start = .;
        *(.rodata .rodata.*)
        . = ALIGN(4096);
        __rodata_end = .;
    } > virt AT> phys

    .data : {
        __data_start = .;
        *(.sdata .sdata.* .data .data.*)
    } > virt AT> phys

    .bss : {
        . = ALIGN(8);
        __bss_start = .;
        *(.sbss .sbss.* .bss .bss.*)
        . = ALIGN(8);
        __bss_end = .;

        /* The boot stack. 64 KiB, growing DOWNWARD from __stack_top. */
        . = ALIGN(16);
        __stack_bottom = .;
        . += 64 * 1024;
        __stack_top = .;

        . = ALIGN(4096);
        __kernel_end = .;
    } > virt AT> phys

    /DISCARD/ : { *(.eh_frame) *(.note .note.*) *(.comment) }
}
