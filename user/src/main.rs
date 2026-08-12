// A LeBOS user program.
//
// It has no standard library, no allocator, no libc, and no way to touch the
// kernel except by trapping. Everything it does, it asks for.
//
// Note what it never does: name a path. There is nowhere to put anything. It
// creates an object with attributes, and finds it again by describing it.

#![no_std]
#![no_main]

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
extern "C" fn umain() -> ! {
    write(b"user: storing three objects, naming no paths\n");

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

    unsafe { syscall4(SYS_EXIT, 0, 0, 0, 0) };
    loop {}
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}
