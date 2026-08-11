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

use core::fmt::{self, Write};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Timer interrupts since boot. Atomic rather than `static mut` so it stays
/// correct once there is more than one hart.
static TICKS: AtomicU64 = AtomicU64::new(0);

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

/// First Rust code to run. Called from `_start` with the two values OpenSBI
/// left in a0/a1.
///
/// `extern "C"` gives it the C ABI so assembly can call it, and `#[no_mangle]`
/// keeps the symbol literally named `kmain` so the `call kmain` resolves.
///
/// It must never return -- hence `-> !`.
#[no_mangle]
pub extern "C" fn kmain(hartid: usize, dtb: *const u8) -> ! {
    // Point stvec at the assembly trampoline in entry.S, not at trap_handler
    // directly: stvec's low two bits are a mode field, so the address must be
    // 4-byte aligned, and Rust cannot align a function.
    unsafe {
        core::arch::asm!("csrw stvec, {}", in(reg) trap_entry as *const () as usize);
    }

    println!("LeBOS booting");
    println!("hart {} | dtb at {:#x}", hartid, dtb as usize);

    // Ask the machine how much RAM it has instead of assuming.
    let (ram_base, ram_size) = dtb_memory(dtb).expect("no memory node in device tree");
    println!(
        "ram: {:#x}..{:#x} ({} MiB), per device tree",
        ram_base,
        ram_base + ram_size,
        ram_size / 1024 / 1024
    );

    let free = frame_init(ram_base + ram_size);
    println!(
        "memory: {} frames free ({} MiB)",
        free,
        free * PAGE_SIZE / 1024 / 1024
    );

    let a = frame_alloc().unwrap();
    let b = frame_alloc().unwrap();
    println!("  alloc a={:p}  b={:p}", a, b);
    frame_free(a);
    let c = frame_alloc().unwrap();
    println!("  freed a, next alloc c={:p}", c);

    // Schedule the first timer interrupt one tick out.
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

    loop {
        // Wait For Interrupt: idles the core instead of spinning it at 100%.
        unsafe { core::arch::asm!("wfi") };
    }
}

fn putchar(c: u8) {
    const UART0: *mut u8 = 0x1000_0000 as *mut u8;

    if c == b'\n' {
        putchar(b'\r');
    }

    unsafe {
        core::ptr::write_volatile(UART0, c);
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
    /// Defined by linker.ld as `__kernel_end = .`. There is no data here --
    /// the symbol's ADDRESS is the value we want, which is why the code below
    /// takes `&__kernel_end` rather than reading it.
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

        // Deliberately do NOT touch sepc. An interrupt means the instruction
        // at sepc has not run yet -- skipping it would silently drop a good
        // instruction. Only exceptions get stepped over.
        return;
    }

    println!("*** TRAP ***");
    println!(
        "  scause {:#x}  sepc {:#x}  stval {:#x}",
        scause, sepc, stval
    );
    println!("  ra {:#x}  sp {:#x}", frame.x[1], frame.x[2]);

    // Step over the faulting instruction, or we return straight back onto it
    // and trap forever. RISC-V instructions are 4 bytes UNLESS the low two
    // bits are something other than 0b11, which marks a 2-byte compressed
    // instruction. `unimp` is compressed -- it is literally 0x0000.
    let insn = unsafe { core::ptr::read_volatile(sepc as *const u16) };
    let len = if insn & 0b11 == 0b11 { 4 } else { 2 };

    unsafe {
        core::arch::asm!("csrw sepc, {}", in(reg) sepc + len);
    }
}
