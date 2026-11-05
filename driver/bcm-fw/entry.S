/*
 * Reset stub for the BCM57762 RX-CPU enable shim (clean-room).
 * The dext loads this image at 0x08000000 and sets the PC here. Sets the stack
 * to the top of the 64 KB scratch pad, zeroes .bss, calls shim_main (which does
 * not return), and parks defensively if it ever does.
 */
    .set noreorder
    .section .text.entry, "ax"
    .globl _start
_start:
    lui   $sp, 0x0801            /* stack top = 0x08010000 (top of the pad) */

    la    $t0, _bss_start        /* zero .bss [_bss_start, _bss_end) */
    la    $t1, _bss_end
1:
    sltu  $t2, $t0, $t1
    beq   $t2, $zero, 2f
    nop
    sw    $zero, 0($t0)
    addiu $t0, $t0, 4
    b     1b
    nop
2:
    jal   shim_main
    nop
3:
    b     3b                     /* shim_main never returns; park if it does */
    nop
