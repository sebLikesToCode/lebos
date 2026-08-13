# RUST.md — the fifteen things your kernel actually uses

Gitignored. Companion to `decoder.md`, which is a quick lookup table. This one
goes deeper on the cluster you asked about — pointers, `unsafe`, `volatile`,
`asm!` — because those four are the whole of "talking to hardware in Rust", and
everything else in `main.rs` is ordinary code.

Every example below is **copied out of your kernel**, not invented.

---

# 1. A pointer is a number

This is the idea Python spent years hiding from you.

```
        RAM
        ┌─────────────────────────────┐
 0x8000 │                             │
 0x8008 │  0x0000_0000_0000_002a  ←── the value 42 lives here
 0x8010 │                             │
        └─────────────────────────────┘
                     ↑
        a "pointer to it" is the number 0x8008.
        That's all. It is an integer that happens to be an address.
```

In Python you can never see `0x8008`. In Rust you can hold it, add to it, and
cast it to and from an integer — because a kernel has to.

Rust has three spellings, and they differ only in **what you promise**:

| you write | it is | what you promise |
|---|---|---|
| `&x` | a shared reference | I will only read. Others may read too. |
| `&mut x` | an exclusive reference | I may write. **Nobody else has a reference at all.** |
| `*mut u8` | a raw pointer | Nothing. It is a bare number. |

The compiler enforces the first two and will not let you break them. It
enforces *nothing* about the third — which is exactly why hardware code uses it.

**Casting between them:**

```rust
let addr: usize = 0x1000_0000;      // just an integer
let p = addr as *mut u8;            // now the compiler calls it a pointer
let back = p as usize;              // and back again. Free, both ways.
```

Neither cast generates a single instruction. It's the same 64 bits; you're only
telling the compiler how to think about it.

---

# 2. `unsafe` is a much smaller word than it looks

**It does not turn off type checking. It does not turn off the borrow checker.
It does not turn off anything you already rely on.**

It permits exactly five things. Your kernel uses two of them:

1. **Dereference a raw pointer** — `*p`, or `p.read()`, or `write_volatile(p, x)`
2. **Call a function that isn't checked Rust** — assembly, or `extern "C"`

*(The other three: read/write a `static mut`, implement an unsafe trait, access
a union field.)*

Everything else still applies inside the block. You cannot use a moved value,
you cannot mismatch types, you cannot alias a `&mut`.

**What it actually means:** *"I have checked something the compiler cannot
see."* In a kernel that's most statements about hardware. The compiler has no
way to know that `0x1000_0000` is a UART. You do.

```rust
fn putchar(c: u8) {
    let base = UART_BASE.load(Ordering::Relaxed) as *mut u8;
    unsafe {
        core::ptr::write_volatile(base, c);
    }
}
```

The `unsafe` there is carrying one claim: *that address is a real UART.* Nothing
more.

---

# 3. `volatile` — why a normal store isn't enough

Write this:

```rust
*base = c;          // WRONG for hardware
```

and the optimiser is allowed to **delete it.** It reasons: nothing in this
program ever reads `*base` back, so the store has no observable effect, so it
can go.

That reasoning is correct — for memory. It is wrong for a UART, because the
reader isn't your program. It's a chip.

```rust
core::ptr::write_volatile(base, c);   // RIGHT
```

`volatile` means: **this access is observable by something I cannot show you.
Do not delete it, do not reorder it with other volatile accesses, do not merge
two of them into one.**

The same in reverse for reads:

```rust
// Reading the same register twice may give two DIFFERENT answers -- the
// device changed it between reads. A normal read could be cached in a
// register and reused.
if core::ptr::read_volatile(base.add(UART_LSR)) & UART_LSR_RX_READY == 0 {
    return None;
}
Some(core::ptr::read_volatile(base))
```

Without `volatile`, the optimiser could read the status register **once**, keep
it in a CPU register, and loop forever on a stale value.

**Rule of thumb:** if the thing at that address can change without your code
changing it, or notices being written, it's `volatile`.

`.add(n)` on a pointer moves it `n` **elements** forward — for `*mut u8`, `n`
bytes. For `*mut u32`, `4n` bytes. That's why `plic_reg` casts to `*mut u32`
first and then indexes.

---

# 4. `core::arch::asm!` — writing instructions directly

Some things have **no** Rust spelling. There is no `read_the_sstatus_register()`
because CSRs aren't memory — no address, so no pointer can reach them. The only
way in is a `csr` instruction.

Your simplest one:

```rust
fn intr_on() {
    unsafe { core::arch::asm!("csrsi sstatus, 2") };
}
```

That's it. One instruction, no inputs, no outputs. `csrsi sstatus, 2` sets bit 1
of `sstatus`, which is SIE — interrupts enabled.

## Getting values in and out

```rust
fn intr_off() -> bool {
    let old: usize;
    unsafe { core::arch::asm!("csrrci {}, sstatus, 2", out(reg) old) };
    old & 2 != 0
}
```

Read it piece by piece:

| part | meaning |
|---|---|
| `"csrrci {}, sstatus, 2"` | the instruction. `{}` is a hole. |
| `out(reg) old` | "pick any register, and after the instruction, copy it into `old`" |
| the compiler | chooses e.g. `t0`, substitutes it into `{}`, emits `csrrci t0, sstatus, 2`, then `mv` into wherever `old` lives |

You don't name the register. You describe what you need and the compiler picks
one that isn't busy.

**The four directions:**

| constraint | meaning |
|---|---|
| `in(reg) x` | put `x` in a register before the instruction |
| `out(reg) y` | after the instruction, copy that register into `y` |
| `inout(reg) x` | goes in holding `x`, comes out overwriting `x` |
| `inout(reg) x => y` | goes in as `x`, comes out into `y` |
| `out(reg) _` | I don't want the value, but **this register gets clobbered** |

That last one matters more than it looks. See §5.

## Naming a specific register

Sometimes you have no choice, because a calling convention says so:

```rust
core::arch::asm!(
    "ecall",
    in("a7") 0x5449_4D45_usize,   // SBI extension id: "TIME"
    in("a6") 0_usize,             // function id
    inout("a0") when => _,
    out("a1") _,
);
```

`in("a7")` means *that exact register*, because the SBI spec says the extension
id goes in `a7`. Not negotiable, so not the compiler's choice.

## `options(noreturn)`

```rust
core::arch::asm!(
    "csrw sepc, {entry}",
    "mv sp, {usp}",
    "sret",
    entry = in(reg) entry,
    usp = in(reg) user_sp,
    in("a0") arg,
    options(noreturn),
);
```

`sret` jumps away and never comes back. `options(noreturn)` tells the compiler
so — otherwise it insists the block falls through and demands a return value
that will never arrive. Note the **named** holes here (`{entry}`, `{usp}`)
instead of positional `{}`; clearer once there's more than one.

## `global_asm!` vs `asm!`

| | |
|---|---|
| `asm!` | instructions **inside** a function |
| `global_asm!` | a whole file of assembly at module level — your `entry.S` |

`entry.S` is `global_asm!`'d in because `_start`, `switch` and `trap_entry`
cannot be Rust functions: they have no valid stack yet, or they must be aligned,
or they return into a different function than the one that called them.

---

# 5. The `asm!` bug you already survived

This one is worth studying, because it's the failure mode of getting a
constraint wrong.

The original was:

```rust
in("a0") when,       // WRONG
```

meaning *"put `when` in `a0`."* Which is true, but incomplete: it also promises
the compiler that **`a0` still holds `when` afterwards**. It doesn't — SBI
returns `sbiret { error, value }` in `a0` and `a1`, so OpenSBI overwrites both.

The compiler, believing its own register was preserved, kept a live value there
across the call and then used OpenSBI's error code as a pointer.

The fix says what actually happens:

```rust
inout("a0") when => _,   // goes in as `when`, comes out as garbage I discard
out("a1") _,             // and this one is clobbered too
```

**The lesson:** an `asm!` constraint is a *contract*. Everything you don't
declare, the compiler assumes is untouched — and it will act on that assumption.

---

# 6. `extern "C"` and `#[no_mangle]`

```rust
#[no_mangle]
extern "C" fn trap_handler(frame: &mut TrapFrame) { ... }
```

| | |
|---|---|
| `extern "C"` | use the C calling convention: args in `a0`, `a1`, … Rust's own convention is unspecified and may change. |
| `#[no_mangle]` | keep the name exactly `trap_handler`. Rust normally decorates names with hashes. |

Both exist so `entry.S` can write `call trap_handler` and have it work.

Declaring the traffic in the other direction:

```rust
extern "C" {
    fn switch(old: *mut Context, new: *const Context);
    fn trap_entry();
}
```

*"These exist somewhere else, here are their shapes, trust me."* Calling one is
`unsafe`, because the compiler cannot check the shape is right.

---

# 7. `static`, `static mut`, and why `UnsafeCell` exists

| | |
|---|---|
| `const X: usize = 4096;` | a name for a value. Pasted in wherever used. No address. |
| `static X: usize = 4096;` | one variable, one fixed address, lives forever, **immutable** |
| `static mut X: usize` | mutable global. Touching it is `unsafe` — nothing stops two threads racing. |

Your kernel mostly avoids `static mut`, and the ways it avoids it are the
interesting part:

```rust
static TICKS: AtomicU64 = AtomicU64::new(0);         // atomic: indivisible +=
static THREADS: SpinLock<Vec<Box<Thread>>> = ...;    // a lock that HOLDS the data
```

`UnsafeCell<T>` is the escape hatch underneath both. It means *"I may hand out a
`&mut` to the inside even through a shared `&`."* It's the only legal way to
build a lock or an allocator, and it's what your bump allocator uses:

```rust
pub struct Bump {
    next: UnsafeCell<usize>,
    limit: UnsafeCell<usize>,
}
unsafe impl Sync for Bump {}
```

That `unsafe impl Sync` is you promising *"this is safe to share between
threads."* For your allocator that's true only because a LeBOS process is
single-threaded — which is why there's a comment saying exactly that. Delete the
assumption, the promise becomes a lie.

---

# 8. Slices vs pointers

```rust
static USER_PROG: &[u8] = include_bytes!("../user/hello.elf");
```

A slice `&[u8]` is **two** numbers: a pointer, and a length. That's the whole
difference from `*const u8`, and it's why slices can bounds-check.

Going between them:

```rust
let p: *const u8 = USER_PROG.as_ptr();     // slice -> pointer, drops the length
let n = USER_PROG.len();

// pointer -> slice: you are ASSERTING the length. Get it wrong and you have
// invented memory.
let s = unsafe { core::slice::from_raw_parts(p, n) };
```

Your `Reader` / `Rd` types exist so that parsing untrusted buffers uses the
checked form. `r.take(n)` returns `Option` — `None` if it would run off the end —
so a malformed request from a user program becomes a refusal rather than a read
past the buffer.

---

# 9. `Option<T>` and `?`, in kernel code

```rust
fn frame_alloc() -> Option<*mut u8>
```

*"A page, or nothing."* The type makes forgetting to check impossible.

`?` means **"if this is `None`, stop and return `None` from my function."**
That's why `load_elf` is readable despite ~15 failure paths:

```rust
let entry = rd_u64(elf, 0x18)? as usize;
let phoff = rd_u64(elf, 0x20)? as usize;
```

Each `?` is a full error check, one character long. In C those are fifteen
`if (x == NULL) return -1;` blocks, and the bug is always the one that's missing.

---

# 10. The `!` type

```rust
fn thread_exit(code: i32) -> !
```

Not "returns nothing" — that's no arrow at all. **Never returns.** The compiler
then knows code after the call is unreachable and won't demand a value.

Used on: `thread_exit`, `enter_user`, `panic`, every thread entry function.

---

# 11. `impl Trait for Type`

```rust
impl core::fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result { puts(s); Ok(()) }
}
```

*"`Uart` now counts as a thing you can write formatted text to."* That single
impl is the entire bridge from `println!("{}", x)` down to your `putchar`.

```rust
impl Drop for SpinGuard<'_, T> {
    fn drop(&mut self) { ...release the lock... }
}
```

`Drop` runs **automatically**, at a precisely known moment: the closing brace.
That precision is the whole trick — you cannot forget to unlock, cannot return
early past it, cannot panic past it. There is no `unlock()` because there is no
way to *not* call it.

---

# 12. Numeric care in kernel code

| you write | when |
|---|---|
| `a + b` | panics on overflow in debug, wraps in release. **Different behaviour per profile** |
| `a.wrapping_add(b)` | I want wrapping. Hashes, counters. |
| `a.checked_add(b)?` | I want `None` on overflow. **Untrusted input.** |
| `a.saturating_sub(b)` | clamp instead of wrapping |

`load_elf` uses `checked_add` throughout, deliberately: a hostile ELF header
that makes `vaddr + memsz` wrap around must not be able to look like a small,
in-range number.

---

# 13. Where `unsafe` actually appears in your kernel

84 blocks, and they're all one of five shapes:

| shape | example |
|---|---|
| touching a device register | `write_volatile(base, c)` |
| walking a page table | `*root.add(i)` — the table is a raw physical address |
| copying to/from user memory | `copy_from_user`, `copy_to_user` |
| calling assembly | `switch(old, new)`, every `asm!` |
| building a struct at a fixed address | the virtio rings |

If you find yourself writing `unsafe` for a *sixth* reason, that's worth
stopping over.

---

# 14. The two patterns you'll write most

**Reading a device register:**

```rust
fn vio_read(base: usize, off: usize) -> u32 {
    unsafe { core::ptr::read_volatile((base + off) as *const u32) }
}
```

**Guarding a critical section:**

```rust
let was_on = intr_off();
// ... something that must not be interrupted ...
if was_on {
    intr_on();
}
```

Note it **restores** rather than enabling — the caller may already have had
interrupts off, and blindly turning them on would break their assumption.

---

# 15. What to write for milestone 21

You said you want to write the Rust. Milestone 21 is the test harness, which is
Python — so here's the Rust-shaped piece of it instead, and it's a real one.

**The task:** the kernel has no way to report internal state to a test. Add a
syscall that does.

```
11  stat(which) -> a number
```

Where `which` selects: object count, blob count, claim count, free heap bytes,
free heap blocks, tick count, console interrupt count.

**What you'd write:** one `match` arm in `trap_handler`, next to the others.
Maybe fifteen lines. Everything you need is already in this file — a `match` on
a user-supplied number, `usize::MAX` for refusal, and the counters already
exist (`STORE.lock().len()`, `heap_stats()`, `TICKS.load(...)`).

**One trap in it, and it's the one from milestone 18:** `which` comes from an
untrusted program. Your `_` arm must **refuse**, not panic and not assume.

Write it however it comes out and I'll review it rather than rewrite it — the
point is you writing Rust, not me producing tidy Rust.
