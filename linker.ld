/* Linker script for the QEMU `virt` RISC-V board.
 *
 * This file answers one question: where does every byte of the kernel go in
 * physical memory? On a hosted system the OS loader decides that for you.
 * You are the OS now, so you decide.
 *
 * Memory map of the QEMU virt board (the parts that matter today):
 *
 *   0x0000_1000   boot ROM, where the CPU starts
 *   0x0200_0000   CLINT   -- timer and software interrupts
 *   0x0c00_0000   PLIC    -- external interrupt controller
 *   0x1000_0000   UART0   -- the serial port. Your printf lives here.
 *   0x8000_0000   RAM starts. OpenSBI firmware occupies the first 2 MiB.
 *   0x8020_0000   <- us. First byte of RAM that OpenSBI leaves to the kernel.
 *
 * You can confirm all of this yourself rather than trusting me:
 *     make dumpdtb        (dumps the device tree QEMU hands the kernel)
 */

OUTPUT_ARCH(riscv)
ENTRY(_start)

/* Two regions, because every section has two addresses:
 *
 *   LMA (load address)    where the bytes physically land. OpenSBI drops the
 *                         kernel at 0x80200000, so this is not negotiable.
 *   VMA (virtual address) what the code believes its address is. This is what
 *                         the linker writes into vtables and any other data
 *                         holding an absolute pointer.
 *
 * Making VMA high is the whole point. code-model=medium already makes CODE
 * position-independent, but it cannot touch absolute addresses the LINKER
 * writes into DATA -- and this binary carries ~436 of them in .rodata, mostly
 * vtables. Those only become high-half addresses if the linker thinks the
 * kernel lives in the high half.
 *
 * `> virt AT> phys` on each section is what separates the two.
 */
MEMORY
{
    phys (wxa) : ORIGIN = 0x80200000,         LENGTH = 126M
    virt (wxa) : ORIGIN = 0xFFFFFFC080200000, LENGTH = 126M
}

SECTIONS
{

    /* Every section boundary below is 4096-aligned. That is not cosmetic:
     * permissions are granted per page, so if .text and .rodata shared a page
     * you could not make one executable and the other not. Alignment is what
     * makes W^X expressible at all. */

    /* .text.entry is forced first so that _start sits at exactly 0x80200000.
     * OpenSBI jumps to that address blindly -- if some other function landed
     * there instead, the machine would execute it and die confusingly. */
    .text : {
        __text_start = .;
        KEEP(*(.text.entry))
        *(.text .text.*)
        . = ALIGN(4096);
        __text_end = .;
    } > virt AT> phys

    /* Read-only data: string literals, constant tables. Mapped R, no W, no X. */
    .rodata : {
        . = ALIGN(4096);
        __rodata_start = .;
        *(.rodata .rodata.*)
        . = ALIGN(4096);
        __rodata_end = .;
    } > virt AT> phys

    /* Initialised globals. Everything from here to __kernel_end is R+W. */
    .data : {
        . = ALIGN(4096);
        __data_start = .;
        *(.sdata .sdata.*)
        *(.data .data.*)
    } > virt AT> phys

    /* Zero-initialised globals. These occupy no space in the ELF file -- the
     * loader is expected to zero them. There is no loader. YOU zero them, in
     * entry.S, which is why these two symbols are exported. */
    .bss : {
        . = ALIGN(16);
        __bss_start = .;
        *(.sbss .sbss.*)
        *(.bss .bss.*)
        . = ALIGN(16);
        __bss_end = .;
    } > virt AT> phys

    /* The boot stack. RISC-V stacks grow downward, so the stack POINTER is
     * initialised to __stack_top (the high end) and moves down toward
     * __stack_bottom. Overflowing past the bottom silently corrupts .bss --
     * a classic and miserable bug. 64 KiB is generous for now. */
    . = ALIGN(16);
    __stack_bottom = .;
    . += 64K;
    __stack_top = .;

    /* Everything above this point is the kernel image. The physical memory
     * allocator (milestone 5) will hand out frames starting here, so it needs
     * to know where "here" is. */
    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ : { *(.eh_frame) *(.note .note.*) *(.comment) }
}
