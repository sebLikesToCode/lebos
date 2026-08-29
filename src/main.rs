// No standard library. `std` assumes an operating system underneath it --
// files, threads, a heap, a way to exit. There is nothing underneath this.
#![no_std]
// No Rust `main` either. `main` is called by a runtime that sets up argv and
// the environment before it. Our entry point is `_start`, in assembly.
#![no_main]

use core::panic::PanicInfo;
use core::fmt::{self, Write};
use core::alloc::{GlobalAlloc, Layout};
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicUsize, Ordering};
use crate::hw::{Trap, HIGH_BASE, memory_loop, Perm, console_relocate, map_devices, paging_on, timer_reset, timer_on, traps_on, unmap_low, enter_high, idle};

extern "C" {
    fn trap_entry();
}

extern crate alloc;

extern "C" {
    static __kernel_end: u8;
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
}

fn _print(args: fmt::Arguments) {
    let _ = Uart.write_fmt(args);
}

const BANNER: &str = include_str!("banner.txt");


static FREE_LIST: AtomicUsize = AtomicUsize::new(0);

static FREE_HEAD: AtomicUsize = AtomicUsize::new(0);

macro_rules! print {
    ($($arg:tt)*) => { _print(format_args!($($arg)*)) };
}

macro_rules! println {
    ()            => { print!("\n") };
    ($($arg:tt)*) => { print!("{}\n", format_args!($($arg)*)) };
}

// entry.S, pasted in at compile time. It has to be assembly: there is no valid
// stack when it runs, and Rust cannot function without one.
mod hw;

/// The first Rust that ever runs.
///
/// `extern "C"` because entry.S calls it, and the C calling convention is the
/// one both sides already agree on. `#[no_mangle]` so the name stays exactly
/// `kmain` for `call kmain` to find.
///
/// `-> !` means this never returns. There is nothing to return TO.
#[no_mangle]
pub extern "C" fn kmain(_hartid: usize, _dtb: *const u8) -> ! {
    // NOTHING MAY PRINT BEFORE THE JUMP.
    //
    // The kernel is linked at its HIGH address, so the ~436 absolute addresses
    // the linker baked into .rodata -- vtables, which is what println!
    // dispatches through -- are all high. They do not resolve until the higher
    // half exists. The banner therefore prints in kmain_high, not here.

    let kernel_end = core::ptr::addr_of!(__kernel_end) as usize;
    let text_start = core::ptr::addr_of!(__text_start) as usize;
    let text_end = core::ptr::addr_of!(__text_end) as usize;
    let rodata_start = core::ptr::addr_of!(__rodata_start) as usize;
    let rodata_end = core::ptr::addr_of!(__rodata_end) as usize;
    let data_start = core::ptr::addr_of!(__data_start) as usize;

    frame_init(kernel_end, 0x8700_0000);

    let un_tableau = frame_alloc().unwrap();
    unsafe { core::ptr::write_bytes(un_tableau as *mut u8, 0, 4096) };

    memory_loop(un_tableau, text_start, text_end, 0, Perm::Code);
    memory_loop(un_tableau, rodata_start, rodata_end, 0, Perm::Rodata);
    memory_loop(un_tableau, data_start, kernel_end, 0, Perm::Data);
    map_devices(un_tableau, 0);

    memory_loop(un_tableau, text_start, text_end, HIGH_BASE, Perm::Code);
    memory_loop(un_tableau, rodata_start, rodata_end, HIGH_BASE, Perm::Rodata);
    memory_loop(un_tableau, data_start, kernel_end, HIGH_BASE, Perm::Data);
    map_devices(un_tableau, HIGH_BASE);
    memory_loop(un_tableau, kernel_end, 0x8800_0000, HIGH_BASE, Perm::Data);

    paging_on(un_tableau);

    enter_high(kmain_high as *const usize as usize, un_tableau);
}

extern "C" fn kmain_high(tabletop: usize) -> ! {
    console_relocate(HIGH_BASE);

    // Safe to print from here: the code is high, so the vtables resolve.
    println!("{}", BANNER);

    traps_on();
    timer_reset();
    timer_on();

    unmap_low(tabletop);

    heap_init(0x8800_0000 - 0x10_0000 + HIGH_BASE, 0x10_0000);

    loop {
        // Wait For Interrupt: parks the core instead of spinning it at 100%.
        idle()
    }
}

fn walk_heap() -> (usize, usize) {
    let mut block: usize = FREE_HEAD.load(Ordering::Relaxed);
    let mut next: usize;
    let mut av_size: usize;

    let mut heap_size: usize = 0;
    let mut heap_blocks: usize = 0;

    while block != 0 {
        next = unsafe { read_volatile((block + 8) as *const usize) };
        av_size = unsafe { read_volatile((block + 0) as *const usize) };

        heap_blocks += 1;
        heap_size += av_size;

        block = next;
    }
    return (heap_blocks, heap_size);
}

fn alloc(size: usize) -> usize {
    let mut block: usize = FREE_HEAD.load(Ordering::Relaxed);
    let mut next: usize;
    let mut av_size: usize;

    while block != 0 {
        next = unsafe { read_volatile((block + 8) as *const usize) };
        av_size = unsafe { read_volatile((block + 0) as *const usize) };
        if size > av_size {
            block = next;
        } else {
            unsafe { write_volatile((block + 0) as *mut usize, av_size - size); }
            return block + (av_size - size);
        }
    }
    return 0;
}

fn dealloc(addr: usize, size: usize) {
    let next_addr: usize = addr + size;
    let mut block = FREE_HEAD.load(Ordering::Relaxed);
    let mut prev: usize = 0;

    while block != 0 && block < addr {
        prev = block;
        block = unsafe { read_volatile((block + 8) as *const usize) };
    }

    let touches_before = prev != 0 && prev + unsafe { read_volatile((prev + 0) as *const usize) } == addr;
    let touches_after = block != 0 && next_addr == block;

    if touches_after && touches_before {
        unsafe {
            write_volatile((prev + 0) as *mut usize, read_volatile((prev + 0) as *const usize) + size + read_volatile((block + 0) as *const usize));
            write_volatile((prev + 8) as *mut usize, read_volatile((block + 8) as *const usize));
        }
    } else if touches_after {
        unsafe {
            write_volatile((addr + 0) as *mut usize, size + read_volatile((block + 0) as *const usize));
            write_volatile((addr + 8) as *mut usize, read_volatile((block + 8) as *const usize));

            if prev != 0 {
                write_volatile((prev + 8) as *mut usize, addr);
            } else {
                FREE_HEAD.store(addr, Ordering::Relaxed);
            }
        }
    } else if touches_before {
        unsafe {
            write_volatile((prev + 0) as *mut usize, read_volatile((prev + 0) as *const usize) + size);
        }
    } else {
        unsafe {
            write_volatile((addr + 0) as *mut usize, size);
            write_volatile((addr + 8) as *mut usize, block);

            if prev != 0 {
                write_volatile((prev + 8) as *mut usize, addr);
            } else {
                FREE_HEAD.store(addr, Ordering::Relaxed);
            }
        }
    }
}


fn heap_init(start: usize, size: usize) {
    unsafe {
        write_volatile((start + 0) as *mut usize, size);
        write_volatile((start + 8) as *mut usize, 0);
    }
    FREE_HEAD.store(start, Ordering::Relaxed);
}

// prints a byte literal character. if it is a newline, isers \r (cairrage return) to return to the start of the next line.
// uses write volatile because rust would delete it in the end

// puts a string of byte literals down with putchar
fn puts(s: &str) {
    for c in s.bytes() {
        hw::putchar(c);
    }
}

struct Kheap;

unsafe impl GlobalAlloc for Kheap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let n = round_16(layout.size());
        let a = alloc(n);
        if a == 0 { core::ptr::null_mut() } else { a as *mut u8 }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let n = round_16(layout.size());
        dealloc(ptr as usize, n);
    }
}

#[global_allocator]
static HEAP: Kheap = Kheap;

struct Uart;
impl Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        puts(s);
        Ok(())
    }
}

fn round_16(n: usize) -> usize {
    (n + 15) & !15
}

fn frame_init(start: usize, end: usize) {
    let mut real_start = start + 4095;
    real_start &= !0xFFF;
    let mut addr = end - 4096;
    while addr >= real_start {
        frame_free(addr);
        addr -= 4096;
    }
}

fn frame_alloc() -> Option<usize> {
    let x: usize = FREE_LIST.load(Ordering::Relaxed);
    if x == 0 {
        return None;
    }
    let next = unsafe {read_volatile(x as *const usize)};
    FREE_LIST.store(next, Ordering::Relaxed);
    Some(x)
}

fn frame_free(page: usize) {
    unsafe {
        write_volatile(page as *mut usize, FREE_LIST.load(Ordering::Relaxed));
    }
    FREE_LIST.store(page, Ordering::Relaxed);
}

pub fn on_trap(t: Trap) {
    println!("TRAP");
    match t {
        Trap::Fault { cause, address, pc} => {
            let name = match cause {
                0 => "instruction address misaligned",
                1 => "instruction access fault",
                2 => "illegal instruction",
                3 => "breakpoint",
                4 => "load address misaligned",
                5 => "load access fault",
                6 => "store/AMO address misaligned",
                7 => "store/AMO access fault",
                8 => "ecall from user mode",
                9 => "ecall from supervisor mode",
                11 => "ecall from machine mode",
                12 => "instruction page fault",
                13 => "load page fault",
                15 => "store/AMO page fault",
                _ => "unknown exception",
            };
            println!("{}", name);
            println!("Address {:#x}, Pc {:#x}", address, pc);
        }
        Trap::Timer => println!("Timer"),
        Trap::Unknown => println!("unknown trap")
    };
}

/// Where a panic ends up.
///
/// Required: `no_std` means nothing else defines it, and the compiler will not
/// build without one. It cannot return, and it cannot unwind, because there is
/// no unwinder -- hence `panic = "abort"` in Cargo.toml.
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    println!("PANIC! AT THE KERNEL");
    println!("The PC is crying for help");
    println!("{}", _info);
    loop {
        idle()
    }
}