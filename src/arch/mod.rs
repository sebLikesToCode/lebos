//! Everything that is true of a CPU, and nothing that is true of LeBOS.
//!
//! Exactly one of these is compiled in, chosen by the target triple. The rest
//! of the kernel calls `arch::whatever()` and never learns which.
//!
//! NOTHING above this directory may name satp, stvec, scause, csrw, or any
//! other RISC-V noun. If one leaks upward the abstraction is already broken,
//! and the second port is where you find out.

#[cfg(target_arch = "riscv64")]
pub mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64::*;
