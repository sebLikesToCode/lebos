# LeBOS
#
#   src/          YOUR kernel          -> make run
#   reference/    milestones 1-20      -> make ref
#
# The reference is a standalone crate. It still boots, and it is there to be
# read while yours is written.

TARGET  := riscv64gc-unknown-none-elf
QEMU    := qemu-system-riscv64
DISK    := lebos.img

MINE    := target/$(TARGET)/debug/lebos
REF     := reference/target/$(TARGET)/debug/lebos-ref

QFLAGS  := -machine virt -cpu rv64 -smp 1 -m 128M -nographic \
           -serial mon:stdio -bios default \
           -drive file=$(DISK),if=none,format=raw,id=d0 \
           -device virtio-blk-device,drive=d0 \
           -global virtio-mmio.force-legacy=false

# The reference crate must NOT inherit the root crate's -Tkernel.ld, and cargo
# merges .cargo/config.toml up the directory tree. Setting RUSTFLAGS in the
# environment REPLACES config rustflags rather than merging with them.
REFFLAGS := -C code-model=medium -C link-arg=-Tlinker.ld -C link-arg=--no-gc-sections
USRFLAGS := -C code-model=medium -C link-arg=-Tuser.ld

.PHONY: all run build nm objdump debug check fmt \
        ref ref-build ref-check user logo disk dumpdtb gdb trace \
        play resync playdiff clean help

all: build

# =========================================================================
#  YOURS
# =========================================================================

## build   -- compile your kernel
build:
	cargo build

## run     -- boot your kernel.   QUIT WITH: Ctrl-A then X
run: build $(DISK)
	$(QEMU) $(QFLAGS) -kernel $(MINE)

## nm      -- symbols by address. Confirms _start really is at 0x80200000.
nm: build
	rust-nm -n $(MINE)

## objdump -- disassembly. Read this when the source and the machine disagree.
objdump: build
	rust-objdump -d --print-imm-hex $(MINE) | less

## debug   -- boot frozen, GDB stub on :1234.  Then `make gdb` in terminal 2.
debug: build $(DISK)
	$(QEMU) $(QFLAGS) -kernel $(MINE) -s -S

## check   -- clippy
check:
	cargo clippy

## fmt     -- rustfmt
fmt:
	cargo fmt

## gdb     -- attach to a waiting `make debug`
##            try: b kmain, c, si, info registers, x/8i $$pc
gdb:
	gdb-multiarch $(MINE) \
	  -ex 'set arch riscv:rv64' \
	  -ex 'target remote localhost:1234' \
	  -ex 'break _start'

## trace   -- boot logging every exception and MMU translation to qemu.log
trace: build $(DISK)
	$(QEMU) $(QFLAGS) -kernel $(MINE) -d int,mmu,guest_errors -D qemu.log

# =========================================================================
#  THE REFERENCE  (milestones 1-20)
# =========================================================================

REFELF  := reference/user/target/$(TARGET)/debug/hello
SHELELF := reference/user/target/$(TARGET)/debug/shell
REFBIN  := reference/user/hello.elf
SHELBIN := reference/user/shell.elf

## user    -- build the reference's user programs (the shell, and a demo)
$(REFBIN) $(SHELBIN): reference/user/src/main.rs reference/user/src/sys.rs \
                      reference/user/src/bin/shell.rs reference/user/user.ld
	cd reference/user && RUSTFLAGS="$(USRFLAGS)" cargo build
	rust-objcopy --strip-all $(REFELF) $(REFBIN)
	rust-objcopy --strip-all $(SHELELF) $(SHELBIN)
	@echo "hello: $$(stat -c%s $(REFBIN)) bytes | shell: $$(stat -c%s $(SHELBIN)) bytes"

user: $(REFBIN) $(SHELBIN)

## ref-build -- compile the reference kernel
ref-build: user reference/src/main2.rs
	cd reference && RUSTFLAGS="$(REFFLAGS)" cargo build --bin lebos-ref

## ref     -- boot the reference kernel
ref: ref-build $(DISK)
	$(QEMU) $(QFLAGS) -kernel $(REF)

ref-check:
	cd reference && RUSTFLAGS="$(REFFLAGS)" cargo clippy --bin lebos-ref

## logo    -- print the reference's colour boot banner without booting.
##            Regenerate from the artwork with:
##              python3 reference/tools/toascii.py reference/assets/logo.png 72
logo:
	@cat reference/src/banner.txt

# The reference's breakable scratch copy. Gitignored; `make play` builds it, so
# breaking it can never block anything.
reference/src/main2.rs:
	@cp reference/src/main.rs $@
	@cp reference/src/entry.S reference/src/entry2.S
	@sed -i 's|include_str!("entry.S")|include_str!("entry2.S")|' $@
	@echo "created $@ + reference/src/entry2.S"

## play    -- run the reference's scratch copy instead of the real one
play: reference/src/main2.rs $(DISK)
	cd reference && RUSTFLAGS="$(REFFLAGS)" cargo build --bin main2
	$(QEMU) $(QFLAGS) -kernel reference/target/$(TARGET)/debug/main2

## resync  -- reset that scratch copy. DISCARDS your experiments.
resync:
	@cp reference/src/main.rs reference/src/main2.rs
	@cp reference/src/entry.S reference/src/entry2.S
	@sed -i 's|include_str!("entry.S")|include_str!("entry2.S")|' reference/src/main2.rs
	@echo "scratch copy reset"

## playdiff -- show what you changed in the scratch copy
playdiff: reference/src/main2.rs
	@diff -u reference/src/main.rs reference/src/main2.rs || true

# =========================================================================
#  SHARED
# =========================================================================

## disk    -- create the backing store image if it does not exist
$(DISK):
	@qemu-img create -f raw $(DISK) 64M >/dev/null
	@echo "created $(DISK) (64 MiB)"

disk: $(DISK)

## dumpdtb -- dump QEMU's device tree -> virt.dts. The authoritative hardware
##            map: every device, its address, its interrupt number.
dumpdtb:
	$(QEMU) $(QFLAGS) -machine virt,dumpdtb=virt.dtb >/dev/null 2>&1 || true
	@dtc -I dtb -O dts virt.dtb -o virt.dts 2>/dev/null \
	  && echo "wrote virt.dts" \
	  || echo "install device-tree-compiler to decode virt.dtb"

clean:
	cargo clean
	cd reference && cargo clean
	cd reference/user && cargo clean
	rm -f $(REFBIN) $(SHELBIN) qemu.log virt.dtb virt.dts $(DISK)

## help    -- list these targets
help:
	@grep -E '^## ' Makefile | sed 's/^## //'
