//! Equivalence testing harness for comparing recorded runs.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::parity::{self, VerificationStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRun {
    pub name: String,
    pub inputs: Vec<String>,
    pub frames: Vec<String>,
    pub audio_events: Vec<String>,
    pub io_events: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffReport {
    pub matching_frames: usize,
    pub divergent_frames: usize,
    pub timing_difference: isize,
    pub divergent_indices: Vec<usize>,
}

impl DiffReport {
    pub fn passed(&self) -> bool {
        self.divergent_frames == 0 && self.timing_difference == 0
    }

    pub fn summary(&self) -> String {
        format!(
            "matching_frames={}, divergent_frames={}, timing_difference={}",
            self.matching_frames, self.divergent_frames, self.timing_difference
        )
    }
}

pub fn run_command_capture_frames(
    name: &str,
    command: &str,
    inputs: &[String],
    work_dir: &Path,
) -> Result<TestRun> {
    let mut child = Command::new("sh")
        .args(["-c", command])
        .current_dir(work_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn equivalence command `{command}`"))?;

    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        for input in inputs {
            writeln!(stdin, "{input}")
                .with_context(|| format!("failed to write input to `{command}`"))?;
        }
    }

    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to read output from `{command}`"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("equivalence command `{command}` failed: {}", stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let frames = stdout
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect();

    Ok(TestRun {
        name: name.to_string(),
        inputs: inputs.to_vec(),
        frames,
        audio_events: Vec::new(),
        io_events: Vec::new(),
    })
}

pub fn compare_outputs(expected: &TestRun, actual: &TestRun) -> DiffReport {
    let compared_len = expected.frames.len().min(actual.frames.len());
    let mut matching_frames = 0;
    let mut divergent_indices = Vec::new();

    for idx in 0..compared_len {
        if expected.frames[idx] == actual.frames[idx] {
            matching_frames += 1;
        } else {
            divergent_indices.push(idx);
        }
    }

    if expected.frames.len() > compared_len {
        divergent_indices.extend(compared_len..expected.frames.len());
    } else if actual.frames.len() > compared_len {
        divergent_indices.extend(compared_len..actual.frames.len());
    }

    DiffReport {
        matching_frames,
        divergent_frames: divergent_indices.len(),
        timing_difference: actual.frames.len() as isize - expected.frames.len() as isize,
        divergent_indices,
    }
}

pub fn update_parity_from_diff(
    project_root: &Path,
    behavior: &str,
    report: &DiffReport,
) -> Result<()> {
    let verification = if report.passed() {
        VerificationStatus::Pass
    } else {
        VerificationStatus::Fail
    };
    parity::update_parity_verification(project_root, behavior, verification, &report.summary())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parity_fixture() -> String {
        r#"---
project: trivial
target: trivial.z80
source_platform: zx-spectrum-z80
target_language: rust
last_verified: 2026-04-05
overall_parity: 0%
---

| Behavior | Spec | Test | Implementation | Verified | Notes |
| --- | --- | --- | --- | --- | --- |
| Screen fill | complete | complete | complete | -- | pending |
"#
        .to_string()
    }

    #[test]
    fn compare_outputs_counts_matching_and_divergent_frames() {
        let expected = TestRun {
            name: "expected".to_string(),
            inputs: vec!["A".to_string()],
            frames: vec!["frame-1".to_string(), "frame-2".to_string()],
            audio_events: Vec::new(),
            io_events: Vec::new(),
        };
        let actual = TestRun {
            name: "actual".to_string(),
            inputs: vec!["A".to_string()],
            frames: vec!["frame-1".to_string(), "frame-x".to_string()],
            audio_events: Vec::new(),
            io_events: Vec::new(),
        };

        let diff = compare_outputs(&expected, &actual);
        assert_eq!(diff.matching_frames, 1);
        assert_eq!(diff.divergent_frames, 1);
        assert_eq!(diff.timing_difference, 0);
        assert_eq!(diff.divergent_indices, vec![1]);
    }

    #[test]
    fn trivial_program_end_to_end_updates_parity() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("PARITY.md"), parity_fixture()).unwrap();

        let original = tmp.path().join("original.sh");
        let reimpl = tmp.path().join("reimpl.sh");
        std::fs::write(
            &original,
            "#!/bin/sh\nwhile IFS= read -r line; do printf 'FRAME:%s\\n' \"$line\"; done\n",
        )
        .unwrap();
        std::fs::write(
            &reimpl,
            "#!/bin/sh\nwhile IFS= read -r line; do printf 'FRAME:%s\\n' \"$line\"; done\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::set_permissions(&reimpl, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let inputs = vec!["fill".to_string(), "flip".to_string()];
        let expected = run_command_capture_frames(
            "original",
            original.to_string_lossy().as_ref(),
            &inputs,
            tmp.path(),
        )
        .unwrap();
        let actual = run_command_capture_frames(
            "reimpl",
            reimpl.to_string_lossy().as_ref(),
            &inputs,
            tmp.path(),
        )
        .unwrap();

        let diff = compare_outputs(&expected, &actual);
        assert!(diff.passed(), "diff should match: {diff:?}");

        update_parity_from_diff(tmp.path(), "Screen fill", &diff).unwrap();

        let updated = std::fs::read_to_string(tmp.path().join("PARITY.md")).unwrap();
        assert!(updated.contains("| Screen fill | complete | complete | complete | PASS |"));
        assert!(updated.contains("matching_frames=2"));
        assert!(updated.contains("overall_parity: 100%"));
    }
}
