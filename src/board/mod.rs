//! Everything around the CPU: where the UART is, how fast the timer ticks,
//! where RAM starts and stops.
//!
//! A different axis from `arch`. QEMU's virt board and a Raspberry Pi are the
//! same architecture and share almost nothing else; the Milk-V Mars and QEMU
//! virt are both riscv64 and put the UART in different places.
//!
//! The interface here is VERBS, not addresses -- `putchar(byte)`, never
//! `UART_BASE`. x86 reaches its serial port with `out` instructions, which are
//! not memory at all, so an address-shaped interface has no x86 implementation.

pub mod qemu_virt;
pub use qemu_virt::*;
