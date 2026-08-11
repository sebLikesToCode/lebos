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

use core::panic::PanicInfo;

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
pub extern "C" fn kmain(_hartid: usize, _dtb: *const u8) -> ! {
    // ================================================================
    // MILESTONE 2 -- this is yours to write.
    //
    // Goal: make the letters "hello" appear in your terminal.
    //
    // The UART (the serial port chip) on the QEMU virt board is a 16550,
    // and it is memory-mapped at physical address 0x1000_0000. Writing a
    // byte to that address transmits that byte out the serial line, which
    // QEMU has connected to your terminal via `-serial mon:stdio`.
    //
    // So: write the byte 0x48 ('H') to the address 0x1000_0000 and it
    // appears on screen. That is the entire mechanism. No driver, no
    // initialisation, no interrupts -- one store instruction.
    //
    // What you need to look up:
    //   - core::ptr::write_volatile, and why a plain `*p = x` is wrong here
    //     (hint: the optimiser can legally delete stores to memory nothing
    //     ever reads back, and MMIO is memory nothing ever reads back)
    //   - why this needs an `unsafe` block, and what you are promising the
    //     compiler by writing one
    //
    // Once a single byte works, in order:
    //   1. a putchar(c: u8) function
    //   2. a puts(s: &str) built on it
    //   3. implement core::fmt::Write for a Uart struct -- this is the
    //      moment you get println!() with formatting, and it is the single
    //      biggest quality-of-life jump in the whole project. Do it early.
    //
    // Reference: xv6-riscv kernel/uart.c is ~100 lines and does all of this.
    // ================================================================

    puts("rust is convolouted\n");
    puts("if you see this it worked");

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

/// Where Rust goes when something goes wrong. On a hosted system this unwinds
/// and prints a backtrace. Here, nothing exists to catch it.
///
/// Once you have serial output working, come back and print `info` -- it
/// carries the file, line, and message of the panic, and having that visible
/// will save you many hours.
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}
