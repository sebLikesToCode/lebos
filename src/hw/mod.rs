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

use core::ptr::write_volatile;
use core::sync::atomic::{AtomicUsize, Ordering};

static UART_BASE: AtomicUsize = AtomicUsize::new(0x1000_0000);

pub fn putchar(c: u8) {
    unsafe {
        write_volatile(UART_BASE.load(Ordering::Relaxed) as *mut u8, c);
    }
    if c == b'\n' {
        putchar(b'\r');
    }
}

pub fn console_relocate(offset: usize) {
    UART_BASE.fetch_add(offset, Ordering::Relaxed);
}