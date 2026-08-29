//! Everything machine-specific.
//!
//! Two kinds of thing live here, and they will eventually split into two
//! folders -- but not until there is a second machine to split for.
//!
//!   CPU facts    -- csrw satp, scause, how page tables are shaped.
//!                   Fixed by the RISC-V spec. Same on every RISC-V board.
//!
//!   BOARD facts  -- the UART is at 0x1000_0000, RAM ends at 0x8800_0000.
//!                   Chosen by whoever built the board. Different on a
//!                   VisionFive 2, which is the same CPU.
//!
//! The rule for what goes above this line: if a line names a CPU register or a
//! hardware address, it belongs in here. If it is about what LeBOS DOES, it
//! does not.
//!
//! The interface is VERBS, not addresses -- putchar(byte), never UART_BASE.
//! An x86 serial port is reached with `out` instructions rather than memory
//! writes, so an address-shaped interface has no x86 implementation at all.

core::arch::global_asm!(include_str!("entry.S"));

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};
use crate::{frame_alloc, trap_entry};

static UART_BASE: AtomicUsize = AtomicUsize::new(0x1000_0000);

pub const HIGH_BASE: usize = 0xFFFF_FFC0_0000_0000;

const INTERVAL: u64 = 10_000_000;

const CTX: usize = 112;

extern "C" {
    fn switch_context(old_sp: *mut usize, new_sp: usize);
}

pub fn init_stack(top: usize, entry: usize) -> usize {
    let frame: usize = top - CTX;
    unsafe {
        core::ptr::write_bytes(frame as *mut u8, 0, CTX);
        write_volatile(frame as *mut usize, entry);
    }
    frame
}
pub fn putchar(c: u8) {
    unsafe {
        write_volatile(UART_BASE.load(Ordering::Relaxed) as *mut u8, c);
    }
    if c == b'\n' {
        putchar(b'\r');
    }
}

pub fn switch(old_sp: *mut usize, new_sp: usize) {
    unsafe {
        switch_context(old_sp, new_sp);
    }
}

pub fn traps_on() {
    unsafe { core::arch::asm!("csrw stvec, {}", in(reg) trap_entry as *const usize as usize); }
}

pub fn timer_on() {
    unsafe {
        core::arch::asm!("csrs sie, {}", in(reg) 1usize << 5);
        core::arch::asm!("csrs sstatus, {}", in(reg) 1usize << 1);
    }
}

pub fn idle() {
    unsafe { core::arch::asm!("wfi") }
}

pub fn unmap_low(root: usize) {
    unsafe {
        write_volatile((root + HIGH_BASE + 0 * 8) as *mut usize, 0);
        write_volatile((root + HIGH_BASE + 2 * 8) as *mut usize, 0);
        core::arch::asm!("sfence.vma");
    }
}

pub fn enter_high(dest: usize, arg: usize) -> ! {
    unsafe {
        core::arch::asm!(
        "add sp, sp, {off}",   // the stack moves up
        "jr {dest}",            // and so does the program counter
        off = in(reg) HIGH_BASE,
        dest = in(reg) dest as usize + HIGH_BASE,
        in("a0") arg,
        options(noreturn),
        )
    }
}

pub fn console_relocate(offset: usize) {
    UART_BASE.fetch_add(offset, Ordering::Relaxed);
}

pub fn timer_reset() {
    sbi_set_timer(now() + INTERVAL);
}

fn now() -> u64 {
    let t: u64;
    unsafe {core::arch::asm!("csrr {}, time", out(reg) t)};
    t
}

fn sbi_set_timer(when: u64) {
    unsafe {
        core::arch::asm!(
        "ecall",
        in("a7") 0x5449_4D45_usize,
        in("a6") 0_usize,
        inout("a0") when => _,
        out("a1") _,
        );
    }
}

fn pte(p: Perm) -> usize {
    let bits = match p {
        Perm::Code => 0xCB,
        Perm::Rodata => 0xC3,
        Perm::Data => 0xC7,
    };
    bits
}

pub fn memory_loop(root: usize, start: usize, end: usize, offset: usize, perm: Perm) {
    let mut real_start = start + 4095;
    real_start &= !0xFFF;
    let mut addr = end - 4096;
    while addr >= real_start {
        map_memory_management_unit(root, addr, addr + offset, perm);
        addr -= 4096;
    }
}

fn map_memory_management_unit(root: usize, physical_address: usize, digital_address: usize, perm: Perm ) {
    let slot = (digital_address >> 30) & 511;
    let entry = unsafe { read_volatile((root + slot * 8) as *const usize) };
    let mut new_table: usize = 0;
    if entry & 1 == 0 {
        new_table = frame_alloc().unwrap();
        unsafe {
            core::ptr::write_bytes(new_table as *mut u8, 0, 4096);
            write_volatile((root + slot * 8) as *mut usize, (new_table >> 12) << 10 | 1);
        }
    } else {
        new_table = (entry >> 10) << 12
    }

    let slot2 = (digital_address >> 21) & 511;
    let entry2 = unsafe { read_volatile((new_table + slot2 * 8) as *const usize) };
    let mut newer_table: usize = 0;
    if entry2 & 1 == 0 {
        newer_table = frame_alloc().unwrap();
        unsafe {
            core::ptr::write_bytes(newer_table as *mut u8, 0, 4096);
            write_volatile((new_table + slot2 * 8) as *mut usize, (newer_table >> 12) << 10 | 1);
        }
    } else {
        newer_table = (entry2 >> 10) << 12
    }

    let slot3 = (digital_address >> 12) & 511;
    unsafe { write_volatile((newer_table + slot3 * 8) as *mut usize, (physical_address >> 12) << 10 | pte(perm)) };
}

pub fn map_devices(root: usize, offset: usize) {
    memory_loop(root, 0x1000_0000, 0x1000_1000, offset, Perm::Data);
}

pub fn paging_on(root: usize) {
    let satp = 0x8000_0000_0000_0000usize | (root >> 12);
    unsafe {
        core::arch::asm!("csrw satp, {}", in(reg) satp);
        core::arch::asm!("sfence.vma");
    }
}

#[derive(Clone, Copy)]
pub enum Perm {
    Code,
    Rodata,
    Data,
}

pub enum Trap {
    Timer,
    Fault { cause: usize, address: usize, pc: usize },
    Unknown,
}
#[no_mangle]
pub extern "C" fn trap_handler() {
    let cause: usize;
    let address: usize;
    let pc: usize;

    unsafe {
        core::arch::asm!("csrr {}, scause", out(reg) cause);
        core::arch::asm!("csrr {}, stval", out(reg) address);
        core::arch::asm!("csrr {}, sepc", out(reg) pc);
    }

    let is_interrupt = (cause as isize) < 0;
    let code = cause & 0xff;

    if is_interrupt {
        if code == 5 {
            timer_reset();
            crate::on_trap(Trap::Timer)
        } else {
            crate::on_trap(Trap::Unknown)
        }
    }

    if !is_interrupt {
        let insn = unsafe { read_volatile(pc as *const u16) };
        let width = if insn & 0b11 == 0b11 { 4 } else { 2 };
        unsafe {core::arch::asm!("csrw sepc, {}", in(reg) pc + width)}
        crate::on_trap(Trap::Fault { cause, address, pc })
    }
}