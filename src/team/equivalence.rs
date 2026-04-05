//! Equivalence testing harness for comparing synthetic emulator runs.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::parity::{self, VerificationStatus};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InputSequence {
    pub events: Vec<String>,
}

impl InputSequence {
    pub fn new(events: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            events: events.into_iter().map(Into::into).collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OutputCapture {
    pub frames: Vec<String>,
    pub audio_events: Vec<String>,
    pub io_events: Vec<String>,
}

impl OutputCapture {
    pub fn new(frames: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            frames: frames.into_iter().map(Into::into).collect(),
            audio_events: Vec::new(),
            io_events: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRun {
    pub name: String,
    pub inputs: InputSequence,
    pub outputs: OutputCapture,
}

impl TestRun {
    pub fn frames(&self) -> &[String] {
        &self.outputs.frames
    }
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Z80Snapshot {
    pub program_counter: u16,
    pub stack_pointer: u16,
    pub memory: Vec<u8>,
}

pub fn load_z80_snapshot(bytes: &[u8]) -> Result<Z80Snapshot> {
    const HEADER_LEN: usize = 30;
    const MEMORY_LEN: usize = 48 * 1024;

    if bytes.len() < HEADER_LEN + MEMORY_LEN {
        bail!(
            ".z80 snapshot too short: expected at least {} bytes, got {}",
            HEADER_LEN + MEMORY_LEN,
            bytes.len()
        );
    }

    let program_counter = u16::from_le_bytes([bytes[6], bytes[7]]);
    if program_counter == 0 {
        bail!("version 2/3 .z80 snapshots are not supported by this fixture loader");
    }

    let flags = bytes[12];
    if flags & 0x20 != 0 {
        bail!("compressed version 1 .z80 snapshots are not supported by this fixture loader");
    }

    let stack_pointer = u16::from_le_bytes([bytes[8], bytes[9]]);
    let memory = bytes[HEADER_LEN..HEADER_LEN + MEMORY_LEN].to_vec();
    Ok(Z80Snapshot {
        program_counter,
        stack_pointer,
        memory,
    })
}

pub trait EmulatorBackend {
    fn run(&self, binary: &Path, inputs: &InputSequence) -> Result<OutputCapture>;
}

#[derive(Debug, Default, Clone)]
pub struct MockBackend {
    fixtures: HashMap<String, OutputCapture>,
}

impl MockBackend {
    pub fn with_fixture(mut self, binary: impl Into<String>, capture: OutputCapture) -> Self {
        self.fixtures.insert(binary.into(), capture);
        self
    }
}

impl EmulatorBackend for MockBackend {
    fn run(&self, binary: &Path, _inputs: &InputSequence) -> Result<OutputCapture> {
        let key = binary.to_string_lossy().into_owned();
        self.fixtures
            .get(&key)
            .cloned()
            .with_context(|| format!("no mock fixture registered for `{key}`"))
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct CommandBackend;

impl EmulatorBackend for CommandBackend {
    fn run(&self, binary: &Path, inputs: &InputSequence) -> Result<OutputCapture> {
        let mut child = Command::new(binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn `{}`", binary.display()))?;

        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            for input in &inputs.events {
                writeln!(stdin, "{input}")
                    .with_context(|| format!("failed to write input to `{}`", binary.display()))?;
            }
        }

        let output = child
            .wait_with_output()
            .with_context(|| format!("failed to read output from `{}`", binary.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "backend command `{}` failed: {}",
                binary.display(),
                stderr.trim()
            );
        }

        let frames = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect();

        Ok(OutputCapture {
            frames,
            audio_events: Vec::new(),
            io_events: Vec::new(),
        })
    }
}

pub fn execute_test_run(
    backend: &dyn EmulatorBackend,
    name: &str,
    binary: &Path,
    inputs: InputSequence,
) -> Result<TestRun> {
    let outputs = backend.run(binary, &inputs)?;
    Ok(TestRun {
        name: name.to_string(),
        inputs,
        outputs,
    })
}

pub fn compare_outputs(expected: &OutputCapture, actual: &OutputCapture) -> DiffReport {
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

    const FIXTURE_PATH: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/zx_spectrum/minimal_test_program.z80"
    );
    const FIXTURE_BYTES: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/zx_spectrum/minimal_test_program.z80"
    ));

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
        let expected = OutputCapture::new(["frame-1", "frame-2"]);
        let actual = OutputCapture::new(["frame-1", "frame-x"]);

        let diff = compare_outputs(&expected, &actual);
        assert_eq!(diff.matching_frames, 1);
        assert_eq!(diff.divergent_frames, 1);
        assert_eq!(diff.timing_difference, 0);
        assert_eq!(diff.divergent_indices, vec![1]);
    }

    #[test]
    fn z80_fixture_loads_with_expected_header_and_program_bytes() {
        let snapshot = load_z80_snapshot(FIXTURE_BYTES).unwrap();

        assert_eq!(snapshot.program_counter, 0x8000);
        assert_eq!(snapshot.stack_pointer, 0x5c00);
        assert_eq!(snapshot.memory.len(), 48 * 1024);

        let program_offset = 0x8000usize - 0x4000usize;
        assert_eq!(snapshot.memory[program_offset], 0xF3);
        assert_eq!(
            &snapshot.memory[program_offset..program_offset + 7],
            &[0xF3, 0x21, 0x00, 0x40, 0x11, 0x01, 0x40]
        );
        assert_eq!(
            &snapshot.memory[0x9000usize - 0x4000usize..0x9003usize - 0x4000usize],
            &[0x00, 0x00, 0x00]
        );
    }

    #[test]
    fn z80_fixture_behavior_doc_mentions_observable_outputs() {
        let behavior =
            std::fs::read_to_string(Path::new(FIXTURE_PATH).with_file_name("BEHAVIOR.md")).unwrap();

        assert!(behavior.contains("Fills the 6912-byte display file"));
        assert!(behavior.contains("Toggles port `0xFE` twice"));
        assert!(behavior.contains("Writes `0x42` to address `0x9000`"));
    }

    #[test]
    fn mock_backend_runs_trivial_fixture_and_updates_parity() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("PARITY.md"), parity_fixture()).unwrap();

        let inputs = InputSequence::new(["fill", "flip"]);
        let backend = MockBackend::default()
            .with_fixture("original.bin", OutputCapture::new(["frame-a", "frame-b"]))
            .with_fixture("reimpl.bin", OutputCapture::new(["frame-a", "frame-b"]));

        let expected = execute_test_run(
            &backend,
            "original",
            Path::new("original.bin"),
            inputs.clone(),
        )
        .unwrap();
        let actual = execute_test_run(&backend, "reimpl", Path::new("reimpl.bin"), inputs).unwrap();

        assert_eq!(
            expected.frames(),
            &["frame-a".to_string(), "frame-b".to_string()]
        );

        let diff = compare_outputs(&expected.outputs, &actual.outputs);
        assert!(diff.passed(), "diff should match: {diff:?}");

        update_parity_from_diff(tmp.path(), "Screen fill", &diff).unwrap();

        let updated = std::fs::read_to_string(tmp.path().join("PARITY.md")).unwrap();
        assert!(updated.contains("| Screen fill | complete | complete | complete | PASS |"));
        assert!(updated.contains("matching_frames=2"));
        assert!(updated.contains("overall_parity: 100%"));
    }
}
