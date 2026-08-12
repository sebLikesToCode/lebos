# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

**LeBOS** — a from-scratch operating system kernel for 64-bit RISC-V, written
in Rust, run under QEMU. It is a learning project with a real design thesis
(see *Design thesis* below) — not a Linux clone and not a tutorial
follow-along. Named for its author, Sebastian LeBlanc; `LeBOS` in prose,
`lebos` as the crate and binary name.

Status: **milestone 6 complete.** Boots under OpenSBI; formatted serial output
(`putchar` -> `puts` -> `impl core::fmt::Write for Uart` -> `println!`); traps
via a full 32-register frame saved in `entry.S`, with unhandled exceptions
fatal and decoded to English; timer interrupts at 100 Hz through an SBI
`ecall`; a physical frame allocator over a range read from the **device tree**;
and Sv39 paging with the kernel **executing in the higher half**, W^X enforced,
and the low half unmapped and reserved for user programs.

Boot sequence, which is subtle and worth not rediscovering:

1. `entry.S` sets `sp`, zeroes `.bss`, calls `kmain` -- all at PHYSICAL
   addresses, paging off. `la` is PC-relative so symbols resolve physically
   for free despite being linked high.
2. `kmain` calls `boot_paging()` **first**, before any print. The kernel is
   linked with a high VMA, so ~435 absolute addresses in `.rodata` (vtables)
   are high, and `println!` dispatches through one -- printing is impossible
   until the high half exists. `boot_paging` uses a static `BOOT_PT` in `.bss`
   and four 1 GiB leaves, and must never print, panic, or dynamically
   dispatch.
3. Normal boot: banner, device tree, frame allocator, then `paging_init`
   builds the real 4 KiB-granular table (identity + higher-half direct map).
4. The kernel adds `HIGH_BASE` to `sp` and jumps to the high alias of its own
   code, moves `stvec` and `UART_BASE` up, then clears root slots 0 and 2.

`va()` / `pa()` convert between the two. Note the asymmetry: `frame_alloc`
returns PHYSICAL addresses and needs `va()` before use, while `&some_symbol`
is PC-relative and already yields whichever alias is executing -- never adjust
those.

`probe()` / `explain()` walk the table by hand and report a translation with
its permissions and RSW tag. Reach for them first when an address faults.

A 1 MiB heap is carved off the top of RAM before `frame_init` claims anything,
and `heap_init` is given its **virtual** address so it outlives the identity
map. `Heap` implements `GlobalAlloc` over an address-sorted free list: first fit,
split when the leftover can hold its own header, and coalesce in **both**
directions on free. Allocated blocks carry no header at all -- Rust returns
the size in `Layout`, so only gaps need signs. Every free block header holds
`BLOCK_MAGIC` (`0x5EBB1E`), checked on every walk, which catches a write past
the end of an allocation at the next allocation rather than a thousand later.

Verified: a freed block is handed straight back to the next request, and after
everything drops the heap returns to exactly one free block of 1048576 bytes.

`switch(old, new)` in entry.S saves 14 callee-saved registers and loads
another set -- only 14, not 32, because a context switch is a function call and
the ABI already spilled the caller-saved ones. Its final `ret` jumps to the new
thread's `ra`, which IS the switch. Threads are `Vec<Thread>` on the heap, each
owning a 16 KiB stack; `thread_spawn` forges a context whose `ra` points at the
entry function, so the first switch "returns" into a function never called.
Cooperative -- `yield_now()` round-robins.

Next: milestone 9, preemption.

Hard-won details that will bite again if forgotten:

- `stvec`'s low two bits are a MODE field, so the trap vector must be at
  least 4-byte aligned. `riscv64gc` functions are only 2-byte aligned
  (compressed instructions) and Rust cannot align a function, which is why
  `trap_entry` lives in assembly.
- `sepc` points **at** the faulting instruction, not past it. The handler
  must advance it or the CPU re-executes the fault forever. Instruction
  length is 4 bytes if the low two bits are `0b11`, else 2.
- Interrupts get the opposite treatment: **never** advance `sepc` for one.
  The instruction there has not run yet.
- The timer interrupt is **level-triggered** on `time >= timecmp`. Rearming
  it is how you acknowledge it, not merely how you schedule the next one —
  skip the rearm and it re-fires at every instruction boundary forever
  (measured: 494k ticks in 3 seconds).

## Returning after a break

Work happens in bursts — roughly two weeks on, two weeks off. Assume the
author has forgotten the details and re-orient before writing any code:

1. `make run` — watch it boot. Seeing it work rebuilds more context in 30
   seconds than reading does in an hour.
2. Read the **Status** line above, then the last 5 commit messages
   (`git log --oneline -5`); they are written to explain *why*, not just what.
3. Check `git status` — the tree should be clean and booting. If it is not,
   that is the first thing to fix, before anything else.
4. Only then pick up the current milestone.

**Never let a burst end on a non-booting commit.** A two-week gap plus a
broken tree is the most likely way this project dies, and it is entirely
avoidable — commit a working partial state instead, and note in the message
what was mid-flight.

## How to work in this repo

The owner is learning systems programming and Rust by building this. That
changes the job:

- **Explain, then write it yourself.** Dictating Rust for him to retype teaches
  nothing — the algorithm is the lesson, not the syntax. He sketches logic in
  Python/JS when he wants to; translate it and say what changed.
- **Do NOT hand him blank `???` scaffolds to fill in.** Tried and failed:
  recognition and generation are different skills, and stalling on an empty
  function body reads to him as "I can't grasp this" when he has already
  reasoned correctly about the same thing minutes earlier. Don't quiz what he
  has demonstrably understood.
- **Always give an explicit picture↔code mapping table** when using an analogy.
  State outright that `free_list` IS the door note and `mem[X]` IS the note
  inside box X. He stalled on the allocator purely because that row was
  missing. When he says he's lost, ask which *symbol* failed to map before
  re-explaining the concept.
- **Write the mechanical parts outright**: assembly, register save/restore,
  linker scripts, build tooling, long boilerplate.
- **Pitch explanations low.** He knows NAND gates, registers, and clocks
  solidly, plus high-level code — but not the layer between. Concrete diagrams
  with real addresses land; abstraction does not. Explain what a CSR *is*
  physically before using one.
- Prefer explaining *why* the hardware demands something over prescribing what
  to type. "The optimiser deletes stores nothing reads back" beats "use
  `write_volatile`".
- **Break-it experiments beat quizzes.** Change something in `make play`,
  predict, then run. This is how the level-triggered timer flood and the
  double-free self-loop got discovered.
- **Plant bugs for him to diagnose (his idea, and a good one).** Introduce a
  deliberate defect in `src/main2.rs`, show him the output, and let him work
  out the cause from the symptom WITHOUT reading the source. Reveal the source
  only once he has committed to a theory. Pick bugs with legible but
  non-obvious symptoms -- e.g. removing backward coalescing from the heap
  leaves the free byte count identical while the block count climbs. This
  trains the exact skill the project exists to build.
- xv6-riscv is the canonical reference implementation to point at. It solves
  nearly every problem this project will hit, in ~9k lines of readable C.

## Easter eggs and tributes

The author wants this codebase full of them. **Flag the opportunity whenever
writing code that has a natural slot** — don't add one silently, and don't add
one where it would cost clarity or correctness.

Legitimate slots are places where an arbitrary constant or string is needed
anyway, so the joke is free:

- magic numbers (format signatures, struct validity markers)
- allocator canaries and stack guard values
- panic and error messages
- the boot banner
- RSW bits — RISC-V reserves PTE bits 8–9 for software use
- comments, in the `/* You are not expected to understand this */` tradition

Already present: `PANIC! AT THE KERNEL` (Panic! At The Disco), and inherited
from the FDT spec, `0xd00dfeed` ("dude feed").

**Reserved:** `0x1EB05` spells LEBOS in hex (1=L, 0=O, 5=S). Earmarked for the
object store's on-disk magic number at milestone 12 — the format signature is
permanent and the most visible constant the project will ever have. Do not
spend it on something smaller.

## Setup

```
rustup target add riscv64gc-unknown-none-elf   # plus rust-src, llvm-tools
cargo install cargo-binutils
sudo apt install qemu-system-riscv gdb-multiarch device-tree-compiler
```

Note for Ubuntu 26.04+: RISC-V lives in `qemu-system-riscv`, **not**
`qemu-system-misc` as on older releases.

## Commands

```
make build      # cargo build (target is pinned in .cargo/config.toml)
make run        # boot in QEMU. QUIT WITH: Ctrl-A then X
make debug      # boot frozen, GDB stub on :1234  (terminal 1)
make gdb        # attach gdb-multiarch to it       (terminal 2)
make trace      # boot logging interrupts+MMU to qemu.log
make nm         # symbols by address — confirm _start is at 0x80200000
make size       # section sizes
make objdump    # disassembly
make dumpdtb    # dump QEMU's device tree → virt.dts (authoritative hw map)
make check      # clippy
```

There are no tests yet. Unit-testing a `no_std` kernel needs a custom test
harness that boots QEMU and inspects serial output; that is worth building
around milestone 8, not before.

`cargo run` also works — the QEMU invocation is duplicated as a `runner` in
`.cargo/config.toml`. If you change QEMU flags, change both.

## Target and boot path

QEMU `virt` board, rv64, `-bios default` (OpenSBI firmware), single hart.

1. OpenSBI initialises RAM, stays resident in the low 2 MiB of RAM, drops the
   CPU to **supervisor mode**, and jumps to `0x8020_0000` with `a0` = hartid
   and `a1` = device tree pointer.
2. `src/entry.S` (`.text.entry`, forced first by the linker script so it lands
   exactly at that address) sets `sp`, zeroes `.bss`, and `call kmain`.
3. `kmain` in `src/main.rs` never returns.

Consequences worth remembering: the kernel starts in S-mode, not M-mode, so
privileged setup that tutorials do in M-mode is either already done or must go
through SBI calls (`ecall`) to OpenSBI. `a1`'s device tree is the only source
of truth for how much RAM exists — the physical allocator will need it.

Fixed addresses on this board: UART0 `0x1000_0000`, CLINT `0x0200_0000`,
PLIC `0x0c00_0000`, RAM `0x8000_0000`, kernel `0x8020_0000`.

## Layout

```
linker.ld          physical memory layout; exports __bss_start/__bss_end,
                   __stack_top/__stack_bottom, __kernel_end
src/entry.S        boot assembly, the only code that runs before Rust
src/main.rs        kmain + panic handler
.cargo/config.toml target, linker flags, `cargo run` → QEMU
rust-toolchain.toml  pinned to stable — nightly is not needed for rv64
```

`panic = "abort"` in both profiles: there is no unwinder. `opt-level = 1` in
dev is deliberate — `opt-level = 0` produces stack frames large enough to
overflow the 64 KiB boot stack.

## Design thesis — the part that makes this not-Linux

**There is no filesystem, no paths, and no directories.** Storage is a
content-store of immutable, stamped objects:

```
id          identity (hash or monotonic)
created_at  timestamp
type        a type tag — semantic, not a file extension
origin      which process/capability produced it   ("ctx-stamp")
attrs       typed key/value pairs
links       typed relations to other object ids
```

Objects are reachable exactly two ways: **by id**, or **by query** over
indexes (by time, by type, by attribute). Mutation is append-only — an edit
writes a new version linked to its predecessor, which yields versioning and
history for free and makes the on-disk format a log (much easier to get
crash-correct than in-place update; see log-structured storage / LSM trees).

The syscall surface for storage is roughly `create` / `get` / `query` / `link`.

## Known improvement, deliberately deferred

`frame_alloc` returns a raw `*mut u8`, which opts out of every guarantee Rust
offers — a double free silently makes a page point at itself, so every
subsequent allocation returns that same page while the rest of the free list
becomes unreachable (demonstrated and measured; three allocations all returned
`0x87fff000`).

The fix is an owning `struct Frame(*mut u8)` with a `Drop` impl that returns
the page, making double-free a compile error. Deferred until after milestone 6,
so page tables and frames can be wrapped together once the shape is known.

**The compatibility story (his idea, 2026-08-11):** ship a familiar-looking
files app so users get what they expect while being eased into the store. The
reconciliation is that **a "folder" is a saved query** — opening one runs it,
dragging something in adds the attribute that makes it match, removing it drops
that attribute, and one object can appear in many folders with no copies and no
canonical location. Gmail is the precedent: labels, not folders, and hundreds
of millions of users migrated without noticing.

This lives in **userspace, and the dependency arrow points one way only**: the
view may render the store; nothing in the store may know the view exists. The
moment a program depends on path semantics, files have been reinvented.

**Open question, decide by milestone 12: what does "delete" mean?** In a
filesystem an object is garbage when nothing links to it from the root. With no
root and no hierarchy, reachability is undefined. The answer shapes the whole
storage layer.

## Design thesis invariants to defend:

- *No hierarchy, ever.* The pressure to add "just a little" path-like nesting
  is exactly what killed WinFS. Push back on it.
- *An object id is a capability.* You can reach an object iff you hold its id.
  This gives capability-style security as a consequence of the storage design
  rather than as a separate subsystem.
- *Mechanism in the kernel, policy in userspace.* The kernel exposes cores,
  memory, and usage data; a userspace daemon decides allocation. This keeps
  the door open for learned/adaptive resource policy later without putting any
  of it in supervisor mode.

Prior art to consult rather than reinvent: Newton OS "soups", Perkeep
(closest living relative), BeOS BFS indexed attributes, WinFS (instructive
failure), KeyKOS/EROS for capabilities.

**Long-term, explicitly deferred:** natural-language search over the store.
Nothing here needs building for a long time; the design above simply must not
foreclose it. Intended shape:

- A query like *"last tuesday's python file about brick breaker"* decomposes
  into structured predicates (`created_at` range, `type` tag — exact index
  lookups, no model involved) plus one residual semantic term. The stamps do
  the filtering; the model only ranks what survives. This is why no vector
  database or ANN index is needed — brute-force cosine over tens of
  prefiltered candidates is microseconds.
- Two small models, both userspace, neither an LLM in the usual sense: a query
  parser (start as a grammar over dates and type keywords; upgrade to a ~0.5B
  model only when the grammar annoys you) and a sentence embedder (see
  model2vec / potion — static distilled embeddings, single-digit MB, no
  transformer forward pass).
- **Embed at write time, not query time.** The indexer embeds each object on
  creation and stores the vector as another index.
- Resource-allocation policy is a *separate* problem wanting a genuinely tiny
  learned model — an MLP with hundreds of parameters, a decision tree, an RL
  policy — running in nanoseconds on integer math. Prior art: learned page
  replacement (LRB), learned prefetchers, learned index structures. Unlike the
  search models, this class *could* live in-kernel, which is what the
  mechanism/policy split above is protecting.

**Unresolved tension, decide by milestone 10:** a global semantic index must
read everything, which is a global authority and contradicts "an object id is
a capability." Either the indexer is a trusted system service with a
deliberate hole in the capability model, or each security domain keeps its own
private index. This affects syscall design.

**The `origin`/ctx stamp is the highest-value field and the easiest to
under-build.** Content search is table stakes (Spotlight approximates it
today); querying the causal graph — *"what was I looking at while working on
X"* — is not possible on any shipping OS and falls out of origin stamps for
free. Metadata cannot be backfilled: stamp generously at creation. This is the
only genuinely irreversible decision in the design.

## Milestone ladder

Phases I–III are deliberately conventional — copy xv6's structure; the
innovation budget is spent at Phase V.

1. ✅ build + boot harness
2. ✅ UART serial output, `println!` via `core::fmt::Write`
3. ✅ trap/exception handler printing the trap frame, resuming via sret
4. ✅ timer interrupts at 100 Hz via SBI, uptime counter
5. ✅ physical frame allocator + device tree memory map
6. ✅ 6a MMU on via Sv39 identity map; ✅ 6b rebuilt at 4 KiB granularity with
   W^X enforced; unhandled exceptions are now fatal
   ✅ 6c-i higher-half direct map alongside the identity map, aliasing proven
   ✅ 6c the kernel executes in the higher half and the identity map is gone
7. ✅ kernel heap: free list, first fit, splitting, two-way coalescing
8. ✅ kernel threads + cooperative context switch
9. ⬅ **current** — preemptive scheduler, spinlocks, wait queues
8. kernel threads + context switch
10. user mode + syscalls
11. process creation
12. **the object store**, in RAM first: indexes, query, versioning
13. virtio-blk + persist the store as an append-only log
14. userspace shell whose commands are queries, not paths
