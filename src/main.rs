//! LeBOS -- the kernel.
//!
//! Written by Sebastian LeBlanc. The kernel in `reference/` is the one built
//! alongside Claude across milestones 1-20; it still boots (`make ref`) and it
//! is there to be READ when this one gets stuck. Same role xv6 plays for
//! everybody else, except every decision in it is already yours.
//!
//! Milestone 1 -- build and boot harness -- is done: the linker script, the
//! boot assembly and the build tooling are mechanical and typing them teaches
//! nothing. Everything from here is yours.

// No standard library. `std` assumes an operating system underneath it --
// files, threads, a heap, a way to exit. There is nothing underneath this.
#![no_std]
// No Rust `main` either. `main` is called by a runtime that sets up argv and
// the environment before it. Our entry point is `_start`, in assembly.
#![no_main]

use core::panic::PanicInfo;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicUsize, Ordering};

extern "C" {
    fn trap_entry();
}

extern "C" {
    static __kernel_end: u8;
}

fn _print(args: fmt::Arguments) {
    let _ = Uart.write_fmt(args);
}

const BANNER: &str = include_str!("banner.txt");

const INTERVAL: u64 = 10_000_000;

static FREE_LIST: AtomicUsize = AtomicUsize::new(0);

macro_rules! print {
    ($($arg:tt)*) => { _print(format_args!($($arg)*)) };
}

macro_rules! println {
    ()            => { print!("\n") };
    ($($arg:tt)*) => { print!("{}\n", format_args!($($arg)*)) };
}

// entry.S, pasted in at compile time. It has to be assembly: there is no valid
// stack when it runs, and Rust cannot function without one.
core::arch::global_asm!(include_str!("entry.S"));

/// The first Rust that ever runs.
///
/// `extern "C"` because entry.S calls it, and the C calling convention is the
/// one both sides already agree on. `#[no_mangle]` so the name stays exactly
/// `kmain` for `call kmain` to find.
///
/// `-> !` means this never returns. There is nothing to return TO.
#[no_mangle]
pub extern "C" fn kmain(_hartid: usize, _dtb: *const u8) -> ! {

    unsafe {
        core::arch::asm!("csrw stvec, {}", in(reg) trap_entry as *const () as usize);
    }

    println!("{}", BANNER);

    unsafe {
        core::arch::asm!("csrs sie, {}", in(reg) 1usize << 5);
        core::arch::asm!("csrs sstatus, {}", in(reg) 1usize << 1);
    }

    sbi_set_timer(now() + INTERVAL);

    let kernel_end = unsafe {core::ptr::addr_of!(__kernel_end) as usize};
    frame_init(kernel_end, 0x8800_0000);

    let a = frame_alloc();
    let b = frame_alloc();
    let c = frame_alloc();
    println!("{:#x}, {:#x}, {:#x}", a.unwrap(), b.unwrap(), c.unwrap());

    frame_free(b.unwrap());
    let d = frame_alloc();
    println!("{:#x}", d.unwrap());

    loop {
        // Wait For Interrupt: parks the core instead of spinning it at 100%.
        unsafe { core::arch::asm!("wfi") };
    }
}

// prints a byte literal character. if it is a newline, isers \r (cairrage return) to return to the start of the next line.
// uses write volatile because rust would delete it in the end
fn putchar(c: u8) {
    unsafe {
        core::ptr::write_volatile(0x1000_0000 as *mut u8, c);
    }
    if c == b'\n' {
            putchar(b'\r');
    }
}

// puts a string of byte literals down with putchar
fn puts(s: &str) {
    for c in s.bytes() {
        putchar(c);
    }
}

struct Uart;
impl Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        puts(s);
        Ok(())
    }
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

fn now() -> u64 {
    let t: u64;
    unsafe {core::arch::asm!("csrr {}, time", out(reg) t)};
    t
}

fn frame_init(start: usize, end: usize) {
    let mut real_start = start + 4095;
    real_start &= !0xFFF;
    let mut addr = end - 4096;
    while addr >= real_start {
        frame_free(addr);
        addr -= 4096;
    }
}

fn frame_alloc() -> Option<usize> {
    let x: usize = FREE_LIST.load(Ordering::Relaxed);
    if x == 0 {
        return None;
    }
    let next = unsafe {core::ptr::read_volatile(x as *const usize)};
    FREE_LIST.store(next, Ordering::Relaxed);
    Some(x)
}

fn frame_free(page: usize) {
    unsafe {
        core::ptr::write_volatile(page as *mut usize, FREE_LIST.load(Ordering::Relaxed));
    }
    FREE_LIST.store(page, Ordering::Relaxed);
}

#[no_mangle]
extern "C" fn trap_handler() {
    let cause: usize;
    let val: usize;
    let sep: usize;

    unsafe {
        core::arch::asm!("csrr {}, scause", out(reg) cause);
        core::arch::asm!("csrr {}, stval", out(reg) val);
        core::arch::asm!("csrr {}, sepc", out(reg) sep);
    }

    let is_interrupt = (cause as isize) < 0;
    let code = cause & 0xff;

    if !is_interrupt {
        let name = match code {
            0  => "instruction address misaligned",
            1  => "instruction access fault",
            2  => "illegal instruction",
            3  => "breakpoint",
            4  => "load address misaligned",
            5  => "load access fault",
            6  => "store/AMO address misaligned",
            7  => "store/AMO access fault",
            8  => "ecall from user mode",
            9  => "ecall from supervisor mode",
            11 => "ecall from machine mode",
            12 => "instruction page fault",
            13 => "load page fault",
            15 => "store/AMO page fault",
            _  => "unknown exception",
        };
        println!("ERROR: {}", name);
        println!("TRAP");
        println!("sepc = {:#x}", sep);
        println!("scause = {}", cause);
        println!("stval = {:#x}", val);
    } else {
        let name = match code {
            1 => "supervisor software interrupt",
            5 => "supervisor timer interrupt",
            9 => "supervisor external interrupt",
            _ => "unknown interrupt",
        };
        if code == 5 {
            sbi_set_timer(now() + INTERVAL);
            println!("tick");
        } else {
            println!("Interrupt: {}", name);
        }
    }

    let insn = unsafe { core::ptr::read_volatile(sep as *const u16) };

    if !is_interrupt {
        let width = if insn & 0b11 == 0b11 { 4 } else { 2 };
        unsafe {core::arch::asm!("csrw sepc, {}", in(reg) sep + width)}
    }
}

/// Where a panic ends up.
///
/// Required: `no_std` means nothing else defines it, and the compiler will not
/// build without one. It cannot return, and it cannot unwind, because there is
/// no unwinder -- hence `panic = "abort"` in Cargo.toml.
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    println!("PANIC! AT THE KERNEL");
    println!("{}", _info);
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}