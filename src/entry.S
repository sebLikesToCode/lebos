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
# Right now this only forwards to Rust, which is safe solely because
# trap_handler never returns. A trap that resumes the interrupted code must
# first save all 31 general-purpose registers here -- the hardware saves none
# of them -- and restore them before sret. That is the next step.
# ---------------------------------------------------------------------------
.section .text
.align 2
.globl trap_entry
trap_entry:
        j       trap_handler
