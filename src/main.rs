// The kernel.
//
// #![no_std]  -- there is no standard library. `std` assumes an OS underneath
//                it (files, threads, heap, stdout). We are the OS. What's left
//                is `core`: types, iterators, Option/Result, no allocation.
//
// #![no_main] -- there is no `main`. The normal Rust entry point is installed
//                by the C runtime, which is also absent. Our entry point is
//                _start in entry.S, which calls kmain below.

#![no_std]
#![no_main]

use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, Ordering};

/// Timer interrupts since boot. Atomic rather than `static mut` so it stays
/// correct once there is more than one hart.
static TICKS: AtomicU64 = AtomicU64::new(0);

macro_rules! print {
    ($($arg:tt)*) => {
        _print(format_args!($($arg)*))
    };
}

macro_rules! println {
    () => {
        print!("\n")
    };
    ($($arg:tt)*) => {
        print!("{}\n", format_args!($($arg)*))
    };
}

// Pull entry.S into the binary. Its `.text.entry` section is placed at
// 0x80200000 by linker.ld, which is where OpenSBI jumps.
core::arch::global_asm!(include_str!("entry.S"));

extern "C" {
    /// The 4-byte-aligned trap trampoline in entry.S. Never called from Rust;
    /// its address is what goes into stvec.
    fn trap_entry();
}

/// First Rust code to run. Called from `_start` with the two values OpenSBI
/// left in a0/a1.
///
/// `extern "C"` gives it the C ABI so assembly can call it, and `#[no_mangle]`
/// keeps the symbol literally named `kmain` so the `call kmain` resolves.
///
/// It must never return -- hence `-> !`.
#[no_mangle]
pub extern "C" fn kmain(hartid: usize, dtb: *const u8) -> ! {
    // Point stvec at the assembly trampoline in entry.S, not at trap_handler
    // directly: stvec's low two bits are a mode field, so the address must be
    // 4-byte aligned, and Rust cannot align a function.
    unsafe {
        core::arch::asm!("csrw stvec, {}", in(reg) trap_entry as *const () as usize);
    }

    println!("LeBOS booting");
    println!("hart {} | dtb at {:#x}", hartid, dtb as usize);

    // Schedule the first timer interrupt one tick out.
    sbi_set_timer(now() + TICK_INTERVAL);

    // Two separate enables, both required:
    //   sie bit 5  (STIE) -- allow supervisor timer interrupts specifically
    //   sstatus bit 1 (SIE) -- the master interrupt enable
    //
    // `csrs` sets the given bits and leaves the rest of the register alone.
    // (`csrw` would overwrite the whole thing and destroy unrelated state;
    //  `csrc` clears bits.)
    unsafe {
        core::arch::asm!("csrs sie, {}", in(reg) 1_usize << 5);
        core::arch::asm!("csrs sstatus, {}", in(reg) 1_usize << 1);
    }

    loop {
        // Wait For Interrupt: idles the core instead of spinning it at 100%.
        unsafe { core::arch::asm!("wfi") };
    }
}

fn putchar(c: u8) {
    const UART0: *mut u8 = 0x1000_0000 as *mut u8;

    if c == b'\n' {
        putchar(b'\r');
    }

    unsafe {
        core::ptr::write_volatile(UART0, c);
    }
}

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

fn _print(args: fmt::Arguments) {
    let mut uart = Uart;
    let _ = uart.write_fmt(args);
}

/// Timer ticks per second on the QEMU virt board. Straight from the OpenSBI
/// banner: "Platform Timer Device : aclint-mtimer @ 10000000Hz".
const TIMER_HZ: u64 = 10_000_000;

/// How often the timer fires. This is POLICY -- ours to choose -- unlike
/// TIMER_HZ above, which is a fact about the hardware. 100 Hz matches Linux's
/// typical HZ. At milestone 9 this becomes the scheduler quantum.
const TICK_INTERVAL: u64 = TIMER_HZ / 100;

/// Read the `time` CSR -- a counter incrementing at TIMER_HZ.
fn now() -> u64 {
    let t: u64;
    unsafe {
        core::arch::asm!("csrr {}, time", out(reg) t);
    }
    t
}

/// Ask OpenSBI to raise a timer interrupt at `when`.
///
/// The timer compare register is M-mode only, so S-mode cannot write it
/// directly. `ecall` traps up to the firmware, which does it on our behalf.
///
/// SBI calling convention: a7 = extension id, a6 = function id, a0.. = args.
/// 0x54494D45 is the ASCII bytes "TIME". This is the same mechanism user
/// programs will use to call into us at milestone 10, one privilege level
/// further down.
fn sbi_set_timer(when: u64) {
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") 0x5449_4D45_usize,
            in("a6") 0_usize,
            in("a0") when,
            out("a1") _,
        );
    }
}

/// Where Rust goes when something goes wrong. On a hosted system this unwinds
/// and prints a backtrace. Here, nothing exists to catch it.
///
/// Once you have serial output working, come back and print `info` -- it
/// carries the file, line, and message of the panic, and having that visible
/// will save you many hours.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("PANIC! AT THE KERNEL: {}", info);
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

#[repr(C)]
pub struct TrapFrame {
    pub x: [usize; 32],
}

#[no_mangle]
extern "C" fn trap_handler(frame: &mut TrapFrame) {
    let scause: usize;
    let sepc: usize;
    let stval: usize;

    unsafe {
        core::arch::asm!("csrr {}, scause", out(reg) scause);
        core::arch::asm!("csrr {}, sepc",   out(reg) sepc);
        core::arch::asm!("csrr {}, stval",  out(reg) stval);
    }

    // scause's top bit: set means interrupt, clear means exception.
    // Reinterpreting as signed makes "top bit set" the same as "negative".
    let is_interrupt = (scause as isize) < 0;
    let code = scause & 0xff;

    if is_interrupt && code == 5 {
        // Supervisor timer. Rearming is not optional and not just about
        // scheduling the next tick -- it is how we ACKNOWLEDGE this one.
        //
        // The timer interrupt is LEVEL-triggered: the condition is
        // `time >= timecmp`, and `time` only counts up, so once it fires the
        // signal stays asserted forever. Returning without pushing timecmp
        // into the future means the CPU re-traps at the very next instruction
        // boundary, forever. Measured: ~165,000 ticks per second instead of 1.
        sbi_set_timer(now() + TICK_INTERVAL);

        // Ticking at 100 Hz, so print once a second rather than flooding the
        // serial line. The counter itself is the kernel's notion of uptime.
        let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
        if n % 100 == 0 {
            println!("tick {} -- up {}s", n, n / 100);
        }

        // Deliberately do NOT touch sepc. An interrupt means the instruction
        // at sepc has not run yet -- skipping it would silently drop a good
        // instruction. Only exceptions get stepped over.
        return;
    }

    println!("*** TRAP ***");
    println!(
        "  scause {:#x}  sepc {:#x}  stval {:#x}",
        scause, sepc, stval
    );
    println!("  ra {:#x}  sp {:#x}", frame.x[1], frame.x[2]);

    // Step over the faulting instruction, or we return straight back onto it
    // and trap forever. RISC-V instructions are 4 bytes UNLESS the low two
    // bits are something other than 0b11, which marks a 2-byte compressed
    // instruction. `unimp` is compressed -- it is literally 0x0000.
    let insn = unsafe { core::ptr::read_volatile(sepc as *const u16) };
    let len = if insn & 0b11 == 0b11 { 4 } else { 2 };

    unsafe {
        core::arch::asm!("csrw sepc, {}", in(reg) sepc + len);
    }
}
