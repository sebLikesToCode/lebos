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

/// First Rust code to run. Called from `_start` with the two values OpenSBI
/// left in a0/a1.
///
/// `extern "C"` gives it the C ABI so assembly can call it, and `#[no_mangle]`
/// keeps the symbol literally named `kmain` so the `call kmain` resolves.
///
/// It must never return -- hence `-> !`.
#[no_mangle]
pub extern "C" fn kmain(hartid: usize, dtb: *const u8) -> ! {
    println!("LeBOS booting");
    println!("hart {} | dtb at {:#x}", hartid, dtb as usize);

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
