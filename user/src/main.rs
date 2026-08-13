// A LeBOS user program.
//
// It has no standard library, no allocator, no libc, and no way to touch the
// kernel except by trapping. Everything it does, it asks for.
//
// Note what it never does: name a path. There is nowhere to put anything. It
// creates an object with attributes, and finds it again by describing it.

#![no_std]
#![no_main]

extern crate alloc;

// Syscall ABI:
//
//   a7 = number, a0.. = arguments, a0 = return value
//
//   0  exit(code)
//   1  write(ptr, len)
//   2  create(buf, len)                  -> object id
//   3  query(buf, len, out, out_cap)     -> number of ids written
//
// Structured arguments arrive as one packed buffer rather than a nest of
// pointers, so the kernel has exactly one range to validate.

core::arch::global_asm!(
    r#"
.section .text.start
.globl _start
_start:
        # No stack setup here. The kernel maps a stack below the program and
        # sets sp before sret -- a program does not get to choose where its
        # own stack lives, any more than it chooses its address space.
        call    umain
1:      j       1b
"#
);

const SYS_EXIT: usize = 0;
const SYS_WRITE: usize = 1;
const SYS_CREATE: usize = 2;
const SYS_QUERY: usize = 3;
const SYS_SBRK: usize = 4;
const SYS_READ: usize = 5;
const SYS_GET: usize = 6;
const SYS_VERB: usize = 7;
const SYS_SAVE: usize = 8;

/// One byte from the console. Blocks until someone types.
///
/// The kernel hands over bytes and nothing else -- echo, backspace and the
/// whole notion of a "line" are built below, in this program, because every
/// one of them is policy.
fn read_char() -> u8 {
    unsafe { syscall4(SYS_READ, 0, 0, 0, 0) as u8 }
}

fn verb(id: u64, which: usize) -> bool {
    unsafe { syscall4(SYS_VERB, id as usize, which, 0, 0) != usize::MAX }
}

fn save() -> bool {
    unsafe { syscall4(SYS_SAVE, 0, 0, 0, 0) != usize::MAX }
}

/// Fetch one object into a buffer we own. `alloc` exists now, so this can grow
/// to fit instead of guessing -- which is exactly what milestone 17 bought.
fn get(id: u64) -> Option<alloc::vec::Vec<u8>> {
    let mut cap = 256;
    loop {
        let mut buf = alloc::vec![0u8; cap];
        let n = unsafe { syscall4(SYS_GET, id as usize, buf.as_mut_ptr() as usize, cap, 0) };
        if n == usize::MAX {
            return None;
        }
        // Top bit set means "too small, and here is the size you need".
        // Truncating would have handed back a half-object that still parses.
        if n & (1 << 63) != 0 {
            cap = n & !(1 << 63);
            continue;
        }
        buf.truncate(n);
        return Some(buf);
    }
}

/// Read a line, echoing as it goes. THIS IS THE SHELL'S LINE EDITOR, and it
/// runs in user mode -- the kernel never sees a line, only bytes.
fn read_line() -> alloc::string::String {
    let mut line = alloc::string::String::new();
    loop {
        let c = read_char();
        match c {
            b'\r' | b'\n' => {
                write(b"\n");
                return line;
            }
            0x7f | 0x08 => {
                if line.pop().is_some() {
                    write(b"\x08 \x08");
                }
            }
            0x20..=0x7e => {
                line.push(c as char);
                write(&[c]);
            }
            _ => {}
        }
    }
}

/// Walks a packed buffer without ever running off the end.
struct Rd<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let out = self.b.get(self.i..self.i + n)?;
        self.i += n;
        Some(out)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn blob(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
}

/// Ask the kernel to move this program's break outward, and get back where it
/// USED to be -- which is the first address of the new memory.
fn sbrk(n: usize) -> usize {
    unsafe { syscall4(SYS_SBRK, n, 0, 0, 0) }
}

unsafe fn syscall4(n: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    core::arch::asm!(
        "ecall",
        in("a7") n,
        inlateout("a0") a0 => ret,
        in("a1") a1,
        in("a2") a2,
        in("a3") a3,
    );
    ret
}

fn write(s: &[u8]) {
    unsafe { syscall4(SYS_WRITE, s.as_ptr() as usize, s.len(), 0, 0) };
}

// ---------------------------------------------------------------------------
// The heap -- milestone 17
//
// A BUMP ALLOCATOR. One pointer walks forward and never comes back.
//
//     next   the first free byte
//     limit  one past the end of the land we own
//
// alloc: round `next` up to the alignment the caller demanded, hand it back,
// move `next` past it. free: DO NOTHING.
//
// That is not laziness. This program's entire address space is torn down in
// one go by thread_exit at milestone 16 -- page tables, program, stack and
// heap, all returned at once. Reclaiming individual allocations would be work
// done to solve a problem that does not exist. Before 16 this would have been
// a genuine leak; after it, it is the correct design for a short-lived
// program.
//
// The chunk size is xv6's rule almost exactly: ask for 64 KiB at a time, or
// for the whole request if it is bigger. Real allocators all land somewhere
// near here -- glibc pads by 128 KiB, jemalloc takes megabytes at once, Go
// reserves 64 MiB arenas. Nobody asks the kernel for exactly what was
// requested, because the syscall costs far more than the wasted bytes.
//
// Note what doubling is NOT. `Vec` doubles when it outgrows its buffer, but
// that happens one layer above this: Vec asks the allocator for a bigger
// block, and the allocator asks the kernel in flat chunks.
// ---------------------------------------------------------------------------

const CHUNK: usize = 64 * 1024;

struct Bump {
    next: core::cell::UnsafeCell<usize>,
    limit: core::cell::UnsafeCell<usize>,
}

// Sound only because a LeBOS process is single-threaded. The moment a process
// can have two threads, this needs a lock -- exactly the one the kernel grew
// at milestone 9b.
unsafe impl Sync for Bump {}

/// Smallest multiple of `align` that is >= `x`.
///
/// Adding `align - 1` pushes anything not already on a boundary past the next
/// one; the mask then chops it back down to it. Works because alignments are
/// always powers of two, so `!(align - 1)` is a clean run of high bits.
fn align_up(x: usize, align: usize) -> usize {
    (x + align - 1) & !(align - 1)
}

unsafe impl core::alloc::GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: core::alloc::Layout) -> *mut u8 {
        let size = layout.size();
        let align = layout.align();

        loop {
            let next = *self.next.get();
            let limit = *self.limit.get();

            let start = align_up(next, align);
            let end = start.wrapping_add(size);

            // `end >= start` catches the wrap. A Layout that huge cannot be
            // honest, but it must not be allowed to look like it fits either.
            if end >= start && end <= limit {
                *self.next.get() = end;
                return start as *mut u8;
            }

            // Out of land. `size + align` rather than `size`, because the
            // alignment skip happens INSIDE the new chunk and has to fit too.
            let want = if size + align > CHUNK {
                align_up(size + align, 4096)
            } else {
                CHUNK
            };

            let got = sbrk(want);
            if got == usize::MAX {
                return core::ptr::null_mut();
            }

            // sbrk always extends from the current break, so `got` is normally
            // exactly the old limit and the land stays one unbroken strip. If
            // it ever is not -- the first call, when there is no limit yet --
            // start over from wherever it actually landed.
            if got != limit {
                *self.next.get() = got;
            }
            *self.limit.get() = got + want;
        }
    }

    /// Nothing. See the note above: the kernel reclaims all of it at exit.
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: core::alloc::Layout) {}
}

#[global_allocator]
static HEAP: Bump = Bump {
    next: core::cell::UnsafeCell::new(0),
    limit: core::cell::UnsafeCell::new(0),
};

/// Builds the packed buffers the store syscalls expect. No allocator, so it
/// writes into a fixed array and tracks how far it got.
struct Buf {
    b: [u8; 512],
    n: usize,
}

impl Buf {
    const fn new() -> Self {
        Buf { b: [0; 512], n: 0 }
    }
    fn raw(&mut self, s: &[u8]) {
        for &x in s {
            self.b[self.n] = x;
            self.n += 1;
        }
    }
    fn u32(&mut self, v: u32) {
        self.raw(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.raw(&v.to_le_bytes());
    }
    fn u8v(&mut self, v: u8) {
        self.raw(&[v]);
    }
    fn text(&mut self, s: &[u8]) {
        self.u32(s.len() as u32);
        self.raw(s);
    }
}

/// create(content, [(key, Text)... , (key, Int)...])
fn make(content: &[u8], name: &[u8], kind: &[u8], day: i64) -> usize {
    let mut b = Buf::new();
    b.u32(content.len() as u32);
    b.raw(content);
    b.u32(3); // three attributes
    b.text(b"name");
    b.u8v(1);
    b.text(name);
    b.text(b"type");
    b.u8v(1);
    b.text(kind);
    b.text(b"created_at");
    b.u8v(0);
    b.i64(day);
    unsafe { syscall4(SYS_CREATE, b.b.as_ptr() as usize, b.n, 0, 0) }
}

/// query(type == kind AND created_at in [lo, hi]) -> count
fn find(kind: &[u8], lo: i64, hi: i64, out: &mut [u64; 16]) -> usize {
    let mut b = Buf::new();
    b.u32(2); // two conditions
    b.u8v(0); // Eq(Text)
    b.text(b"type");
    b.text(kind);
    b.u8v(1); // Between(Int)
    b.text(b"created_at");
    b.i64(lo);
    b.i64(hi);
    unsafe {
        syscall4(
            SYS_QUERY,
            b.b.as_ptr() as usize,
            b.n,
            out.as_mut_ptr() as usize,
            out.len(),
        )
    }
}

fn print_num(mut n: usize) {
    let mut d = [0u8; 20];
    let mut i = 20;
    if n == 0 {
        write(b"0");
        return;
    }
    while n > 0 {
        i -= 1;
        d[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    write(&d[i..]);
}

#[no_mangle]
extern "C" fn umain(tag: usize) -> ! {
    // Who am I? The kernel put it in a0. The program is TOLD its identity
    // rather than having a byte of its own text rewritten behind its back --
    // which is what happened before .rodata became genuinely read-only, and
    // is the first seed of argv.
    write(b"user: I am process ");
    write(&[tag as u8]);
    write(b", storing three objects, naming no paths\n");

    make(b"import pygame  # paddle", b"brick breaker", b"python", 101);
    make(b"remember the paddle", b"notes", b"text", 101);
    make(b"def solve(): pass", b"solver", b"python", 100);

    let mut out = [0u64; 16];

    let n = find(b"python", 0, 999, &mut out);
    write(b"user: python, any day        -> ");
    print_num(n);
    write(b"\n");

    let n = find(b"python", 101, 101, &mut out);
    write(b"user: python, created day 101 -> ");
    print_num(n);
    write(b"\n");

    // A deliberately malformed request: the count says 99 attributes but the
    // buffer holds none. The kernel should refuse rather than trust it.
    let mut bad = Buf::new();
    bad.u32(0);
    bad.u32(99);
    let r = unsafe { syscall4(SYS_CREATE, bad.b.as_ptr() as usize, bad.n, 0, 0) };
    write(b"user: malformed create returned ");
    print_num(if r == usize::MAX { 1 } else { 0 });
    write(b" (1 = refused)\n");

    // Milestone 17: things that cannot exist without a heap.
    //
    // Every line below was impossible in this program yesterday. Buf is a
    // fixed [u8; 512] decided at compile time precisely because there was
    // nowhere to put anything else.
    write(b"user: break starts at ");
    print_num(sbrk(0));
    write(b"\n");

    let mut v: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
    for i in 1..=8u64 {
        v.push(i * i);
    }
    write(b"user: Vec of squares ->");
    for x in &v {
        write(b" ");
        print_num(*x as usize);
    }
    write(b"\n");

    // format! -- allocation, formatting and a String, in one line.
    let s = alloc::format!("user: a String built in userspace, {} squares\n", v.len());
    write(s.as_bytes());

    // Force a second chunk: 64 KiB is one sbrk, so this crosses the boundary
    // and proves the allocator asks for more rather than falling over.
    let big: alloc::vec::Vec<u8> = alloc::vec![0xAB; 100 * 1024];
    write(b"user: allocated ");
    print_num(big.len());
    write(b" bytes, last byte is ");
    print_num(big[big.len() - 1] as usize);
    write(b"\nuser: break is now ");
    print_num(sbrk(0));
    write(b"\n");

    // Milestone 18: the whole shell loop, from userspace.
    //
    // Only the child spawned by `run` does this. procA and procB start at boot
    // alongside the kernel's own shell, and two readers pulling from one
    // console ring would race for every keystroke. `run` blocks the kernel
    // shell in thread_wait, which is what makes the console free to take.
    if tag == b'C' as usize {
        write(b"child: type something, then enter> ");
        let text = read_line();

        // create -- the object is a statement about those bytes
        let mut b = Buf::new();
        b.u32(text.len() as u32);
        b.raw(text.as_bytes());
        b.u32(2);
        b.text(b"name");
        b.u8v(1);
        b.text(b"typed");
        b.text(b"type");
        b.u8v(1);
        b.text(b"note");
        let id = unsafe { syscall4(SYS_CREATE, b.b.as_ptr() as usize, b.n, 0, 0) };

        // get -- read it straight back out of the kernel's store
        match get(id as u64) {
            Some(raw) => {
                let mut r = Rd { b: &raw, i: 0 };
                let content = r.u64().unwrap_or(0);
                let n = r.u32().unwrap_or(0);
                write(b"child: object has ");
                print_num(n as usize);
                write(b" attributes, content #");
                print_num(content as usize);
                write(b"\n");
                for _ in 0..n {
                    let key = r.blob().unwrap_or(b"?");
                    let kind = r.take(1).map(|k| k[0]).unwrap_or(255);
                    write(b"child:   ");
                    write(key);
                    write(b" = ");
                    match kind {
                        0 => {
                            let v = r.take(8).unwrap_or(&[0; 8]);
                            print_num(i64::from_le_bytes(v.try_into().unwrap()) as usize);
                        }
                        1 => write(r.blob().unwrap_or(b"?")),
                        _ => write(b"(other)"),
                    }
                    write(b"\n");
                }
                let bytes = r.blob().unwrap_or(b"");
                write(b"child:   bytes = \"");
                write(bytes);
                write(b"\"\n");
            }
            None => write(b"child: get failed\n"),
        }

        // hide -- and then prove it left the default view but still exists
        let hidden = verb(id as u64, 1);
        let mut out = [0u64; 16];
        let visible = find(b"note", 0, i64::MAX, &mut out);
        write(b"child: hid it (");
        print_num(if hidden { 1 } else { 0 });
        write(b"), type=note now returns ");
        print_num(visible);
        write(b", but get still works: ");
        print_num(if get(id as u64).is_some() { 1 } else { 0 });
        write(b"\n");

        write(if save() { b"child: saved to disk\n" } else { b"child: save failed\n" });
    }

    unsafe { syscall4(SYS_EXIT, 0, 0, 0, 0) };

    // exit returns, since there is no parent to reap this program yet. Spin
    // until the scheduler stops picking us; the timer preempts it, so this
    // costs a slice rather than the machine.
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
