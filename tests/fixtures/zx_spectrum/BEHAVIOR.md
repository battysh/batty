# Minimal ZX Spectrum Test Program

Snapshot: `minimal_test_program.z80`
Type: `.z80` version 1, 48K, uncompressed
Start address: `0x8000`

## Observable Behavior

When started from the snapshot, the program performs a deterministic one-shot sequence:

1. Fills the 6912-byte display file beginning at `0x4000`.
   The 6144-byte bitmap region is written with `0xFF`, producing a fully set pixel map.
   The 768-byte attribute region is written with `0x47`, yielding a uniform attribute value across the screen.
2. Toggles port `0xFE` twice.
   First it outputs `0x10`, then `0x00`, creating a short audible click / beep pulse on real hardware or an emulator that models the speaker.
3. Writes `0x42` to address `0x9000`.
4. Executes `HALT` with interrupts disabled, so execution stops in a stable terminal state.

## Manual Verification

If you load the snapshot into a ZX Spectrum emulator:

- the display should become uniformly filled
- a short click / beep pulse should occur once
- memory address `0x9000` should contain `0x42`
- the CPU should remain halted after the sequence completes

## Notes

The program is intentionally tiny and fully specified so it can serve as a clean-room equivalence fixture for future emulator-backed tests.
