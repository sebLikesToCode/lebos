# The first instructions of your operating system.
#
# OpenSBI -- the firmware QEMU ships -- has already done the ugly parts. It
# started in machine mode, initialised RAM, dropped the CPU into SUPERVISOR
# mode, and jumped here with:
#
#     a0 = hartid    which CPU core this is (0, since we run -smp 1)
#     a1 = a pointer to the device tree blob describing the hardware
#
# Those two registers are the entire inheritance. Do not clobber a1 before
# kmain reads it: that pointer is how the kernel finds out how much RAM exists.
#
# Rust cannot run yet. Rust code assumes two things this machine does not
# provide: a valid stack pointer, and a zeroed .bss. Supply both, then leave.

.section .text.entry
.globl _start
_start:
        # 1. A stack. Until this instruction lands, any function call anywhere
        #    writes through a garbage sp and corrupts RAM.
        la      sp, __stack_top

        # 2. Zero .bss. The ELF only records HOW MANY of these bytes there are,
        #    not what is in them, so on real hardware they hold whatever the
        #    last power cycle left behind. Rust statics assume zero.
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

        # kmain must never return. If it does, halt rather than running off
        # into whatever bytes follow -- a hang is far easier to diagnose than a
        # runaway program.
3:      wfi
        j       3b


# ---------------------------------------------------------------------------
# Trap entry.
#
# Every exception and interrupt lands here, because kmain writes this address
# into stvec.
#
# It is assembly for two reasons.
#
# ONE: stvec's low two bits are a MODE field, not address bits. The entry point
# must therefore be at least 4-byte aligned, and Rust cannot align a function
# -- #[repr(align)] applies to structs, enums and unions, not to fn. So the
# alignment has to be asserted here. (Watch out: RISC-V `.align` takes a POWER
# OF TWO. `.align 2` means 2^2 = 4 bytes, not 2 bytes.)
#
# TWO: the hardware saves NO general-purpose registers on a trap. It sets sepc,
# scause and stval, and that is all. Every one of the 32 registers still holds
# whatever the interrupted code had in it -- and Rust will start clobbering
# them the instant it runs. So all of them are saved here, before any Rust, and
# put back afterwards.
#
# That block of saved registers is the "trap frame". Its layout is exactly
# x0..x31 in register-number order, 8 bytes each, 256 bytes total, so that Rust
# can eventually view it as [usize; 32] and index it by register number.
#
# Not saved yet: sepc and sstatus. They are single global CSRs, and the moment
# a scheduler exists that becomes the worst bug in this kernel -- another
# thread traps, overwrites sepc, and this sret jumps to ITS program counter.
# Right now nothing can trap between entry and exit, so reading them live is
# correct. Milestone 8 is when it stops being.
#
# A note on stacks: this runs on whatever stack was already active, which is
# fine while everything is kernel code. Once user mode exists, sp will hold an
# untrusted value and this has to swap to a known-good kernel stack first.
# ---------------------------------------------------------------------------

.equ FRAME, 256

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
        addi    sp, sp, -FRAME

        # x0 is hardwired zero -- nothing to save. x2 is sp, which we have just
        # moved, so it is recorded separately below.
        .irp n, 1,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        SAVE_GP \n
        .endr

        # The interrupted sp is exactly one frame above where we are now. t0 is
        # already saved, so clobbering it is free.
        addi    t0, sp, FRAME
        sd      t0, 2*8(sp)

        call    trap_handler

        .irp n, 1,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        LOAD_GP \n
        .endr

        addi    sp, sp, FRAME

        # NOT `ret`. `ret` jumps to ra, which belongs to the interrupted code
        # and has nothing to do with where the trap happened. `sret` jumps to
        # sepc and restores the pre-trap interrupt-enable state.
        sret

# ---------------------------------------------------------------------------
# switch_context(old_sp: *mut usize, new_sp: usize)
#
#   a0 -- where to write the outgoing thread's stack pointer
#   a1 -- the incoming thread's stack pointer
#
# Saves ra and s0-s11 and nothing else. This is an ordinary function call, so
# the calling convention has already forced the caller to spill anything it
# cared about in t0-t6 and a0-a7. A callee is only obliged to preserve s0-s11,
# and honouring that obligation is the entire job.
#
# trap_entry above saves all 31 registers instead, because an interrupt is not
# a call: it lands between two arbitrary instructions and nobody agreed to
# save anything first.
#
# 13 registers * 8 = 104 bytes, rounded to 112 so sp stays 16-byte aligned.

.equ CTX, 112

.section .text
.align 2
.globl switch_context
switch_context:
        addi    sp, sp, -CTX
        sd      ra,   0*8(sp)
        sd      s0,   1*8(sp)
        sd      s1,   2*8(sp)
        sd      s2,   3*8(sp)
        sd      s3,   4*8(sp)
        sd      s4,   5*8(sp)
        sd      s5,   6*8(sp)
        sd      s6,   7*8(sp)
        sd      s7,   8*8(sp)
        sd      s8,   9*8(sp)
        sd      s9,  10*8(sp)
        sd      s10, 11*8(sp)
        sd      s11, 12*8(sp)

        # hand the outgoing stack pointer back to the scheduler
        sd      sp, 0(a0)

        # everything below this line runs on the INCOMING thread's stack
        mv      sp, a1

        ld      ra,   0*8(sp)
        ld      s0,   1*8(sp)
        ld      s1,   2*8(sp)
        ld      s2,   3*8(sp)
        ld      s3,   4*8(sp)
        ld      s4,   5*8(sp)
        ld      s5,   6*8(sp)
        ld      s6,   7*8(sp)
        ld      s7,   8*8(sp)
        ld      s8,   9*8(sp)
        ld      s9,  10*8(sp)
        ld      s10, 11*8(sp)
        ld      s11, 12*8(sp)
        addi    sp, sp, CTX

        # Returns to whoever called switch_context on the INCOMING thread --
        # a different place than the one we arrived from.
        ret
