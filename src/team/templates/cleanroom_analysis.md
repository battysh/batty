# Analysis Workspace

The `analysis/` directory is the reverse-engineering side of the clean-room boundary.

## Backend Selection

- Use SkoolKit for ZX Spectrum Z80 snapshots such as `.z80` and `.sna`.
- Use Ghidra headless analysis for non-Z80 targets such as NES (`.nes`, 6502), Game Boy (`.gb`, `.gbc`, SM83), and DOS (`.com`, `.exe`, x86).
- Record the detected target, selected backend, and detection evidence before producing downstream notes.

## Normalized Analysis Artifact

Keep backend-specific raw output behind the barrier, but also write one normalized summary at `analysis/DISASSEMBLY.md` with this structure:

```md
# Annotated Disassembly

## Backend
- Tool: <SkoolKit|Ghidra>
- Target: <platform / cpu>
- Input: <binary filename>
- Detection Evidence: <header, extension, or container match>

## Memory Layout
- Region: <name>
- Range: <address range or bank>
- Notes: <why it matters>

## Entry Points and Routines
- Label: <symbol or inferred routine name>
- Location: <address or function id>
- Summary: <observable purpose>

## Data Structures and Assets
- Name: <structure, table, or asset block>
- Location: <address, bank, or file offset>
- Summary: <behavioral significance>

## Observed Behaviors
- Behavior: <observable behavior>
- Evidence: <where it appears in analysis>
- Open Questions: <unknowns for spec writer>
```

This normalized file is the preferred handoff for the spec writer. Raw backend output may remain in adjacent files under `analysis/`, but must not cross the barrier.
