# Clean-Room Process

This project follows a two-side clean-room workflow.

## Roles

- `decompiler` may inspect the original binary and produce analysis artifacts in `analysis/`
- For ZX Spectrum snapshots, `decompiler` should use SkoolKit `sna2skool` to generate `.skool` output under `analysis/`
- `spec-writer` may read `analysis/` and translate findings into behavior-only specs under `specs/<behavior>/SPEC.md`
- `test-writer` and `implementer` may read `SPEC.md`, `PARITY.md`, and files under `implementation/`

## Backend Selection

- Detect the binary format first, then choose the analysis backend.
- Use SkoolKit for ZX Spectrum Z80 snapshots (`.z80`, `.sna`).
- Use Ghidra headless analysis for other supported targets, including NES/6502, Game Boy/SM83, and DOS/x86 binaries.
- Capture the selected backend, target, and detection evidence in `analysis/DISASSEMBLY.md`.

## Normalized Analysis Output

- Regardless of backend, publish one normalized analysis artifact at `analysis/DISASSEMBLY.md`.
- The normalized artifact must include backend metadata, target metadata, memory layout, routines, data structures, and observed behaviors.
- Backend-specific raw exports may stay under `analysis/`, but the normalized artifact is the contract consumed by `spec-writer`.

## Information Barrier

- The implementation side must not read the original binary, decompiler output, memory maps, or files under `analysis/`
- The analysis side must not write production implementation code
- Behavior spec files under `specs/**/SPEC.md` are the only documents allowed to cross from analysis to implementation
- `PARITY.md` may track coverage and verification state, but it must not contain implementation detail from the original binary

## Audit Requirements

- Keep analysis artifacts in `analysis/`
- Keep generated behavior specs in `specs/`
- Keep SkoolKit disassembly outputs on the analysis side only; implementation roles must not read them directly
- Keep implementation code and tests in `implementation/`
- Record each behavior in `PARITY.md`
- Route ambiguity resolution through `spec-writer`
- Preserve commit history so the analysis-side and implementation-side outputs remain reviewable
