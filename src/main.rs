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

/// The first user program, baked into the kernel image.
///
/// Built by the Makefile from the standalone crate in `user/`: linked at
/// 0x1000 in the LOW half, then flattened from ELF to raw bytes with objcopy.
///
/// Embedding is the simplest way to get a second binary into memory when there
/// is no disk driver yet -- xv6 does exactly this with its `initcode`. The
/// disk exists as of milestone 13, but loading a PROGRAM off it still needs an
/// ELF parser and a way to name one without a path. That is milestone 19.
static USER_PROG: &[u8] = include_bytes!("../user/hello.bin");

/// Where user programs are mapped. Not 0, so that a null dereference faults
/// rather than silently reading whatever sits at address zero. Must match
/// `. = 0x1000` in user/user.ld.
const USER_BASE: usize = 0x1000;

/// Printed at boot, directly under OpenSBI's banner and deliberately in the
/// same style.
///
/// A raw string (`r"..."`) so the backslashes stay backslashes instead of
/// being read as escape sequences.
///
/// Legal note for the confused: LeBOS is also a French womenswear label. This
/// is the other one.
/// The boot banner, generated from the logo by `toascii.py` and embedded
/// verbatim.
///
/// It is in colour, which costs nothing: ANSI escape codes are the interface
/// every terminal has understood since the 1970s, and QEMU's `-nographic`
/// hands the serial line straight to the real one. So the kernel renders its
/// own logo, in 24-bit colour, three milestones before it owns a single pixel.
///
/// An escape is only emitted where the colour CHANGES -- otherwise each
/// character carries 19 bytes of preamble and the banner outweighs the frame
/// allocator.
const BANNER: &str = include_str!("banner.txt");

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

    // sscratch = 0 means "currently in the kernel". trap_entry relies on this
    // to tell a user trap from a kernel one; a nonzero value here would make it
    // treat the very first kernel trap as if it came from userspace.
    unsafe { core::arch::asm!("csrw sscratch, zero") };

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

    // Milestone 13: persistence.
    if virtio_blk_init() {
        println!(
            "disk: virtio-blk at {:#x}",
            BLK_BASE.load(Ordering::Relaxed)
        );

        // Is there already a LeBOS store on this disk?
        match store_load() {
            Some((b, o, c)) => println!(
                "disk: LOADED an existing store -- {} blobs, {} objects, {} claims",
                b, o, c
            ),
            None => println!("disk: no LeBOS store found, this disk is new"),
        }

        store_demo();

        if store_save() {
            println!(
                "disk: saved {} objects. reboot and they will still be here.",
                STORE.lock().len()
            );
        } else {
            println!("disk: SAVE FAILED");
        }
    } else {
        println!("disk: no virtio-blk device found");
        store_demo();
    }

    // 10a: a second binary now exists inside the kernel image. It is not
    // mapped -- this print happens before any address space exists. User mode
    // arrived at milestone 10b; this line only proves the toolchain produced
    // the program and the kernel can see the bytes.
    println!(
        "user: {} bytes of program embedded, to be mapped at {:#x}",
        USER_PROG.len(),
        USER_BASE
    );
    print!("user: first instructions:");
    for b in USER_PROG.iter().take(12) {
        print!(" {:02x}", b);
    }
    println!();
    // It used to print the message it intended to write, read straight out of
    // the tail of the image. That stopped meaning anything once the program
    // grew a data section at a fixed address -- the last bytes of the file are
    // no longer the last bytes of any string.

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

    // Milestone 11: two processes, each in its own address space.
    //
    // Same binary, same virtual address, different physical memory. Neither can
    // name the other's -- not "is denied", but has no address that reaches it.
    let a_root = process_spawn("procA", b'A');
    let b_root = process_spawn("procB", b'B');

    println!("proc: two processes created, both running the same binary");
    println!(
        "proc: virtual {:#x} in procA -> physical {:#x}",
        USER_BASE,
        probe(a_root, USER_BASE).map(pte_to_pa).unwrap_or(0)
    );
    println!(
        "proc: virtual {:#x} in procB -> physical {:#x}",
        USER_BASE,
        probe(b_root, USER_BASE).map(pte_to_pa).unwrap_or(0)
    );
    println!("proc: same address, different city");

    // =====================================================================
    //  END SCRATCH ZONE
    // =====================================================================

    // Milestone 15: the console stops being polled.
    plic_init();

    // Milestone 14: hand the machine over to whoever is typing.
    thread_spawn("shell", shell);

    // Thread 0 is now init. Everything spawned at boot is its child, and a
    // zombie nobody collects is a table slot and 16 KiB held forever -- which
    // is exactly what an orphaned process is in any Unix, and exactly why init
    // has this job there too.
    loop {
        while let Some((name, code)) = thread_wait() {
            println!("init: reaped {} (exit {})", name, code);
        }
        // No children left. Idle -- and yield first, because wfi with a
        // runnable thread waiting would park the core instead of running it.
        yield_now();
        unsafe { core::arch::asm!("wfi") };
    }
}

/// Entry point for every user process's thread.
///
/// By the time this runs, `yield_now` has already switched `satp` to this
/// thread's address space -- so `USER_BASE` here means *this* process's copy of
/// the program, not anyone else's.
extern "C" fn user_thread_entry() -> ! {
    let (user_sp, arg) = {
        let t = THREADS.lock();
        let cur = CURRENT.load(Ordering::Relaxed);
        (t[cur].user_sp, t[cur].user_arg)
    };
    enter_user(user_sp, arg);
}

/// Create a process: a private address space, the program loaded into it, and
/// a thread to run it.
///
/// Everything here is cheap. The address space is one frame plus 256 copied
/// entries; the program is a couple of frames; the thread already existed as a
/// concept. Two processes then run the same binary, at the same virtual
/// address, in completely different physical memory.
fn process_spawn(name: &'static str, tag: u8) -> usize {
    let root_pa = proc_pagetable();
    let user_sp = map_user_tagged(root_pa, tag);
    let satp = (8_usize << 60) | (root_pa >> 12);

    let mut stack = alloc::vec![0u8; STACK_SIZE];
    let top = (stack.as_mut_ptr() as usize + STACK_SIZE) & !15;

    let mut ctx = Context::zeroed();
    ctx.ra = thread_start as *const () as usize;
    ctx.sp = top;
    ctx.s[0] = user_thread_entry as *const () as usize;

    // The heap starts on the first page past the writable data region --
    // NOT past the image. The two stopped being the same thing when data got
    // its own fixed address, and a break that overlapped .bss would hand the
    // allocator pages the program's globals were already living in.
    let brk = USER_DATA_BASE + USER_DATA_SIZE;

    thread_insert(Thread {
        ctx,
        satp,
        root_pa,
        user_sp,
        user_arg: tag as usize,
        brk,
        name,
        state: ThreadState::Runnable,
        parent: current_addr(),
        stack,
    });

    root_pa
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
        // Report a few times and then shut up. Milestone 14 gave the machine
        // a human at the other end of the serial line, and a demo that proved
        // its point twenty seconds ago is now just noise over the prompt.
        if spins.is_multiple_of(4_000_000) && spins <= 12_000_000 {
            println!("[greedy] {}M spins, never yielded once", spins / 1_000_000);
        }
        // Note the absence of yield_now(). That is the whole point.
    }
}

/// Fill the store with a plausible mess and then narrow it down, to show that
/// a handful of orthogonal tags beats knowing where anything is.
fn store_demo() {
    use alloc::string::ToString;
    use alloc::vec;

    // Pretend timestamps: day numbers. Real ones would come from `now()`.
    const MON: i64 = 100;
    const TUE: i64 = 101;
    const WED: i64 = 102;
    const LAST_MONTH: i64 = 40;

    let put = |name: &str, kind: &str, day: i64, origin: &str, body: &str| {
        store_create(
            body.as_bytes().to_vec(),
            vec![
                ("name".to_string(), Value::Text(name.to_string())),
                ("type".to_string(), Value::Text(kind.to_string())),
                ("created_at".to_string(), Value::Int(day)),
                ("origin".to_string(), Value::Text(origin.to_string())),
            ],
        )
    };

    put(
        "brick breaker",
        "python",
        TUE,
        "editor",
        "import pygame  # paddle",
    );
    put("notes", "text", TUE, "editor", "remember to fix the paddle");
    put(
        "brick breaker",
        "python",
        LAST_MONTH,
        "editor",
        "first attempt, bad",
    );
    put("solver", "python", MON, "editor", "def solve(): pass");
    put("screenshot", "image", TUE, "camera", "PNG-ish bytes");
    put("todo", "text", WED, "editor", "buy milk");
    put("scratch", "python", WED, "repl", "2+2");
    put("paddle sketch", "image", TUE, "editor", "PNG-ish paddle");

    println!(
        "store: {} objects, no paths, no directories",
        STORE.lock().len()
    );

    // The query from day one: "last tuesday's python file about brick breaker"
    let all = store_query(&[]);
    let by_type = store_query(&[Cond::Eq("type", Value::Text("python".to_string()))]);
    let by_type_day = store_query(&[
        Cond::Eq("type", Value::Text("python".to_string())),
        Cond::Between("created_at", TUE, TUE),
    ]);
    let final_set = store_query(&[
        Cond::Eq("type", Value::Text("python".to_string())),
        Cond::Between("created_at", TUE, TUE),
        Cond::Contains("name", "brick"),
    ]);

    println!("query: \"last tuesday's python file about brick breaker\"");
    println!("  everything                     -> {}", all.len());
    println!("  + type = python                -> {}", by_type.len());
    println!("  + created tuesday              -> {}", by_type_day.len());
    println!("  + name contains \"brick\"        -> {}", final_set.len());

    for id in &final_set {
        let store = STORE.lock();
        let o = &store[id];
        println!(
            "  => {:?}  name={:?}  {} bytes",
            o.id,
            o.attr("name"),
            blob_len(o.content)
        );
    }

    // Content addressing: identical bytes are the same object, always.
    let a = store_create(b"duplicate".to_vec(), vec![]);
    let b = store_create(b"duplicate".to_vec(), vec![]);
    println!(
        "dedup: same bytes + same attrs -> {} id",
        if a == b { "1" } else { "2" }
    );

    // The bug this split exists to fix: identical content, different metadata.
    let tax = store_create(
        b"ok".to_vec(),
        vec![("name".to_string(), Value::Text("tax return".to_string()))],
    );
    let shop = store_create(
        b"ok".to_vec(),
        vec![("name".to_string(), Value::Text("shopping list".to_string()))],
    );
    // Scoped deliberately: SpinLock is NOT reentrant, and holding this guard
    // into the next section deadlocked the kernel the first time this was
    // written. Every lock here is released before the next thing that takes it.
    {
        let store = STORE.lock();
        println!(
            "split: same bytes, different names -> distinct: {}  ({:?} / {:?})",
            tax != shop,
            store[&tax].attr("name"),
            store[&shop].attr("name")
        );
        let objects = store.len();
        drop(store);
        println!(
            "       {} objects but only {} blobs -- content stored once",
            objects,
            BLOBS.lock().len()
        );
    }

    // ---- the three verbs that replace rm ----
    let py = || store_query(&[Cond::Eq("type", Value::Text("python".to_string()))]);
    let victim = py()[0];

    println!("verbs: {} python objects visible", py().len());

    hide(victim, true);
    println!(
        "  hide    -> {} visible, still in the store: {}",
        py().len(),
        STORE.lock().contains_key(&victim)
    );

    hide(victim, false);
    println!(
        "  unhide  -> {} visible again (nothing was destroyed)",
        py().len()
    );

    let content = STORE.lock()[&victim].content;
    evict(victim);
    println!(
        "  evict   -> bytes gone ({} left), record survives: {} -- still a valid coordinate",
        blob_len(content),
        STORE.lock().contains_key(&victim)
    );

    forget(victim);
    println!(
        "  forget  -> record gone too: still present = {}",
        STORE.lock().contains_key(&victim)
    );
    println!(
        "  history: {} claims recorded -- nothing overwritten, so WHEN is answerable",
        CLAIMS.lock().len()
    );

    // The usage log: reading things is itself recorded.
    for id in &final_set {
        log_event(*id);
    }
    println!(
        "events: {} access records so far (what was happening, not what exists)",
        EVENTS.lock().len()
    );
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

/// The UART's Line Status Register. Bit 0 = a received byte is waiting.
const UART_LSR: usize = 5;
const UART_LSR_RX_READY: u8 = 1 << 0;

/// Interrupt Enable Register. Bit 0 = interrupt me when a byte arrives.
///
/// Switch one of three. A UART that has not been asked to speak up will sit
/// there holding your keystroke in perfect silence.
const UART_IER: usize = 1;
const UART_IER_RX: u8 = 1 << 0;

// ---------------------------------------------------------------------------
// The console ring buffer
//
// The interrupt handler writes; the shell thread reads. A classic producer /
// consumer pair, and the classic way to kill a kernel with one:
//
//     shell takes the lock
//     a keystroke arrives -> the ISR runs
//     the ISR waits for the lock
//     the lock will be released by a thread that cannot resume until the ISR
//     returns
//
// That deadlock cannot happen here, because `SpinLock::lock` turns interrupts
// OFF before it takes the lock. The handler physically cannot run while the
// shell holds it. Milestone 9b built that discipline for its own sake; this is
// the bug it was actually for.
//
// Overflow drops the newest byte rather than overwriting the oldest. Losing a
// keystroke you just typed is confusing; losing one from a second ago, after
// several have already appeared on screen, is baffling.
// ---------------------------------------------------------------------------

const CONSOLE_CAP: usize = 256;

struct Ring {
    buf: [u8; CONSOLE_CAP],
    head: usize, // next slot to write
    tail: usize, // next slot to read
}

static CONSOLE: SpinLock<Ring> = SpinLock::new(Ring {
    buf: [0; CONSOLE_CAP],
    head: 0,
    tail: 0,
});

/// How many keystrokes arrived as interrupts. Proof, not decoration: if this
/// climbs while you type, nothing is polling.
static CONSOLE_IRQS: AtomicU64 = AtomicU64::new(0);

fn console_push(c: u8) {
    let mut r = CONSOLE.lock();
    let next = (r.head + 1) % CONSOLE_CAP;
    if next == r.tail {
        return; // full
    }
    let h = r.head;
    r.buf[h] = c;
    r.head = next;
}

fn console_pop() -> Option<u8> {
    let mut r = CONSOLE.lock();
    if r.head == r.tail {
        return None;
    }
    let c = r.buf[r.tail];
    r.tail = (r.tail + 1) % CONSOLE_CAP;
    Some(c)
}

// ---------------------------------------------------------------------------
// PLIC -- Platform-Level Interrupt Controller
//
// A hart has exactly ONE external-interrupt input line and this board has
// dozens of devices, so something has to multiplex them. That something is not
// part of the CPU: it is a separate chip at 0x0c000000 whose whole job is to be
// a switchboard, and to answer the question "who was it?"
//
// A CONTEXT is a (hart, privilege level) pair -- the PLIC must distinguish
// hart 0's M-mode firmware from hart 0's S-mode kernel, because they want
// different interrupts. hart0-M is context 0; hart0-S, which is us, is 1.
// ---------------------------------------------------------------------------

const PLIC: usize = 0x0c00_0000;
const PLIC_PRIORITY: usize = PLIC; // + irq*4
const PLIC_ENABLE: usize = PLIC + 0x2000; // + ctx*0x80
const PLIC_THRESHOLD: usize = PLIC + 0x20_0000; // + ctx*0x1000
const PLIC_CLAIM: usize = PLIC + 0x20_0004; // + ctx*0x1000

/// hart 0, supervisor mode.
const PLIC_CTX: usize = 1;

/// UART0's interrupt number on the QEMU virt board -- straight out of the
/// device tree: `serial@10000000 { interrupts = <0x0a>; }`. Not a constant to
/// memorise; a fact to look up, which is what milestone 5 built the parser for.
const IRQ_UART0: u32 = 10;

fn plic_reg(off: usize) -> *mut u32 {
    // The PLIC is mapped in both halves, so this must follow the kernel
    // wherever it is executing -- exactly like UART_BASE.
    (off + if here() > HIGH_BASE { HIGH_BASE } else { 0 }) as *mut u32
}

fn plic_init() {
    unsafe {
        // Priority 0 means DISABLED, so a source left at its reset value never
        // fires no matter what else is switched on. Any non-zero value works
        // when there is only one source to arbitrate between.
        core::ptr::write_volatile(plic_reg(PLIC_PRIORITY + IRQ_UART0 as usize * 4), 1);

        // One bit per source, so source N lives in word N/32 at bit N%32.
        let word = PLIC_ENABLE + PLIC_CTX * 0x80 + (IRQ_UART0 as usize / 32) * 4;
        let bit = 1u32 << (IRQ_UART0 % 32);
        let cur = core::ptr::read_volatile(plic_reg(word));
        core::ptr::write_volatile(plic_reg(word), cur | bit);

        // Threshold is a floor: the PLIC delivers only interrupts with
        // priority STRICTLY ABOVE it. Zero means "let everything through".
        core::ptr::write_volatile(plic_reg(PLIC_THRESHOLD + PLIC_CTX * 0x1000), 0);

        // Switch one of three: tell the UART itself to speak up.
        let uart = UART_BASE.load(Ordering::Relaxed) as *mut u8;
        core::ptr::write_volatile(uart.add(UART_IER), UART_IER_RX);

        // Switch three of three: sie bit 9, SEIE -- the hart's willingness to
        // hear external interrupts at all.
        core::arch::asm!("csrs sie, {}", in(reg) 1usize << 9);
    }
    println!("plic: UART0 (irq {}) -> hart 0 supervisor", IRQ_UART0);
}

/// "Which interrupt fired, and I am taking it."
///
/// One read does both: it returns the highest-priority pending source AND
/// marks it in-flight so it will not be delivered again until completed. Zero
/// means nothing was pending.
fn plic_claim() -> u32 {
    unsafe { core::ptr::read_volatile(plic_reg(PLIC_CLAIM + PLIC_CTX * 0x1000)) }
}

/// "Finished with that one; you may raise it again."
///
/// Skip this and the device goes permanently silent -- the PLIC keeps waiting
/// for a reply that never comes. Same shape as rearming the timer: the write
/// is an ACKNOWLEDGEMENT, and forgetting it is not a missed optimisation but a
/// dead device.
fn plic_complete(irq: u32) {
    unsafe { core::ptr::write_volatile(plic_reg(PLIC_CLAIM + PLIC_CTX * 0x1000), irq) }
}

/// Drain everything the UART has and hand it to the shell.
///
/// A loop, not a single read: several keystrokes can arrive between one
/// interrupt and the handler running, and the 16550 only re-raises its
/// interrupt when the receive register goes from empty to non-empty. Read one
/// byte of three and the other two are stranded, with no interrupt coming to
/// remind you.
fn console_interrupt() {
    let base = UART_BASE.load(Ordering::Relaxed) as *mut u8;
    let mut got = false;
    unsafe {
        while core::ptr::read_volatile(base.add(UART_LSR)) & UART_LSR_RX_READY != 0 {
            console_push(core::ptr::read_volatile(base));
            CONSOLE_IRQS.fetch_add(1, Ordering::Relaxed);
            got = true;
        }
    }
    // Shout the channel. Anyone dozing on the console buffer stands up.
    if got {
        wakeup(chan_console());
    }
}

/// The console's channel: the address of the thing being waited for.
fn chan_console() -> usize {
    core::ptr::addr_of!(CONSOLE) as usize
}

/// Wait for a keystroke. The thread stops running entirely until one arrives.
///
/// Milestone 14 spun around `yield_now`, which worked but meant being handed
/// the CPU, finding nothing, and giving it back, over and over, for as long as
/// you took to decide what to type. Now the thread leaves the run queue and
/// the interrupt handler puts it back.
///
/// Interrupts go off BEFORE the buffer is checked and stay off until `sleep`
/// has marked this thread Sleeping. That window is the lost wakeup, and this
/// is the whole defence against it.
fn getchar_blocking() -> u8 {
    loop {
        let was_on = intr_off();
        match console_pop() {
            Some(c) => {
                if was_on {
                    intr_on();
                }
                return c;
            }
            None => {
                sleep(chan_console());
                if was_on {
                    intr_on();
                }
            }
        }
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
            // BOTH a0 and a1 are clobbered, not just a1. An SBI call returns
            // `sbiret { error, value }` in a0 and a1, so OpenSBI overwrites
            // a0. Declaring it `in("a0")` promised the compiler the register
            // survives the call, which let it keep a live value there and
            // reuse OpenSBI's error code afterwards -- as a pointer, in at
            // least one case.
            inout("a0") when => _,
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
    probe_in(va(root_pa) as *mut usize, addr)
}

/// Same walk, but starting from an already-usable table pointer.
fn probe_in(root: *mut usize, addr: usize) -> Option<usize> {
    let mut table = root as *const usize;
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

// ===========================================================================
// User mode -- milestone 10b
// ===========================================================================

const PTE_U: usize = 1 << 4; // user mode may access this page

/// Bytes of user stack. One frame is plenty for a program this small.
const USER_STACK_SIZE: usize = PAGE_SIZE;

/// Where a user program's WRITABLE data begins. Must match `user/user.ld`.
///
/// A flat binary carries no section table, so the kernel cannot see where
/// read-only code stops and writable globals start -- and it has to know,
/// because W^X applies to user pages. Map the lot r-x and the first global
/// variable faults; map the lot rw-x and a program can rewrite its own code.
///
/// Deliberately temporary. Milestone 19 must parse an ELF to run programs out
/// of the store, and an ELF states permissions per segment, which deletes this.
const USER_DATA_BASE: usize = 0x8000;
const USER_DATA_SIZE: usize = 0x4000;

/// How far a process may push its break. A cap rather than "until memory runs
/// out", because one greedy program should not be able to starve the machine
/// -- and because it makes the overflow check on the request trivially safe.
const USER_HEAP_MAX: usize = 4 * 1024 * 1024;

/// The kernel's own satp value, so kernel threads can be switched back to it.
static KERNEL_SATP: AtomicUsize = AtomicUsize::new(0);

/// Build a fresh address space -- a new city.
///
/// The whole cost is one frame plus copying the kernel's high-half root
/// entries. The kernel occupies the SAME virtual addresses in every address
/// space, so those few 8-byte numbers give the new city a complete, correct
/// kernel: the one building that stands at the same street address in every
/// town.
///
/// Root slots 0..255 are the low half and stay empty -- that is the new
/// process's private space. Slots 256..511 are the high half and are shared.
fn proc_pagetable() -> usize {
    let root_pa = frame_alloc().expect("no frame for a process page table") as usize;
    let root = va(root_pa) as *mut usize;
    unsafe { core::ptr::write_bytes(root as *mut u8, 0, PAGE_SIZE) };

    let kroot = va(ROOT.load(Ordering::Relaxed)) as *const usize;
    for i in 256..512 {
        unsafe { *root.add(i) = *kroot.add(i) };
    }

    root_pa
}

/// Copy the embedded user program into fresh frames and map it into the LOW
/// half with U=1, plus a stack. Returns the initial user stack pointer.
///
/// `tag` is written over the program's identity byte, so two processes running
/// the same binary announce themselves differently.
///
/// The low half is free real estate: the kernel relocated to the high half and
/// dropped the identity map, so user and kernel share one page table and can
/// never collide. That is what milestone 6c bought.
fn map_user_tagged(root_pa: usize, _tag: u8) -> usize {
    // The tag used to be patched directly into a byte of the program's image.
    // That worked only while every page was writable by the kernel and the
    // message sat at a predictable offset -- and it stopped being true the
    // moment the program grew and .rodata became genuinely read-only.
    //
    // It arrives in a0 now instead, which is both simpler and the first seed
    // of argv: a program is told who it is rather than having its own text
    // rewritten behind its back.
    map_user(root_pa)
}

/// Copy the embedded user program into fresh frames and map it into the LOW
/// half with U=1, plus a stack. Returns the initial user stack pointer.
fn map_user(root_pa: usize) -> usize {
    let root = va(root_pa) as *mut usize;

    // --- code and rodata: readable, executable, NOT writable ---
    let mut addr = USER_BASE;
    while addr < USER_DATA_BASE {
        let frame = frame_alloc().expect("no frame for the user program");
        let dst = va(frame as usize) as *mut u8;
        unsafe { core::ptr::write_bytes(dst, 0, PAGE_SIZE) };

        // Copy whatever the image has for this page. Past the end of the
        // image there is nothing to copy and the page stays zero.
        let off = addr - USER_BASE;
        if off < USER_PROG.len() {
            let n = core::cmp::min(PAGE_SIZE, USER_PROG.len() - off);
            unsafe { core::ptr::copy_nonoverlapping(USER_PROG.as_ptr().add(off), dst, n) };
        }

        map(root, addr, frame as usize, PTE_R | PTE_X | PTE_U | RSW_TEXT);
        addr += PAGE_SIZE;
    }

    // --- data and bss: readable, WRITABLE, not executable ---
    //
    // .bss occupies no space in a flat binary -- it is a promise that some
    // zeroed bytes will exist, not a record of them -- so these pages are
    // zeroed first and the image is copied over whatever part of them it
    // reaches. Everything past the image end is the .bss, already correct.
    while addr < USER_DATA_BASE + USER_DATA_SIZE {
        let frame = frame_alloc().expect("no frame for user data");
        let dst = va(frame as usize) as *mut u8;
        unsafe { core::ptr::write_bytes(dst, 0, PAGE_SIZE) };

        let off = addr - USER_BASE;
        if off < USER_PROG.len() {
            let n = core::cmp::min(PAGE_SIZE, USER_PROG.len() - off);
            unsafe { core::ptr::copy_nonoverlapping(USER_PROG.as_ptr().add(off), dst, n) };
        }

        map(root, addr, frame as usize, PTE_R | PTE_W | PTE_U | RSW_DATA);
        addr += PAGE_SIZE;
    }

    // --- the user stack, immediately below the program ---
    let sframe = frame_alloc().expect("no frame for the user stack");
    unsafe { core::ptr::write_bytes(va(sframe as usize) as *mut u8, 0, PAGE_SIZE) };
    let stack_va = USER_BASE - USER_STACK_SIZE;
    map(
        root,
        stack_va,
        sframe as usize,
        PTE_R | PTE_W | PTE_U | RSW_DATA,
    );

    unsafe { core::arch::asm!("sfence.vma") };

    println!(
        "user: {:#x}..{:#x} r-x, {:#x}..{:#x} rw-, stack {:#x}..{:#x} rw-",
        USER_BASE,
        USER_DATA_BASE,
        USER_DATA_BASE,
        USER_DATA_BASE + USER_DATA_SIZE,
        stack_va,
        USER_BASE
    );

    stack_va + USER_STACK_SIZE
}

// ---------------------------------------------------------------------------
// Store syscall wire format
//
// Structured arguments arrive as ONE packed buffer rather than a nest of
// pointers. That is a security decision, not a tidiness one: a nested layout
// would mean validating 2N untrusted pointers per call, and every accepted
// pointer is an attack surface. This way the kernel checks one range, copies it
// in, and parses in its own memory where nothing can change underneath it.
//
// Content addressing also forces it. An object's id depends on its complete
// contents, so it cannot be built up field by field -- it has to arrive whole.
//
// create buffer, all little-endian:
//     u32  content_len
//     ..   content bytes
//     u32  attr_count
//     per attr:
//         u32 key_len, key bytes
//         u8  kind        0 = Int, 1 = Text
//         Int:  i64
//         Text: u32 len, bytes
//
// query buffer:
//     u32  cond_count
//     per cond:
//         u8  op          0 = Eq(Text), 1 = Between(Int)
//         u32 key_len, key bytes
//         Eq:      u32 len, bytes
//         Between: i64 lo, i64 hi
// ---------------------------------------------------------------------------

/// Reads a packed buffer, refusing to run off the end.
struct Reader<'a> {
    b: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Reader { b, at: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        if end > self.b.len() {
            return None;
        }
        let s = &self.b[self.at..end];
        self.at = end;
        Some(s)
    }
    fn u32(&mut self) -> Option<u32> {
        let s = self.take(4)?;
        Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn i64(&mut self) -> Option<i64> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Some(i64::from_le_bytes(a))
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn text(&mut self) -> Option<alloc::string::String> {
        let n = self.u32()? as usize;
        let s = self.take(n)?;
        core::str::from_utf8(s)
            .ok()
            .map(alloc::string::ToString::to_string)
    }
}

/// Copy a range of user memory into kernel memory, once, with SUM enabled only
/// for the copy itself.
///
/// # Safety
/// The range must already have been checked with `user_range_ok`.
unsafe fn copy_in(addr: usize, len: usize) -> alloc::vec::Vec<u8> {
    // Allocate first, so no allocation happens with SUM enabled.
    let mut v = alloc::vec::Vec::with_capacity(len);
    core::arch::asm!("csrs sstatus, {}", in(reg) 1_usize << 18);
    for i in 0..len {
        v.push(core::ptr::read_volatile((addr + i) as *const u8));
    }
    core::arch::asm!("csrc sstatus, {}", in(reg) 1_usize << 18);
    v
}

/// Is `[ptr, ptr+len)` memory that USER mode is allowed to read?
///
/// This is the single most important function in the milestone. A user program
/// hands the kernel an address; without this check the kernel will happily
/// dereference it, and a program could ask to have kernel memory printed, or
/// point at a device register, or at nothing at all.
///
/// The rule is not "is it mapped" but **"is it mapped for THEM"** -- the U bit.
/// Kernel pages are mapped and readable and must still be refused.
///
/// Walks the same page table the MMU walks, using the `probe` written back at
/// milestone 6 for exactly this kind of question.
fn user_range_ok(root_pa: usize, ptr: usize, len: usize) -> bool {
    if len == 0 {
        return true;
    }

    // Overflow first: a length near usize::MAX would otherwise wrap and make
    // the range look tiny.
    let end = match ptr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };

    // A legitimate user pointer lives in the low half. Anything at or above
    // 2^38 is either the invalid hole or the kernel's half, and no user program
    // has any business naming it.
    if end > (1_usize << 38) {
        return false;
    }

    // Every page the range touches must be present, readable, AND user-
    // accessible. Checking only the first page is a classic hole: a program can
    // pass a valid pointer with a length that runs off the end of its mapping.
    let mut p = ptr & !(PAGE_SIZE - 1);
    while p < end {
        match probe(root_pa, p) {
            Some(e) if (e & PTE_U != 0) && (e & PTE_R != 0) => {}
            _ => return false,
        }
        p += PAGE_SIZE;
    }
    true
}

/// Read one byte of user memory, with SUM enabled for exactly that one access.
///
/// SUM (sstatus bit 18) permits supervisor code to touch U=1 pages. It is off
/// the rest of the time on purpose: with it off, a stray kernel dereference of
/// a user pointer FAULTS instead of quietly succeeding. Turning it on only
/// around a deliberate copy means accidents stay loud.
///
/// # Safety
/// The range must already have been checked with `user_range_ok`.
unsafe fn copy_from_user(addr: usize) -> u8 {
    core::arch::asm!("csrs sstatus, {}", in(reg) 1_usize << 18);
    let b = core::ptr::read_volatile(addr as *const u8);
    core::arch::asm!("csrc sstatus, {}", in(reg) 1_usize << 18);
    b
}

/// Same as `user_range_ok`, but the range must also be WRITABLE by user mode.
///
/// A separate check because letting a program nominate somewhere for the
/// kernel to write is strictly more dangerous than letting it nominate
/// somewhere to read: it turns the kernel into a writer of the program's
/// choosing.
fn user_range_writable(root_pa: usize, ptr: usize, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    let end = match ptr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    if end > (1_usize << 38) {
        return false;
    }
    let mut p = ptr & !(PAGE_SIZE - 1);
    while p < end {
        match probe(root_pa, p) {
            Some(e) if (e & PTE_U != 0) && (e & PTE_W != 0) => {}
            _ => return false,
        }
        p += PAGE_SIZE;
    }
    true
}

/// Drop to user mode and start the program. Never returns -- from here on this
/// thread IS the user program, and only a trap brings the kernel back.
fn enter_user(user_sp: usize, arg: usize) -> ! {
    unsafe {
        // SPP (bit 8) = 0  -> sret returns to USER mode, not supervisor
        core::arch::asm!("csrc sstatus, {}", in(reg) 1_usize << 8);
        // SPIE (bit 5) = 1 -> interrupts on after sret, so it stays preemptible
        core::arch::asm!("csrs sstatus, {}", in(reg) 1_usize << 5);
        // SUM stays OFF. Supervisor code cannot touch U=1 pages by default,
        // and that default is a feature: it means the kernel cannot be tricked
        // into dereferencing a user pointer by accident. It is enabled only
        // around explicit copies, in `copy_from_user`.

        core::arch::asm!(
            // Hand trap_entry a kernel stack for this thread. sscratch nonzero
            // is also how it knows the next trap came from user mode.
            "csrw sscratch, sp",
            "csrw sepc, {entry}",
            "mv sp, {usp}",
            "sret",
            entry = in(reg) USER_BASE,
            usp = in(reg) user_sp,
            // a0 is the first argument in the RISC-V calling convention, and
            // _start does not touch it before `call umain` -- so this arrives
            // as umain's first parameter.
            in("a0") arg,
            options(noreturn),
        );
    }
}

/// Write one byte INTO user memory.
///
/// The mirror of `copy_from_user`, and it takes the same care: SUM is on for
/// exactly one store and off again immediately. Leaving it on would mean a
/// stray kernel pointer into user space silently succeeds instead of faulting,
/// and accidents should stay loud.
///
/// The range must already have been checked with `user_range_writable`.
unsafe fn copy_to_user(addr: usize, byte: u8) {
    unsafe {
        core::arch::asm!("csrs sstatus, {}", in(reg) 1_usize << 18); // SUM on
        core::ptr::write_volatile(addr as *mut u8, byte);
        core::arch::asm!("csrc sstatus, {}", in(reg) 1_usize << 18); // SUM off
    }
}

/// Pack one object into a byte stream for `get`.
///
/// Same shape as the on-disk record and the create request, deliberately: one
/// wire format, learned once.
fn serialize_object(id: ObjId) -> Option<alloc::vec::Vec<u8>> {
    let (content, attrs) = {
        let store = STORE.lock();
        let o = store.get(&id)?;
        (
            o.content,
            o.attrs
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<alloc::vec::Vec<_>>(),
        )
    };

    let mut w = Writer::new();
    w.u64(content.0);
    w.u32(attrs.len() as u32);
    for (k, v) in &attrs {
        w.blob(k.as_bytes());
        w.value(v);
    }
    // The bytes last, so a caller that only wants metadata can stop early.
    // An evicted object has none, and that is a fact rather than a failure.
    match BLOBS.lock().get(&content) {
        Some(b) => w.blob(b),
        None => w.u32(0),
    }
    Some(w.0)
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
fn map(root: *mut usize, vaddr: usize, pa: usize, flags: usize) {
    let mut table = root;

    // Levels 2 then 1. The index is just a 9-bit slice of the address:
    //   level 2 -> bits 38..30      level 1 -> bits 29..21
    for level in [2_usize, 1_usize] {
        let idx = (vaddr >> (12 + 9 * level)) & 0x1ff;
        let slot = unsafe { table.add(idx) };
        let mut entry = unsafe { *slot };

        if entry & PTE_V == 0 {
            // Empty. Build the missing table: allocate, ERASE, link.
            //
            // The erase is not optional -- a frame off the free list still
            // holds its free-list link, and the hardware would follow that
            // as a real entry.
            let next = frame_alloc().expect("out of frames while building page tables");
            // Erase through the higher-half alias. frame_alloc returns a
            // PHYSICAL address, and once the identity map is gone a physical
            // address is not something the kernel can dereference.
            unsafe { core::ptr::write_bytes(va(next as usize) as *mut u8, 0, PAGE_SIZE) };
            PT_FRAMES.fetch_add(1, Ordering::Relaxed);

            // A BRANCH: valid, but R/W/X all clear. That combination is what
            // tells the hardware to keep walking rather than stop here.
            entry = pte(next as usize, PTE_V);
            unsafe { *slot = entry };
        }

        // A page table entry stores a PHYSICAL address, so descending means
        // converting to the higher-half alias to get something dereferenceable.
        // This used to work without va() only because the identity map existed.
        table = va(pte_to_pa(entry)) as *mut usize;
    }

    // Level 0: the leaf. Setting any of R/W/X ends the walk.
    //
    // A and D are pre-set because not every implementation updates them in
    // hardware; on those, an unset bit faults on first touch.
    let idx = (vaddr >> 12) & 0x1ff;
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
    // Keep the physical address for satp and for ROOT, but do all the WRITING
    // through the higher-half alias, so this code works identically before and
    // after the identity map goes away.
    let root_pa = frame_alloc().expect("no frame for the root page table") as usize;
    let root = va(root_pa) as *mut usize;
    unsafe { core::ptr::write_bytes(root as *mut u8, 0, PAGE_SIZE) };
    PT_FRAMES.fetch_add(1, Ordering::Relaxed);

    // Remembered as a bare physical number so the relocated kernel can still
    // find it after the identity map is gone.
    ROOT.store(root_pa, Ordering::Relaxed);

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
                                                                           // Eight virtio-mmio slots, immediately above the UART.
    map_range(root, 0x1000_1000, 0x1000_9000, 0, PTE_R | PTE_W | RSW_DEV);
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
    // physical page by adding a constant. As of milestone 11, editing another
    // process's page tables becomes arithmetic instead of a special case.
    map_range(
        root,
        0x1000_0000,
        0x1000_1000,
        HIGH_BASE,
        PTE_R | PTE_W | RSW_DEV,
    );
    // Eight virtio-mmio slots, immediately above the UART. Needed in the HIGH
    // map specifically -- the driver runs long after the identity map is gone.
    map_range(
        root,
        0x1000_1000,
        0x1000_9000,
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
    let satp = (8_usize << 60) | (root_pa >> 12);

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
    KERNEL_SATP.store(satp, Ordering::Relaxed);

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

// The allocator walks and rewrites a shared free list across several steps.
// That was safe when nothing could interrupt it; with preemption, a timer
// landing mid-walk lets another thread see a half-updated list.
//
// Interrupts off for the duration is the same lock yield_now uses, and it is
// enough here because the critical sections are a few dozen instructions.
unsafe impl GlobalAlloc for Heap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let was_on = intr_off();
        let p = self.alloc_locked(layout);
        if was_on {
            intr_on();
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let was_on = intr_off();
        self.dealloc_locked(ptr, layout);
        if was_on {
            intr_on();
        }
    }
}

impl Heap {
    unsafe fn alloc_locked(&self, layout: Layout) -> *mut u8 {
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

    unsafe fn dealloc_locked(&self, ptr: *mut u8, layout: Layout) {
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

/// What a thread is currently doing, which is mostly about what it is NOT
/// doing: only `Runnable` threads are ever handed the CPU.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ThreadState {
    /// Wants the CPU.
    Runnable,
    /// Waiting for something, identified by a CHANNEL -- and a channel is
    /// just the address of the thing being waited for. Addresses are already
    /// unique, so this gives collision-free wait queues for free. Waiting on
    /// keyboard input? Sleep on the address of the console buffer.
    Sleeping(usize),
    /// Exited, but nobody has collected the exit code yet.
    ///
    /// This state exists for one physical reason: a thread CANNOT free its own
    /// kernel stack, because it is standing on it. Freeing 16 KiB of stack and
    /// then executing one more instruction -- which pushes to that stack --
    /// writes into memory the heap has already given to someone else. So death
    /// is two phases: the dying thread drops its address space, and somebody
    /// else drops the stack afterwards. That gap IS the zombie.
    Zombie(i32),
    /// Reaped. The slot is reusable, which is why threads are never removed
    /// from the table -- `CURRENT` and every `parent` field are indices into
    /// it, and removing an element would silently renumber them all.
    Free,
}

struct Thread {
    ctx: Context,
    /// The address space this thread runs in -- the city it lives in. Kernel
    /// threads all share the kernel's; each user process has its own.
    satp: usize,
    /// The root page table's PHYSICAL address, or 0 for a kernel thread that
    /// borrows the kernel's. Kept so `exit` knows what to dismantle.
    root_pa: usize,
    /// Initial user stack pointer, for a thread that will drop to user mode.
    user_sp: usize,
    /// What lands in a0 when this thread enters user mode. Today it is an
    /// identity tag so two processes running one binary can tell themselves
    /// apart; tomorrow it is argv.
    user_arg: usize,
    /// The BREAK: the boundary between this process's mapped memory and
    /// nothing. One of the oldest names in Unix. `sbrk` moves it outward and
    /// the kernel maps the land that just came inside the fence. 0 for a
    /// kernel thread, which has no such boundary -- the kernel heap is one
    /// fixed region carved out at boot.
    brk: usize,
    name: &'static str,
    state: ThreadState,
    /// The address of the Thread that spawned this one, or 0 for thread 0.
    /// An ADDRESS rather than an index because boxed threads never move,
    /// so it stays valid even as the table grows.
    parent: usize,
    /// Owns the thread's stack. Replacing this with an empty Vec is what
    /// actually returns the 16 KiB -- the heap from milestone 7 doing real
    /// work, and only ever from a thread that is not standing on it.
    stack: alloc::vec::Vec<u8>,
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
// clippy suggests Vec<Thread> here. Do NOT take that advice: the Box is what
// keeps each Thread at a FIXED ADDRESS. yield_now hands `switch` raw pointers
// to contexts that must stay valid across the switch, and as Vec<Thread> the
// fifth spawn reallocated, moved every suspended thread's context, and turned
// those pointers into freed memory.
#[allow(clippy::vec_box)]
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
        satp: KERNEL_SATP.load(Ordering::Relaxed),
        root_pa: 0,
        user_sp: 0,
        user_arg: 0,
        brk: 0,
        name: "main",
        state: ThreadState::Runnable,
        parent: 0, // nothing spawned it; it has been running since _start
        stack: alloc::vec::Vec::new(), // main runs on the boot stack
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

    thread_insert(Thread {
        ctx,
        satp: KERNEL_SATP.load(Ordering::Relaxed),
        root_pa: 0,
        user_sp: 0,
        user_arg: 0,
        brk: 0,
        name,
        state: ThreadState::Runnable,
        parent: current_addr(),
        stack,
    });
}

/// Put a thread in the table, reusing a reaped slot if there is one.
///
/// Reuse rather than push-forever because nothing is ever REMOVED: `CURRENT`
/// and every `parent` field would be silently renumbered by a `remove`.
fn thread_insert(t: Thread) -> usize {
    let mut threads = THREADS.lock();
    if let Some(i) = threads.iter().position(|x| x.state == ThreadState::Free) {
        *threads[i] = t;
        return i;
    }
    threads.push(alloc::boxed::Box::new(t));
    threads.len() - 1
}

/// This thread's stable identity: the address of its box. Threads are boxed
/// precisely so this never changes, however much the table grows.
fn current_addr() -> usize {
    let threads = THREADS.lock();
    let cur = CURRENT.load(Ordering::Relaxed);
    match threads.get(cur) {
        Some(t) => &**t as *const Thread as usize,
        None => 0,
    }
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

    // Loop, because "nothing is runnable" is now a real state rather than an
    // impossibility. Every thread can be asleep at once -- the shell waiting
    // on a keystroke while everything else waits on it -- and the only thing
    // that can change that is an interrupt. So idle with interrupts ON and
    // wait for one, rather than scanning a table that cannot change.
    let (old, new, next_satp) = loop {
        let found = {
            let threads = THREADS.lock();
            let cur = CURRENT.load(Ordering::Relaxed);
            let n = threads.len();

            // No thread table yet. The timer is armed before `threads_init`
            // runs, so early boot traps land here with nothing to schedule --
            // and this code has no thread to switch away FROM. Falling into
            // the idle path instead would park the core forever, one timer
            // tick into the boot, having printed just enough to look like the
            // hang was somewhere else entirely.
            if n == 0 {
                drop(threads);
                if was_on {
                    intr_on();
                }
                return;
            }

            // Start at cur+1 so it is round robin, and go all the way round to
            // cur itself -- a lone runnable thread must be allowed to keep
            // running rather than falling through to the idle path.
            (1..=n)
                .map(|k| (cur + k) % n)
                .find(|&i| threads[i].state == ThreadState::Runnable)
        };

        let next = match found {
            Some(i) => i,
            None => {
                // Nothing to run. wfi parks the core until an interrupt
                // arrives; interrupts must be ON for one to be delivered, and
                // OFF again before re-examining the table.
                intr_on();
                unsafe { core::arch::asm!("wfi") };
                intr_off();
                continue;
            }
        };

        let threads = THREADS.lock();
        let cur = CURRENT.load(Ordering::Relaxed);
        if next == cur {
            // Already running the only runnable thread; a switch to yourself
            // would save a context on top of the one being restored.
            drop(threads);
            if was_on {
                intr_on();
            }
            return;
        }
        CURRENT.store(next, Ordering::Relaxed);

        // Raw pointers, never references: `switch` writes through one while
        // reading the other, and two live &mut would be instant undefined
        // behaviour. Two SHARED borrows are fine, so both are taken as
        // `addr_of!` and one is cast.
        //
        // These are safe to hold across the switch only because the threads
        // are boxed -- see the note on THREADS.
        break (
            core::ptr::addr_of!(threads[cur].ctx) as *mut Context,
            core::ptr::addr_of!(threads[next].ctx),
            threads[next].satp,
        );
    };
    // Lock released here. Interrupts stay off, because they were already off
    // when the guard was taken and Drop restores what it found.

    // Move to the next thread's city BEFORE switching to it. Safe to do while
    // still executing kernel code, because the kernel occupies identical
    // addresses in every address space -- including this stack.
    let cur_satp: usize;
    unsafe { core::arch::asm!("csrr {}, satp", out(reg) cur_satp) };
    if next_satp != 0 && next_satp != cur_satp {
        unsafe {
            core::arch::asm!("csrw satp, {}", "sfence.vma", in(reg) next_satp);
        }
    }

    unsafe { switch(old, new) };

    // Execution resumes HERE, but possibly minutes later and after several
    // other threads have run. Nothing local survived except what was in
    // callee-saved registers or on this thread's own stack.
    if was_on {
        intr_on();
    }
}

// ---------------------------------------------------------------------------
// Sleep and wake -- milestone 16
//
// A channel is the ADDRESS of the thing being waited for. Addresses are
// already unique, so this gives collision-free wait queues with no registry
// and no allocation: waiting on input sleeps on the console buffer's address,
// waiting on a child sleeps on the parent's own address.
//
// THE LOST WAKEUP, which is the whole reason this is delicate:
//
//     if console_is_empty() {      // (1) check
//                                  // (2) <-- the interrupt lands HERE
//         sleep(CONSOLE);          // (3) sleep
//     }
//
// At (2) a key is pressed, the handler pushes the byte and shouts -- into an
// empty room, because nobody is asleep yet. Then (3) sleeps forever with the
// keystroke sitting one instruction away. It needs the interrupt to land in a
// one-instruction window, so it passes every test and hangs in front of an
// audience.
//
// The fix is that (1)(2)(3) must be indivisible. On a single hart that is
// exactly `intr_off()`: the shout cannot happen because the interrupt cannot.
// So sleep() REQUIRES interrupts to be off already -- the caller turns them
// off before testing the condition, and they stay off until this thread is
// genuinely marked Sleeping. Same discipline as SpinLock, wider window.
// ---------------------------------------------------------------------------

/// Sleep on `chan`. **Interrupts must already be off**, and must have been off
/// since the condition was tested -- see the lost wakeup above.
fn sleep(chan: usize) {
    {
        let mut threads = THREADS.lock();
        let cur = CURRENT.load(Ordering::Relaxed);
        threads[cur].state = ThreadState::Sleeping(chan);
    }
    // Not runnable any more, so this hands the CPU away and does not come back
    // until someone shouts our channel. yield_now leaves interrupts off,
    // because they were off when it was entered.
    yield_now();
}

/// Wake everyone waiting on `chan`. Safe from an interrupt handler: every
/// holder of THREADS has interrupts off, so this can never arrive mid-update.
fn wakeup(chan: usize) {
    let mut threads = THREADS.lock();
    for t in threads.iter_mut() {
        if t.state == ThreadState::Sleeping(chan) {
            t.state = ThreadState::Runnable;
        }
    }
}

/// Tear down a process's address space: its frames, and the tables describing
/// them.
///
/// **Only slots 0..256.** Slots 256..511 are the KERNEL, copied into every
/// address space at milestone 11 -- freeing those would delete the kernel out
/// from under every process on the machine, including the one doing the
/// freeing.
///
/// Every address in a page table is PHYSICAL, so descending one needs `va()`
/// before the read. The rule that bit three separate places at milestone 6c.
fn free_user_space(root_pa: usize) {
    unsafe {
        let root = va(root_pa) as *mut usize;
        for i in 0..256 {
            let e1 = *root.add(i);
            if e1 & PTE_V == 0 {
                continue;
            }
            let l1 = va(pte_to_pa(e1)) as *mut usize;
            for j in 0..512 {
                let e2 = *l1.add(j);
                if e2 & PTE_V == 0 {
                    continue;
                }
                let l0 = va(pte_to_pa(e2)) as *mut usize;
                for k in 0..512 {
                    let e3 = *l0.add(k);
                    if e3 & PTE_V != 0 {
                        frame_free(pte_to_pa(e3) as *mut u8);
                    }
                }
                frame_free(pte_to_pa(e2) as *mut u8);
            }
            frame_free(pte_to_pa(e1) as *mut u8);
        }
        frame_free(root_pa as *mut u8);
    }
}

/// End this thread. Never returns.
///
/// Phase one of two. The address space goes here; the kernel stack cannot,
/// because this code is standing on it. What is left -- a table slot and an
/// exit code -- is the zombie, and `thread_wait` collects it.
fn thread_exit(code: i32) -> ! {
    let (root_pa, parent) = {
        let threads = THREADS.lock();
        let cur = CURRENT.load(Ordering::Relaxed);
        (threads[cur].root_pa, threads[cur].parent)
    };

    if root_pa != 0 {
        // ORDER MATTERS ABSOLUTELY. This code is executing THROUGH the page
        // table it is about to free -- every instruction fetch goes through
        // it. Move into the kernel's address space first, then dismantle the
        // old one from outside. Freeing first means the next instruction has
        // nowhere to come from.
        let ksatp = KERNEL_SATP.load(Ordering::Relaxed);
        unsafe { core::arch::asm!("csrw satp, {}", "sfence.vma", in(reg) ksatp) };
        free_user_space(root_pa);
    }

    {
        let mut threads = THREADS.lock();
        let cur = CURRENT.load(Ordering::Relaxed);
        let me = &*threads[cur] as *const Thread as usize;

        // Hand any surviving children to init BEFORE dying.
        //
        // Slots are reused, so a Thread's address is reused -- and `parent` is
        // an address. A child that outlived its parent would otherwise be
        // adopted by whatever thread lands in that slot next, which is the
        // PID-reuse bug wearing different clothes. Unix re-parents orphans to
        // init for exactly this reason.
        let init = &*threads[0] as *const Thread as usize;
        for t in threads.iter_mut() {
            if t.parent == me {
                t.parent = init;
            }
        }

        threads[cur].satp = KERNEL_SATP.load(Ordering::Relaxed);
        threads[cur].root_pa = 0;
        threads[cur].state = ThreadState::Zombie(code);
    }

    // Tell whoever is waiting for us. The parent sleeps on its OWN address,
    // so that is what gets shouted.
    if parent != 0 {
        wakeup(parent);
    }

    // Not runnable, so this never comes back. The loop is not optimism, it is
    // the guarantee: if some future bug makes a zombie runnable, it lands here
    // rather than running off the end of a function that promised never to
    // return.
    loop {
        yield_now();
    }
}

/// Collect one dead child. Returns its name and exit code, or None if this
/// thread has no children left at all.
///
/// Phase two. The stack is freed HERE, from a thread that is not standing on
/// it, and the slot goes back in the pool.
fn thread_wait() -> Option<(&'static str, i32)> {
    let me = current_addr();
    loop {
        // Interrupts off BEFORE looking, and still off when we sleep: between
        // finding no zombie and going to sleep, a child could exit and shout
        // into an empty room.
        let was_on = intr_off();

        let found = {
            let threads = THREADS.lock();
            threads.iter().enumerate().find_map(|(i, t)| match t.state {
                ThreadState::Zombie(c) if t.parent == me => Some((i, c, t.name)),
                _ => None,
            })
        };

        if let Some((i, code, name)) = found {
            let mut threads = THREADS.lock();
            threads[i].state = ThreadState::Free;
            // The 16 KiB nobody could free from the inside. Dropping the Vec
            // returns it to the heap.
            threads[i].stack = alloc::vec::Vec::new();
            drop(threads);
            if was_on {
                intr_on();
            }
            return Some((name, code));
        }

        let any_children = {
            let threads = THREADS.lock();
            threads
                .iter()
                .any(|t| t.parent == me && t.state != ThreadState::Free)
        };
        if !any_children {
            if was_on {
                intr_on();
            }
            return None;
        }

        sleep(me);
        if was_on {
            intr_on();
        }
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
// The object store -- milestone 12
//
// No files. No paths. No directories. There is nowhere to put anything.
//
// An object is bytes plus a set of typed attributes, named by a hash of its
// own content. It is reachable exactly two ways: by id, or by query. Nothing
// records WHERE anything is, because there is no where.
//
// A NAME is just another attribute. That is not a contradiction: a path is an
// ADDRESS (unique, hierarchical, says where), while a name is a LABEL (not
// unique, flat, says what it is called). Deleting addresses is the whole
// design; deleting names would just be inconvenient. And because the name is
// data rather than a lookup key, approximate matching on it is legal --
// `open("todo.txt")` must be exact, `name ~= "todo"` need not be.
// ===========================================================================

/// An object's identity: a hash of its content.
///
/// Content-addressed, so identical bytes get an identical id on every machine
/// forever. That buys dedup for free, makes an id globally meaningful (so
/// "that object lives on my desktop" is expressible), and makes immutability
/// arithmetic rather than a rule -- you cannot alter an object without
/// changing its name.
///
/// FNV-1a for now: about five lines, and enough to make content-addressing
/// real. It is NOT cryptographic -- collisions can be constructed
/// deliberately. Swap in SHA-256 before anything untrusted can write to the
/// store.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
struct ObjId(u64);

/// Streaming FNV-1a, so an id can be computed over several pieces.
struct Hasher(u64);

impl Hasher {
    fn new() -> Self {
        Hasher(0xcbf2_9ce4_8422_2325) // FNV offset basis
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
        }
    }
    fn finish(self) -> ObjId {
        ObjId(self.0)
    }
}

fn hash_bytes(bytes: &[u8]) -> ObjId {
    let mut h = Hasher::new();
    h.write(bytes);
    h.finish()
}

/// What an attribute holds.
///
/// Typed rather than raw bytes, and that is not tidiness -- it is what makes
/// range queries possible. Stored as bytes, `created_at` could only be tested
/// for exact equality, because "is this after Tuesday" would compare text
/// alphabetically and "9" sorts after "1754870400". Time is the axis that
/// narrows hardest, so it is precisely the one that must be typed.
#[derive(Clone, PartialEq, Debug)]
#[allow(dead_code)] // Id and Bytes are part of the schema; nothing stores them yet
enum Value {
    Int(i64),
    Text(alloc::string::String),
    Id(ObjId),
    Bytes(alloc::vec::Vec<u8>),
}

/// An object is a STATEMENT ABOUT content, not the content itself.
///
/// Splitting these apart fixes a real data-loss bug. Hashing only the bytes
/// meant two genuinely different documents that happened to contain identical
/// content collapsed into one, and the second one's metadata was silently
/// discarded -- a shopping list could overwrite a tax return's name.
///
/// So: the BYTES are addressed by their own hash and stored once (dedup still
/// free, and it is the bytes that are large). The OBJECT is addressed by a
/// hash of its metadata plus which blob it points at, so two objects sharing
/// content stay distinct. Git does exactly this: blobs are content-addressed,
/// and trees and commits are separate objects that reference them.
struct Object {
    id: ObjId,
    /// Which blob holds this object's bytes.
    content: ObjId,
    attrs: alloc::vec::Vec<(alloc::string::String, Value)>,
}

impl Object {
    fn attr(&self, key: &str) -> Option<&Value> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

/// Everything that exists. A map, not a tree -- there is no parent, no child,
/// and no order except the one a query asks for.
static STORE: SpinLock<alloc::collections::BTreeMap<ObjId, Object>> =
    SpinLock::new(alloc::collections::BTreeMap::new());

/// The content itself, addressed by its own hash and stored exactly once no
/// matter how many objects point at it.
static BLOBS: SpinLock<alloc::collections::BTreeMap<ObjId, alloc::vec::Vec<u8>>> =
    SpinLock::new(alloc::collections::BTreeMap::new());

/// What happened. Append-only, and the kernel never interprets it.
///
/// Creation stamps say where an object CAME FROM. This says what was happening
/// AROUND it -- which is what "the file I was working on while that video was
/// open" actually needs, and it cannot be reconstructed later. Userspace turns
/// these raw events into sessions and co-occurrence; the kernel only writes
/// down that something happened.
#[derive(Clone, Copy)]
// Userspace exists now, but nothing there can READ these -- there is no
// syscall for the event log. Deliberate: deciding who may see the causal graph
// is the unresolved capability question, and it should not be answered by
// accident.
#[allow(dead_code)]
struct Event {
    time: u64,
    process: usize,
    id: ObjId,
}

static EVENTS: SpinLock<alloc::vec::Vec<Event>> = SpinLock::new(alloc::vec::Vec::new());

fn log_event(id: ObjId) {
    EVENTS.lock().push(Event {
        time: now(),
        process: CURRENT.load(Ordering::Relaxed),
        id,
    });
}

/// Put an object in the store. Returns its id.
///
/// Identical content returns the identical id and stores nothing twice.
fn store_create(
    bytes: alloc::vec::Vec<u8>,
    attrs: alloc::vec::Vec<(alloc::string::String, Value)>,
) -> ObjId {
    // The content goes in once, under its own hash. Storing the same bytes
    // again stores nothing -- dedup, where it actually matters.
    let content = hash_bytes(&bytes);
    BLOBS.lock().entry(content).or_insert(bytes);

    // The object's id covers which blob it points at AND everything said about
    // it, so two objects sharing content stay distinct.
    let mut h = Hasher::new();
    h.write(&content.0.to_le_bytes());
    for (k, v) in &attrs {
        h.write(k.as_bytes());
        match v {
            Value::Int(n) => {
                h.write(b"i");
                h.write(&n.to_le_bytes());
            }
            Value::Text(t) => {
                h.write(b"t");
                h.write(t.as_bytes());
            }
            Value::Id(i) => {
                h.write(b"r");
                h.write(&i.0.to_le_bytes());
            }
            Value::Bytes(b) => {
                h.write(b"b");
                h.write(b);
            }
        }
    }
    let id = h.finish();

    STORE
        .lock()
        .entry(id)
        .or_insert(Object { id, content, attrs });
    id
}

/// How many bytes an object's content occupies.
fn blob_len(content: ObjId) -> usize {
    BLOBS.lock().get(&content).map(|b| b.len()).unwrap_or(0)
}

/// Does this object satisfy one condition?
enum Cond {
    /// Attribute equals a value exactly.
    Eq(&'static str, Value),
    /// Integer attribute falls in [lo, hi]. The range query typing exists for.
    Between(&'static str, i64, i64),
    /// Text attribute contains a substring -- the crude ancestor of the
    /// fuzzy name matching a semantic layer would do later.
    Contains(&'static str, &'static str),
}

fn matches(obj: &Object, c: &Cond) -> bool {
    match c {
        Cond::Eq(k, want) => obj.attr(k) == Some(want),
        Cond::Between(k, lo, hi) => match obj.attr(k) {
            Some(Value::Int(n)) => n >= lo && n <= hi,
            _ => false,
        },
        Cond::Contains(k, needle) => match obj.attr(k) {
            Some(Value::Text(t)) => t.as_str().contains(needle),
            _ => false,
        },
    }
}

/// What a parsed create request contains: the content, and everything said
/// about it.
type CreateReq = (
    alloc::vec::Vec<u8>,
    alloc::vec::Vec<(alloc::string::String, Value)>,
);

/// Parse a create request. Returns None on anything malformed -- a user
/// program is allowed to send nonsense, and the kernel's job is to notice.
fn parse_create(buf: &[u8]) -> Option<CreateReq> {
    let mut r = Reader::new(buf);
    let content_len = r.u32()? as usize;
    let content = r.take(content_len)?.to_vec();

    let n = r.u32()? as usize;
    // A count that could not possibly fit in the buffer is a lie; refuse it
    // rather than trying to allocate for it.
    if n > buf.len() {
        return None;
    }

    let mut attrs = alloc::vec::Vec::with_capacity(n);
    for _ in 0..n {
        let key = r.text()?;
        let value = match r.u8()? {
            0 => Value::Int(r.i64()?),
            1 => Value::Text(r.text()?),
            _ => return None,
        };
        attrs.push((key, value));
    }
    Some((content, attrs))
}

/// Parse a query request.
fn parse_query(buf: &[u8]) -> Option<alloc::vec::Vec<OwnedCond>> {
    let mut r = Reader::new(buf);
    let n = r.u32()? as usize;
    if n > buf.len() {
        return None;
    }
    let mut conds = alloc::vec::Vec::with_capacity(n);
    for _ in 0..n {
        let op = r.u8()?;
        let key = r.text()?;
        conds.push(match op {
            0 => OwnedCond::Eq(key, r.text()?),
            1 => OwnedCond::Between(key, r.i64()?, r.i64()?),
            _ => return None,
        });
    }
    Some(conds)
}

/// A condition whose key came from userspace, so it cannot be `&'static str`.
enum OwnedCond {
    Eq(alloc::string::String, alloc::string::String),
    Between(alloc::string::String, i64, i64),
    /// Substring match. Legal ONLY because a name is a label and not an
    /// address -- `open("todo.txt")` must be exact, `name ~ todo` need not be.
    Contains(alloc::string::String, alloc::string::String),
}

fn matches_owned(obj: &Object, c: &OwnedCond) -> bool {
    match c {
        OwnedCond::Eq(k, want) => matches!(obj.attr(k), Some(Value::Text(t)) if t == want),
        OwnedCond::Between(k, lo, hi) => {
            matches!(obj.attr(k), Some(Value::Int(n)) if n >= lo && n <= hi)
        }
        OwnedCond::Contains(k, needle) => {
            matches!(obj.attr(k), Some(Value::Text(t)) if t.contains(needle.as_str()))
        }
    }
}

fn store_query_owned(conds: &[OwnedCond]) -> alloc::vec::Vec<ObjId> {
    let ids: alloc::vec::Vec<ObjId> = {
        let store = STORE.lock();
        store
            .values()
            .filter(|o| conds.iter().all(|c| matches_owned(o, c)))
            .map(|o| o.id)
            .collect()
    };
    ids.into_iter().filter(|id| !is_hidden(*id)).collect()
}

// ---------------------------------------------------------------------------
// Claims -- how anything mutable exists in an immutable store
//
// Objects are content-addressed, so changing an attribute changes the id.
// There is no way to modify one; that is the point. But some facts DO change:
// whether something is hidden, whether its bytes still exist.
//
// So mutation is expressed as an append-only CLAIM: "as of time T, object X's
// key K is V." The current state of a key is simply the latest claim about it.
// Nothing is ever overwritten, so the history of what was hidden and when is
// preserved for free.
// ---------------------------------------------------------------------------

struct Claim {
    at: u64,
    id: ObjId,
    key: &'static str,
    value: Value,
}

static CLAIMS: SpinLock<alloc::vec::Vec<Claim>> = SpinLock::new(alloc::vec::Vec::new());

fn claim(id: ObjId, key: &'static str, value: Value) {
    CLAIMS.lock().push(Claim {
        at: now(),
        id,
        key,
        value,
    });
}

/// The most recent claim about `key` for `id`, if any.
fn current_claim(id: ObjId, key: &str) -> Option<Value> {
    let claims = CLAIMS.lock();
    claims
        .iter()
        .filter(|c| c.id == id && c.key == key)
        .max_by_key(|c| c.at)
        .map(|c| c.value.clone())
}

fn is_hidden(id: ObjId) -> bool {
    matches!(current_claim(id, "hidden"), Some(Value::Int(1)))
}

// ---------------------------------------------------------------------------
// The three verbs that replace `rm`
//
// "Delete" is three unrelated problems wearing one word, and separating them
// is what dissolves the "no root, so what is garbage" question entirely.
// ---------------------------------------------------------------------------

/// CLUTTER. Reversible, destroys nothing, and it is what most deletion
/// actually is. The "Cluttered" view is just a saved query for hidden = 1.
fn hide(id: ObjId, hidden: bool) {
    claim(id, "hidden", Value::Int(if hidden { 1 } else { 0 }));
}

/// SPACE. Drop the bytes, keep the record.
///
/// Only possible because ids are content hashes: the object remains a valid,
/// globally meaningful coordinate even with nothing behind it. So "the file I
/// was working on while that video was open" still answers after the video is
/// gone. A filesystem cannot do this -- when the file goes, every trace that it
/// existed goes with it.
fn evict(id: ObjId) {
    let content = match STORE.lock().get(&id) {
        Some(o) => o.content,
        None => return,
    };
    // Only drop the bytes if no OTHER object still points at them.
    let still_used = STORE
        .lock()
        .values()
        .any(|o| o.content == content && o.id != id);
    if !still_used {
        BLOBS.lock().remove(&content);
    }
    claim(id, "evicted", Value::Int(1));
}

/// PRIVACY. The bytes AND the record go. Irreversible, and rare.
///
/// The difference from evict is deliberate: eviction leaves a tombstone
/// because you still want to reason about the thing. Forgetting leaves nothing
/// because the whole point is that it should not be reasoned about.
fn forget(id: ObjId) {
    evict(id);
    STORE.lock().remove(&id);
    claim(id, "forgotten", Value::Int(1));
}

/// Every object satisfying ALL conditions.
///
/// A linear scan, deliberately: indexes are an optimisation, and the semantics
/// have to be right before the speed matters. Still a linear scan as of 17 --
/// indexes were pencilled in for 13 and got spent on the disk instead, which
/// was the right call: nothing is slow yet.
fn store_query(conds: &[Cond]) -> alloc::vec::Vec<ObjId> {
    let ids: alloc::vec::Vec<ObjId> = {
        let store = STORE.lock();
        store
            .values()
            .filter(|o| conds.iter().all(|c| matches(o, c)))
            .map(|o| o.id)
            .collect()
    };
    // Hidden objects drop out by default. They still exist, still have their
    // id, and a query for hidden = 1 finds them -- that view IS the
    // "Cluttered" folder.
    ids.into_iter().filter(|id| !is_hidden(*id)).collect()
}

// ===========================================================================
// virtio-blk -- milestone 13
//
// Real disk controllers are decades of accumulated registers and quirks, so
// virtual machines use virtio instead: rather than emulating hardware, the
// guest and hypervisor agree on a shared-memory protocol.
//
// The whole device is three arrays we share with QEMU:
//
//   descriptors   { physical address, length, flags, next }
//   available     indices the DRIVER has queued
//   used          indices the DEVICE has finished
//
// A restaurant pass: descriptors are the shelf of trays, `available` is the
// order rail, `used` is the done rail.
//
// THE IMPORTANT PART: the device does DMA. It does not go through the MMU.
// Every address in a descriptor is PHYSICAL, and the device writes straight to
// physical memory -- no page table, no permission bits, no U bit, no SUM.
// Everything milestone 6 built to control what may touch what does not apply
// to hardware. A wrong descriptor address is unbounded silent corruption with
// no fault to catch it. (Real machines put an IOMMU in front of devices for
// exactly this; using QEMU's is a much later problem.)
// ===========================================================================

/// QEMU's virt board puts eight virtio-mmio slots here, 0x1000 apart.
const VIRTIO_MMIO_BASE: usize = 0x1000_1000;
const VIRTIO_MMIO_STRIDE: usize = 0x1000;
const VIRTIO_MMIO_SLOTS: usize = 8;

const VIRTIO_MAGIC: u32 = 0x7472_6976; // "virt"
const VIRTIO_ID_BLOCK: u32 = 2;

// Register offsets, virtio-mmio version 2.
const R_MAGIC: usize = 0x000;
const R_VERSION: usize = 0x004;
const R_DEVICE_ID: usize = 0x008;
const R_DEVICE_FEATURES: usize = 0x010;
const R_DEVICE_FEATURES_SEL: usize = 0x014;
const R_DRIVER_FEATURES: usize = 0x020;
const R_DRIVER_FEATURES_SEL: usize = 0x024;
const R_QUEUE_SEL: usize = 0x030;
const R_QUEUE_NUM_MAX: usize = 0x034;
const R_QUEUE_NUM: usize = 0x038;
const R_QUEUE_READY: usize = 0x044;
const R_QUEUE_NOTIFY: usize = 0x050;
const R_INTERRUPT_STATUS: usize = 0x060;
const R_INTERRUPT_ACK: usize = 0x064;
const R_STATUS: usize = 0x070;
const R_QUEUE_DESC_LOW: usize = 0x080;
const R_QUEUE_DESC_HIGH: usize = 0x084;
const R_QUEUE_DRIVER_LOW: usize = 0x090;
const R_QUEUE_DRIVER_HIGH: usize = 0x094;
const R_QUEUE_DEVICE_LOW: usize = 0x0a0;
const R_QUEUE_DEVICE_HIGH: usize = 0x0a4;

// Status bits, written in this order during negotiation.
const S_ACKNOWLEDGE: u32 = 1;
const S_DRIVER: u32 = 2;
const S_DRIVER_OK: u32 = 4;
const S_FEATURES_OK: u32 = 8;

const VRING_DESC_F_NEXT: u16 = 1;
const VRING_DESC_F_WRITE: u16 = 2; // device writes, i.e. we are reading

const QUEUE_SIZE: usize = 8;
const SECTOR: usize = 512;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VringDesc {
    addr: u64, // PHYSICAL -- the device does not use our page tables
    len: u32,
    flags: u16,
    next: u16,
}

#[repr(C)]
struct BlkReqHeader {
    kind: u32, // 0 = read, 1 = write
    reserved: u32,
    sector: u64,
}

/// Base address of the block device, once found. 0 = none.
static BLK_BASE: AtomicUsize = AtomicUsize::new(0);
/// Physical address of the queue's frame.
static BLK_QUEUE: AtomicUsize = AtomicUsize::new(0);

fn vio_read(base: usize, off: usize) -> u32 {
    unsafe { core::ptr::read_volatile(va(base + off) as *const u32) }
}

fn vio_write(base: usize, off: usize, v: u32) {
    unsafe { core::ptr::write_volatile(va(base + off) as *mut u32, v) }
}

// The queue lives in one physical frame, laid out by hand:
//     0      descriptor table   8 * 16 = 128 bytes
//   256      available ring
//   512      used ring
// All three well inside a page and correctly aligned.
const OFF_DESC: usize = 0;
const OFF_AVAIL: usize = 256;
const OFF_USED: usize = 512;

/// Find the block device and bring it up.
///
/// The negotiation order is fixed by the spec: acknowledge, claim the driver
/// role, agree features, confirm, set up the queue, then say we are ready.
/// Deviating silently gets ignored by the device rather than reported.
fn virtio_blk_init() -> bool {
    let mut base = 0;
    for i in 0..VIRTIO_MMIO_SLOTS {
        let b = VIRTIO_MMIO_BASE + i * VIRTIO_MMIO_STRIDE;
        if vio_read(b, R_MAGIC) == VIRTIO_MAGIC
            && vio_read(b, R_VERSION) == 2
            && vio_read(b, R_DEVICE_ID) == VIRTIO_ID_BLOCK
        {
            base = b;
            break;
        }
    }
    if base == 0 {
        println!("disk: scanning virtio slots --");
        for i in 0..VIRTIO_MMIO_SLOTS {
            let b = VIRTIO_MMIO_BASE + i * VIRTIO_MMIO_STRIDE;
            println!(
                "  {:#x}  magic={:#x} version={} device_id={}",
                b,
                vio_read(b, R_MAGIC),
                vio_read(b, R_VERSION),
                vio_read(b, R_DEVICE_ID)
            );
        }
        return false;
    }

    vio_write(base, R_STATUS, 0); // reset
    vio_write(base, R_STATUS, S_ACKNOWLEDGE);
    vio_write(base, R_STATUS, S_ACKNOWLEDGE | S_DRIVER);

    // Accept no optional features: the plain read/write path is all we need,
    // and every feature accepted is protocol we then have to implement.
    vio_write(base, R_DEVICE_FEATURES_SEL, 0);
    let _ = vio_read(base, R_DEVICE_FEATURES);
    vio_write(base, R_DRIVER_FEATURES_SEL, 0);
    vio_write(base, R_DRIVER_FEATURES, 0);
    vio_write(base, R_DRIVER_FEATURES_SEL, 1);
    vio_write(base, R_DRIVER_FEATURES, 1); // VIRTIO_F_VERSION_1, bit 32

    vio_write(base, R_STATUS, S_ACKNOWLEDGE | S_DRIVER | S_FEATURES_OK);
    if vio_read(base, R_STATUS) & S_FEATURES_OK == 0 {
        return false; // the device refused our feature set
    }

    let q = frame_alloc().expect("no frame for the virtio queue") as usize;
    unsafe { core::ptr::write_bytes(va(q) as *mut u8, 0, PAGE_SIZE) };
    BLK_QUEUE.store(q, Ordering::Relaxed);

    vio_write(base, R_QUEUE_SEL, 0);
    if (vio_read(base, R_QUEUE_NUM_MAX) as usize) < QUEUE_SIZE {
        return false;
    }
    vio_write(base, R_QUEUE_NUM, QUEUE_SIZE as u32);

    // PHYSICAL addresses. The device cannot follow our page tables.
    vio_write(base, R_QUEUE_DESC_LOW, (q + OFF_DESC) as u32);
    vio_write(base, R_QUEUE_DESC_HIGH, ((q + OFF_DESC) >> 32) as u32);
    vio_write(base, R_QUEUE_DRIVER_LOW, (q + OFF_AVAIL) as u32);
    vio_write(base, R_QUEUE_DRIVER_HIGH, ((q + OFF_AVAIL) >> 32) as u32);
    vio_write(base, R_QUEUE_DEVICE_LOW, (q + OFF_USED) as u32);
    vio_write(base, R_QUEUE_DEVICE_HIGH, ((q + OFF_USED) >> 32) as u32);
    vio_write(base, R_QUEUE_READY, 1);

    vio_write(
        base,
        R_STATUS,
        S_ACKNOWLEDGE | S_DRIVER | S_FEATURES_OK | S_DRIVER_OK,
    );
    BLK_BASE.store(base, Ordering::Relaxed);
    true
}

/// One sector in or out. `write` selects the direction.
///
/// Builds the three-descriptor chain the block device expects: a header it
/// reads, a data buffer, and a status byte it writes.
fn blk_rw(sector: u64, buf_phys: usize, write: bool) -> bool {
    let base = BLK_BASE.load(Ordering::Relaxed);
    let q = BLK_QUEUE.load(Ordering::Relaxed);
    if base == 0 {
        println!("disk: scanning virtio slots --");
        for i in 0..VIRTIO_MMIO_SLOTS {
            let b = VIRTIO_MMIO_BASE + i * VIRTIO_MMIO_STRIDE;
            println!(
                "  {:#x}  magic={:#x} version={} device_id={}",
                b,
                vio_read(b, R_MAGIC),
                vio_read(b, R_VERSION),
                vio_read(b, R_DEVICE_ID)
            );
        }
        return false;
    }

    // Header and status live in the tail of the queue frame -- already
    // physically contiguous and known to the device.
    let hdr_phys = q + 1024;
    let status_phys = q + 1024 + core::mem::size_of::<BlkReqHeader>();

    unsafe {
        core::ptr::write_volatile(
            va(hdr_phys) as *mut BlkReqHeader,
            BlkReqHeader {
                kind: if write { 1 } else { 0 },
                reserved: 0,
                sector,
            },
        );
        core::ptr::write_volatile(va(status_phys) as *mut u8, 0xff);

        let desc = va(q + OFF_DESC) as *mut VringDesc;

        // 0: the request header. The device READS this.
        *desc.add(0) = VringDesc {
            addr: hdr_phys as u64,
            len: core::mem::size_of::<BlkReqHeader>() as u32,
            flags: VRING_DESC_F_NEXT,
            next: 1,
        };
        // 1: the data. Device WRITES it on a read, reads it on a write.
        *desc.add(1) = VringDesc {
            addr: buf_phys as u64,
            len: SECTOR as u32,
            flags: VRING_DESC_F_NEXT | if write { 0 } else { VRING_DESC_F_WRITE },
            next: 2,
        };
        // 2: one status byte. The device WRITES it.
        *desc.add(2) = VringDesc {
            addr: status_phys as u64,
            len: 1,
            flags: VRING_DESC_F_WRITE,
            next: 0,
        };

        // Clip the ticket to the order rail: ring[idx % size] = head, then
        // bump idx. The device reads idx to know how far we have got.
        let avail = va(q + OFF_AVAIL) as *mut u16;
        let idx = core::ptr::read_volatile(avail.add(1));
        core::ptr::write_volatile(avail.add(2 + (idx as usize % QUEUE_SIZE)), 0);
        core::arch::asm!("fence ow, ow"); // descriptors visible before idx
        core::ptr::write_volatile(avail.add(1), idx.wrapping_add(1));

        vio_write(base, R_QUEUE_NOTIFY, 0);

        // Watch the done rail. Polling rather than interrupts: the PLIC is not
        // wired up yet, and a spin is honest for a single blocking read.
        let used = va(q + OFF_USED) as *const u16;
        let start = now();
        loop {
            core::arch::asm!("fence ir, ir");
            if core::ptr::read_volatile(used.add(1)) != idx {
                break;
            }
            if now() - start > TIMER_HZ {
                return false; // one second: the device is not answering
            }
        }

        vio_write(base, R_INTERRUPT_ACK, vio_read(base, R_INTERRUPT_STATUS));
        core::ptr::read_volatile(va(status_phys) as *const u8) == 0
    }
}

// ===========================================================================
// The shell -- milestone 14
//
// Every shell ever written resolves a NAME to a LOCATION. `cat notes.txt`
// means "walk the tree to this leaf." This one cannot: there is no tree, no
// leaf, and nothing has a location.
//
// So an argument is one of exactly two things:
//
//     a QUERY            find type=python created>100
//     an INDEX into the last result set   hide 2
//
// The numbered list is what replaces a path. It is ephemeral, contextual and
// meaningless a minute later -- which is fine, because you are looking at it
// while you use it. That is what "narrow to ~20 so a human can scan" was for.
//
// Friction is matched to consequence, deliberately: `hide` is silent because
// it destroys nothing, `evict` and `forget` announce exactly what they did.
// ===========================================================================

/// The last result set. THIS IS THE PATH REPLACEMENT.
static LAST: SpinLock<alloc::vec::Vec<ObjId>> = SpinLock::new(alloc::vec::Vec::new());

const MAX_LINE: usize = 256;

/// Read one line, echoing as it goes.
///
/// The terminal is raw: no line buffering, no echo, no editing. A keystroke
/// arrives as a single byte and NOTHING appears on screen unless this function
/// puts it there. Backspace is not an action, it is the byte 0x7f -- erasing
/// means printing "\x08 \x08": step left, paint a space over the character,
/// step left again. Everything a terminal seems to do for free is done here.
fn readline() -> alloc::string::String {
    let mut line = alloc::string::String::new();
    loop {
        let c = getchar_blocking();
        match c {
            b'\r' | b'\n' => {
                println!();
                return line;
            }
            0x7f | 0x08 => {
                if line.pop().is_some() {
                    puts("\x08 \x08");
                }
            }
            0x15 => {
                // Ctrl-U: kill the line.
                while line.pop().is_some() {
                    puts("\x08 \x08");
                }
            }
            // Printable, and there is room. A full line stops echoing rather
            // than growing without bound -- the guard is the only thing
            // between a held-down key and the heap.
            0x20..=0x7e if line.len() < MAX_LINE => {
                line.push(c as char);
                putchar(c);
            }
            _ => {} // control characters, and overflow, are dropped
        }
    }
}

/// Parse one predicate: `key=value`, `key~substring`, `key>n`, `key<n`.
///
/// Four operators is not a limitation, it is the whole retrieval model. Eq
/// narrows by kind, `~` by name, and `<`/`>` by time -- and time is the axis
/// that narrows hardest, which is exactly why attribute values are typed.
fn parse_pred(s: &str) -> Option<OwnedCond> {
    for (i, ch) in s.char_indices() {
        // Short names for the attributes worth typing constantly. Aliases are
        // safe here in a way a path alias never is: this expands a LABEL, and
        // if it expands to something nothing has, the query simply returns
        // nothing. There is no wrong directory to end up in.
        let key = match &s[..i] {
            "t" => "created_at",
            "n" => "name",
            k => k,
        };
        let rest = &s[i + ch.len_utf8()..];
        if key.is_empty() {
            continue;
        }
        return Some(match ch {
            '=' => OwnedCond::Eq(key.into(), rest.into()),
            '~' => OwnedCond::Contains(key.into(), rest.into()),
            '>' => OwnedCond::Between(key.into(), rest.parse::<i64>().ok()? + 1, i64::MAX),
            '<' => OwnedCond::Between(key.into(), i64::MIN, rest.parse::<i64>().ok()? - 1),
            _ => continue,
        });
    }
    None
}

/// Run a query and remember the answer as the new result set.
///
/// `hidden` selects which of the two views you get: the default drops hidden
/// objects, and asking for them IS the Cluttered folder. No special case in
/// the store -- it is one boolean over the same query.
fn shell_find(conds: &[OwnedCond], hidden: bool) {
    let ids: alloc::vec::Vec<ObjId> = {
        let store = STORE.lock();
        store
            .values()
            .filter(|o| conds.iter().all(|c| matches_owned(o, c)))
            .map(|o| o.id)
            .collect()
    };
    let ids: alloc::vec::Vec<ObjId> = ids
        .into_iter()
        .filter(|id| is_hidden(*id) == hidden)
        .collect();

    if ids.is_empty() {
        println!("  nothing matches");
    }
    for (i, id) in ids.iter().enumerate() {
        // Copy out what we need, THEN drop the lock. Holding STORE while
        // calling blob_len would be fine (different lock), but holding it
        // while printing is a long time to stop the world.
        let row = {
            let store = STORE.lock();
            store.get(id).map(|o| {
                let name = match o.attr("name") {
                    Some(Value::Text(t)) => t.clone(),
                    _ => alloc::string::String::from("(unnamed)"),
                };
                let kind = match o.attr("type") {
                    Some(Value::Text(t)) => t.clone(),
                    _ => alloc::string::String::from("-"),
                };
                let when = match o.attr("created_at") {
                    Some(Value::Int(n)) => *n,
                    _ => -1,
                };
                (name, kind, when, o.content)
            })
        };
        if let Some((name, kind, when, content)) = row {
            let bytes = blob_len(content);
            println!(
                "  {:>2}  {:<18} {:<9} t={:<6} {:>5}b  #{:012x}{}",
                i,
                name,
                kind,
                when,
                bytes,
                id.0 & 0xffff_ffff_ffff,
                if bytes == 0 { "  [evicted]" } else { "" }
            );
        }
    }
    *LAST.lock() = ids;
}

/// Turn "2" into the object it named in the last result set.
///
/// The error message matters: an index is only meaningful relative to a query
/// you just ran, so saying so is more useful than "not found".
fn shell_pick(arg: Option<&str>) -> Option<ObjId> {
    let n: usize = match arg.and_then(|a| a.parse().ok()) {
        Some(n) => n,
        None => {
            println!("  that wants a number from the last result list");
            return None;
        }
    };
    let last = LAST.lock();
    match last.get(n) {
        Some(id) => Some(*id),
        None => {
            println!("  no {} in the last result list ({} shown)", n, last.len());
            None
        }
    }
}

fn shell_show(id: ObjId) {
    let (attrs, content) = {
        let store = STORE.lock();
        match store.get(&id) {
            Some(o) => (
                o.attrs
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect::<alloc::vec::Vec<_>>(),
                o.content,
            ),
            None => {
                println!("  gone -- forgotten, not merely hidden");
                return;
            }
        }
    };
    println!("  id         #{:016x}", id.0);
    println!("  content    #{:016x}", content.0);
    for (k, v) in &attrs {
        match v {
            Value::Int(n) => println!("  {:<10} {}", k, n),
            Value::Text(t) => println!("  {:<10} {}", k, t),
            Value::Id(i) => println!("  {:<10} #{:016x}", k, i.0),
            Value::Bytes(b) => println!("  {:<10} {} bytes", k, b.len()),
        }
    }
    match BLOBS.lock().get(&content) {
        Some(b) => {
            print!("  ----\n  ");
            for &c in b.iter() {
                putchar(if (0x20..0x7f).contains(&c) || c == b'\n' {
                    c
                } else {
                    b'.'
                });
            }
            println!();
        }
        None => println!("  ---- bytes evicted; the record still means something"),
    }
}

fn shell_help() {
    println!("  find <preds>     narrow the store. no preds = everything");
    println!("  cluttered        the hidden ones. a saved query, not a folder");
    println!("  show N           N indexes the LAST result list");
    println!("  new <type> <name> <text...>");
    println!("  hide N | unhide N        clutter    -- reversible");
    println!("  evict N                  space      -- bytes go, record stays");
    println!("  forget N                 privacy    -- both go");
    println!("  save | load | stats | help");
    println!();
    println!("  preds:  type=python   name~brick   created_at>100   t<200");
    println!("          t = created_at, n = name");
    println!("  there are no filenames to type, because there are no files.");
}

fn shell_stats() {
    let (blocks, free) = heap_stats();
    println!(
        "  {} objects, {} blobs, {} claims, {} events",
        STORE.lock().len(),
        BLOBS.lock().len(),
        CLAIMS.lock().len(),
        EVENTS.lock().len()
    );
    // Free BLOCKS, not used bytes. A rising block count against a flat byte
    // count is fragmentation, and is the number worth watching.
    println!("  heap {} bytes free in {} block(s)", free, blocks);
    println!(
        "  up {} ticks ({}s)",
        TICKS.load(Ordering::Relaxed),
        TICKS.load(Ordering::Relaxed) / 100
    );
    // The proof that milestone 15 is real. If this climbs while you type,
    // nothing anywhere is polling the UART -- the chip is raising IRQ 10 and
    // the PLIC is routing it here.
    println!(
        "  {} bytes arrived by interrupt",
        CONSOLE_IRQS.load(Ordering::Relaxed)
    );
}

/// The shell thread. Never returns; it is the machine's front door.
extern "C" fn shell() -> ! {
    println!();
    println!("LeBOS shell. There are no paths. Type `help`.");

    loop {
        print!("> ");
        let line = readline();
        let mut words = line.split_whitespace();
        let cmd = match words.next() {
            Some(c) => c,
            None => continue,
        };
        let rest: alloc::vec::Vec<&str> = words.collect();

        match cmd {
            "help" | "?" => shell_help(),

            "find" | "ls" => {
                let mut conds = alloc::vec::Vec::new();
                let mut bad = false;
                for w in &rest {
                    match parse_pred(w) {
                        Some(c) => conds.push(c),
                        None => {
                            println!("  `{}` is not a predicate -- try type=python", w);
                            bad = true;
                        }
                    }
                }
                if !bad {
                    shell_find(&conds, false);
                }
            }

            "cluttered" => shell_find(&[], true),

            "new" => {
                if rest.len() < 3 {
                    println!("  new <type> <name> <text...>");
                    continue;
                }
                let text = rest[2..].join(" ");
                let id = store_create(
                    text.clone().into_bytes(),
                    alloc::vec![
                        ("name".into(), Value::Text(rest[1].into())),
                        ("type".into(), Value::Text(rest[0].into())),
                        (
                            "created_at".into(),
                            Value::Int(TICKS.load(Ordering::Relaxed) as i64)
                        ),
                    ],
                );
                log_event(id);
                println!("  #{:016x}", id.0);
            }

            "show" | "cat" => {
                if let Some(id) = shell_pick(rest.first().copied()) {
                    log_event(id);
                    shell_show(id);
                }
            }

            // Silent on success: hiding destroys nothing, so it should not
            // demand attention. The three verbs get three different volumes.
            "hide" => {
                if let Some(id) = shell_pick(rest.first().copied()) {
                    hide(id, true);
                }
            }
            "unhide" => {
                if let Some(id) = shell_pick(rest.first().copied()) {
                    hide(id, false);
                }
            }
            "evict" => {
                if let Some(id) = shell_pick(rest.first().copied()) {
                    evict(id);
                    println!("  bytes gone. the record still answers questions.");
                }
            }
            "forget" => {
                if let Some(id) = shell_pick(rest.first().copied()) {
                    forget(id);
                    println!("  gone. that was the point.");
                }
            }

            "save" => {
                if store_save() {
                    println!("  written to disk");
                } else {
                    println!("  SAVE FAILED");
                }
            }
            "load" => match store_load() {
                Some((b, o, c)) => println!("  replayed {} blobs, {} objects, {} claims", b, o, c),
                None => println!("  no LeBOS store on that disk"),
            },

            "stats" => shell_stats(),

            "ps" => {
                let threads = THREADS.lock();
                for (i, t) in threads.iter().enumerate() {
                    let state = match t.state {
                        ThreadState::Runnable => alloc::string::String::from("runnable"),
                        ThreadState::Sleeping(c) => alloc::format!("sleeping on {:#x}", c),
                        ThreadState::Zombie(c) => alloc::format!("zombie (exit {})", c),
                        ThreadState::Free => alloc::string::String::from("-- free slot --"),
                    };
                    println!("  {:>2}  {:<8} {}", i, t.name, state);
                }
            }

            // Spawn the embedded program and collect it. Not exec-by-query
            // yet -- that is 19, and it needs the program to be an OBJECT.
            "run" => {
                process_spawn("child", b'C');
                match thread_wait() {
                    Some((name, code)) => println!("  {} exited with {}", name, code),
                    None => println!("  nothing to wait for"),
                }
            }

            // The commands people will reach for out of muscle memory. Every
            // one of them is a path operation, and none of them can mean
            // anything here -- so say why rather than "command not found".
            "cd" | "pwd" | "mkdir" | "rmdir" | "touch" | "mv" | "cp" | "rm" => {
                println!(
                    "  `{}` needs somewhere to put things. there is nowhere.",
                    cmd
                );
                println!(
                    "  nothing is anywhere. describe it instead: find name~{}",
                    "todo"
                );
            }

            // "if you see me use ubuntu, i might say hi,
            //   but if you see me using arch, i'm a talkative guy"
            //        -- "Too Late I Already Deleted Windows", parody, via Seb
            //
            // Which is the correct song to quote in the one shell on earth
            // where deleting Windows would not be enough -- the paths would
            // still be there.
            "ubuntu" => println!("  hi"),
            "arch" => println!("  blah blah blah"),

            _ => println!("  no such command: {}. try `help`.", cmd),
        }
    }
}

// ===========================================================================
// Persistence -- milestone 13b
//
// The store is already append-only and immutable, so the on-disk format writes
// itself: a header, then a stream of records. Recovery is "replay it".
//
// That is not laziness, it is what buys crash safety almost for free. A torn
// record at the end fails to parse and gets discarded, and there is no
// half-updated structure to repair because nothing is ever updated. No fsck.
// Journalling filesystems bolt a log onto a mutable structure to get this;
// there is no mutable structure here to bolt it to.
//
// Current version serialises the whole store on save. True incremental
// appending -- only writing records that are new -- is the next refinement,
// and the format is already shaped for it.
// ===========================================================================

/// First four bytes of every LeBOS disk, forever.
///
/// 0xF01DAB1E spells FOLDABLE, on an operating system with no folders. In the
/// tradition of 0xCAFEBABE (Java, named after coffee) and 0xD00DFEED (the
/// device tree you parsed at milestone 5) -- a magic number should be a pun on
/// what the format is.
///
/// It is also not quite a lie: the files app's folders exist as saved queries.
const LEBOS_MAGIC: u32 = 0xF01D_AB1E;
const LEBOS_VERSION: u32 = 1;

const REC_BLOB: u8 = 1;
const REC_OBJECT: u8 = 2;
const REC_CLAIM: u8 = 3;
const REC_END: u8 = 0;

/// Appends to a growing byte stream. The mirror of `Reader`.
struct Writer(alloc::vec::Vec<u8>);

impl Writer {
    fn new() -> Self {
        Writer(alloc::vec::Vec::new())
    }
    fn u8v(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn blob(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.0.extend_from_slice(b);
    }
    fn value(&mut self, v: &Value) {
        match v {
            Value::Int(n) => {
                self.u8v(0);
                self.i64(*n);
            }
            Value::Text(t) => {
                self.u8v(1);
                self.blob(t.as_bytes());
            }
            Value::Id(i) => {
                self.u8v(2);
                self.u64(i.0);
            }
            Value::Bytes(b) => {
                self.u8v(3);
                self.blob(b);
            }
        }
    }
}

fn read_value(r: &mut Reader) -> Option<Value> {
    Some(match r.u8()? {
        0 => Value::Int(r.i64()?),
        1 => Value::Text(r.text()?),
        2 => Value::Id(ObjId(u64::from_le_bytes({
            let mut a = [0u8; 8];
            a.copy_from_slice(r.take(8)?);
            a
        }))),
        3 => {
            let n = r.u32()? as usize;
            Value::Bytes(r.take(n)?.to_vec())
        }
        _ => return None,
    })
}

fn read_u64(r: &mut Reader) -> Option<u64> {
    let mut a = [0u8; 8];
    a.copy_from_slice(r.take(8)?);
    Some(u64::from_le_bytes(a))
}

/// Turn the whole store into one byte stream.
fn serialize_store() -> alloc::vec::Vec<u8> {
    let mut w = Writer::new();

    for (id, bytes) in BLOBS.lock().iter() {
        w.u8v(REC_BLOB);
        w.u64(id.0);
        w.blob(bytes);
    }
    for (id, o) in STORE.lock().iter() {
        w.u8v(REC_OBJECT);
        w.u64(id.0);
        w.u64(o.content.0);
        w.u32(o.attrs.len() as u32);
        for (k, v) in &o.attrs {
            w.blob(k.as_bytes());
            w.value(v);
        }
    }
    for c in CLAIMS.lock().iter() {
        w.u8v(REC_CLAIM);
        w.u64(c.at);
        w.u64(c.id.0);
        w.blob(c.key.as_bytes());
        w.value(&c.value);
    }
    w.u8v(REC_END);
    w.0
}

/// Replay a byte stream back into the store.
///
/// Anything malformed stops the replay rather than failing the boot -- a torn
/// record at the end of a log is the expected result of losing power mid-write,
/// not a corrupt disk. Everything before it is still good.
fn deserialize_store(buf: &[u8]) -> (usize, usize, usize) {
    let mut r = Reader::new(buf);
    let (mut blobs, mut objects, mut claims) = (0, 0, 0);

    loop {
        match r.u8() {
            Some(REC_BLOB) => {
                let (id, n) = match (read_u64(&mut r), r.u32()) {
                    (Some(a), Some(b)) => (a, b as usize),
                    _ => break,
                };
                match r.take(n) {
                    Some(b) => {
                        BLOBS.lock().insert(ObjId(id), b.to_vec());
                        blobs += 1;
                    }
                    None => break,
                }
            }
            Some(REC_OBJECT) => {
                let (id, content, na) = match (read_u64(&mut r), read_u64(&mut r), r.u32()) {
                    (Some(a), Some(b), Some(c)) => (a, b, c as usize),
                    _ => break,
                };
                if na > buf.len() {
                    break;
                }
                let mut attrs = alloc::vec::Vec::with_capacity(na);
                let mut ok = true;
                for _ in 0..na {
                    match (r.text(), read_value(&mut r)) {
                        (Some(k), Some(v)) => attrs.push((k, v)),
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
                if !ok {
                    break;
                }
                STORE.lock().insert(
                    ObjId(id),
                    Object {
                        id: ObjId(id),
                        content: ObjId(content),
                        attrs,
                    },
                );
                objects += 1;
            }
            Some(REC_CLAIM) => {
                let (at, id) = match (read_u64(&mut r), read_u64(&mut r)) {
                    (Some(a), Some(b)) => (a, b),
                    _ => break,
                };
                let key = match r.text() {
                    Some(k) => k,
                    None => break,
                };
                let value = match read_value(&mut r) {
                    Some(v) => v,
                    None => break,
                };
                // Claim keys are &'static str in memory; map the known ones
                // back and drop anything unrecognised.
                let k: &'static str = match key.as_str() {
                    "hidden" => "hidden",
                    "evicted" => "evicted",
                    "forgotten" => "forgotten",
                    _ => continue,
                };
                CLAIMS.lock().push(Claim {
                    at,
                    id: ObjId(id),
                    key: k,
                    value,
                });
                claims += 1;
            }
            _ => break, // REC_END, or a torn record: stop cleanly
        }
    }
    (blobs, objects, claims)
}

/// Write the store to disk, one sector at a time through a bounce buffer.
///
/// A single frame is used because DMA needs physically contiguous memory and
/// one page is the largest run the frame allocator can promise.
fn store_save() -> bool {
    let data = serialize_store();
    let buf = match frame_alloc() {
        Some(b) => b as usize,
        None => return false,
    };
    let p = va(buf) as *mut u8;

    // Sector 0: the header.
    unsafe {
        core::ptr::write_bytes(p, 0, SECTOR);
        core::ptr::write_volatile(p as *mut u32, LEBOS_MAGIC);
        core::ptr::write_volatile((p as *mut u32).add(1), LEBOS_VERSION);
        core::ptr::write_volatile((p as *mut u64).add(1), data.len() as u64);
    }
    let mut ok = blk_rw(0, buf, true);

    // Sectors 1..: the records.
    let mut off = 0;
    let mut sector = 1u64;
    while off < data.len() && ok {
        let n = core::cmp::min(SECTOR, data.len() - off);
        unsafe {
            core::ptr::write_bytes(p, 0, SECTOR);
            core::ptr::copy_nonoverlapping(data.as_ptr().add(off), p, n);
        }
        ok = blk_rw(sector, buf, true);
        off += n;
        sector += 1;
    }

    frame_free(buf as *mut u8);
    ok
}

/// Read the store back off disk. Returns None if this is not a LeBOS disk.
fn store_load() -> Option<(usize, usize, usize)> {
    let buf = frame_alloc()? as usize;
    let p = va(buf) as *mut u8;

    if !blk_rw(0, buf, false) {
        frame_free(buf as *mut u8);
        return None;
    }
    let (magic, version, len) = unsafe {
        (
            core::ptr::read_volatile(p as *const u32),
            core::ptr::read_volatile((p as *const u32).add(1)),
            core::ptr::read_volatile((p as *const u64).add(1)) as usize,
        )
    };
    if magic != LEBOS_MAGIC || version != LEBOS_VERSION || len > 32 * 1024 * 1024 {
        frame_free(buf as *mut u8);
        return None;
    }

    let mut data = alloc::vec::Vec::with_capacity(len);
    let mut sector = 1u64;
    while data.len() < len {
        if !blk_rw(sector, buf, false) {
            break;
        }
        let n = core::cmp::min(SECTOR, len - data.len());
        for i in 0..n {
            data.push(unsafe { core::ptr::read_volatile(p.add(i)) });
        }
        sector += 1;
    }

    frame_free(buf as *mut u8);
    Some(deserialize_store(&data))
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
    //
    // Through the higher-half alias: the LIST holds physical addresses, since
    // that is what callers want, but the kernel can only dereference virtual
    // ones once the identity map is gone. This read was unqualified for the
    // whole project and worked only because the identity map existed -- it
    // broke the first time anything allocated a frame after relocation.
    let next = unsafe { core::ptr::read(va(head) as *const usize) };
    FREE_LIST.store(next, Ordering::Relaxed);

    Some(head as *mut u8)
}

/// Put a page back on the free list.
fn frame_free(page: *mut u8) {
    let addr = page as usize;
    assert!(
        addr.is_multiple_of(PAGE_SIZE),
        "frame_free: address not page aligned"
    );

    // Write the current head into this page's first 8 bytes, then point the
    // list at this page. Classic linked-list push -- through the higher-half
    // alias, for the same reason as frame_alloc.
    let head = FREE_LIST.load(Ordering::Relaxed);
    unsafe { core::ptr::write(va(addr) as *mut usize, head) };
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

/// Must match the layout asserted in entry.S. 34 words = 272 bytes, already a
/// multiple of 16, so no padding is needed.
#[repr(C)]
pub struct TrapFrame {
    pub x: [usize; 32],
    /// `sstatus` at the moment of the trap. Its SPP bit says which privilege
    /// level to return to.
    pub sstatus: usize,
    /// `sepc` at the moment of the trap -- where to resume.
    ///
    /// The handler EDITS this to step over a faulting instruction or past an
    /// `ecall`, and `trap_entry` writes it back to the CSR on the way out.
    pub sepc: usize,
}

#[no_mangle]
extern "C" fn trap_handler(frame: &mut TrapFrame) {
    let scause: usize;
    let stval: usize;

    unsafe {
        core::arch::asm!("csrr {}, scause", out(reg) scause);
        core::arch::asm!("csrr {}, stval",  out(reg) stval);
    }

    // sepc comes from the FRAME, never from the live CSR. Between this trap and
    // its return the scheduler may run other threads that trap and overwrite
    // it; reading it live would give another thread's program counter.
    let sepc = frame.sepc;

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

        // Ticking at 100 Hz. The counter is the kernel's notion of uptime;
        // it used to announce every second, which was proof the timer worked
        // and is now a machine talking over its user. `stats` reports it on
        // demand instead.
        TICKS.fetch_add(1, Ordering::Relaxed);

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

    if is_interrupt && code == 9 {
        // Supervisor EXTERNAL interrupt. Unlike the timer, this does not say
        // which device -- the hart has one wire for all of them. Ask the PLIC.
        let irq = plic_claim();
        if irq == IRQ_UART0 {
            console_interrupt();
        }
        // Complete even an unrecognised or zero irq. Claiming without
        // completing leaves that source in-flight forever, so a device we do
        // not handle yet would take its interrupt line to the grave.
        if irq != 0 {
            plic_complete(irq);
        }
        return;
    }

    // scause 8 -- ecall from USER mode. This is a syscall, and it is the first
    // exception that is not a bug: a user program legitimately asking for
    // something.
    if !is_interrupt && code == 8 {
        let num = frame.x[17]; // a7 = which syscall
        let arg0 = frame.x[10]; // a0
        let arg1 = frame.x[11]; // a1

        // Which address space is the caller in? Every pointer they hand us is
        // only meaningful in their own city, so this is needed by every
        // syscall that takes one.
        let satp: usize;
        unsafe { core::arch::asm!("csrr {}, satp", out(reg) satp) };
        let cur_root = (satp & 0xfff_ffff_ffff) << 12;

        let ret: usize = match num {
            // write(ptr, len)
            //
            // The pointer is checked before it is touched. This is the office
            // worker reading the request slip and refusing an absurd one.
            1 => {
                // Validated against the address space that is CURRENTLY
                // active, not the kernel's -- a pointer only means something
                // in the city its owner lives in.
                if !user_range_ok(cur_root, arg0, arg1) {
                    println!(
                        "user: REFUSED write({:#x}, {}) -- not user-readable memory",
                        arg0, arg1
                    );
                    // -1 as a usize, the conventional "no" for a syscall.
                    usize::MAX
                } else {
                    for i in 0..arg1 {
                        let b = unsafe { copy_from_user(arg0 + i) };
                        putchar(b);
                    }
                    arg1
                }
            }
            // exit(code)
            0 => {
                println!("user: program exited with code {}", arg0);
                // Never returns. Milestone 14 returned here and let the
                // program fall into its own spin loop -- a corpse the
                // scheduler kept handing the CPU to. Now the address space
                // is dismantled and the thread leaves the run queue for good.
                thread_exit(arg0 as i32);
            }
            // sbrk(n) -> the OLD break, or usize::MAX on refusal.
            //
            // "Move my fence out by n bytes." Only the kernel can do it,
            // because only the kernel writes page tables.
            //
            // The return value is where the fence USED TO BE, which trips
            // everyone the first time -- but that is the START ADDRESS of the
            // land just enclosed, and the new far end is of no interest to the
            // caller. Same convention as Unix has used since the beginning.
            4 => {
                let cur = CURRENT.load(Ordering::Relaxed);
                let old = THREADS.lock()[cur].brk;

                if arg0 == 0 {
                    // Asking for nothing is how a program finds out where its
                    // fence currently stands.
                    old
                } else if arg0 > USER_HEAP_MAX || old + arg0 > USER_BASE + USER_HEAP_MAX {
                    // A request that could not possibly be honest. Refusing is
                    // not politeness -- an untrusted program asking for
                    // 2^63 bytes must not be allowed to wrap the addition.
                    println!("user: REFUSED sbrk({}) -- too large", arg0);
                    usize::MAX
                } else {
                    let new = old + arg0;
                    let mut page = align_up(old, PAGE_SIZE);
                    let end = align_up(new, PAGE_SIZE);
                    let root = va(cur_root) as *mut usize;
                    let mut ok = true;

                    // Only whole pages can be mapped, so a partial page at the
                    // old break is already there and gets skipped.
                    while page < end {
                        match frame_alloc() {
                            Some(f) => {
                                // Zero it. A fresh page carries whatever the
                                // last process left behind, and handing that
                                // to a new one leaks its memory across a
                                // security boundary.
                                unsafe {
                                    core::ptr::write_bytes(va(f as usize) as *mut u8, 0, PAGE_SIZE)
                                };
                                map(root, page, f as usize, PTE_R | PTE_W | PTE_U | RSW_DATA);
                            }
                            None => {
                                ok = false;
                                break;
                            }
                        }
                        page += PAGE_SIZE;
                    }

                    if ok {
                        unsafe { core::arch::asm!("sfence.vma") };
                        THREADS.lock()[cur].brk = new;
                        old
                    } else {
                        println!("user: sbrk({}) -- out of memory", arg0);
                        usize::MAX
                    }
                }
            }

            // create(buf, len) -> object id, or usize::MAX on refusal.
            //
            // One pointer to validate. Everything nested lives inside the
            // buffer, where it is copied into kernel memory before being
            // parsed and cannot change while we look at it.
            2 => {
                if !user_range_ok(cur_root, arg0, arg1) || arg1 > 64 * 1024 {
                    println!("store: REFUSED create -- bad buffer");
                    usize::MAX
                } else {
                    let buf = unsafe { copy_in(arg0, arg1) };
                    match parse_create(&buf) {
                        Some((content, attrs)) => {
                            let id = store_create(content, attrs);
                            log_event(id);
                            id.0 as usize
                        }
                        None => {
                            println!("store: REFUSED create -- malformed request");
                            usize::MAX
                        }
                    }
                }
            }

            // query(buf, len, out_ptr, out_cap) -> how many ids written
            3 => {
                let out_ptr = frame.x[12]; // a2
                let out_cap = frame.x[13]; // a3
                if !user_range_ok(cur_root, arg0, arg1)
                    || !user_range_writable(cur_root, out_ptr, out_cap * 8)
                {
                    println!("store: REFUSED query -- bad buffer");
                    usize::MAX
                } else {
                    let buf = unsafe { copy_in(arg0, arg1) };
                    match parse_query(&buf) {
                        Some(conds) => {
                            let hits = store_query_owned(&conds);
                            let n = core::cmp::min(hits.len(), out_cap);
                            unsafe {
                                core::arch::asm!("csrs sstatus, {}", in(reg) 1_usize << 18);
                                for (i, h) in hits.iter().take(n).enumerate() {
                                    core::ptr::write_volatile((out_ptr + i * 8) as *mut u64, h.0);
                                }
                                core::arch::asm!("csrc sstatus, {}", in(reg) 1_usize << 18);
                            }
                            for h in hits.iter().take(n) {
                                log_event(*h);
                            }
                            n
                        }
                        None => {
                            println!("store: REFUSED query -- malformed request");
                            usize::MAX
                        }
                    }
                }
            }

            // read_char() -> one byte from the console. BLOCKS.
            //
            // The kernel hands over bytes and nothing else. Echo, backspace,
            // line editing and the notion of a "line" all live in userspace,
            // because every one of them is policy. A program that wants raw
            // keystrokes gets raw keystrokes.
            5 => getchar_blocking() as usize,

            // get(id, buf, cap) -> bytes written, or usize::MAX.
            //
            // Serialises one object into the caller's buffer: its content id,
            // its attributes, and its bytes. One packed buffer out, exactly as
            // create takes one packed buffer in -- so there is a single range
            // to validate rather than a nest of pointers to chase.
            6 => {
                let cap = frame.x[12]; // a2
                if !user_range_writable(cur_root, arg1, cap) {
                    println!("store: REFUSED get -- buffer not user-writable");
                    usize::MAX
                } else {
                    let id = ObjId(arg0 as u64);
                    match serialize_object(id) {
                        Some(buf) if buf.len() <= cap => {
                            log_event(id);
                            for (i, b) in buf.iter().enumerate() {
                                unsafe { copy_to_user(arg1 + i, *b) };
                            }
                            buf.len()
                        }
                        // Too big is not an error, it is an answer: ask again
                        // with a buffer this size. Truncating silently would
                        // hand back a half-object that still parses.
                        Some(buf) => buf.len() | (1 << 63),
                        None => usize::MAX,
                    }
                }
            }

            // verb(id, which) -> 0, or usize::MAX.
            //
            //   0 unhide   1 hide   2 evict   3 forget
            //
            // The three that are not `hide` are the three that "delete" was
            // always hiding: clutter, space and privacy are different problems
            // and this is where they stop sharing a word.
            7 => {
                let id = ObjId(arg0 as u64);
                if !STORE.lock().contains_key(&id) {
                    usize::MAX
                } else {
                    // Every arm returns a value. The unknown case REFUSES --
                    // it must never panic, because the caller is an untrusted
                    // program and `arg1` is whatever it felt like putting in
                    // a1. A syscall that a user program can crash the kernel
                    // with is not a syscall, it is a denial of service.
                    match arg1 {
                        0 => {
                            hide(id, false);
                            0
                        }
                        1 => {
                            hide(id, true);
                            0
                        }
                        2 => {
                            evict(id);
                            0
                        }
                        3 => {
                            forget(id);
                            0
                        }
                        _ => {
                            println!("store: REFUSED verb {} -- no such verb", arg1);
                            usize::MAX
                        }
                    }
                }
            }

            // save() -> 0, or usize::MAX. Flush the store to disk.
            8 => {
                if store_save() {
                    0
                } else {
                    usize::MAX
                }
            }

            _ => {
                println!("user: unknown syscall {}", num);
                usize::MAX
            }
        };

        // The answer goes into the SAVED a0, not the live register: trap_entry
        // is about to overwrite all 32 registers from this frame on the way
        // out, so anything left in a live register would be wiped.
        frame.x[10] = ret;

        // A syscall is an EXCEPTION -- the ecall executed and did its whole
        // job. Not advancing sepc would re-execute it forever. Interrupts get
        // the opposite treatment, which is the branch above.
        frame.sepc = sepc + 4;
        return;
    }

    // An exception that is not a syscall. Every one of these is a kernel bug
    // or a misbehaving user program.
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
