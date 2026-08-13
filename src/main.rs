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
    // ---------------------------------------------------------------
    //  Nothing here yet, so booting this prints nothing at all.
    //
    //  Milestone 2 is the first thing that is yours: make the machine
    //  say something. The UART lives at 0x1000_0000 and writing a byte
    //  to that address sends it out the serial port.
    // ---------------------------------------------------------------
    puts("hi");
    puts("im doin this myself");
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


/// Where a panic ends up.
///
/// Required: `no_std` means nothing else defines it, and the compiler will not
/// build without one. It cannot return, and it cannot unwind, because there is
/// no unwinder -- hence `panic = "abort"` in Cargo.toml.
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}
