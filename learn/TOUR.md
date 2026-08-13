# TOUR.md — a walk through LeBOS, one milestone at a time

Gitignored. `CLAUDE.md` is the design record: *what was decided and why.* This
is the map: *what runs, in what order, and why each thing has to exist before
the next one can.*

Line numbers are live as of milestone 20. They drift — `grep` for the function
name if one is off by a few.

**How to use this.** Read a section, then open that part of `main.rs`. Every
section has the same four parts: **the problem**, **the picture**, **the code**,
**the gotcha**. The gotcha is usually the part that cost hours.

---

# Phase I — a machine that can talk

## 1. Build and boot

**The problem.** A RISC-V CPU powers on. There is no operating system, no
loader, no C library, no `main`. There is RAM, a CPU, and some devices at fixed
addresses. Something has to be the first instruction.

**The picture.** OpenSBI is the building superintendent. It gets there before
you, turns on the power and water (initialises RAM), unlocks the door, and
hands you keys to *your floor only* — not the whole building. Then it leaves.

| the picture | the code |
|---|---|
| the superintendent | OpenSBI, the firmware QEMU ships |
| your floor, not the building | supervisor mode, not machine mode |
| the keys it hands over | `a0` = hart id, `a1` = device tree pointer |
| the door it points you at | `0x8020_0000` — where `_start` must be |
| a note listing what's in the building | the device tree blob in `a1` |

**The code.** `linker.ld` forces `.text.entry` first so `_start` lands exactly
at `0x8020_0000`. `src/entry.S` does three things and leaves:

1. `la sp, __stack_top` — give ourselves a stack. Until this instruction, any
   function call anywhere writes through a garbage `sp` and corrupts RAM.
2. Zero `.bss`. The ELF doesn't store those bytes — it only records *how many*
   — so on real hardware they hold whatever the last power cycle left.
3. `call kmain`.

**The gotcha.** You start in **supervisor mode, not machine mode.** Every
tutorial that does privileged setup directly will not work; you go through
OpenSBI with `ecall` instead. That's why `sbi_set_timer` (line 1121) exists at
all.

---

## 2. Serial output

**The problem.** Print something.

**The picture.** The UART is a mailbox nailed to a specific wall. Not a
function you call — an *address you write to*. Store a byte at `0x1000_0000`
and it comes out the serial port.

| the picture | the code |
|---|---|
| the wall slot | `UART0_PHYS = 0x1000_0000` |
| posting a letter | `write_volatile(base, c)` in `putchar`, line 853 |
| checking if mail arrived | LSR at `base + 5`, bit 0 |

**The code.** `putchar` → `puts` → `impl core::fmt::Write for Uart` →
`println!`. Four layers, each one thin.

**The gotcha.** `write_volatile`, not `*base = c`. The optimiser deletes stores
nothing ever reads back — and nothing in *your program* reads it back. The
reader is a chip. `volatile` means "this store is observable by someone I can't
show you."

---

# Phase II — a machine that can be interrupted

## 3. Traps

**The problem.** Something goes wrong — a bad instruction, a bad address. The
CPU needs somewhere to go.

**The picture.** A fire alarm with one hard-wired destination. You don't get to
handle it where it happened; the hardware *teleports* you to one address you
registered in advance, and everything you were holding is still in your hands.

| the picture | the code |
|---|---|
| the address the alarm sends you to | `stvec`, set in `kmain` |
| the note saying what happened | `scause` |
| the note saying what address caused it | `stval` |
| where you were standing | `sepc` |
| everything still in your hands | all 32 registers, **untouched by hardware** |

**The code.** `trap_entry` in `entry.S` saves all 32 registers to the stack —
256 bytes, in register-number order, so Rust can view it as `[usize; 32]` and
index by register number. Then it calls `trap_handler` (line 4256).

**The gotcha, and it cost real time.** `stvec`'s low two bits are a **MODE
field**, not address bits. A handler at `0x...22a` sets mode `0b10`, which is
reserved. The entry point must be 4-byte aligned — and Rust can't align a
function, so the alignment has to be asserted in assembly.

---

## 4. Timer

**The problem.** Nothing can be preempted if nothing ever interrupts.

**The picture.** Not an egg timer that rings once. A **flood sensor**: the
condition is `time >= timecmp`, and `time` only counts up. Once it trips, it
stays tripped forever until you move the threshold.

| the picture | the code |
|---|---|
| the water level | the `time` CSR, always rising |
| the line on the wall | `timecmp`, set via SBI |
| moving the line up | `sbi_set_timer(now() + TICK_INTERVAL)`, line 1121 |

**The gotcha.** Rearming is **acknowledgement**, not scheduling. Return from
the handler without pushing `timecmp` forward and the CPU re-traps at the very
next instruction boundary, forever — measured at ~165,000 ticks per second
instead of 1. *(You caught this one from the output before I did.)*

---

# Phase III — memory

## 5. Frame allocator + device tree

**The problem.** How much RAM is there, and who hands out pages of it?

**The picture — the boxes.** A row of numbered boxes, all empty. You need to
remember which are free, but you have nowhere to keep a list — the list would
itself need memory, and this *is* the memory system.

So: **the note goes inside the box.** Each free box holds a slip of paper with
the number of the next free box. One variable by the door says which box is
first.

| the picture | the code |
|---|---|
| a box | a 4096-byte page |
| the note by the door | `FREE_LIST` — one `usize` |
| the slip inside box X | the first 8 bytes of that page |
| taking a box | `frame_alloc`, line 4191 — read the slip, make it the new door note |
| putting one back | `frame_free` — write the old door note into the box |

The allocator needs **zero** memory of its own. That's the entire trick.

**Where the boxes are** comes from the device tree — a blob QEMU hands you in
`a1`, parsed by `dtb_memory` (line 4080). Big-endian, a token stream, magic
`0xd00dfeed`.

**The gotcha.** `frame_free(x)` twice makes a page point at *itself*. Every
subsequent allocation returns that same page and the rest of the list becomes
unreachable — demonstrated live: three allocations all returned `0x87fff000`.
`pykernel.py` section 1 reproduces it.

---

## 6. The MMU — Sv39

**The problem.** Every address a program uses is a real address. Nothing stops
it touching anything.

**The picture — the cities (yours).** *"Same address in two different towns —
180 Virginia Street in Colorado and 180 Virginia Street in Kentucky. Same
address, different place."*

That's it exactly. A virtual address is a street address; `satp` says which
city you're in; the page table is that city's map.

| the picture | the code |
|---|---|
| which city you're in | the `satp` register |
| that city's street map | the page table, 3 levels deep |
| one street in the directory | a PTE — 8 bytes holding **both** the destination and the rules |
| "residents only" | `PTE_U` — the user bit |
| "look but don't build" | `PTE_R` without `PTE_W` |
| an address with no entry | page fault |

**The code.** `map` (1757) writes an entry, `probe` (1194) walks the table by
hand and tells you what an address resolves to, `paging_init` (1822) builds the
whole map. `va()` / `pa()` (1734) convert between the two aliases.

**The gotcha — two of them, both severe.**

*The chicken and egg (6c).* The kernel is linked high, so ~435 absolute
addresses in `.rodata` (vtables) are high addresses. `println!` dispatches
through one. **You cannot print until the higher half exists.** Hence
`boot_paging()` at line 117: a static table, four 1 GiB leaves, and it must
never print, panic, or dynamically dispatch.

*Everything `frame_alloc` returns is PHYSICAL.* Once the identity map went
away, a physical address stopped being something the kernel can dereference.
This bit three separate places, all latent, each exposed only when its code
path first ran after relocation.

---

## 7. The heap

**The problem.** Frames are 4096 bytes. A `String` is 37 bytes.

**The picture.** A shelf. Occupied stretches have no label at all — Rust tells
you the size when you hand a block back, so only the **gaps** need signs. Each
gap holds a card: *"this gap is N bytes, the next gap starts at X."*

| the picture | the code |
|---|---|
| the cards in the gaps | `FreeBlock { size, next, magic }` |
| the first card | `FREE_HEAD` |
| taking the first gap that fits | first fit, `impl Heap`, line 2083 |
| splitting a too-big gap | only if the leftover can hold its own card |
| merging gaps that touch | coalescing, **both directions** |
| `0x5EBB1E` on every card | SEBBIE — the validity check |

**The gotcha.** Remove backward coalescing and the free **byte** count stays
identical while the free **block** count climbs. Same memory, more confetti,
until a large request fails with plenty free. `pykernel.py` section 3 shows
both side by side.

---

# Phase IV — many things at once

## 8. Threads

**The problem.** One CPU. Two things that both want it.

**The picture — the desk.** One desk, and the registers are what's on it. To
switch tasks you sweep everything on the desk into box A, then lay out box B.
Same desk, different work.

| the picture | the code |
|---|---|
| the desk | the 32 CPU registers |
| box A / box B | `Context` — 14 registers each |
| sweeping the desk | `switch(old, new)` in `entry.S` |
| the last thing you do | `ret` — **which is the switch** |

**Why `ret` is the whole thing.** `ret` jumps to whatever is in `ra`. `switch`
loads `ra` from the *new* thread's box just before it. So `ret` jumps into
where the other thread stopped. One ordinary instruction doing something
enormous, because of what was quietly put in a register beforehand.

**Why only 14 registers, not 32.** A trap is involuntary — it can land between
any two instructions, so `trap_entry` must save everything. A context switch is
an ordinary **function call**, and the calling convention has already forced
the caller to spill anything it cared about. Only the callee-saved set needs
preserving.

**The code.** `thread_spawn` (2456) forges a context whose `ra` points at
`thread_start`, so the first `switch` "returns" into a function that was never
called. `yield_now` (2518) round-robins.

**The gotcha.** `THREADS` is `Vec<Box<Thread>>` and the `Box` is load-bearing.
Written as `Vec<Thread>`, spawning the fifth thread grew the Vec past capacity
4, moved every suspended thread's saved registers, and turned the scheduler's
pointers into freed memory.

---

## 9. Preemption and locks

**The problem.** A thread that never yields owns the machine forever — the
Windows 3.1 failure mode.

**The fix.** The timer's trap handler calls `yield_now()`. A greedy thread gets
switched away whether it cooperates or not.

**The picture — the lock.** Not a sign on a door saying "occupied." The lock
*is the doorframe*: you can't reach the room without going through it.
`SpinLock<T>` **holds** the data rather than sitting beside it, so touching the
value requires holding the lock. There is no way to write the bug.

| the picture | the code |
|---|---|
| the doorframe | `SpinLock<T>` wrapping the data |
| the key you hold while inside | `SpinGuard` |
| dropping the key on the way out | `impl Drop for SpinGuard` |
| bolting the door from inside first | `intr_off()` **before** taking the lock |

**Why interrupts go off first, and it matters twice later.** If the timer fires
while you hold a lock and the handler wants the same lock, it spins forever
waiting for a thread that can't resume until the handler returns. Turning
interrupts off first makes that unreachable — not "avoided", *unreachable*.

**The gotcha (`kmain_high` ordering).** The timer must be armed **before** the
scratch zone. It wasn't, and a greedy thread monopolising the CPU looked exactly
like a scheduler bug when the real cause was that no timer had ever been
scheduled. *This same class of bug returned at milestone 16.*

---

## 10. User mode

**The problem.** Run code you don't trust.

**The picture.** The kernel is staff-only. A user program has a visitor badge:
it can walk the public corridors, and every door to the back office is locked
*by the hardware*, not by politeness.

| the picture | the code |
|---|---|
| the visitor badge | `PTE_U` on a page |
| the staff door | any page without `U` |
| "staff may not use visitor entrances either" | `sstatus.SUM` **off** |
| the one way to talk to staff | `ecall` — a syscall |
| the staff-only stairwell | `sscratch` holding a kernel stack |

**The stack problem, which is the good part.** A trap from user mode arrives
with `sp` chosen by the untrusted program. It could aim the kernel's own
register dump anywhere, or point somewhere unmapped so the trap itself faults.
So `sscratch` holds this thread's kernel `sp` while in user mode, and 0 while in
the kernel — and `trap_entry` opens with `csrrw sp, sscratch, sp`, a **swap**,
because that's the only way to obtain a usable stack without already having one.

**The code.** `user_range_ok` (1521) validates every user pointer — and the
rule is not "is it mapped" but **"is it mapped for *them*"**, because kernel
pages are mapped and readable and must still be refused. It checks overflow,
rejects anything ≥ 2³⁸, and walks **every page** in the range: a valid pointer
with a length running off the end of its mapping is the classic hole.

`copy_from_user` (1563) turns SUM on for exactly one byte and off again. With
SUM off by default, a stray kernel dereference of a user pointer *faults*
instead of quietly working — accidents stay loud.

**The gotcha — the biggest bug in the project.** See §"sepc" below. It belongs
to this milestone and it took hours.

---

## The one that cost the most: `sepc` and `sstatus` are single global CSRs

There is exactly **one** `sepc` on the chip. A trap sets it; `sret` reads it.
Fine with no scheduler. With one:

```
thread A traps           sepc = A's PC
handler → yield_now →    switch to thread B
B's handler finishes →   sret
sret reads sepc          which now describes A, not B
```

So `sret` jumps to **another thread's program counter, at another thread's
privilege level.**

What it looked like: *"a caller-saved register holding a live pointer contains
garbage."* Chased through two tripwires, a full `asm!` audit, and
disassembly-level review of `trap_entry` and `switch` — all clean, because
nothing was corrupting anything.

Fixed by saving both into the `TrapFrame` (offsets 256 and 264) and writing them
back on the way out. `trap_handler` reads and edits `frame.sepc`, never the CSR.

**Two commits were reverted to keep the tree green before the third worked.**
That's the part worth keeping.

---

## 11. Processes

**The picture — the cities again, now with a shared district.** Every city has
its own street map. But the top half of every map is **identical** — the same
government district, at the same addresses, in every city.

| the picture | the code |
|---|---|
| a city | a process's address space |
| its own map | `proc_pagetable()`, line 1383 |
| the shared government district | root slots **256..511**, copied from the kernel |
| its private half | slots 0..255 |
| moving between cities | writing `satp` in `yield_now` |

Copying 256 eight-byte numbers gives a new process a complete, correct kernel.
And because the kernel is at identical addresses everywhere — **including the
stack being used at that moment** — switching `satp` mid-kernel is safe.

**The gotcha.** Syscall pointers are validated against the **currently active**
address space, read from `satp`, not against the kernel's. A pointer only means
anything in the city its owner lives in.

---

# Phase V — the part that isn't Linux

## 12. The store

**The problem.** Files have paths. Paths are addresses. You wanted neither.

**The picture — the encyclopedia.** *(Your analogy — tell me if I've placed it
where you meant.)* A path is a shelf location: aisle 4, shelf 2, third book
along. An encyclopedia entry has no shelf location — it has a **subject**, and
you find it by describing what you want. Two people can file the same fact
under different headings without copying it.

| the picture | the code |
|---|---|
| the bound content | a **blob** — `hash(bytes)`, stored once |
| an entry *about* that content | an **object** — `hash(blob + attributes)` |
| the headings you can search by | typed attributes: `Int`, `Text`, `Id`, `Bytes` |
| looking something up by description | `store_query_owned`, line 3280 |
| a marginal note added later | a **claim** — append-only |

**Why blobs and objects are separate, and it's not tidiness.** Hashing only the
bytes caused real data loss: two different documents that happened to contain
identical content collapsed into one, and the second's metadata was silently
discarded — a shopping list overwrote a tax return's name. Git does exactly this
split for exactly this reason.

**Why values are typed.** Stored as raw bytes, `created_at` could only be tested
for equality, because alphabetically `"9"` sorts after `"1754870400"`. Time is
the axis that narrows hardest, so it's precisely the one that must be typed.

**"Delete" is three problems wearing one word** — lines 3359, 3370, 3391:

| problem | verb | behaviour |
|---|---|---|
| clutter | `hide` | an attribute. Reversible, nothing lost. Most deletion is this. |
| space | `evict` | the bytes go, **the record stays** |
| privacy | `forget` | both go |

**Eviction is the one no filesystem can do.** The record survives as a valid,
globally meaningful coordinate with nothing behind it — so *"the file I was
working on while that video was open"* still answers after the video is gone.

**Mutation is a claim** (3327) because objects are content-addressed and
changing an attribute would change the id. A claim says *"as of time T, object
X's key K is V"*, and the current value is simply the latest claim. Nothing is
overwritten, so **when** something was hidden stays answerable.

---

## 13. The disk

**The picture — the restaurant pass.** Three shared arrays between you and the
device: a shelf of trays (descriptors), an order rail (available), and a done
rail (used). You put a tray on the order rail; the device works on it and moves
it to the done rail.

| the picture | the code |
|---|---|
| the shelf of trays | descriptors — address, length, flags, next |
| the order rail | the available ring |
| the done rail | the used ring |
| one order = three trays | header the device reads, data buffer, status byte it writes |

**The gotcha, and it's the scariest thing in the codebase.** The device does
**DMA and does not go through the MMU.** Every address in a descriptor is
PHYSICAL, and the device writes straight to physical memory — no page table, no
permission bits, no `U` bit, no SUM. Everything milestone 6 built to control
what may touch what **does not apply to hardware.** A wrong descriptor address
is unbounded silent corruption with no fault to catch it. Real machines put an
IOMMU in front of devices for exactly this.

**The format (13b).** Sector 0 is a header — magic `0xF01DAB1E`, version,
length. Then a stream of tag-prefixed records. Recovery is `deserialize_store`
walking the stream: **replay is the recovery algorithm.**

That fell out of the design rather than being engineered. The store is
append-only and immutable, so a record torn in half by a power cut fails to
parse and gets discarded, and there's nothing half-updated behind it to repair.
No journal, no fsck. ext4 bolts a log onto a mutable structure to buy this;
there's no mutable structure here to bolt it to.

---

## 14. The shell

**The problem.** The machine had written thousands of lines and read zero bytes.

Every shell ever written resolves a **name** to a **location**. This one can't.
So an argument is one of exactly two things:

| you type | what it is |
|---|---|
| `find type=python created_at>100` | a **query** |
| `hide 2` | an **index into the last result set** |

The numbered list is the path replacement. Ephemeral, contextual, meaningless a
minute later — fine, because you're looking at it while you use it.

---

# Phase VI — out of the kernel

## 15. Interrupt-driven console

**The picture — the switchboard.** The hart has exactly **one** external
interrupt wire and the board has dozens of devices. The PLIC is a separate chip
whose only job is to multiplex them and answer *"who was it?"*

| the picture | the code |
|---|---|
| the switchboard | the PLIC at `0x0c00_0000` |
| which extensions may ring you | the enable bitmap |
| picking up the phone | **claim** — returns the source and marks it in flight |
| hanging up | **complete** — write the number back |

**Claim/complete is a handshake, and forgetting the second half silences that
device forever.** Same lesson as the timer: the write is an *acknowledgement*.

**The gotcha you already had a defence for.** The handler writes to the console
ring; the shell reads it. Both need the lock. An interrupt landing while the
shell holds it would deadlock the machine — except `SpinLock::lock` disables
interrupts *before* taking the lock, so it physically cannot happen. Milestone
9b fixed this bug weeks before it could be written.

---

## 16. Life and death

**The picture — the waiting room.** Take a numbered ticket, sit down, doze.
Someone calls your number; everyone else sleeps on.

| the picture | the code |
|---|---|
| your ticket number | a **channel** — the *address* of the thing you wait for |
| dozing | `sleep(chan)`, line 2650 |
| the nurse calling a number | `wakeup(chan)`, line 2664 |
| nobody offers a dozing person the desk | `yield_now` skips non-`Runnable` |

Channels are addresses because addresses are already unique — collision-free
wait queues with no registry and no allocation.

**The lost wakeup.** Check the buffer, take an interrupt, have the handler shout
into an empty room because nobody's asleep yet, then sleep forever one
instruction from the data. It needs the interrupt to land in a one-instruction
window, so it passes every test and hangs in front of an audience. The three
steps must be indivisible — on one hart, that's `intr_off()`.

**Why zombies exist, and it's physics not taste.** *A thread cannot free its own
kernel stack, because it is standing on it.* Free 16 KiB and execute one more
instruction — which pushes to that stack — and you're writing into memory the
heap already gave away. So death is two phases, and the gap between them **is**
the zombie.

| phase | who | what goes |
|---|---|---|
| `thread_exit` (2717) | the dying thread | the address space |
| `thread_wait` (2779) | somebody else | the 16 KiB stack, and the slot |

**Two orderings that aren't negotiable.** Move `satp` to the kernel's table
*before* dismantling your own — you're executing through the table you're about
to free. And free root slots **0..256 only**; 256..511 are the kernel, shared
into every address space.

**The gotcha, and it's milestone 9's returning.** The timer is armed before
`threads_init` runs, so early boot traps reach `yield_now` with an **empty
thread table**. The old `if threads.len() < 2 { return }` had been covering that
by accident. Without a guard the scan finds nothing runnable, takes the idle
path, and parks the core on `wfi` one tick into boot — having printed just
enough to make the hang look like it was in the store or the disk driver.

---

## 17. A heap in userspace

**The picture — the curb.** *(Your word — I've read it as the boundary of what
you own. Correct me.)* Your property runs up to the curb. Past it is the
street: step there and you fault. `sbrk(n)` asks the city to move the curb out
by `n` feet, and hands you back **where the curb used to be** — because that's
the first address of the new land.

| the picture | the code |
|---|---|
| your property | mapped user pages |
| the curb | the **break** — one `usize` per process |
| past the curb | unmapped. Page fault. |
| "move it out 4096" | `sbrk(4096)` — syscall 4 |
| the strip you just gained | the return value: the **old** curb |
| the only one allowed to move it | the kernel — it owns the page tables |

**The allocator on top** is a bump allocator: one pointer walks forward,
`dealloc` does nothing. Correct rather than lazy — `thread_exit` unmaps the
whole address space at once, so reclaiming individual allocations solves a
problem that doesn't exist. Before milestone 16, it would have been a real leak.

**Your call:** 64 KiB chunks, or the request rounded up to a page if bigger —
xv6's rule. Verified: the break grows by exactly `65536 + 106496` across the
whole demo. **Two syscalls for the entire program.**

**The gotcha it exposed.** The first allocation faulted, writing to the
program's own image. `map_user` mapped **every** page `r-x`, and nobody had
noticed because until then the user program had *no writable globals at all*.
W^X caught the first program that genuinely needed writable data.

---

## 18. The syscalls a shell needs

`read_char`, `get`, `verb`, `save`. `read_char` hands over **bytes and nothing
else** — echo, backspace, and the entire notion of "a line" are built in
userspace, because every one of them is policy.

**The bug worth remembering.** The `verb` dispatch ended with `_ =>` calling a
function that **panicked**, with a comment saying "validated by the caller." The
caller is an untrusted program putting whatever it likes in `a1`. **A syscall a
user program can halt the kernel with is a denial of service, not a syscall.**
Every `match` on user-supplied data needs its `_` arm to refuse, not assume.

---

## 19. exec by query

**The picture — the packing list.** A flat binary is a box of parts with no
labels; you must already know where everything goes. An ELF is the same box
with a manifest.

| the manifest says | the ELF field |
|---|---|
| "the list starts at item 64, 5 lines long" | `e_phoff`, `e_phnum` |
| "take bytes 0x1000–0x4924…" | `p_offset`, `p_filesz` |
| "…put them at address 0x1000" | `p_vaddr` |
| "this shelf is read-only, executable" | `p_flags` — 4=R, 2=W, 1=X |
| "then add 16 more blank bytes" | `p_memsz − p_filesz` |
| "start at address 0x1000" | `e_entry` |

That second-to-last row **is `.bss`** — stated by the file rather than invented
by a constant two unrelated files had to agree on. Parsing it deleted the
`USER_DATA_BASE = 0x8000` hack.

**And the part nobody else has.** A program is an object with `type=program`.
Running one means **running a query** — so ambiguity stops being an error. Two
matches isn't "not found", it's a numbered list, the same interface as
everything else, because it's the same operation.

`load_elf` is at 1265, and it treats every field as hostile: these bytes come
out of the store, and the store is whatever somebody put there.

**The gotcha, latent since milestone 10.** The null page was **mapped**. The
user stack sat at `USER_BASE - PAGE_SIZE`, which is 0, so a null dereference
quietly read the bottom of the stack instead of faulting — defeating the entire
reason `user.ld` links at `0x1000`.

---

## 20. The shell moves out

280 lines left `main.rs`. Everything the shell does was policy; it lived in
supervisor mode for six milestones only because the kernel was the only place
with a heap.

**SHA-256 (line 2917), and the reason is specific.** FNV-1a is **trivially
invertible** — xor and multiply by an odd constant, both reversible — so given
any target id you can *compute* bytes that produce it. Under content addressing
that means "I can make my object be your object". It didn't matter while only
the kernel wrote to the store. It started mattering the instant an untrusted
program could.

`sha256_selftest()` runs every boot and is silent unless it fails, because a
subtly wrong hash still produces stable, random-looking ids: every test passes,
dedup appears to work, the disk round-trips, and the store has quietly stopped
being content addressed. Nothing else in the kernel can be wrong that invisibly.

---

# Where the analogies came from

| analogy | whose | what it explains |
|---|---|---|
| cities, same street name | **yours** | address spaces |
| boxes with notes inside | shared | the frame allocator's free list |
| the curb | **yours** — check my reading | the break / `sbrk` |
| the encyclopedia | **yours** — check my placement | the store vs. paths |
| the desk and two boxes | mine | context switch |
| the waiting room | mine | sleep / wake |
| the restaurant pass | mine | virtio rings |
| the packing list | mine | ELF program headers |
| the switchboard | mine | the PLIC |
| the superintendent | mine | OpenSBI |

Two of these are marked "check my reading" — say where you actually meant them
and I'll rewrite those sections around your version, since yours are the ones
that will still make sense in six months.
