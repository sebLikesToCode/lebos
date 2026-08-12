// The kernel.
//
// #![no_std]  -- there is no standard library. `std` assumes an OS underneath
//                it (files, threads, heap, stdout). We are the OS. What's left
//                is `core`: types, iterators, Option/Result, no allocation.
//
// #![no_main] -- there is no `main`. The normal Rust entry point is installed
//                by the C runtime, which is also absent. Our entry point is
//                _start in entry.S, which calls kmain below.

#![no_std]
#![no_main]

// `alloc` is the part of the standard library that needs a heap but not an OS:
// Vec, Box, String, BTreeMap. It refuses to exist until something is marked
// #[global_allocator], which is what milestone 7 provides.
extern crate alloc;

use core::alloc::{GlobalAlloc, Layout};
use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Timer interrupts since boot. Atomic rather than `static mut` so it stays
/// correct once there is more than one hart.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// Printed at boot, directly under OpenSBI's banner and deliberately in the
/// same style.
///
/// A raw string (`r"..."`) so the backslashes stay backslashes instead of
/// being read as escape sequences.
///
/// Legal note for the confused: LeBOS is also a French womenswear label. This
/// is the other one.
const BANNER: &str = r"
 _          ____   ___  ____
| |    ___ | __ ) / _ \/ ___|
| |   / _ \|  _ \| | | \___ \
| |__|  __/| |_) | |_| |___) |
|_____\___||____/ \___/|____/
        no files. no paths. no regrets.
";

macro_rules! print {
    ($($arg:tt)*) => {
        _print(format_args!($($arg)*))
    };
}

macro_rules! println {
    () => {
        print!("\n")
    };
    ($($arg:tt)*) => {
        print!("{}\n", format_args!($($arg)*))
    };
}

// Pull entry.S into the binary. Its `.text.entry` section is placed at
// 0x80200000 by linker.ld, which is where OpenSBI jumps.
core::arch::global_asm!(include_str!("entry.S"));

extern "C" {
    /// The 4-byte-aligned trap trampoline in entry.S. Never called from Rust;
    /// its address is what goes into stvec.
    fn trap_entry();
}

/// One page table, statically allocated so it exists before the frame
/// allocator does. Lives in .bss, which entry.S has already zeroed.
#[repr(C, align(4096))]
struct PageTable([usize; 512]);

static mut BOOT_PT: PageTable = PageTable([0; 512]);

/// Map the high half before doing anything else at all.
///
/// This exists because the kernel is linked with its VMA in the high half, so
/// every absolute address the linker wrote into .rodata -- ~435 of them, all
/// vtables -- is a high address. `println!` reaches the UART through
/// `&mut dyn Write`, which is a vtable call. So printing is impossible until
/// the high half is mapped, which makes this the first thing that must run.
///
/// Nothing in here may print, panic, or dynamically dispatch: none of that
/// works yet. Four 1 GiB leaves, no allocator, no error handling.
///
/// # Safety
/// Must be called exactly once, before anything else, with paging off.
unsafe fn boot_paging() {
    let pt = core::ptr::addr_of_mut!(BOOT_PT) as *mut usize;
    let flags = PTE_V | PTE_R | PTE_W | PTE_X | PTE_A | PTE_D;

    *pt.add(0) = pte(0x0000_0000, flags); // devices, identity
    *pt.add(2) = pte(0x8000_0000, flags); // RAM, identity
    *pt.add(256) = pte(0x0000_0000, flags); // devices, high half
    *pt.add(258) = pte(0x8000_0000, flags); // RAM, high half

    // `addr_of_mut!` is PC-relative, and the PC is still physical, so this is
    // the physical address of the table -- which is what satp wants.
    let satp = (8_usize << 60) | ((pt as usize) >> 12);
    core::arch::asm!("sfence.vma", "csrw satp, {}", "sfence.vma", in(reg) satp);
}

/// First Rust code to run. Called from `_start` with the two values OpenSBI
/// left in a0/a1, executing at PHYSICAL addresses with paging off.
///
/// `extern "C"` gives it the C ABI so assembly can call it, and `#[no_mangle]`
/// keeps the symbol literally named `kmain` so the `call kmain` resolves.
///
/// It must never return -- hence `-> !`.
#[no_mangle]
pub extern "C" fn kmain(hartid: usize, dtb: *const u8) -> ! {
    // Before literally anything else -- see boot_paging.
    unsafe { boot_paging() };

    // Point stvec at the assembly trampoline in entry.S, not at trap_handler
    // directly: stvec's low two bits are a mode field, so the address must be
    // 4-byte aligned, and Rust cannot align a function.
    unsafe {
        core::arch::asm!("csrw stvec, {}", in(reg) trap_entry as *const () as usize);
    }

    println!("{}", BANNER);
    println!("hart {} | dtb at {:#x}", hartid, dtb as usize);

    // Ask the machine how much RAM it has instead of assuming.
    let (ram_base, ram_size) = dtb_memory(dtb).expect("no memory node in device tree");
    println!(
        "ram: {:#x}..{:#x} ({} MiB), per device tree",
        ram_base,
        ram_base + ram_size,
        ram_size / 1024 / 1024
    );

    // Carve the heap off the top of RAM BEFORE the frame allocator claims
    // anything, so the two can never fight over the same bytes.
    let ram_end = ram_base + ram_size;
    let heap_phys = ram_end - HEAP_SIZE;

    let free = frame_init(heap_phys);
    println!(
        "memory: {} frames free ({} MiB)",
        free,
        free * PAGE_SIZE / 1024 / 1024
    );
    println!(
        "heap:   reserved {:#x}..{:#x} ({} KiB)",
        heap_phys,
        ram_end,
        HEAP_SIZE / 1024
    );

    println!("paging: building kernel page table...");
    paging_init(ram_base + ram_size);
    println!("paging: MMU is on -- this line was fetched through a page table");

    // Virtual, not physical -- the heap has to outlive the identity map.
    heap_init(va(heap_phys), HEAP_SIZE);

    // Prove the higher-half alias is real: write through the low address,
    // read it back through the high one. If they are the same memory, the
    // MMU is genuinely translating rather than passing addresses through.
    //
    // 0x1EB05 spells LEBOS in hex. Throwaway probe value here, not the
    // reserved on-disk magic number.
    {
        let page = frame_alloc().expect("no frame for the alias test");
        let low = page as usize;
        let high = HIGH_BASE + low;
        unsafe {
            core::ptr::write_volatile(low as *mut usize, 0x1EB05);
            let back = core::ptr::read_volatile(high as *const usize);
            println!("alias: wrote {:#x} to {:#x}", 0x1EB05_usize, low);
            println!("       read  {:#x} at {:#x}", back, high);
            println!(
                "       one physical page, two virtual addresses: {}",
                if back == 0x1EB05 {
                    "CONFIRMED"
                } else {
                    "FAILED"
                }
            );
        }
        frame_free(page);
    }

    println!("reloc: currently at pc = {:#x}", here());

    // Move to the higher half.
    //
    // Both maps are live, so the kernel is reachable at two addresses right
    // now. This shifts the stack pointer and the program counter to the high
    // alias of exactly the memory already executing.
    //
    // `la t0, kmain_high` is PC-relative, so it yields the LOW address of that
    // function -- adding HIGH_BASE turns it into the high one. After `jr`, the
    // PC is high and every subsequent PC-relative reference resolves high too,
    // automatically, with no further work.
    unsafe {
        core::arch::asm!(
            "add sp, sp, {off}",
            "la  t0, {f}",
            "add t0, t0, {off}",
            "jr  t0",
            off = in(reg) HIGH_BASE,
            f = sym kmain_high,
            options(noreturn),
        );
    }
}

/// Everything after the kernel has relocated. The PC is in the higher half
/// from the first instruction of this function.
extern "C" fn kmain_high() -> ! {
    // The UART is the kernel's one hand-written absolute address, so it is the
    // one thing that does not relocate itself. Point it at the high alias
    // before the identity map goes away.
    UART_BASE.store(va(UART0_PHYS), Ordering::Relaxed);

    println!("reloc: now at pc = {:#x}", here());

    // stvec still holds the LOW address of trap_entry, which is about to stop
    // existing. `trap_entry as usize` is PC-relative and the PC is now high,
    // so this reads back the high address with no adjustment -- unlike ROOT
    // below, which was stored as a bare physical number.
    let entry = trap_entry as *const () as usize;
    unsafe { core::arch::asm!("csrw stvec, {}", in(reg) entry) };
    println!("reloc: trap vector moved to {:#x}", entry);

    // ---------------------------------------------------------------------
    // The identity map CANNOT be removed yet. Measured, not assumed.
    //
    // Clearing root slots 0 and 2 boots, relocates, and then dies on the first
    // `println!` from the trap handler: a store page fault in trap_entry with
    // sp far below the stack, after the kernel had run away into unmapped low
    // memory.
    //
    // Cause: `code-model=medium` makes CODE position-independent, but it does
    // nothing about DATA the linker fills in with absolute addresses. This
    // binary has 436 absolute low addresses embedded in .rodata -- vtables.
    // core::fmt reaches the UART through `&mut dyn Write`, and calling through
    // that vtable jumps to 0x802xxxxx, which no longer exists.
    //
    // Direct calls survive (a bare `putchar` ticks along fine); anything
    // dynamically dispatched does not.
    //
    // The real fix is 6c-ii proper: relink with VMA in the high half and LMA
    // at 0x80200000 (`> virt AT> phys` in linker.ld), so the linker writes
    // HIGH addresses into those 436 slots. Early boot must then reach symbols
    // physically, which PC-relative `la` already does for free.
    //
    // Until then the identity map stays. The kernel genuinely executes in the
    // higher half, which was the point; the low half simply is not reclaimed
    // yet.
    // ---------------------------------------------------------------------
    // Burn the bridge. Devices sit below 0x4000_0000 so their identity
    // mappings live in root slot 0; RAM at 0x8000_0000 lives in slot 2. The
    // higher-half twins are slots 256 and 258 and are untouched.
    //
    // This only became possible once the kernel was relinked with a high VMA:
    // before that, ~435 vtable entries in .rodata held low addresses and the
    // first println! after this point jumped into unmapped memory.
    //
    // sfence.vma afterwards, because the TLB is still holding translations
    // that just became lies.
    let root = va(ROOT.load(Ordering::Relaxed)) as *mut usize;
    unsafe {
        *root.add(0) = 0;
        *root.add(2) = 0;
        core::arch::asm!("sfence.vma");
    }
    println!("reloc: identity map removed -- the low half is free for user programs");

    let rp = ROOT.load(Ordering::Relaxed);
    println!("map:");
    explain(rp, here());
    explain(rp, va(UART0_PHYS));
    explain(rp, &BANNER as *const _ as usize);
    explain(rp, 0x8020_0000);

    // The payoff: collections that were impossible ten minutes ago.
    {
        use alloc::boxed::Box;
        use alloc::string::String;
        use alloc::vec::Vec;

        let (n0, f0) = heap_stats();
        println!("heap: {} free block, {} bytes free", n0, f0);

        let mut v: Vec<u64> = Vec::new();
        for i in 1..=8u64 {
            v.push(i * i);
        }
        println!("heap: Vec    {:?}", v);

        let boxed = Box::new(0x1EB05_u64);
        println!("heap: Box    at {:p} holds {:#x}", boxed, *boxed);

        let text = String::from("allocated on a heap in an OS I wrote");
        println!("heap: String \"{}\"", text);

        let (n1, f1) = heap_stats();
        println!("heap: {} free blocks, {} bytes free (in use)", n1, f1);

        // What the bump allocator could not do: hand the same space back.
        let x = Box::new(1u64);
        let px = &*x as *const u64;
        drop(x);
        let y = Box::new(2u64);
        println!("heap: freed {:p}, next alloc {:p}", px, &*y);

        drop(v);
        drop(boxed);
        drop(text);
        drop(y);

        // If coalescing works, every gap merges back into one and the heap
        // looks untouched. If it did not, this would report a pile of
        // separate fragments.
        let (n2, f2) = heap_stats();
        println!("heap: {} free block, {} bytes free (all released)", n2, f2);
    }

    // Schedule the first timer interrupt one tick out.
    //
    // This has to happen BEFORE the scratch zone, or experiments run on a
    // kernel where no timer has ever been armed -- which silently disables
    // preemption and made a greedy thread look like a scheduler bug.
    sbi_set_timer(now() + TICK_INTERVAL);

    // Two separate enables, both required:
    //   sie bit 5  (STIE) -- allow supervisor timer interrupts specifically
    //   sstatus bit 1 (SIE) -- the master interrupt enable
    //
    // `csrs` sets the given bits and leaves the rest of the register alone.
    // (`csrw` would overwrite the whole thing and destroy unrelated state;
    //  `csrc` clears bits.)
    unsafe {
        core::arch::asm!("csrs sie, {}", in(reg) 1_usize << 5);
        core::arch::asm!("csrs sstatus, {}", in(reg) 1_usize << 1);
    }

    // =====================================================================
    //  SCRATCH ZONE
    //
    //  Put experiments HERE, in src/main2.rs, then `make play`.
    //
    //  By this point everything is up: paging, the heap, println!, traps,
    //  AND the timer, so preemption is live. Code here runs once, before the
    //  idle loop below.
    //
    //  This marker lives in main.rs so that `make resync` always gives you a
    //  fresh copy of it. Nothing you write inside it is precious --
    //  `make resync` wipes main2.rs back to this file.
    //
    //  Useful imports:
    //      use alloc::boxed::Box;
    //      use alloc::string::String;
    //      use alloc::vec::Vec;
    // =====================================================================

    // ---- your experiments go here ----

    threads_init();
    thread_spawn("alpha", thread_alpha);
    thread_spawn("beta", thread_beta);
    thread_spawn("greedy", thread_greedy);

    println!("threads: spawned alpha, beta and greedy (which never yields)");
    for round in 0..4 {
        println!("[main ] round {}", round);
        yield_now();
    }
    println!(
        "threads: back in {} -- greedy is now preempted forever",
        current_name()
    );

    // Two threads hammering one counter, preempted at random moments.
    thread_spawn("race1", thread_racer);
    thread_spawn("race2", thread_racer);
    println!(
        "lock: two threads will each add {} under a spinlock",
        RACE_ITERS
    );

    while RACERS_DONE.load(Ordering::Relaxed) < 2 {
        yield_now();
    }

    let total = *COUNTER.lock();
    println!(
        "lock: expected {}, got {} -- {}",
        RACE_ITERS * 2,
        total,
        if total == RACE_ITERS * 2 {
            "no updates lost"
        } else {
            "UPDATES LOST"
        }
    );

    // =====================================================================
    //  END SCRATCH ZONE
    // =====================================================================

    loop {
        // Wait For Interrupt: idles the core instead of spinning it at 100%.
        unsafe { core::arch::asm!("wfi") };
    }
}

/// Two threads that do nothing but announce themselves and hand the desk
/// back. `-> !` because a thread has nowhere to return TO -- its `ra` was
/// forged, and running off the end would jump into whatever the fake return
/// address happened to be.
extern "C" fn thread_alpha() -> ! {
    for i in 0..4 {
        println!("[alpha] tick {}", i);
        yield_now();
    }
    loop {
        yield_now();
    }
}

extern "C" fn thread_beta() -> ! {
    for i in 0..4 {
        println!("[beta ] tick {}", i);
        yield_now();
    }
    loop {
        yield_now();
    }
}

/// A thread that never yields, on purpose.
///
/// Under cooperative scheduling this would own the CPU forever and freeze the
/// machine -- the Windows 3.1 failure mode. If anything else prints after this
/// starts, preemption is real.
extern "C" fn thread_greedy() -> ! {
    let mut spins: u64 = 0;
    loop {
        spins = spins.wrapping_add(1);
        if spins % 4_000_000 == 0 {
            println!("[greedy] {}M spins, never yielded once", spins / 1_000_000);
        }
        // Note the absence of yield_now(). That is the whole point.
    }
}

/// The address this instruction is executing at. `auipc rd, 0` puts the
/// current PC into a register -- the most direct possible answer to "where am
/// I actually running?"
fn here() -> usize {
    let pc: usize;
    unsafe { core::arch::asm!("auipc {}, 0", out(reg) pc) };
    pc
}

/// Physical address of UART0 on the QEMU virt board.
const UART0_PHYS: usize = 0x1000_0000;

/// Where putchar currently reaches the UART.
///
/// This is the kernel's one hand-written absolute address, so it is the one
/// thing that breaks when the identity map is removed. It starts as the
/// physical address and is bumped to the higher-half alias once the kernel
/// has relocated.
static UART_BASE: AtomicUsize = AtomicUsize::new(UART0_PHYS);

fn putchar(c: u8) {
    if c == b'\n' {
        putchar(b'\r');
    }

    let base = UART_BASE.load(Ordering::Relaxed) as *mut u8;
    unsafe {
        core::ptr::write_volatile(base, c);
    }
}

fn puts(s: &str) {
    for c in s.bytes() {
        putchar(c);
    }
}

struct Uart;

impl Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        puts(s);
        Ok(())
    }
}

fn _print(args: fmt::Arguments) {
    let mut uart = Uart;
    let _ = uart.write_fmt(args);
}

/// Timer ticks per second on the QEMU virt board. Straight from the OpenSBI
/// banner: "Platform Timer Device : aclint-mtimer @ 10000000Hz".
const TIMER_HZ: u64 = 10_000_000;

/// How often the timer fires. This is POLICY -- ours to choose -- unlike
/// TIMER_HZ above, which is a fact about the hardware. 100 Hz matches Linux's
/// typical HZ. At milestone 9 this becomes the scheduler quantum.
const TICK_INTERVAL: u64 = TIMER_HZ / 100;

/// Read the `time` CSR -- a counter incrementing at TIMER_HZ.
fn now() -> u64 {
    let t: u64;
    unsafe {
        core::arch::asm!("csrr {}, time", out(reg) t);
    }
    t
}

/// Ask OpenSBI to raise a timer interrupt at `when`.
///
/// The timer compare register is M-mode only, so S-mode cannot write it
/// directly. `ecall` traps up to the firmware, which does it on our behalf.
///
/// SBI calling convention: a7 = extension id, a6 = function id, a0.. = args.
/// 0x54494D45 is the ASCII bytes "TIME". This is the same mechanism user
/// programs will use to call into us at milestone 10, one privilege level
/// further down.
fn sbi_set_timer(when: u64) {
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") 0x5449_4D45_usize,
            in("a6") 0_usize,
            in("a0") when,
            out("a1") _,
        );
    }
}

// ===========================================================================
// Paging -- Sv39
//
// A page table is one 4096-byte frame holding 512 entries of 8 bytes. An
// address is chopped into three 9-bit slot numbers plus a 12-bit offset, and
// the MMU walks up to three tables to find the physical page. The offset is
// never translated.
//
// Entry layout:
//
//   63       54 53                          10  9 8  7 6 5 4 3 2 1 0
//   [ reserved ][   PPN  -- 44 bits          ][RSW][D][A][G][U][X][W][R][V]
//
// R/W/X all zero means the entry is a BRANCH -- keep walking. Any of them set
// makes it a LEAF, and the walk stops. A leaf may appear at any level: at
// level 0 it maps 4 KiB, at level 1 2 MiB, at level 2 a full 1 GiB.
// ===========================================================================

const PTE_V: usize = 1 << 0; // valid
const PTE_R: usize = 1 << 1; // readable
const PTE_W: usize = 1 << 2; // writable
const PTE_X: usize = 1 << 3; // executable
const PTE_A: usize = 1 << 6; // accessed  (hardware sets this)
const PTE_D: usize = 1 << 7; // dirty     (hardware sets this)

// Bits 8-9 are RSW: "reserved for software". The spec guarantees the hardware
// ignores them entirely, which makes them two free bits on every single page.
//
// Spent here on recording WHY a page was mapped. When a fault arrives with
// nothing but an address, `probe()` can answer "that's device MMIO" or "that's
// kernel text" instead of leaving you to grep the source for the range.
//
// Real kernels use these for things like marking copy-on-write pages, which
// is where these will probably end up eventually.
const RSW_SHIFT: usize = 8;
#[allow(dead_code)] // the zero tag; here for completeness
const RSW_NONE: usize = 0 << RSW_SHIFT;
const RSW_TEXT: usize = 1 << RSW_SHIFT;
const RSW_DEV: usize = 2 << RSW_SHIFT;
const RSW_DATA: usize = 3 << RSW_SHIFT;

fn rsw_name(pte: usize) -> &'static str {
    match pte & (3 << RSW_SHIFT) {
        RSW_TEXT => "kernel text",
        RSW_DEV => "device mmio",
        RSW_DATA => "data/heap",
        _ => "untagged",
    }
}

/// Walk the page table by hand for one virtual address -- exactly what the MMU
/// does, but visible. Returns the leaf entry, or None if nothing is mapped.
///
/// This is the tool for "why did that address fault?", and the reason the RSW
/// tags above are worth spending.
fn probe(root_pa: usize, addr: usize) -> Option<usize> {
    let mut table = va(root_pa) as *const usize;
    for level in [2_usize, 1, 0] {
        let idx = (addr >> (12 + 9 * level)) & 0x1ff;
        let entry = unsafe { *table.add(idx) };
        if entry & PTE_V == 0 {
            return None;
        }
        // A leaf: any of R/W/X set. Branches have all three clear.
        if entry & (PTE_R | PTE_W | PTE_X) != 0 {
            return Some(entry);
        }
        table = va(pte_to_pa(entry)) as *const usize;
    }
    None
}

/// Print what a virtual address resolves to, and why it exists.
fn explain(root_pa: usize, addr: usize) {
    match probe(root_pa, addr) {
        None => println!("  {:#018x} -> UNMAPPED", addr),
        Some(e) => println!(
            "  {:#018x} -> {:#012x}  {}{}{}  {}",
            addr,
            pte_to_pa(e) | (addr & (PAGE_SIZE - 1)),
            if e & PTE_R != 0 { 'r' } else { '-' },
            if e & PTE_W != 0 { 'w' } else { '-' },
            if e & PTE_X != 0 { 'x' } else { '-' },
            rsw_name(e),
        ),
    }
}

/// Build an entry pointing at physical address `pa`.
///
/// The stored field is a page NUMBER, not an address: shift off the low 12
/// bits (always zero for an aligned page), then shift into position at bit 10.
fn pte(pa: usize, flags: usize) -> usize {
    ((pa >> 12) << 10) | flags
}

/// Base of the high half of the address space.
///
/// Sv39 uses 39-bit addresses in 64-bit registers, and requires bits 63..39 to
/// all copy bit 38 -- sign extension. So only two ranges are legal:
///
///   0x0000_0000_0000_0000 .. 0x0000_003F_FFFF_FFFF   low half  (user)
///   0xFFFF_FFC0_0000_0000 .. 0xFFFF_FFFF_FFFF_FFFF   high half (kernel)
///
/// with an enormous invalid hole between them. In the root table that lands
/// exactly on the halfway line: slots 0..255 are the low half, 256..511 the
/// high half. The kernel lives in the top half at the SAME virtual addresses
/// in every process, so creating a process page table later is just copying
/// those few root entries.
const HIGH_BASE: usize = 0xFFFF_FFC0_0000_0000;

/// Physical address of the root page table. Stored as a plain number, so it
/// stays physical no matter which alias the kernel is executing from.
static ROOT: AtomicUsize = AtomicUsize::new(0);

/// physical -> virtual, and back. Linux calls these __va and __pa.
///
/// These matter once the kernel runs high: `frame_alloc` returns PHYSICAL
/// addresses, and after the identity map is gone a physical address is not
/// directly usable. Anything the allocator hands out must go through `va`
/// before it is written to.
///
/// Note the asymmetry with linker symbols: `&some_symbol` is materialised
/// PC-relative, so it already yields whichever alias the kernel is currently
/// executing from, and must NOT be adjusted.
fn va(pa: usize) -> usize {
    pa.wrapping_add(HIGH_BASE)
}

#[allow(dead_code)]
fn pa(va: usize) -> usize {
    va.wrapping_sub(HIGH_BASE)
}

/// Physical address a page table entry points at -- the reverse of `pte`.
fn pte_to_pa(e: usize) -> usize {
    ((e >> 10) & 0xfff_ffff_ffff) << 12
}

/// Frames consumed by page tables themselves, so the cost is visible.
static PT_FRAMES: AtomicUsize = AtomicUsize::new(0);

/// Map one 4 KiB virtual page to one physical page, creating whatever
/// intermediate tables are missing along the way.
///
/// This is the same walk the hardware does, with one difference: where the
/// hardware gives up on an empty entry and faults, this builds the missing
/// table and keeps descending.
fn map(root: *mut usize, va: usize, pa: usize, flags: usize) {
    let mut table = root;

    // Levels 2 then 1. The index is just a 9-bit slice of the address:
    //   level 2 -> bits 38..30      level 1 -> bits 29..21
    for level in [2_usize, 1_usize] {
        let idx = (va >> (12 + 9 * level)) & 0x1ff;
        let slot = unsafe { table.add(idx) };
        let mut entry = unsafe { *slot };

        if entry & PTE_V == 0 {
            // Empty. Build the missing table: allocate, ERASE, link.
            //
            // The erase is not optional -- a frame off the free list still
            // holds its free-list link, and the hardware would follow that
            // as a real entry.
            let next = frame_alloc().expect("out of frames while building page tables");
            unsafe { core::ptr::write_bytes(next, 0, PAGE_SIZE) };
            PT_FRAMES.fetch_add(1, Ordering::Relaxed);

            // A BRANCH: valid, but R/W/X all clear. That combination is what
            // tells the hardware to keep walking rather than stop here.
            entry = pte(next as usize, PTE_V);
            unsafe { *slot = entry };
        }

        table = pte_to_pa(entry) as *mut usize;
    }

    // Level 0: the leaf. Setting any of R/W/X ends the walk.
    //
    // A and D are pre-set because not every implementation updates them in
    // hardware; on those, an unset bit faults on first touch.
    let idx = (va >> 12) & 0x1ff;
    unsafe { *table.add(idx) = pte(pa, flags | PTE_V | PTE_A | PTE_D) };
}

/// Map every page of physical `[start, end)` at virtual address
/// `offset + physical`. Returns the page count.
///
/// `offset == 0` gives an identity map. `offset == HIGH_BASE` gives the
/// higher-half alias of the same physical memory.
fn map_range(root: *mut usize, start: usize, end: usize, offset: usize, flags: usize) -> usize {
    let mut pa = start & !(PAGE_SIZE - 1);
    let end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let mut pages = 0;
    while pa < end {
        map(root, offset.wrapping_add(pa), pa, flags);
        pa += PAGE_SIZE;
        pages += 1;
    }
    pages
}

/// Build the kernel page table out of 4 KiB pages and switch the MMU on.
///
/// Still an identity map -- virtual X is physical X -- but now each region
/// carries only the permissions it actually needs, so W^X holds: no page is
/// both writable and executable.
fn paging_init(ram_end: usize) {
    let root = frame_alloc().expect("no frame for the root page table") as *mut usize;
    unsafe { core::ptr::write_bytes(root as *mut u8, 0, PAGE_SIZE) };
    PT_FRAMES.fetch_add(1, Ordering::Relaxed);

    // Remembered as a bare physical number so the relocated kernel can still
    // find it after the identity map is gone.
    ROOT.store(root as usize, Ordering::Relaxed);

    let sym = |s: &u8| s as *const u8 as usize;
    let (text_start, text_end, rodata_start, rodata_end, data_start, kernel_end) = unsafe {
        (
            sym(&__text_start),
            sym(&__text_end),
            sym(&__rodata_start),
            sym(&__rodata_end),
            sym(&__data_start),
            sym(&__kernel_end),
        )
    };

    // --- the identity map, still what the kernel actually runs on ---

    // Devices. Read/write, never executable.
    map_range(root, 0x1000_0000, 0x1000_1000, 0, PTE_R | PTE_W | RSW_DEV); // UART0
    map_range(root, 0x0200_0000, 0x0201_0000, 0, PTE_R | PTE_W | RSW_DEV); // CLINT
    map_range(root, 0x0c00_0000, 0x0c60_0000, 0, PTE_R | PTE_W | RSW_DEV); // PLIC

    // Kernel code: executable, NOT writable.
    map_range(root, text_start, text_end, 0, PTE_R | PTE_X | RSW_TEXT);
    // Constants and string literals: read only. Not writable, not executable.
    map_range(root, rodata_start, rodata_end, 0, PTE_R | RSW_DATA);
    // Globals, .bss and the boot stack: writable, NOT executable.
    map_range(root, data_start, kernel_end, 0, PTE_R | PTE_W | RSW_DATA);
    // Everything the frame allocator hands out: writable, not executable.
    map_range(root, kernel_end, ram_end, 0, PTE_R | PTE_W | RSW_DATA);

    // --- the higher-half alias of the same physical memory ---
    //
    // Every physical address P is now ALSO reachable at HIGH_BASE + P, with
    // the same permissions. Two virtual addresses, one physical page.
    //
    // This is a "direct map", and it is worth more than just being a stepping
    // stone to relocating the kernel: it means the kernel can reach ANY
    // physical page by adding a constant. At milestone 11, editing another
    // process's page tables becomes arithmetic instead of a special case.
    map_range(
        root,
        0x1000_0000,
        0x1000_1000,
        HIGH_BASE,
        PTE_R | PTE_W | RSW_DEV,
    );
    map_range(
        root,
        0x0200_0000,
        0x0201_0000,
        HIGH_BASE,
        PTE_R | PTE_W | RSW_DEV,
    );
    map_range(
        root,
        0x0c00_0000,
        0x0c60_0000,
        HIGH_BASE,
        PTE_R | PTE_W | RSW_DEV,
    );
    map_range(
        root,
        text_start,
        text_end,
        HIGH_BASE,
        PTE_R | PTE_X | RSW_TEXT,
    );
    map_range(root, rodata_start, rodata_end, HIGH_BASE, PTE_R | RSW_DATA);
    map_range(
        root,
        data_start,
        kernel_end,
        HIGH_BASE,
        PTE_R | PTE_W | RSW_DATA,
    );
    map_range(
        root,
        kernel_end,
        ram_end,
        HIGH_BASE,
        PTE_R | PTE_W | RSW_DATA,
    );

    println!("  text   {:#x}..{:#x}  R-X", text_start, text_end);
    println!("  rodata {:#x}..{:#x}  R--", rodata_start, rodata_end);
    println!("  data   {:#x}..{:#x}  RW-", data_start, kernel_end);
    println!("  heap   {:#x}..{:#x}  RW-", kernel_end, ram_end);

    // satp: mode 8 (Sv39) in the top 4 bits, root table's page number below.
    let satp = (8_usize << 60) | ((root as usize) >> 12);

    // sfence.vma flushes the MMU's cached translations (the TLB). Required
    // after changing satp, or the hardware may keep using stale answers.
    //
    // The instruction AFTER the csrw is already fetched through the MMU. It
    // only survives because this mapping is an identity map.
    unsafe {
        core::arch::asm!(
            "sfence.vma",
            "csrw satp, {}",
            "sfence.vma",
            in(reg) satp,
        );
    }

    // Read satp back. An identity map is invisible by construction, so the
    // kernel surviving proves nothing on its own -- a silently ignored write
    // would look identical. This confirms the mode field actually took.
    let readback: usize;
    unsafe { core::arch::asm!("csrr {}, satp", out(reg) readback) };
    println!(
        "paging: satp = {:#x}  mode={} (8 = Sv39)  root = {:#x}",
        readback,
        readback >> 60,
        (readback & 0xfff_ffff_ffff) << 12
    );
    let pt = PT_FRAMES.load(Ordering::Relaxed);
    println!(
        "paging: {} frames of page tables ({} KiB) to describe it all",
        pt,
        pt * PAGE_SIZE / 1024
    );
}

// ===========================================================================
// Kernel heap -- 7a, the bump allocator
//
// The frame allocator hands out 4096 bytes, always. This hands out any size.
//
// A bump allocator is the simplest thing that can possibly work: one pointer
// that only ever moves forward. Allocating means rounding up to the required
// alignment and advancing. Freeing does nothing at all.
//
// Using the curb picture: this parks vehicles nose to tail down the street and
// NEVER lets anyone leave. It works perfectly until the street ends, and then
// it is over -- no gaps are ever reclaimed, because gaps are never created.
//
// That is deliberate. It makes Vec and Box work in ~30 lines, and its single
// flaw is exactly the thing 7b exists to fix.
// ===========================================================================

/// How much RAM to reserve for the heap, carved off the top before the frame
/// allocator claims anything.
const HEAP_SIZE: usize = 1024 * 1024; // 1 MiB

/// Everything in the heap is rounded to this. Keeps every block address a
/// multiple of 16, which satisfies the alignment of anything normal.
const HEAP_ALIGN: usize = 16;

/// Written into every FREE block's header. If a block turns up on the list
/// without it, something wrote past the end of its allocation and trampled
/// the neighbour -- catching that here beats discovering it a thousand
/// allocations later.
///
/// 0x5EBB1E spells SEBBIE. (0xDEBB1E is still unspent.)
const BLOCK_MAGIC: usize = 0x5EBB1E;

/// Header sitting at the start of every FREE block.
///
/// Allocated blocks carry NO header at all: Rust hands the size back in
/// `Layout` on dealloc, so unlike C there is nothing to hide in front of the
/// pointer. Only gaps need signs, and gaps have room for them -- exactly the
/// trick the frame allocator uses, plus a length.
#[repr(C)]
struct FreeBlock {
    magic: usize,
    size: usize, // total bytes in this block, header included
    next: usize, // address of the next free block, 0 = end of list
}

/// The smallest block worth tracking: anything under this cannot hold its own
/// sign, so it can never go back on the list.
const MIN_BLOCK: usize = core::mem::size_of::<FreeBlock>();

/// Head of the free list, kept sorted by ADDRESS so neighbouring gaps end up
/// adjacent in the list and coalescing is a comparison rather than a search.
static FREE_HEAD: AtomicUsize = AtomicUsize::new(0);

fn align_up(x: usize, a: usize) -> usize {
    (x + a - 1) & !(a - 1)
}

/// Turn the reserved region into one enormous free block.
fn heap_init(start: usize, size: usize) {
    unsafe {
        let blk = start as *mut FreeBlock;
        (*blk).magic = BLOCK_MAGIC;
        (*blk).size = size;
        (*blk).next = 0;
    }
    FREE_HEAD.store(start, Ordering::Relaxed);
}

/// Free blocks and free bytes. Watching the block COUNT is how you see
/// coalescing working -- without it the count only ever climbs.
fn heap_stats() -> (usize, usize) {
    let mut blocks = 0;
    let mut bytes = 0;
    let mut cur = FREE_HEAD.load(Ordering::Relaxed);
    while cur != 0 {
        let blk = cur as *const FreeBlock;
        unsafe {
            blocks += 1;
            bytes += (*blk).size;
            cur = (*blk).next;
        }
    }
    (blocks, bytes)
}

struct Heap;

unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // Every block is 16-aligned, so anything asking for more than that
        // would need front-padding this allocator does not do. Nothing normal
        // does -- Box, Vec, BTreeMap all want 8 or less.
        if layout.align() > HEAP_ALIGN {
            return core::ptr::null_mut();
        }

        // A block must be able to hold a sign again once it is freed, so
        // nothing smaller than a header is ever handed out.
        let want = align_up(layout.size().max(MIN_BLOCK), HEAP_ALIGN);

        // First fit: walk the curb, take the first gap long enough.
        let mut prev = 0usize;
        let mut cur = FREE_HEAD.load(Ordering::Relaxed);

        while cur != 0 {
            let blk = cur as *mut FreeBlock;
            assert!(
                (*blk).magic == BLOCK_MAGIC,
                "heap corruption: free block at {:#x} has magic {:#x}, expected {:#x} -- something wrote past the end of its allocation",
                cur,
                (*blk).magic,
                BLOCK_MAGIC
            );

            let have = (*blk).size;
            if have >= want {
                let next = (*blk).next;

                let replacement = if have - want >= MIN_BLOCK {
                    // Big enough to split: the tail stays free.
                    let tail = cur + want;
                    let t = tail as *mut FreeBlock;
                    (*t).magic = BLOCK_MAGIC;
                    (*t).size = have - want;
                    (*t).next = next;
                    tail
                } else {
                    // The leftover could not hold its own sign, so hand out
                    // the whole block. Those few bytes are lost until this
                    // block is freed -- that is INTERNAL fragmentation.
                    next
                };

                if prev == 0 {
                    FREE_HEAD.store(replacement, Ordering::Relaxed);
                } else {
                    (*(prev as *mut FreeBlock)).next = replacement;
                }

                return cur as *mut u8;
            }

            prev = cur;
            cur = (*blk).next;
        }

        core::ptr::null_mut() // no gap long enough
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let size = align_up(layout.size().max(MIN_BLOCK), HEAP_ALIGN);
        let addr = ptr as usize;

        // Find where this block belongs in address order.
        let mut prev = 0usize;
        let mut cur = FREE_HEAD.load(Ordering::Relaxed);
        while cur != 0 && cur < addr {
            prev = cur;
            cur = (*(cur as *mut FreeBlock)).next;
        }

        // Write its sign and link it in.
        let blk = addr as *mut FreeBlock;
        (*blk).magic = BLOCK_MAGIC;
        (*blk).size = size;
        (*blk).next = cur;

        if prev == 0 {
            FREE_HEAD.store(addr, Ordering::Relaxed);
        } else {
            (*(prev as *mut FreeBlock)).next = addr;
        }

        // Coalesce forward: if the next gap starts exactly where this one
        // ends, they are one gap.
        if cur != 0 && addr + size == cur {
            let n = cur as *mut FreeBlock;
            (*blk).size += (*n).size;
            (*blk).next = (*n).next;
        }

        // Coalesce backward, same test from the other side. Without BOTH
        // directions the curb still turns to dust, just more slowly.
        if prev != 0 {
            let p = prev as *mut FreeBlock;
            if prev + (*p).size == addr {
                (*p).size += (*blk).size;
                (*p).next = (*blk).next;
            }
        }
    }
}

#[global_allocator]
static ALLOCATOR: Heap = Heap;

// ===========================================================================
// Threads -- milestone 8
//
// A thread is a saved copy of the registers plus a stack. That is the whole
// definition; the CPU has no idea threads exist. One worker, one desk, and a
// stack of cardboard boxes each holding a half-finished job.
//
// Switching is: sweep the desk into box A, lay out box B exactly as it was,
// carry on. The worker never notices.
//
// Cooperative for now -- a thread runs until it CHOOSES to yield. Milestone 9
// hands the timer the power to take the desk away by force.
// ===========================================================================

/// Per-thread stack. 16 KiB is roomy for println!'s formatting depth without
/// being so large that an overflow bug can hide for long.
const STACK_SIZE: usize = 16 * 1024;

/// The 14 registers that survive a function call, which is exactly the set a
/// context switch must preserve.
///
/// #[repr(C)] is load-bearing: `switch` in entry.S indexes this by hand at
/// fixed offsets, and Rust is otherwise free to reorder fields.
#[repr(C)]
#[derive(Clone, Copy)]
struct Context {
    ra: usize,      // offset 0  -- where `ret` will jump
    sp: usize,      // offset 8
    s: [usize; 12], // offset 16 -- s0..s11
}

impl Context {
    const fn zeroed() -> Self {
        Context {
            ra: 0,
            sp: 0,
            s: [0; 12],
        }
    }
}

extern "C" {
    /// Save the current registers into `old`, load `new`, and return into
    /// whatever thread `new` belongs to.
    fn switch(old: *mut Context, new: *const Context);

    /// Where a new thread begins: enables interrupts, then jumps to the entry
    /// function held in s0. See entry.S for why the indirection is required.
    fn thread_start();
}

// ---------------------------------------------------------------------------
// Spinlocks
//
// One sign on one hook. To hang it you must take it off, and taking it is a
// single indivisible motion -- `swap` writes "taken" and hands back whatever
// was there, so there is no gap between looking and claiming for anyone to
// slip into.
//
// Holding one also disables interrupts, and that is not an optimisation. The
// timer does not respect the sign: it would tap mid-critical-section, the
// handler would reach for a sign the interrupted code is still holding, and
// spin forever waiting for a thread that cannot run. One core, one lock,
// nobody at fault.
//
// Release restores the PREVIOUS interrupt state rather than blindly enabling,
// because the lock may well have been taken somewhere they were already off.
// ---------------------------------------------------------------------------

/// A lock that guards a value, rather than sitting next to one. Reaching the
/// data requires holding the lock, so forgetting to lock is a compile error
/// instead of a race.
struct SpinLock<T> {
    locked: core::sync::atomic::AtomicBool,
    data: core::cell::UnsafeCell<T>,
}

// Safe because the lock is what provides the exclusion.
unsafe impl<T: Send> Sync for SpinLock<T> {}

impl<T> SpinLock<T> {
    const fn new(data: T) -> Self {
        SpinLock {
            locked: core::sync::atomic::AtomicBool::new(false),
            data: core::cell::UnsafeCell::new(data),
        }
    }

    fn lock(&self) -> SpinGuard<'_, T> {
        // Interrupts OFF first, then take the lock. The other order leaves a
        // window where the timer can land on a thread that already holds it.
        let intr_was_on = intr_off();

        // swap always writes `true` and reports what was there. Got `false`
        // back and the sign is yours; got `true` and someone else has it.
        while self.locked.swap(true, Ordering::Acquire) {
            core::hint::spin_loop();
        }

        SpinGuard {
            lock: self,
            intr_was_on,
        }
    }
}

/// Proof that the lock is held. Releasing happens in `Drop`, so it is not
/// possible to forget -- the same trick the `Frame` type will use.
struct SpinGuard<'a, T> {
    lock: &'a SpinLock<T>,
    intr_was_on: bool,
}

impl<T> core::ops::Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> core::ops::DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        // Release BEFORE restoring interrupts. The other order lets a timer
        // land while the lock is still held by code that is done with it.
        self.lock.locked.store(false, Ordering::Release);
        if self.intr_was_on {
            intr_on();
        }
    }
}

/// Turn interrupts off, reporting whether they had been on.
///
/// `csrrci` reads sstatus and clears bit 1 (SIE) in a single instruction, so
/// there is no window where an interrupt could land between the read and the
/// write.
fn intr_off() -> bool {
    let old: usize;
    unsafe { core::arch::asm!("csrrci {}, sstatus, 2", out(reg) old) };
    old & 2 != 0
}

/// Turn interrupts back on.
fn intr_on() {
    unsafe { core::arch::asm!("csrsi sstatus, 2") };
}

struct Thread {
    ctx: Context,
    name: &'static str,
    /// Owns the thread's stack. Dropping the Thread frees it -- the heap from
    /// milestone 7 doing real work.
    _stack: alloc::vec::Vec<u8>,
}

/// The thread table, behind a lock. `push` is several stores -- bump the
/// length, write the element -- and the timer landing between them would hand
/// the scheduler an element that does not exist yet.
///
/// `Box<Thread>`, not `Thread`, and that is load-bearing. `yield_now` hands
/// `switch` raw pointers to contexts, and a suspended thread's context must
/// stay put while it is switched away. If the threads lived inline, growing
/// this Vec would reallocate and move every context out from under those
/// pointers -- which is exactly what happened the first time this was written
/// as `Vec<Thread>`: spawning the fifth thread grew it past capacity 4 and the
/// suspended threads' contexts became freed memory.
///
/// Boxing means the Vec of pointers can move freely while the threads
/// themselves never do.
static THREADS: SpinLock<alloc::vec::Vec<alloc::boxed::Box<Thread>>> =
    SpinLock::new(alloc::vec::Vec::new());
static CURRENT: AtomicUsize = AtomicUsize::new(0);

/// Register the currently-executing code as thread 0.
///
/// Its context is left zeroed on purpose: nothing needs to be restored to get
/// back here, because the first `switch` away will fill it in.
fn threads_init() {
    THREADS.lock().push(alloc::boxed::Box::new(Thread {
        ctx: Context::zeroed(),
        name: "main",
        _stack: alloc::vec::Vec::new(), // main runs on the boot stack
    }));
}

/// Create a thread that will begin at `entry` the first time it is scheduled.
///
/// The trick: build a context whose `ra` points at `entry` and whose `sp`
/// points at a fresh stack. When `switch` runs its final `ret`, it "returns"
/// into a function that has never been called. Thread creation is a forged
/// return address.
fn thread_spawn(name: &'static str, entry: extern "C" fn() -> !) {
    let mut stack = alloc::vec![0u8; STACK_SIZE];

    // Stacks grow DOWN, so the pointer starts at the high end. 16-aligned
    // because the ABI requires it.
    let top = (stack.as_mut_ptr() as usize + STACK_SIZE) & !15;

    let mut ctx = Context::zeroed();
    // Start at the trampoline, not the entry function -- it enables interrupts
    // first. The real destination rides in s0, which `switch` restores.
    ctx.ra = thread_start as *const () as usize;
    ctx.sp = top;
    ctx.s[0] = entry as *const () as usize;

    THREADS.lock().push(alloc::boxed::Box::new(Thread {
        ctx,
        name,
        _stack: stack,
    }));
}

/// Give up the CPU to the next thread, round robin.
///
/// Called two ways now: voluntarily by a thread, and involuntarily from the
/// timer's trap handler. Interrupts are off for the switch itself either way --
/// a timer landing halfway through swapping contexts would be reading a thread
/// that is neither running nor saved.
///
/// This is a lock in everything but name, and 9b replaces it with a real one.
fn yield_now() {
    // Interrupts stay off for the WHOLE operation, the switch included. A
    // timer landing midway through swapping contexts would find a thread that
    // is neither running nor fully saved.
    let was_on = intr_off();

    let (old, new) = {
        let mut threads = THREADS.lock();
        if threads.len() < 2 {
            drop(threads);
            if was_on {
                intr_on();
            }
            return;
        }

        let cur = CURRENT.load(Ordering::Relaxed);
        let next = (cur + 1) % threads.len();
        CURRENT.store(next, Ordering::Relaxed);

        // Raw pointers, never references: `switch` writes through one while
        // reading the other, and two live &mut would be instant undefined
        // behaviour. Two SHARED borrows are fine, so both are taken as
        // `addr_of!` and one is cast.
        //
        // These are safe to hold across the switch only because the threads
        // are boxed -- see the note on THREADS.
        (
            core::ptr::addr_of!(threads[cur].ctx) as *mut Context,
            core::ptr::addr_of!(threads[next].ctx),
        )
    };
    // Lock released here. Interrupts stay off, because they were already off
    // when the guard was taken and Drop restores what it found.

    unsafe { switch(old, new) };

    // Execution resumes HERE, but possibly minutes later and after several
    // other threads have run. Nothing local survived except what was in
    // callee-saved registers or on this thread's own stack.
    if was_on {
        intr_on();
    }
}

fn current_name() -> &'static str {
    THREADS.lock()[CURRENT.load(Ordering::Relaxed)].name
}

/// A counter two threads fight over, to demonstrate the lock doing its job.
static COUNTER: SpinLock<u64> = SpinLock::new(0);

/// How many times each racer increments it.
const RACE_ITERS: u64 = 20_000;

/// Set when a racer finishes, so main knows when to read the result.
static RACERS_DONE: AtomicUsize = AtomicUsize::new(0);

extern "C" fn thread_racer() -> ! {
    for _ in 0..RACE_ITERS {
        // read-modify-write, entirely inside the lock. Without it, the timer
        // can land between the read and the write and an update vanishes.
        *COUNTER.lock() += 1;
    }
    RACERS_DONE.fetch_add(1, Ordering::Relaxed);
    loop {
        yield_now();
    }
}

// ===========================================================================
// Device tree (FDT / "flattened device tree")
//
// OpenSBI leaves a pointer to this blob in a1 at boot. It describes the whole
// machine: how much RAM, where the UART is, which interrupt controller exists.
// Every magic address hardcoded elsewhere in this file is really described
// here, and eventually should be read from here instead.
//
// The layout is a 40-byte header followed by a flat stream of 4-byte tokens:
//
//   BEGIN_NODE (1)  followed by a NUL-terminated node name
//   END_NODE   (2)
//   PROP       (3)  followed by: data length, name offset, then the data
//   NOP        (4)  skip
//   END        (9)  end of the stream
//
// Property NAMES live in a separate strings block at the end of the blob; PROP
// stores an offset into it, because names like "reg" repeat constantly.
//
// Two things that will bite: every integer is BIG-endian (RISC-V is little),
// and every name and data blob is padded up to a 4-byte boundary.
// ===========================================================================

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// Read a big-endian u32. `from_be_bytes` does the byte reversal for us.
unsafe fn be32(p: *const u8) -> u32 {
    u32::from_be_bytes([*p, *p.add(1), *p.add(2), *p.add(3)])
}

/// Read a big-endian u64.
unsafe fn be64(p: *const u8) -> u64 {
    let mut v = 0u64;
    for i in 0..8 {
        v = (v << 8) | *p.add(i) as u64;
    }
    v
}

/// Length of a NUL-terminated string, not counting the NUL.
unsafe fn cstr_len(p: *const u8) -> usize {
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    n
}

/// Does the NUL-terminated string at `p` begin with `s`?
unsafe fn cstr_starts_with(p: *const u8, s: &str) -> bool {
    s.as_bytes()
        .iter()
        .enumerate()
        .all(|(i, &b)| *p.add(i) == b)
}

/// Is the NUL-terminated string at `p` exactly `s`?
unsafe fn cstr_eq(p: *const u8, s: &str) -> bool {
    cstr_starts_with(p, s) && *p.add(s.len()) == 0
}

/// Walk the device tree looking for the `memory` node, and return its
/// (base address, size) from the `reg` property.
///
/// Returns None if the blob is not a device tree or has no memory node.
fn dtb_memory(dtb: *const u8) -> Option<(usize, usize)> {
    unsafe {
        if be32(dtb) != FDT_MAGIC {
            return None;
        }

        let off_struct = be32(dtb.add(8)) as usize; // where the token stream starts
        let off_strings = be32(dtb.add(12)) as usize; // where property names live

        let mut p = dtb.add(off_struct);
        let mut in_memory = false;

        loop {
            let token = be32(p);
            p = p.add(4);

            match token {
                FDT_BEGIN_NODE => {
                    // Nodes are named like "memory@80000000" -- the part after
                    // the @ is the address, so match on the prefix.
                    in_memory = cstr_starts_with(p, "memory@");
                    // Skip the name, rounded up to a 4-byte boundary. Same
                    // trick as page alignment, one size down.
                    let n = cstr_len(p) + 1;
                    p = p.add((n + 3) & !3);
                }

                FDT_END_NODE => in_memory = false,

                FDT_PROP => {
                    let len = be32(p) as usize;
                    let nameoff = be32(p.add(4)) as usize;
                    p = p.add(8);

                    let name = dtb.add(off_strings + nameoff);
                    if in_memory && cstr_eq(name, "reg") && len >= 16 {
                        // reg = <base_hi base_lo size_hi size_lo>, i.e. two
                        // big-endian 64-bit values back to back.
                        return Some((be64(p) as usize, be64(p.add(8)) as usize));
                    }

                    p = p.add((len + 3) & !3);
                }

                FDT_NOP => {}

                FDT_END => return None, // walked the whole tree, no memory node
                _ => return None,       // malformed blob
            }
        }
    }
}

// ===========================================================================
// Physical frame allocator
//
// Hands out 4096-byte pages of physical memory. 4096 because that is the page
// size of Sv39, the paging scheme milestone 6 uses -- frames of any other size
// or alignment would be useless for page tables.
//
// The free list is threaded through the free pages themselves: the first 8
// bytes of every free page hold the address of the next free page. Tracking
// ~31,000 pages therefore costs zero bytes of dedicated memory. Once a page is
// handed out the caller overwrites that field, which is fine -- the link only
// has meaning while the page sits on the list.
// ===========================================================================

const PAGE_SIZE: usize = 4096;

extern "C" {
    /// Defined by linker.ld. There is no data at any of these -- the symbol's
    /// ADDRESS is the value we want, which is why the code takes `&sym`
    /// rather than reading it. Each is 4096-aligned so that regions with
    /// different permissions never share a page.
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __kernel_end: u8;
}

/// Head of the free list; 0 means empty.
///
/// Atomic only so we can have a mutable global without `static mut`. This is
/// NOT concurrency-safe: alloc reads the head and then writes it as two
/// separate steps, so a timer interrupt landing in between could corrupt the
/// list. Nothing allocates from interrupt context yet. Milestone 9 replaces
/// this with a real lock.
static FREE_LIST: AtomicUsize = AtomicUsize::new(0);

/// Build the free list from every page between the end of the kernel image and
/// `ram_end`. Returns how many pages were added.
fn frame_init(ram_end: usize) -> usize {
    let kernel_end = unsafe { &__kernel_end as *const u8 as usize };

    // Round up to a page boundary. `PAGE_SIZE - 1` is 0xFFF, so `!(PAGE_SIZE-1)`
    // is ...FFFFF000 -- ANDing with it clears the low 12 bits, rounding DOWN.
    // Adding 0xFFF first turns that into rounding UP.
    let mut p = (kernel_end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);

    let mut count = 0;
    while p + PAGE_SIZE <= ram_end {
        frame_free(p as *mut u8);
        p += PAGE_SIZE;
        count += 1;
    }
    count
}

/// Take a page off the free list. None when memory is exhausted.
fn frame_alloc() -> Option<*mut u8> {
    let head = FREE_LIST.load(Ordering::Relaxed);
    if head == 0 {
        return None;
    }

    // Read the link the page is storing, and make that the new head.
    let next = unsafe { core::ptr::read(head as *const usize) };
    FREE_LIST.store(next, Ordering::Relaxed);

    Some(head as *mut u8)
}

/// Put a page back on the free list.
fn frame_free(page: *mut u8) {
    let addr = page as usize;
    assert!(
        addr % PAGE_SIZE == 0,
        "frame_free: address not page aligned"
    );

    // Write the current head into this page's first 8 bytes, then point the
    // list at this page. Classic linked-list push.
    let head = FREE_LIST.load(Ordering::Relaxed);
    unsafe { core::ptr::write(addr as *mut usize, head) };
    FREE_LIST.store(addr, Ordering::Relaxed);
}

/// Where Rust goes when something goes wrong. On a hosted system this unwinds
/// and prints a backtrace. Here, nothing exists to catch it.
///
/// Once you have serial output working, come back and print `info` -- it
/// carries the file, line, and message of the panic, and having that visible
/// will save you many hours.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("PANIC! AT THE KERNEL: {}", info);
    loop {
        unsafe { core::arch::asm!("wfi") };
    }
}

#[repr(C)]
pub struct TrapFrame {
    pub x: [usize; 32],
}

#[no_mangle]
extern "C" fn trap_handler(frame: &mut TrapFrame) {
    let scause: usize;
    let sepc: usize;
    let stval: usize;

    unsafe {
        core::arch::asm!("csrr {}, scause", out(reg) scause);
        core::arch::asm!("csrr {}, sepc",   out(reg) sepc);
        core::arch::asm!("csrr {}, stval",  out(reg) stval);
    }

    // scause's top bit: set means interrupt, clear means exception.
    // Reinterpreting as signed makes "top bit set" the same as "negative".
    let is_interrupt = (scause as isize) < 0;
    let code = scause & 0xff;

    if is_interrupt && code == 5 {
        // Supervisor timer. Rearming is not optional and not just about
        // scheduling the next tick -- it is how we ACKNOWLEDGE this one.
        //
        // The timer interrupt is LEVEL-triggered: the condition is
        // `time >= timecmp`, and `time` only counts up, so once it fires the
        // signal stays asserted forever. Returning without pushing timecmp
        // into the future means the CPU re-traps at the very next instruction
        // boundary, forever. Measured: ~165,000 ticks per second instead of 1.
        sbi_set_timer(now() + TICK_INTERVAL);

        // Ticking at 100 Hz, so print once a second rather than flooding the
        // serial line. The counter itself is the kernel's notion of uptime.
        let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
        if n % 100 == 0 {
            println!("tick {} -- up {}s", n, n / 100);
        }

        // PREEMPTION. Up to now the timer only counted; now it takes the CPU
        // away from whatever was running, whether that thread cooperates or
        // not.
        //
        // Switching from inside a trap handler works because the trap frame
        // lives on THIS thread's stack, so it simply rides along in the saved
        // context. When something switches back here later, execution resumes
        // inside yield_now, returns through this handler, and unwinds normally
        // through trap_entry's register restore and sret.
        //
        // Rearm before switching, or the next thread inherits a timer that has
        // already been acknowledged and never fires again.
        yield_now();

        // Deliberately do NOT touch sepc. An interrupt means the instruction
        // at sepc has not run yet -- skipping it would silently drop a good
        // instruction. Only exceptions get stepped over.
        return;
    }

    // An exception. Every one of these is currently a kernel bug -- there is
    // no user mode yet, so nothing faults legitimately.
    //
    // This used to print and then step over the faulting instruction, which
    // was right while deliberately executing `unimp`. Now that page faults are
    // real, silently resuming would let a corrupted kernel stagger on and fail
    // somewhere unrelated. Loud and immediate beats subtle and later.
    let kind = match code {
        0 => "instruction address misaligned",
        1 => "instruction access fault",
        2 => "illegal instruction",
        3 => "breakpoint",
        5 => "load access fault",
        7 => "store access fault",
        8 => "ecall from user mode",
        9 => "ecall from supervisor mode",
        12 => "instruction page fault -- executing a non-executable page",
        13 => "load page fault -- reading an unmapped or unreadable page",
        15 => "store page fault -- writing an unmapped or read-only page",
        _ => "unknown exception",
    };

    println!("*** TRAP ***");
    println!("  {}", kind);
    println!(
        "  scause {:#x}  sepc {:#x}  stval {:#x}",
        scause, sepc, stval
    );
    println!("  ra {:#x}  sp {:#x}", frame.x[1], frame.x[2]);

    panic!("unhandled exception: {}", kind);
}
