# Clean-Room Process

This project follows a two-side clean-room workflow.

## Roles

- `decompiler` may inspect the original binary and produce analysis artifacts in `analysis/`
- For ZX Spectrum snapshots, `decompiler` should use SkoolKit `sna2skool` to generate `.skool` output under `analysis/`
- `spec-writer` may read `analysis/` and translate findings into behavior-only specs under `specs/<behavior>/SPEC.md`
- `test-writer` and `implementer` may read `SPEC.md`, `PARITY.md`, and files under `implementation/`

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
