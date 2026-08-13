# Decoder ring

Gitignored. For reading `src/main.rs` when the Rust is in the way.

Every entry is: **what you see** → **what it means in Python** → **why Rust
makes you say it**. If the third column doesn't matter to you yet, ignore it.
The middle column is the point.

---

## Punctuation you'll hit in the first ten lines

| Rust | Python | why |
|---|---|---|
| `let x = 5;` | `x = 5` | `let` introduces a name. Semicolons end statements. |
| `let mut x = 5;` | `x = 5` | Without `mut`, `x` can never be changed again. Python has no equivalent — everything is `mut`. |
| `fn f(a: usize) -> usize` | `def f(a):` | Types are written down. `usize` = an unsigned integer the width of the machine (64 bits here). |
| `//` and `///` | `#` | `///` is a doc comment: it belongs to the thing below it. |
| `x as u8` | `int(x) & 0xFF` | A cast. Rust never converts number types silently. |
| `0x1000` | `0x1000` | Same. Hex. |
| `1 << 9` | `1 << 9` | Same. Bit 9 set, everything else zero. |
| `x & !(a - 1)` | `x & ~(a - 1)` | `!` on a *number* is bitwise NOT. On a `bool` it's `not`. |

## Blocks that return values

Rust expressions evaluate to a value, including `if` and blocks. **A line with
no semicolon at the end of a block is the return value.**

```rust
let n = if hidden { 1 } else { 0 };     // n = 1 if hidden else 0
```

```rust
fn double(x: usize) -> usize {
    x * 2          // <- no semicolon: this is the return value
}
```

That missing semicolon is not a typo. It is the difference between returning
something and discarding it.

## `match` — the one you'll see constantly

```rust
match state {
    ThreadState::Runnable    => "runnable",
    ThreadState::Sleeping(c) => format!("sleeping on {}", c),
    _                        => "something else",
}
```

Python:

```python
if state == RUNNABLE:      "runnable"
elif state == SLEEPING:    f"sleeping on {c}"
else:                      "something else"
```

`_` means "anything else". The `(c)` pulls the value *out* of the case — the
channel was stored inside `Sleeping`, and this names it `c`.

## `Option<T>` — the thing instead of `None`

```rust
fn frame_alloc() -> Option<*mut u8>
```

Python: *"this returns a page, or `None`."*

The difference: Python lets you forget to check, and you find out with an
`AttributeError` at 3am. Rust makes `Option<Page>` a **different type** from
`Page`, so the compiler refuses to let you use it until you've said what
happens when it's `None`.

Three ways you'll see it unwrapped, all meaning "get the value out":

```rust
match frame_alloc() { Some(f) => ..., None => ... }   // handle both
frame_alloc().expect("no frames left")                // or panic
let f = frame_alloc()?;                               // or give up and return None myself
```

That last `?` is why `parse_create` is short: every `?` means *"if this failed,
stop and report failure to whoever called me."* Fifteen error checks, one
character each.

## `Result` — same idea, but the failure carries a reason

`Option` says "nothing". `Result` says "here's what went wrong". Same `?`.

## References and pointers

This is the one that actually matters, so here it is with a picture.

```
     a page of memory somewhere
            |
  0x87f00000 [ ... 4096 bytes ... ]
            ^
            |
        a "pointer" is just this number: 0x87f00000
```

| Rust | what it is |
|---|---|
| `&x` | *"where x lives"*, and I promise only to read it |
| `&mut x` | *"where x lives"*, and I may write it — **and nobody else has a reference at all** |
| `*mut u8` | a raw address. No promises. This is a plain number. |
| `*p` | go to that address and get what's there |

Python has references everywhere and hides them. When you pass a list to a
function, you pass its address — Python just never lets you see or type the
number.

Rust's rule: **either one writer, or any number of readers. Never both.** That
rule is what makes data races impossible... and it is also exactly why
`switch(old, new)` uses raw `*mut` pointers instead: it writes through one
while reading the other, and two `&mut` in flight would break the rule.

## `Box<T>` — "put this on the heap and don't move it"

```rust
Vec<Thread>          // the threads live INSIDE the vector's buffer
Vec<Box<Thread>>     // the vector holds ADDRESSES; threads live elsewhere
```

Why it's load-bearing here: a `Vec` that outgrows its buffer allocates a bigger
one and **moves everything**. With `Vec<Thread>`, spawning the fifth thread
moved every suspended thread's saved registers, and the scheduler was still
holding pointers to where they used to be. Boxing means the list of addresses
can move all it likes; the threads never do.

## `impl` — attaching functions to a type

```rust
impl Heap {
    fn alloc(&self, size: usize) -> *mut u8 { ... }
}
```

Python:

```python
class Heap:
    def alloc(self, size): ...
```

`&self` **is** `self`. Same thing, spelled with an ampersand because it's a
reference.

`impl SomeTrait for MyType` means "MyType now counts as a SomeTrait". So
`impl core::fmt::Write for Uart` means: the formatting machinery accepts a
`Uart` as somewhere to write, which is how `println!` reaches your `putchar`.

## `Drop` — a destructor that runs automatically

```rust
impl Drop for SpinGuard { fn drop(&mut self) { ...release the lock... } }
```

Python's `__del__`, except Rust runs it at an exactly known moment: when the
value goes out of scope, at the closing brace.

That precision is the whole trick. You cannot forget to unlock, you cannot
return early and skip the unlock, and you cannot panic past it. There is no
`unlock()` to call because there is no way to *not* call it.

## `unsafe` — a much smaller word than it sounds

It does **not** turn off type checking, borrow checking, or anything else.
It permits exactly five things, and the two that appear in this kernel are:

- dereference a raw pointer (`*p`)
- call assembly, or a function that isn't Rust

Everything else still applies inside an `unsafe` block. It means *"I have
checked something the compiler cannot see"* — which in a kernel is most
statements about hardware.

## `static` and `const`

| Rust | meaning |
|---|---|
| `const PAGE_SIZE: usize = 4096;` | a name for a number. Pasted in at compile time. |
| `static THREADS: ... = ...;` | one global variable, one fixed address, lives forever |

## `AtomicUsize` — a global integer that survives interrupts

```rust
TICKS.fetch_add(1, Ordering::Relaxed)
```

means `TICKS += 1`, but as **one indivisible step**.

Why that's needed: `TICKS += 1` is really *load, add one, store*. An interrupt
can land between any two of those. Two increments can interleave and one is
lost. `Relaxed` means "indivisible, but I don't care what order it appears in
relative to other memory" — enough for a counter, not enough for a lock.

## `-> !` — this function never returns

```rust
fn thread_exit(code: i32) -> !
```

Not "returns nothing" (that's no arrow at all). **Never comes back.** The
compiler then knows any code after a call to it is unreachable, and won't
demand a return value that will never arrive.

## `#[...]` attributes

Instructions to the compiler, not code.

| what | meaning |
|---|---|
| `#[repr(C)]` | lay this struct out in memory in declaration order, no reordering. Required for `TrapFrame`, because `entry.S` writes registers at hardcoded byte offsets and Rust would otherwise be free to rearrange fields. |
| `#[no_mangle]` | keep this function's name exactly as written, so assembly can call it |
| `#[allow(dead_code)]` | stop warning that nothing uses this yet |
| `#[global_allocator]` | this is what `Vec` and `String` call |

## Assembly, the six instructions that matter

| instruction | meaning |
|---|---|
| `la sp, __stack_top` | load address: put the address of that symbol into `sp` |
| `sd x, 8(sp)` | store doubleword: write the 8 bytes in `x` to memory at `sp + 8` |
| `ld x, 8(sp)` | the reverse: read 8 bytes from `sp + 8` into `x` |
| `csrr t0, sepc` | read a Control and Status Register into `t0` |
| `csrw sepc, t0` | write one |
| `ret` | jump to whatever address is in the `ra` register |

**What a CSR physically is:** a register inside the CPU that isn't one of the
32 general-purpose ones. It has no memory address, so no load or store can
reach it — the only way in or out is a `csr` instruction. `sepc` is a handful
of flip-flops wired directly into the trap logic. There is exactly one of them
on the whole chip, which is the entire reason for the biggest bug in this
project's history.

**Why `ret` is a context switch:** `ret` jumps to `ra`. `switch` loads `ra`
from the *new* thread's saved registers just before it. So `ret` jumps to where
the other thread stopped. One ordinary instruction, doing something enormous,
because of what was quietly put in the register beforehand.
