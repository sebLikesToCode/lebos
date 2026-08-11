# The first instructions of your operating system.
#
# OpenSBI (the firmware, shipped with QEMU) has already done the truly ugly
# parts: it started in machine mode, initialised RAM, and dropped the CPU into
# supervisor mode. It then jumps here with:
#
#     a0 = hartid          (which CPU core we are; 0, since we run -smp 1)
#     a1 = pointer to the device tree blob describing the hardware
#
# Those two registers are the only inheritance you get. Do not clobber a1
# before kmain reads it -- when you write the memory-map parser at milestone 5,
# that pointer is how you find out how much RAM exists.
#
# Rust cannot run yet. Rust code assumes two things this machine does not yet
# provide: a valid stack pointer, and a zeroed .bss. We supply both, then leave.

.section .text.entry
.globl _start
_start:
        # 1. Give ourselves a stack. Until this instruction lands, any function
        #    call anywhere would write through a garbage sp and corrupt RAM.
        la      sp, __stack_top

        # 2. Zero .bss. The ELF does not store these bytes -- it only records
        #    how many there are -- so on real hardware they contain whatever
        #    the last power cycle left behind. Rust statics assume zero.
        la      t0, __bss_start
        la      t1, __bss_end
1:      bgeu    t0, t1, 2f              # while t0 < t1:
        sd      zero, (t0)              #     *t0 = 0
        addi    t0, t0, 8               #     t0 += 8
        j       1b
2:

        # 3. Into Rust. a0 and a1 still hold hartid and the device tree
        #    pointer, which is exactly the RISC-V calling convention for
        #    kmain(hartid: usize, dtb: *const u8).
        call    kmain

        # kmain must never return. If it does, wfi in a loop rather than
        # running off into whatever bytes follow -- a hang is far easier to
        # diagnose than a runaway program.
3:      wfi
        j       3b


# ---------------------------------------------------------------------------
# Trap entry.
#
# Every exception and interrupt lands here, because kmain writes this address
# into the stvec CSR.
#
# It lives in assembly for a specific reason: stvec's low two bits are a MODE
# field (00 = direct, 01 = vectored, 10/11 = reserved), not address bits. So
# the entry point must be at least 4-byte aligned or the mode field gets
# garbage. Rust cannot align a function -- #[repr(align(4))] applies only to
# structs, enums, and unions -- so the alignment has to be asserted here.
#
# Watch out: RISC-V .align takes a POWER OF TWO. `.align 2` means 2^2 = 4
# bytes, not 2 bytes.
#
# The hardware saves NO general-purpose registers on a trap. It only sets
# sepc, scause, and stval. Every one of the 32 registers still holds whatever
# the interrupted code had in it, and Rust will start clobbering them the
# instant it runs. So this code saves all of them to the stack, calls Rust,
# and puts them back.
#
# That block of saved registers is the "trap frame". Its layout is exactly
# x0..x31 in register-number order, 8 bytes each, 256 bytes total, so Rust can
# view it as [usize; 32] and index it by register number.
#
# A note on stacks: this runs on whatever stack was already active, which is
# fine while everything is kernel code. Once user mode exists (milestone 10),
# trapping from userspace means sp holds a USER stack pointer that cannot be
# trusted or even relied on to be mapped -- at that point this has to swap to
# a known-good kernel stack via the sscratch CSR first.
# ---------------------------------------------------------------------------

.macro SAVE_GP i
        sd      x\i, \i*8(sp)
.endm

.macro LOAD_GP i
        ld      x\i, \i*8(sp)
.endm

.section .text
.align 2
.globl trap_entry
trap_entry:
        addi    sp, sp, -256

        # Save everything except x2 (sp), which we just changed. Its original
        # value is reconstructed a few instructions down.
        .irp n, 0,1,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        SAVE_GP \n
        .endr

        # Record the interrupted code's sp into its slot. Safe to clobber t0
        # (x5) here because it was saved by the loop above.
        addi    t0, sp, 256
        sd      t0, 2*8(sp)

        # First argument to trap_handler: a pointer to the frame we just built.
        mv      a0, sp
        call    trap_handler

        # Restore. x0 is skipped because it is hardwired to zero, and x2
        # because the stack adjustment below is what restores sp.
        .irp n, 1,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        LOAD_GP \n
        .endr

        addi    sp, sp, 256

        # Return to sepc and restore the pre-trap interrupt-enable state.
        # NOT `ret` -- that would jump to ra, which belongs to the interrupted
        # code and has nothing to do with where the trap happened.
        sret
