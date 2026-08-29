# LeBOS

An operating system with **no files, no paths, and no directories.**

Storage is a queryable store of content-addressed, stamped objects. You do not
say *where* something is, because nothing is anywhere. You describe it.

64-bit RISC-V, written in Rust, run under QEMU. By Sebastian LeBlanc.

---

## The whole project

```
src/main.rs      the kernel -- not one CPU register, not one device address
src/hw/mod.rs    the RISC-V driver: CSRs, page tables, the UART
src/hw/entry.S   the twelve instructions before Rust can run
src/banner.txt   the boot logo, generated from the artwork
kernel.ld        where everything goes in memory
Cargo.toml       build settings
DESIGN.md        every decision and why
```

That is all of it. `make run` builds exactly those.

## Commands

```
make run       build and boot it        (quit: Ctrl-A then X)
make build     build only
make nm        symbols by address -- confirm _start is at 0x80200000
make objdump   disassembly
make debug     boot frozen with a GDB stub on :1234
make gdb       attach to a waiting `make debug`  (second terminal)
make trace     log exceptions and MMU translations to qemu.log
make check     clippy
make dumpdtb   dump QEMU's device tree -- the real hardware description
make help      list every target
```

`make trace` is the one that matters when nothing prints. It is the only
instrument that works once the console is broken.

## Where it is

```
[███████▌░░░░░░░░░░░░]  7.5 / 20 milestones  --  37.5%
```

| milestone | |
|---|---|
| 1 | build and boot harness |
| 2 | serial output, `println!` built from nothing |
| 3 | traps -- decode, save registers, resume |
| 4 | timer interrupts via SBI |
| 5 | physical frame allocator |
| 6 | paging: identity map, W^X, higher half |
| 7 | the heap. `Vec`, `String` and `Box` work |
| 7.5 | the arch split -- every machine-specific line behind a verb in `src/hw/` |
| 8 | **current** -- threads |
| 9-20 | user mode, processes, the store, disk, shell |

Full ladder and reasoning in `DESIGN.md`.

## Setup

```
rustup target add riscv64gc-unknown-none-elf
rustup component add rust-src llvm-tools
cargo install cargo-binutils
sudo apt install qemu-system-riscv gdb-multiarch device-tree-compiler
```

On Ubuntu 26.04+, RISC-V is in `qemu-system-riscv`, not `qemu-system-misc`.

## The previous implementation

An earlier, more complete kernel (through milestone 20 -- store, disk, shell,
user programs) lives at `~/code/lebos-reference`. It is not part of this build.
It is kept because it still boots and covers ground this kernel has not reached
yet, which makes it a fast way to check intended behaviour against working code.

It is also in this repository's git history, before the commit that removed it.
