/* Link the BCM57762 RX-CPU enable shim as a flat image at 0x08000000.
 * Layout mirrors the scratch pad: text/rodata/data grow up from the base, the
 * stack grows down from 0x08010000 (top of the 64 KB pad). The image carries
 * only .text/.rodata/.data; .bss is zeroed at runtime by entry.S. */
ENTRY(_start)

SECTIONS
{
    . = 0x08000000;

    .text : {
        *(.text.entry)
        *(.text*)
    }
    .rodata : { *(.rodata*) }
    .data   : { *(.data*) }

    _bss_start = .;
    .bss : { *(.bss*) *(COMMON) }
    _bss_end = .;
    _end = .;

    /* Leave at least 8 KB of headroom below the stack. */
    ASSERT(_end <= 0x0800E000, "bcm-fw image overflows the RX-CPU scratch pad")

    /DISCARD/ : {
        *(.eh_frame*)
        *(.pdr)
        *(.reginfo)
        *(.MIPS.abiflags)
        *(.comment)
        *(.gnu.attributes)
    }
}
