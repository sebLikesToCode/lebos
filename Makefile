TARGET  := riscv64gc-unknown-none-elf
KERNEL  := target/$(TARGET)/debug/lebos

# Scratch copy for experiments. Gitignored; see `play` / `resync` below.
PLAY    := src/main2.rs
PLAYBIN := target/$(TARGET)/debug/main2

QEMU    := qemu-system-riscv64
DISK    := lebos.img

QFLAGS  := -machine virt -cpu rv64 -smp 1 -m 128M -nographic \
           -serial mon:stdio -bios default \
           -drive file=$(DISK),if=none,format=raw,id=d0 \
           -device virtio-blk-device,drive=d0 \
           -global virtio-mmio.force-legacy=false

## disk    -- create the backing store image if it does not exist
$(DISK):
	@qemu-img create -f raw $(DISK) 64M >/dev/null
	@echo "created $(DISK) (64 MiB)"

.PHONY: build run debug gdb clean objdump nm size dumpdtb check fmt \
        play resync playdiff user logo

USERELF := user/target/$(TARGET)/debug/hello
USERBIN := user/hello.elf

## user    -- build the user program and flatten it to a raw binary.
##            The kernel bakes this in with include_bytes!, so it must exist
##            before the kernel compiles.
#            RUSTFLAGS is set in the environment rather than in
#            user/.cargo/config.toml because cargo MERGES config files up the
#            directory tree -- the kernel's -Tlinker.ld would otherwise leak in
#            and link this into the higher half. The env var replaces them.
# No objcopy any more. The kernel parses the ELF itself as of milestone 19,
# because an ELF states where each segment goes and what may be done to it --
# which a flat binary throws away, and which the kernel then had to guess at
# with a hardcoded constant.
$(USERBIN): user/src/main.rs user/user.ld
	cd user && RUSTFLAGS="-C code-model=medium -C link-arg=-Tuser.ld" cargo build
	rust-objcopy --strip-all $(USERELF) $(USERBIN)
	@echo "user program: $$(stat -c%s $(USERBIN)) bytes (ELF)"

user: $(USERBIN)

## logo    -- print the boot banner in colour, without booting anything.
##            Regenerate it from the artwork with:
##              python3 toascii.py assets/logo.png 72 > src/banner_art.txt
logo:
	@cat src/banner.txt

## build   -- compile the kernel ELF.
##            Only --bin lebos, so a broken main2.rs can never block this.
build: $(USERBIN) $(PLAY) $(DISK)
	cargo build --bin lebos

## run     -- boot the kernel in QEMU. Quit with: Ctrl-A then X
run: build
	$(QEMU) $(QFLAGS) -kernel $(KERNEL)

# -------------------------------------------------------------------------
# Scratch copy. src/main2.rs is a gitignored duplicate of main.rs that exists
# purely to be broken. Change it, run it, watch it fail, and the real kernel
# is untouched -- git never sees it and `make run` never builds it.
# -------------------------------------------------------------------------

# Created on demand so a fresh checkout works without extra steps.
$(PLAY):
	@cp src/main.rs $(PLAY)
	@cp src/entry.S src/entry2.S
	@sed -i 's|include_str!("entry.S")|include_str!("entry2.S")|' $(PLAY)
	@echo "created $(PLAY) + src/entry2.S from the real kernel"

## play    -- build and run the scratch copy instead of the real kernel
play: $(PLAY)
	cargo build --bin main2
	$(QEMU) $(QFLAGS) -kernel $(PLAYBIN)

## resync  -- overwrite the scratch copy with the current main.rs.
##            DISCARDS whatever you were experimenting with.
##            Also refreshes src/entry2.S, the scratch copy of the assembly,
##            so bugs can be planted there without touching the real kernel.
resync:
	@cp src/main.rs $(PLAY)
	@cp src/entry.S src/entry2.S
	@sed -i 's|include_str!("entry.S")|include_str!("entry2.S")|' $(PLAY)
	@echo "$(PLAY) + src/entry2.S reset to match the real kernel"

## playdiff -- show what you changed in the scratch copy vs the real kernel
playdiff: $(PLAY)
	@diff -u src/main.rs $(PLAY) || true

## debug   -- boot QEMU frozen before the first instruction, waiting for GDB
##            on port 1234. Run this, then `make gdb` in a second terminal.
debug: build
	$(QEMU) $(QFLAGS) -kernel $(KERNEL) -s -S

## gdb     -- attach to a waiting `make debug`. Try: `b kmain`, `c`, `si`,
##            `info registers`, `x/8i $pc`
gdb:
	gdb-multiarch $(KERNEL) \
	  -ex 'set arch riscv:rv64' \
	  -ex 'target remote localhost:1234' \
	  -ex 'break _start'

## trace   -- boot with QEMU logging every exception and MMU translation to
##            qemu.log. Indispensable when the machine dies silently.
trace: build
	$(QEMU) $(QFLAGS) -kernel $(KERNEL) -d int,mmu,guest_errors -D qemu.log

## objdump -- disassemble the kernel. Read this when the source and the
##            machine disagree about what you wrote.
objdump: build
	rust-objdump -d --print-imm-hex $(KERNEL) | less

## nm      -- list symbols with addresses. Confirms _start really is at
##            0x80200000 and shows you where __bss_start etc. landed.
nm: build
	rust-nm -n $(KERNEL)

## size    -- section sizes. Watch .bss grow.
size: build
	rust-size -A $(KERNEL)

## dumpdtb -- dump the device tree QEMU passes the kernel, as readable text.
##            This is the authoritative description of the hardware you are
##            running on: every device, its address, its interrupt number.
dumpdtb:
	$(QEMU) $(QFLAGS) -machine virt,dumpdtb=virt.dtb >/dev/null 2>&1 || true
	dtc -I dtb -O dts virt.dtb -o virt.dts 2>/dev/null \
	  && echo "wrote virt.dts" \
	  || echo "install device-tree-compiler to decode virt.dtb"

check:
	cargo clippy --bin lebos

fmt:
	cargo fmt

clean:
	cargo clean
	cd user && cargo clean
	rm -f $(USERBIN)
	rm -f qemu.log virt.dtb virt.dts $(DISK)
