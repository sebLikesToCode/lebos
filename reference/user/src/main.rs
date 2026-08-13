//! `hello` -- the demo program. Not the shell any more; that is its own binary.
//!
//! It exists to exercise the syscall surface from the far side of the privilege
//! boundary: it allocates, it creates objects, it reads them back, and it
//! deliberately sends the kernel one malformed request to prove it gets
//! refused.

#![no_std]
#![no_main]

extern crate alloc;

#[path = "sys.rs"]
mod sys;
use sys::*;

#[no_mangle]
extern "C" fn umain(tag: usize) -> ! {
    // Who am I? The kernel put it in a0. The program is TOLD its identity
    // rather than having a byte of its own text rewritten behind its back --
    // which is what happened before .rodata became genuinely read-only, and is
    // the first seed of argv.
    write(b"hello: I am process ");
    write(&[tag as u8]);
    write(b", naming no paths\n");

    // Things that cannot exist without a heap. Buf was a fixed [u8; 512] until
    // milestone 17, because there was nowhere to put anything else.
    let mut v: alloc::vec::Vec<u64> = alloc::vec::Vec::new();
    for i in 1..=8u64 {
        v.push(i * i);
    }
    write(b"hello: squares ->");
    for x in &v {
        write(b" ");
        print_num(*x as i64);
    }
    write(b"\n");

    // Cross a chunk boundary, to prove the allocator asks for more rather than
    // falling over.
    let big: alloc::vec::Vec<u8> = alloc::vec![0xAB; 100 * 1024];
    write(b"hello: allocated ");
    print_num(big.len() as i64);
    write(b" bytes across ");
    print_num((sbrk(0) as i64 - 0x8000) / 1024);
    write(b" KiB of break\n");

    // Create, then find it again by describing it -- never by naming a place.
    create(
        b"import pygame  # paddle",
        &[
            ("name", Val::Text(alloc::string::String::from("brick breaker"))),
            ("type", Val::Text(alloc::string::String::from("python"))),
            ("created_at", Val::Int(101)),
        ],
    );
    let n = query(&[Cond::Eq(
        alloc::string::String::from("type"),
        alloc::string::String::from("python"),
    )])
    .len();
    write(b"hello: type=python -> ");
    print_num(n as i64);
    write(b" objects\n");

    // A deliberately malformed request: the count claims 99 attributes and the
    // buffer holds none. The kernel must refuse rather than trust it.
    let mut bad = Buf::new();
    bad.u32(0);
    bad.u32(99);
    let r = unsafe { syscall4(SYS_CREATE, bad.0.as_ptr() as usize, bad.0.len(), 0, 0) };
    write(b"hello: malformed create refused: ");
    print_num((r == usize::MAX) as i64);
    write(b"\n");

    exit(0)
}
