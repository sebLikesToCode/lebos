//! Everything a LeBOS program needs to exist.
//!
//! Shared by every binary in this crate. There is no libc, no runtime and no
//! startup code except the twelve instructions below -- a program gets a stack
//! from the kernel, an entry point, and the ability to trap. Everything else is
//! built here.

// This module is compiled into every binary in the crate, and no single
// program uses all of it -- `hello` never reads the console, the shell never
// sends a deliberately malformed request. Unused-in-this-binary is the normal
// case for a shared module, not a smell.
#![allow(dead_code)]

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;

// The first instructions of any LeBOS program.
//
// No stack setup: the kernel maps a stack and sets `sp` before `sret`. A
// program does not choose where its own stack lives any more than it chooses
// its address space. a0 arrives holding this process's identity tag and passes
// straight through to umain, because nothing here touches it.
core::arch::global_asm!(
    r#"
.section .text.start
.globl _start
_start:
        call    umain
1:      j       1b
"#
);

pub const SYS_EXIT: usize = 0;
pub const SYS_WRITE: usize = 1;
pub const SYS_CREATE: usize = 2;
pub const SYS_QUERY: usize = 3;
pub const SYS_SBRK: usize = 4;
pub const SYS_READ: usize = 5;
pub const SYS_GET: usize = 6;
pub const SYS_VERB: usize = 7;
pub const SYS_SAVE: usize = 8;
pub const SYS_SPAWN: usize = 9;
pub const SYS_WAIT: usize = 10;

/// The only way out of a user program.
///
/// `a7` selects the call, `a0..a3` carry arguments, `a0` comes back with the
/// answer. `ecall` raises an exception the kernel handles; there is no other
/// door.
pub unsafe fn syscall4(n: usize, a0: usize, a1: usize, a2: usize, a3: usize) -> usize {
    let ret: usize;
    unsafe {
        core::arch::asm!(
            "ecall",
            in("a7") n,
            inlateout("a0") a0 => ret,
            in("a1") a1,
            in("a2") a2,
            in("a3") a3,
        );
    }
    ret
}

pub fn write(s: &[u8]) {
    unsafe { syscall4(SYS_WRITE, s.as_ptr() as usize, s.len(), 0, 0) };
}

pub fn exit(code: usize) -> ! {
    unsafe { syscall4(SYS_EXIT, code, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

pub fn sbrk(n: usize) -> usize {
    unsafe { syscall4(SYS_SBRK, n, 0, 0, 0) }
}

/// One byte from the console. Blocks.
///
/// The kernel hands over bytes and nothing else -- echo, backspace and the
/// entire notion of a "line" are built below, because every one of them is
/// policy and policy does not belong in supervisor mode.
pub fn read_char() -> u8 {
    unsafe { syscall4(SYS_READ, 0, 0, 0, 0) as u8 }
}

pub fn verb(id: u64, which: usize) -> bool {
    unsafe { syscall4(SYS_VERB, id as usize, which, 0, 0) != usize::MAX }
}

pub fn save() -> bool {
    unsafe { syscall4(SYS_SAVE, 0, 0, 0, 0) != usize::MAX }
}

/// Run the program stored in object `id`. WHICH id is a query, and a query is
/// something a program does for itself.
pub fn spawn(id: u64) -> bool {
    unsafe { syscall4(SYS_SPAWN, id as usize, 0, 0, 0) != usize::MAX }
}

/// Block until a child exits; returns its exit code.
pub fn wait() -> Option<usize> {
    match unsafe { syscall4(SYS_WAIT, 0, 0, 0, 0) } {
        usize::MAX => None,
        code => Some(code),
    }
}

// ---------------------------------------------------------------------------
// The heap -- milestone 17
//
// A BUMP ALLOCATOR. One pointer walks forward and never comes back; `dealloc`
// does nothing.
//
// That is correct rather than lazy. This program's entire address space is
// torn down in one go when it exits, so reclaiming individual allocations
// solves a problem that does not exist. Before milestone 16 gave exit a
// teardown, it would have been a real leak.
//
// 64 KiB at a time, or the request rounded up to a page if larger -- xv6's
// rule. Nobody asks the OS for exactly what was requested, because the syscall
// costs far more than the wasted bytes.
// ---------------------------------------------------------------------------

const CHUNK: usize = 64 * 1024;

pub struct Bump {
    next: UnsafeCell<usize>,
    limit: UnsafeCell<usize>,
}

// Sound only because a LeBOS process is single-threaded. The moment a process
// can have two threads this needs a lock -- the one the kernel grew at 9b.
unsafe impl Sync for Bump {}

/// Smallest multiple of `align` that is >= `x`.
///
/// Adding `align - 1` pushes anything not already on a boundary past the next
/// one; the mask chops it back down. Works because alignments are always powers
/// of two, so `!(align - 1)` is a clean run of high bits.
pub fn align_up(x: usize, align: usize) -> usize {
    (x + align - 1) & !(align - 1)
}

unsafe impl GlobalAlloc for Bump {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let (size, align) = (layout.size(), layout.align());
        loop {
            let next = unsafe { *self.next.get() };
            let limit = unsafe { *self.limit.get() };

            let start = align_up(next, align);
            let end = start.wrapping_add(size);

            // `end >= start` catches the wrap. A Layout that large cannot be
            // honest, but it must not be allowed to look like it fits either.
            if end >= start && end <= limit {
                unsafe { *self.next.get() = end };
                return start as *mut u8;
            }

            // `size + align`, not `size`: the alignment skip happens inside the
            // new chunk and has to fit there too.
            let want = if size + align > CHUNK {
                align_up(size + align, 4096)
            } else {
                CHUNK
            };

            let got = sbrk(want);
            if got == usize::MAX {
                return core::ptr::null_mut();
            }
            // sbrk always extends from the break, so `got` is normally exactly
            // the old limit and the land stays one unbroken strip. On the first
            // call there is no limit yet, so start from wherever it landed.
            if got != limit {
                unsafe { *self.next.get() = got };
            }
            unsafe { *self.limit.get() = got + want };
        }
    }

    /// Nothing. The kernel reclaims all of it at exit.
    unsafe fn dealloc(&self, _ptr: *mut u8, _layout: Layout) {}
}

#[global_allocator]
pub static HEAP: Bump = Bump {
    next: UnsafeCell::new(0),
    limit: UnsafeCell::new(0),
};

// ---------------------------------------------------------------------------
// Packed buffers
//
// Structured syscall arguments arrive as ONE packed buffer rather than a nest
// of pointers. That is security, not tidiness: a nested layout means the kernel
// validating 2N untrusted pointers per call, and every accepted pointer is
// attack surface. One range is checked, copied in, and parsed where nothing can
// change underneath it.
// ---------------------------------------------------------------------------

/// Builds one.
#[derive(Default)]
pub struct Buf(pub alloc::vec::Vec<u8>);

impl Buf {
    pub fn new() -> Self {
        Buf(alloc::vec::Vec::new())
    }
    pub fn u8v(&mut self, v: u8) {
        self.0.push(v);
    }
    pub fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    pub fn raw(&mut self, s: &[u8]) {
        self.0.extend_from_slice(s);
    }
    pub fn text(&mut self, s: &[u8]) {
        self.u32(s.len() as u32);
        self.raw(s);
    }
}

/// Walks one, and never runs off the end.
pub struct Rd<'a> {
    pub b: &'a [u8],
    pub i: usize,
}

impl<'a> Rd<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Rd { b, i: 0 }
    }
    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let out = self.b.get(self.i..self.i.checked_add(n)?)?;
        self.i += n;
        Some(out)
    }
    pub fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    pub fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub fn i64(&mut self) -> Option<i64> {
        Some(i64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    pub fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    pub fn blob(&mut self) -> Option<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    pub fn text(&mut self) -> Option<alloc::string::String> {
        Some(alloc::string::String::from_utf8_lossy(self.blob()?).into_owned())
    }
}

// ---------------------------------------------------------------------------
// The store, from userspace
// ---------------------------------------------------------------------------

/// One attribute of an object, as the shell needs to see it.
pub enum Val {
    Int(i64),
    Text(alloc::string::String),
    Other,
}

/// What `get` gives back.
pub struct Obj {
    pub content: u64,
    pub attrs: alloc::vec::Vec<(alloc::string::String, Val)>,
    pub len: usize,
    pub bytes: alloc::vec::Vec<u8>,
}

impl Obj {
    pub fn attr(&self, key: &str) -> Option<&Val> {
        self.attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
    pub fn text(&self, key: &str) -> Option<&str> {
        match self.attr(key) {
            Some(Val::Text(t)) => Some(t.as_str()),
            _ => None,
        }
    }
    pub fn int(&self, key: &str) -> Option<i64> {
        match self.attr(key) {
            Some(Val::Int(n)) => Some(*n),
            _ => None,
        }
    }
}

/// Fetch one object. `meta_only` skips the bytes -- listing twenty results
/// should not drag twenty programs across the privilege boundary.
pub fn get(id: u64, meta_only: bool) -> Option<Obj> {
    let mut cap = 512;
    let raw = loop {
        let mut buf = alloc::vec![0u8; cap];
        let n = unsafe {
            syscall4(
                SYS_GET,
                id as usize,
                buf.as_mut_ptr() as usize,
                cap,
                meta_only as usize,
            )
        };
        if n == usize::MAX {
            return None;
        }
        // Top bit set means "too small, and here is the size you need".
        // Truncating would hand back a half-object that still parses, which is
        // worse than an error.
        if n & (1 << 63) != 0 {
            cap = n & !(1 << 63);
            continue;
        }
        buf.truncate(n);
        break buf;
    };

    let mut r = Rd::new(&raw);
    let content = r.u64()?;
    let n = r.u32()? as usize;
    let mut attrs = alloc::vec::Vec::new();
    for _ in 0..n {
        let key = r.text()?;
        let val = match r.u8()? {
            0 => Val::Int(r.i64()?),
            1 => Val::Text(r.text()?),
            2 => {
                r.take(8)?;
                Val::Other
            }
            _ => {
                r.blob()?;
                Val::Other
            }
        };
        attrs.push((key, val));
    }
    let len = r.u32()? as usize;
    let bytes = if meta_only {
        alloc::vec::Vec::new()
    } else {
        r.take(len).unwrap_or(&[]).to_vec()
    };
    Some(Obj {
        content,
        attrs,
        len,
        bytes,
    })
}

/// One clause of a query.
pub enum Cond {
    Eq(alloc::string::String, alloc::string::String),
    Contains(alloc::string::String, alloc::string::String),
    Between(alloc::string::String, i64, i64),
    /// Which side of the hidden line to look at. `Hidden(true)` IS the
    /// Cluttered view -- the same query with one boolean flipped, not a
    /// separate code path.
    Hidden(bool),
}

pub fn query(conds: &[Cond]) -> alloc::vec::Vec<u64> {
    let mut b = Buf::new();
    b.u32(conds.len() as u32);
    for c in conds {
        match c {
            Cond::Eq(k, v) => {
                b.u8v(0);
                b.text(k.as_bytes());
                b.text(v.as_bytes());
            }
            Cond::Between(k, lo, hi) => {
                b.u8v(1);
                b.text(k.as_bytes());
                b.i64(*lo);
                b.i64(*hi);
            }
            Cond::Contains(k, v) => {
                b.u8v(2);
                b.text(k.as_bytes());
                b.text(v.as_bytes());
            }
            Cond::Hidden(v) => {
                b.u8v(3);
                b.text(b"hidden");
                b.u8v(*v as u8);
            }
        }
    }

    let mut out = alloc::vec![0u64; 64];
    let n = unsafe {
        syscall4(
            SYS_QUERY,
            b.0.as_ptr() as usize,
            b.0.len(),
            out.as_mut_ptr() as usize,
            out.len(),
        )
    };
    if n == usize::MAX {
        return alloc::vec::Vec::new();
    }
    out.truncate(n);
    out
}

/// Create an object: some bytes, and everything said about them.
pub fn create(content: &[u8], attrs: &[(&str, Val)]) -> Option<u64> {
    let mut b = Buf::new();
    b.u32(content.len() as u32);
    b.raw(content);
    b.u32(attrs.len() as u32);
    for (k, v) in attrs {
        b.text(k.as_bytes());
        match v {
            Val::Int(n) => {
                b.u8v(0);
                b.i64(*n);
            }
            Val::Text(t) => {
                b.u8v(1);
                b.text(t.as_bytes());
            }
            Val::Other => return None,
        }
    }
    match unsafe { syscall4(SYS_CREATE, b.0.as_ptr() as usize, b.0.len(), 0, 0) } {
        usize::MAX => None,
        id => Some(id as u64),
    }
}

// ---------------------------------------------------------------------------
// Terminal
// ---------------------------------------------------------------------------

/// Read a line, echoing as it goes.
///
/// The terminal is raw. Nothing appears on screen unless this puts it there,
/// backspace is the byte 0x7f rather than an action, and erasing means printing
/// "\x08 \x08" -- step left, paint a space over the character, step left again.
/// Everything a terminal appears to do for free is done here, in user mode.
pub fn read_line() -> alloc::string::String {
    let mut line = alloc::string::String::new();
    loop {
        match read_char() {
            b'\r' | b'\n' => {
                write(b"\n");
                return line;
            }
            0x7f | 0x08 => {
                if line.pop().is_some() {
                    write(b"\x08 \x08");
                }
            }
            0x15 => {
                while line.pop().is_some() {
                    write(b"\x08 \x08");
                }
            }
            // Printable, and there is room. The cap is the only thing between
            // a held-down key and the heap.
            c @ 0x20..=0x7e if line.len() < 256 => {
                line.push(c as char);
                write(&[c]);
            }
            _ => {}
        }
    }
}

pub fn print_num(n: i64) {
    if n < 0 {
        write(b"-");
    }
    let mut m = n.unsigned_abs();
    let mut d = [0u8; 21];
    let mut i = 21;
    if m == 0 {
        write(b"0");
        return;
    }
    while m > 0 {
        i -= 1;
        d[i] = b'0' + (m % 10) as u8;
        m /= 10;
    }
    write(&d[i..]);
}

pub fn print_hex(n: u64, digits: usize) {
    let mut out = [0u8; 16];
    for i in 0..digits {
        out[digits - 1 - i] = b"0123456789abcdef"[((n >> (i * 4)) & 0xf) as usize];
    }
    write(&out[..digits]);
}

/// Pad with spaces to `w` columns. A table that does not line up is harder to
/// scan than no table, and scanning is the entire retrieval model.
pub fn pad(s: &str, w: usize) {
    write(s.as_bytes());
    for _ in s.len()..w {
        write(b" ");
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    write(b"\nprogram panicked\n");
    exit(1)
}
