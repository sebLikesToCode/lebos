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
