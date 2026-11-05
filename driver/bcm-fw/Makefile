# Clean-room BCM57762 RX-CPU enable shim.
#
# Builds with the mips-elf -mtigon cross toolchain to a flat image loaded at
# 0x08000000 by the dext's LoadCoreImage path. The toolchain is built by
# .agents/tmp/thundergate/misc/build-toolchain.sh (see
# docs/bcm-firmware-shim-design.md); override TOOLCHAIN to point elsewhere.
#
# The built image (bcmfw.img) is committed, so consumers (the probe / dext) do
# not need the toolchain -- only rebuilding the shim does.
#
# Run from the workspace root: make -C driver/bcm-fw

TOOLCHAIN ?= /Users/dexter/AVB-Audio/.agents/tmp/toolchain
CC      := $(TOOLCHAIN)/bin/mips-elf-gcc
LD      := $(TOOLCHAIN)/bin/mips-elf-ld
OBJCOPY := $(TOOLCHAIN)/bin/mips-elf-objcopy

# Tigon MIPS: soft-float, no mul/div/unaligned, non-PIC, no abicalls (-mtigon).
CFLAGS  := -mtigon -mips2 -march=r6000 -mfix-r4000 -msoft-float -G0 \
           -fno-pic -mno-abicalls -mno-llsc -ffreestanding -fno-builtin \
           -fomit-frame-pointer -Os -Wall -Wextra -std=gnu11
LDFLAGS := -nostdlib -static -T image.ld --build-id=none

OBJ := entry.o shim.o

.PHONY: all clean
all: bcmfw.img

entry.o: entry.S
	$(CC) $(CFLAGS) -c -o $@ $<

shim.o: shim.c regs.h
	$(CC) $(CFLAGS) -c -o $@ $<

bcmfw.elf: $(OBJ) image.ld
	$(LD) $(LDFLAGS) $(OBJ) -o $@

bcmfw.img: bcmfw.elf
	$(OBJCOPY) -O binary -j .text -j .rodata -j .data $< $@
	@printf 'bcmfw.img: %s bytes\n' "$$(wc -c < $@)"

clean:
	rm -f $(OBJ) bcmfw.elf bcmfw.img
