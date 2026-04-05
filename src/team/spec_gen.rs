//! Clean-room behavior spec validation, handoff discovery, and parity syncing.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::team::parity::{
    ParityMetadata, ParityReport, ParityRow, ParityStatus, VerificationStatus,
};

const SPEC_ROOT: &str = "specs";
const SPEC_FILENAME: &str = "SPEC.md";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BehaviorSpec {
    pub behavior: String,
    pub relative_path: PathBuf,
    pub content: String,
}

pub fn load_behavior_specs(project_root: &Path) -> Result<Vec<BehaviorSpec>> {
    let specs_root = project_root.join(SPEC_ROOT);
    if !specs_root.is_dir() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_spec_files(&specs_root, &mut files)?;
    files.sort();

    let mut specs = Vec::new();
    for path in files {
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        validate_spec_content(&content)
            .with_context(|| format!("invalid clean-room spec {}", path.display()))?;
        let behavior = extract_behavior_name(&content)
            .with_context(|| format!("missing behavior heading in {}", path.display()))?;
        let relative_path = path
            .strip_prefix(project_root)
            .with_context(|| format!("{} is not under {}", path.display(), project_root.display()))?
            .to_path_buf();
        specs.push(BehaviorSpec {
            behavior,
            relative_path,
            content,
        });
    }

    Ok(specs)
}

pub fn sync_specs_to_parity(project_root: &Path, specs: &[BehaviorSpec]) -> Result<bool> {
    if specs.is_empty() {
        return Ok(false);
    }

    let parity_path = project_root.join("PARITY.md");
    let existing = std::fs::read_to_string(&parity_path)
        .with_context(|| format!("failed to read {}", parity_path.display()))?;
    let mut report = ParityReport::parse(&existing)
        .with_context(|| format!("failed to parse {}", parity_path.display()))?;

    let mut changed = false;
    let mut row_index: HashMap<String, usize> = report
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| (row.behavior.clone(), index))
        .collect();

    for spec in specs {
        if let Some(index) = row_index.get(&spec.behavior).copied() {
            let row = &mut report.rows[index];
            if row.spec != ParityStatus::Complete {
                row.spec = ParityStatus::Complete;
                changed = true;
            }
            let note = format!("spec: {}", spec.relative_path.display());
            if row.notes != note {
                row.notes = note;
                changed = true;
            }
            continue;
        }

        report.rows.push(ParityRow {
            behavior: spec.behavior.clone(),
            spec: ParityStatus::Complete,
            test: ParityStatus::NotStarted,
            implementation: ParityStatus::NotStarted,
            verified: VerificationStatus::NotStarted,
            notes: format!("spec: {}", spec.relative_path.display()),
        });
        let index = report.rows.len() - 1;
        row_index.insert(report.rows[index].behavior.clone(), index);
        changed = true;
    }

    if !changed {
        return Ok(false);
    }

    let updated = render_parity_report(&report);
    std::fs::write(&parity_path, updated)
        .with_context(|| format!("failed to write {}", parity_path.display()))?;
    Ok(true)
}

pub fn validate_spec_content(content: &str) -> Result<()> {
    let behavior = extract_behavior_name(content)?;
    if behavior.trim().is_empty() {
        bail!("behavior heading must not be empty");
    }

    let required_sections = [
        "## Purpose",
        "## Inputs",
        "## Outputs",
        "## State Transitions",
        "## Edge Cases",
        "## Acceptance Criteria",
    ];
    for section in required_sections {
        if !content.contains(section) {
            bail!("missing required section '{section}'");
        }
    }

    let lower = content.to_ascii_lowercase();
    for forbidden in [
        "register",
        "opcode",
        "instruction",
        "address",
        "memory address",
        "decompiled",
        "disassembly",
        "0x",
    ] {
        if lower.contains(forbidden) {
            bail!("contains forbidden implementation detail '{forbidden}'");
        }
    }

    for register in ["AF", "BC", "DE", "HL", "IX", "IY", "SP", "PC"] {
        if content
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|token| token == register)
        {
            bail!("contains forbidden register name '{register}'");
        }
    }

    Ok(())
}

fn extract_behavior_name(content: &str) -> Result<String> {
    let heading = content
        .lines()
        .find_map(|line| line.strip_prefix("# Behavior:"))
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .context("spec must start with '# Behavior: <name>'")?;
    Ok(heading.to_string())
}

fn collect_spec_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in
        std::fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_spec_files(&path, files)?;
            continue;
        }
        if path.file_name().and_then(|name| name.to_str()) == Some(SPEC_FILENAME) {
            files.push(path);
        }
    }
    Ok(())
}

fn render_parity_report(report: &ParityReport) -> String {
    let mut output = String::new();
    output.push_str("---\n");
    render_metadata(&mut output, &report.metadata);
    output.push_str("---\n\n");
    output.push_str("| Behavior | Spec | Test | Implementation | Verified | Notes |\n");
    output.push_str("| --- | --- | --- | --- | --- | --- |\n");
    for row in &report.rows {
        let _ = writeln!(
            output,
            "| {} | {} | {} | {} | {} | {} |",
            row.behavior, row.spec, row.test, row.implementation, row.verified, row.notes
        );
    }
    output
}

fn render_metadata(output: &mut String, metadata: &ParityMetadata) {
    let _ = writeln!(output, "project: {}", metadata.project);
    let _ = writeln!(output, "target: {}", metadata.target);
    let _ = writeln!(output, "source_platform: {}", metadata.source_platform);
    let _ = writeln!(output, "target_language: {}", metadata.target_language);
    let _ = writeln!(output, "last_verified: {}", metadata.last_verified);
    if let Some(overall_parity) = &metadata.overall_parity {
        let _ = writeln!(output, "overall_parity: {}", overall_parity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_SPEC: &str = r#"# Behavior: Player movement

## Purpose

Describe how the player moves in response to directional input.

## Inputs

- Directional control input.

## Outputs

- The visible player position changes on screen.

## State Transitions

- Movement begins after input and stops when input ends.

## Edge Cases

- Movement is blocked by solid obstacles.

## Acceptance Criteria

- Given a move input, the player advances exactly one walk step per tick.
"#;

    const PARITY: &str = r#"---
project: manic-miner
target: analysis-artifacts
source_platform: zx-spectrum
target_language: rust
last_verified: pending
overall_parity: 0%
---

| Behavior | Spec | Test | Implementation | Verified | Notes |
| --- | --- | --- | --- | --- | --- |
| Startup behavior | -- | -- | -- | -- | |
| Player movement | draft | -- | -- | -- | stale |
"#;

    #[test]
    fn validate_spec_rejects_implementation_leaks() {
        let leaked = VALID_SPEC.replace(
            "Describe how the player moves in response to directional input.",
            "Describe how the player moves using register HL and opcode 0x7E.",
        );
        let err = validate_spec_content(&leaked).unwrap_err().to_string();
        assert!(err.contains("forbidden"));
    }

    #[test]
    fn load_behavior_specs_discovers_nested_spec_files() {
        let tmp = tempfile::tempdir().unwrap();
        let spec_path = tmp.path().join("specs/player-movement/SPEC.md");
        std::fs::create_dir_all(spec_path.parent().unwrap()).unwrap();
        std::fs::write(&spec_path, VALID_SPEC).unwrap();

        let specs = load_behavior_specs(tmp.path()).unwrap();
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].behavior, "Player movement");
        assert_eq!(
            specs[0].relative_path,
            PathBuf::from("specs/player-movement/SPEC.md")
        );
    }

    #[test]
    fn sync_specs_to_parity_marks_specs_complete_and_adds_rows() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("PARITY.md"), PARITY).unwrap();

        let specs = vec![
            BehaviorSpec {
                behavior: "Player movement".to_string(),
                relative_path: PathBuf::from("specs/player-movement/SPEC.md"),
                content: VALID_SPEC.to_string(),
            },
            BehaviorSpec {
                behavior: "Collision detection".to_string(),
                relative_path: PathBuf::from("specs/collision-detection/SPEC.md"),
                content: VALID_SPEC.replace("Player movement", "Collision detection"),
            },
        ];

        let changed = sync_specs_to_parity(tmp.path(), &specs).unwrap();
        assert!(changed);

        let updated = std::fs::read_to_string(tmp.path().join("PARITY.md")).unwrap();
        let report = ParityReport::parse(&updated).unwrap();
        let movement = report
            .rows
            .iter()
            .find(|row| row.behavior == "Player movement")
            .unwrap();
        assert_eq!(movement.spec, ParityStatus::Complete);
        assert_eq!(movement.notes, "spec: specs/player-movement/SPEC.md");

        let collision = report
            .rows
            .iter()
            .find(|row| row.behavior == "Collision detection")
            .unwrap();
        assert_eq!(collision.spec, ParityStatus::Complete);
        assert_eq!(collision.test, ParityStatus::NotStarted);
    }
}
