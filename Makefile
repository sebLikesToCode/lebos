# LeBOS -- a RISC-V kernel.
#
#   src/main.rs   the kernel
#   src/hw/       machine-specific: CPU registers and hardware addresses
#   src/entry.S   the twelve instructions before Rust can run
#   kernel.ld     where it all goes in memory

TARGET := riscv64gc-unknown-none-elf
KERNEL := target/$(TARGET)/debug/lebos
QEMU   := qemu-system-riscv64

QFLAGS := -machine virt -cpu rv64 -smp 1 -m 128M -nographic \
          -serial mon:stdio -bios default

.PHONY: build run nm objdump debug gdb trace check fmt dumpdtb clean help

## build    -- compile it
build:
	cargo build

## run      -- boot it.   QUIT WITH: Ctrl-A then X
run: build
	$(QEMU) $(QFLAGS) -kernel $(KERNEL)

## nm       -- symbols by address. Confirms _start is at 0x80200000.
nm: build
	rust-nm -n $(KERNEL)

## objdump  -- disassembly. Read this when the source and the machine disagree.
objdump: build
	rust-objdump -d --print-imm-hex $(KERNEL) | less

## debug    -- boot frozen, GDB stub on :1234. Then `make gdb` in terminal 2.
debug: build
	$(QEMU) $(QFLAGS) -kernel $(KERNEL) -s -S

## gdb      -- attach to a waiting `make debug`
##             try: b kmain, c, si, info registers, x/8i $$pc
gdb:
	gdb-multiarch $(KERNEL) \
	  -ex 'set arch riscv:rv64' \
	  -ex 'target remote localhost:1234' \
	  -ex 'break _start'

## trace    -- log every exception and MMU translation to qemu.log.
##             The only instrument that works when printing does not.
trace: build
	$(QEMU) $(QFLAGS) -kernel $(KERNEL) -d int,mmu,guest_errors -D qemu.log

## check    -- clippy
check:
	cargo clippy

## fmt      -- rustfmt
fmt:
	cargo fmt

## dumpdtb  -- dump QEMU's device tree -> virt.dts. The authoritative
##             description of the machine: every device and its address.
dumpdtb:
	$(QEMU) $(QFLAGS) -machine virt,dumpdtb=virt.dtb >/dev/null 2>&1 || true
	@dtc -I dtb -O dts virt.dtb -o virt.dts 2>/dev/null \
	  && echo "wrote virt.dts" || echo "install device-tree-compiler"

clean:
	cargo clean
	rm -f qemu.log virt.dtb virt.dts

## help     -- list these
help:
	@grep -E '^## ' Makefile | sed 's/^## //'
