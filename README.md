# LeBOS

An operating system with **no files, no paths, and no directories.**

Storage is a queryable store of content-addressed, stamped objects. You don't
say *where* something is, because nothing is anywhere. You describe it.

```
> find name~brick created_at>100
   0  brick breaker      python    t=101       23b  #8b1c92a5c7bb
> hide 0
> cd /home
  `cd` needs somewhere to put things. there is nowhere.
```

64-bit RISC-V, written in Rust, runs under QEMU. By Sebastian LeBlanc.

---

## Layout

```
src/             the kernel.  main.rs, entry.S        -> make run
kernel.ld        where it goes in memory
Cargo.toml

reference/       previous implementation, self-contained -> make ref
  src/             its kernel
  user/            its user programs (the shell, a demo)
  assets/ tools/   its logo and banner renderer
  linker.ld        its linker script (links high)
  Cargo.toml       its own crate

DESIGN.md        the design record: every decision and why
README.md        this
```

`reference/` is the previous implementation. It stays in the tree because it
still boots and still covers ground the current kernel hasn't reached yet, which
makes it the fastest way to check intended behaviour against working code.
`make run` boots the current kernel; `make ref` boots the old one.

### The three top-level crates

| crate | what | linker script |
|---|---|---|
| root | the current kernel, links flat at `0x8020_0000` | `kernel.ld` |
| `reference/` | the previous kernel, links into the higher half | `reference/linker.ld` |
| `reference/user/` | user programs, link at `0x1000` | `reference/user/user.ld` |

Each is separate because **cargo merges `.cargo/config.toml` up the directory
tree**, so one crate's `-Tlinker.ld` leaks into any nested crate. `reference/` and
`reference/user/` therefore keep only the *target* in their config, and the
Makefile sets `RUSTFLAGS` in the environment — which **replaces** config
rustflags instead of merging with them.

---

## Commands

### The kernel

```
make run       build and boot it              (quit: Ctrl-A then X)
make build     build only
make nm        symbols by address -- confirm _start is at 0x80200000
make objdump   disassembly
make debug     boot frozen with a GDB stub on :1234
make check     clippy
make help      list every target
```

### The reference

```
make ref       boot it
make user      build its user programs
make logo      print its colour boot banner without booting
make play      run its breakable scratch copy
make resync    reset that scratch copy
make playdiff  show what changed in it
```

### Shared

```
make disk      create lebos.img
make dumpdtb   dump QEMU's device tree -> virt.dts (the real hardware map)
make gdb       attach to a waiting `make debug`  (second terminal)
make trace     boot logging exceptions and MMU translations to qemu.log
make clean
```

---

## Setup

```
rustup target add riscv64gc-unknown-none-elf
rustup component add rust-src llvm-tools
cargo install cargo-binutils
sudo apt install qemu-system-riscv gdb-multiarch device-tree-compiler
```

On Ubuntu 26.04+, RISC-V lives in `qemu-system-riscv`, **not**
`qemu-system-misc` as on older releases.

---

## The idea

An object is:

```
id          a content hash -- identical bytes get an identical id, forever
created_at  a timestamp
type        a semantic tag, not a file extension
origin      which process produced it
attrs       typed key/value pairs
```

Reachable exactly two ways: **by id**, or **by query**. Nothing is ever
modified — an edit appends a new version, so history and versioning are free and
the on-disk format is a log, which is much easier to make crash-safe than
in-place update.

**"Delete" is three unrelated problems wearing one word:**

| problem | verb | what happens |
|---|---|---|
| clutter | `hide` | an attribute. Reversible, nothing lost. Most deletion is this. |
| space | `evict` | the bytes go, **the record stays** |
| privacy | `forget` | both go |

Eviction is the one no filesystem can do: the record survives as a valid,
globally meaningful coordinate with nothing behind it. So *"the file I was
working on while that video was open"* still answers after the video is gone.

**A folder is a saved query.** Opening one runs it; dragging something in adds
the attribute that makes it match. One object can appear in many folders, with
no copies and no canonical location. Gmail is the precedent — labels, not
folders, and hundreds of millions of people migrated without noticing.

Full reasoning, including what was rejected and why, is in `DESIGN.md`.
