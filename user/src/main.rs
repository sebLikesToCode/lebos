// The first LeBOS user program.
//
// It has no standard library, no allocator, no libc, and no way to touch the
// kernel except by trapping. Everything it can do, it must ask for.
//
// Written as assembly rather than Rust for now, because the entry point has to
// be exactly at the start of .text with no prologue -- the kernel jumps
// straight to it, having set up nothing but a stack.

#![no_std]
#![no_main]

// Syscall convention, defined here for the first time:
//
//   a7 = syscall number
//   a0..a5 = arguments
//   a0 = return value
//
// Identical in shape to the SBI calls the kernel makes into OpenSBI, one
// privilege level further down.
//
//   1 = write(ptr, len)
//   0 = exit(code)
core::arch::global_asm!(
    r#"
.section .text.start
.globl _start
_start:
        # 1. A legitimate request. Should succeed and return 14.
        lla     a0, msg         # PC-relative, so it works wherever we are mapped
        li      a1, 14          # length of msg
        li      a7, 1           # SYS_WRITE
        ecall

        # 2. Ask the kernel to print memory we do not own -- a low address that
        #    is simply not mapped for us. Should be refused.
        li      a0, 0x9000
        li      a1, 8
        li      a7, 1
        ecall

        # 3. Ask the kernel to print KERNEL memory. -1 is 0xFFFF...FFFF, which
        #    is in the high half where the kernel lives. Should be refused.
        li      a0, -1
        li      a1, 8
        li      a7, 1
        ecall

        # 4. A valid pointer with a length that runs off the end of our
        #    mapping. Checking only the first page would let this through.
        lla     a0, msg
        li      a1, 8192        # two pages past what we own
        li      a7, 1
        ecall

        li      a0, 0
        li      a7, 0           # SYS_EXIT
        ecall

        # If the kernel ever returns from exit, do not run off the end.
1:      j       1b

msg:
        .ascii "hi from user!\n"
"#
);

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
