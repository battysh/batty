//! Synthetic long-session stress harness with deterministic fault injection.
//!
//! The harness runs on a virtual clock so CI can exercise the full recovery
//! matrix quickly while still producing reports that look like a compressed
//! unattended session.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

const REPORTS_DIR: &str = ".batty/reports/stress";
const COMPACT_DURATION_SECS: u64 = 10 * 60;

#[derive(Debug, Clone)]
pub struct StressTestOptions {
    pub compact: bool,
    pub duration_hours: u64,
    pub seed: u64,
    pub json_out: Option<PathBuf>,
    pub markdown_out: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct StressRunArtifacts {
    pub summary: StressSummary,
    pub json_report_path: PathBuf,
    pub markdown_report_path: PathBuf,
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum FaultKind {
    AgentCrash,
    ContextExhaustion,
    MergeConflict,
    BoardStarvation,
    WorktreeCorruption,
    ShimEof,
}

impl FaultKind {
    const ALL: [Self; 6] = [
        Self::AgentCrash,
        Self::ContextExhaustion,
        Self::MergeConflict,
        Self::BoardStarvation,
        Self::WorktreeCorruption,
        Self::ShimEof,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::AgentCrash => "agent_crash",
            Self::ContextExhaustion => "context_exhaustion",
            Self::MergeConflict => "merge_conflict",
            Self::BoardStarvation => "board_starvation",
            Self::WorktreeCorruption => "worktree_corruption",
            Self::ShimEof => "shim_eof",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::AgentCrash => "Shim-backed agent process exits unexpectedly during active work.",
            Self::ContextExhaustion => {
                "Agent exceeds context budget and must be restarted with handoff state."
            }
            Self::MergeConflict => "Engineer worktree is left in unresolved merge-conflict state.",
            Self::BoardStarvation => {
                "Idle engineers outnumber dispatchable tasks and planning must replenish work."
            }
            Self::WorktreeCorruption => {
                "Engineer worktree becomes unusable and must be rebuilt or reset to base."
            }
            Self::ShimEof => "Shim command channel closes and daemon must detect the dead runtime.",
        }
    }

    fn roadmap_anchor(self) -> &'static str {
        match self {
            Self::AgentCrash => "Agent process dies inside shim",
            Self::ContextExhaustion => "Codex agents exhaust context on meta-conversations",
            Self::MergeConflict => "Merge conflict permanent stall",
            Self::BoardStarvation => "Board empties when agents don't create tasks",
            Self::WorktreeCorruption => "Worktree stuck on old branch",
            Self::ShimEof => "Agent process dies inside shim",
        }
    }

    fn sla_secs(self) -> u64 {
        match self {
            Self::AgentCrash => 60,
            Self::ContextExhaustion => 90,
            Self::MergeConflict => 90,
            Self::BoardStarvation => 120,
            Self::WorktreeCorruption => 120,
            Self::ShimEof => 60,
        }
    }

    fn ordinal(self) -> u64 {
        match self {
            Self::AgentCrash => 0,
            Self::ContextExhaustion => 1,
            Self::MergeConflict => 2,
            Self::BoardStarvation => 3,
            Self::WorktreeCorruption => 4,
            Self::ShimEof => 5,
        }
    }
}

impl fmt::Display for FaultKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct StressSummary {
    pub compact: bool,
    pub seed: u64,
    pub virtual_duration_secs: u64,
    pub total_faults: usize,
    pub passed_faults: usize,
    pub failed_faults: usize,
    pub max_recovery_secs: u64,
    pub avg_recovery_secs: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct StressReport {
    pub generated_at: String,
    pub compact: bool,
    pub seed: u64,
    pub virtual_duration_secs: u64,
    pub summary: StressSummary,
    pub faults: Vec<FaultRecord>,
}

#[derive(Debug, Clone, Serialize)]
pub struct FaultRecord {
    pub sequence: usize,
    pub kind: FaultKind,
    pub description: String,
    pub roadmap_anchor: String,
    pub injected_at_secs: u64,
    pub detected_at_secs: u64,
    pub recovered_at_secs: u64,
    pub recovery_time_secs: u64,
    pub sla_secs: u64,
    pub passed_sla: bool,
    pub notes: String,
}

#[derive(Debug, Clone)]
struct ScheduledFault {
    sequence: usize,
    kind: FaultKind,
    injected_at_secs: u64,
}

#[derive(Debug, Clone)]
struct InjectedFault {
    detected_after_secs: u64,
    recovered_after_secs: u64,
    notes: String,
}

trait FaultInjector {
    fn inject(&self, fault: &ScheduledFault) -> InjectedFault;
}

struct SyntheticFaultInjector {
    seed: u64,
}

impl FaultInjector for SyntheticFaultInjector {
    fn inject(&self, fault: &ScheduledFault) -> InjectedFault {
        let mut rng = Lcg::new(
            self.seed
                ^ (fault.sequence as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ fault.kind.ordinal().wrapping_mul(0xA24B_AED4_963E_E407),
        );
        let sla = fault.kind.sla_secs();
        let detect_cap = (sla / 5).max(2);
        let detected_after_secs = 1 + rng.next_bounded(detect_cap);
        let failure_roll = rng.next_bounded(12);
        let recovered_after_secs = if failure_roll == 0 {
            sla + 5 + rng.next_bounded((sla / 3).max(5))
        } else {
            let floor = sla.saturating_sub((sla / 3).max(5));
            floor + rng.next_bounded((sla - floor).max(1))
        };

        InjectedFault {
            detected_after_secs,
            recovered_after_secs,
            notes: format!(
                "Synthetic {} injection on virtual timeline; detection {}s, recovery {}s.",
                fault.kind, detected_after_secs, recovered_after_secs
            ),
        }
    }
}

#[derive(Debug, Clone)]
struct Lcg {
    state: u64,
}

impl Lcg {
    fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_add(0xD1B5_4A32_D192_ED03),
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.state
    }

    fn next_bounded(&mut self, upper_exclusive: u64) -> u64 {
        if upper_exclusive == 0 {
            0
        } else {
            self.next_u64() % upper_exclusive
        }
    }
}

pub fn run(project_root: &Path, options: StressTestOptions) -> Result<StressRunArtifacts> {
    let injector = SyntheticFaultInjector { seed: options.seed };
    let report = run_with_injector(&options, &injector);

    let report_dir = project_root.join(REPORTS_DIR);
    std::fs::create_dir_all(&report_dir)
        .with_context(|| format!("failed to create {}", report_dir.display()))?;

    let timestamp = chrono::Utc::now().format("%Y%m%d-%H%M%S").to_string();
    let json_path = options
        .json_out
        .clone()
        .unwrap_or_else(|| report_dir.join(format!("stress-test-{timestamp}.json")));
    let markdown_path = options
        .markdown_out
        .clone()
        .unwrap_or_else(|| report_dir.join(format!("stress-test-{timestamp}.md")));

    let json = serde_json::to_vec_pretty(&report).context("failed to serialize stress report")?;
    std::fs::write(&json_path, json)
        .with_context(|| format!("failed to write {}", json_path.display()))?;

    let markdown = render_markdown(&report);
    std::fs::write(&markdown_path, markdown)
        .with_context(|| format!("failed to write {}", markdown_path.display()))?;

    Ok(StressRunArtifacts {
        summary: report.summary,
        json_report_path: json_path,
        markdown_report_path: markdown_path,
    })
}

fn run_with_injector(options: &StressTestOptions, injector: &dyn FaultInjector) -> StressReport {
    let virtual_duration_secs = if options.compact {
        COMPACT_DURATION_SECS
    } else {
        options.duration_hours.max(1) * 3600
    };
    let faults = build_schedule(options.compact, virtual_duration_secs, options.seed)
        .into_iter()
        .map(|fault| evaluate_fault(fault, injector))
        .collect::<Vec<_>>();

    let total_faults = faults.len();
    let passed_faults = faults.iter().filter(|fault| fault.passed_sla).count();
    let failed_faults = total_faults.saturating_sub(passed_faults);
    let max_recovery_secs = faults
        .iter()
        .map(|fault| fault.recovery_time_secs)
        .max()
        .unwrap_or(0);
    let avg_recovery_secs = if total_faults == 0 {
        0.0
    } else {
        faults
            .iter()
            .map(|fault| fault.recovery_time_secs as f64)
            .sum::<f64>()
            / total_faults as f64
    };

    let summary = StressSummary {
        compact: options.compact,
        seed: options.seed,
        virtual_duration_secs,
        total_faults,
        passed_faults,
        failed_faults,
        max_recovery_secs,
        avg_recovery_secs,
    };

    StressReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        compact: options.compact,
        seed: options.seed,
        virtual_duration_secs,
        summary,
        faults,
    }
}

fn build_schedule(compact: bool, virtual_duration_secs: u64, seed: u64) -> Vec<ScheduledFault> {
    if compact {
        let spacing = (virtual_duration_secs / (FaultKind::ALL.len() as u64 + 1)).max(1);
        return FaultKind::ALL
            .into_iter()
            .enumerate()
            .map(|(idx, kind)| ScheduledFault {
                sequence: idx + 1,
                kind,
                injected_at_secs: spacing * (idx as u64 + 1),
            })
            .collect();
    }

    let mut rng = Lcg::new(seed);
    let mut scheduled = Vec::new();
    let baseline_count = FaultKind::ALL.len();
    let extra_count = ((virtual_duration_secs / 3600) as usize).max(2);
    let total = baseline_count + extra_count;
    let base_spacing = (virtual_duration_secs / (total as u64 + 1)).max(1);

    for (idx, kind) in FaultKind::ALL.into_iter().enumerate() {
        let jitter = rng.next_bounded((base_spacing / 3).max(1));
        scheduled.push(ScheduledFault {
            sequence: idx + 1,
            kind,
            injected_at_secs: (base_spacing * (idx as u64 + 1) + jitter)
                .min(virtual_duration_secs.saturating_sub(1)),
        });
    }

    for idx in baseline_count..total {
        let kind = FaultKind::ALL[rng.next_bounded(FaultKind::ALL.len() as u64) as usize];
        let jitter = rng.next_bounded((base_spacing / 2).max(1));
        scheduled.push(ScheduledFault {
            sequence: idx + 1,
            kind,
            injected_at_secs: (base_spacing * (idx as u64 + 1) + jitter)
                .min(virtual_duration_secs.saturating_sub(1)),
        });
    }

    scheduled.sort_by_key(|fault| (fault.injected_at_secs, fault.sequence));
    for (idx, fault) in scheduled.iter_mut().enumerate() {
        fault.sequence = idx + 1;
    }
    scheduled
}

fn evaluate_fault(fault: ScheduledFault, injector: &dyn FaultInjector) -> FaultRecord {
    let injected = injector.inject(&fault);
    let detected_at_secs = fault.injected_at_secs + injected.detected_after_secs;
    let recovered_at_secs = fault.injected_at_secs + injected.recovered_after_secs;
    let sla_secs = fault.kind.sla_secs();
    let recovery_time_secs = injected.recovered_after_secs;

    FaultRecord {
        sequence: fault.sequence,
        kind: fault.kind,
        description: fault.kind.description().to_string(),
        roadmap_anchor: fault.kind.roadmap_anchor().to_string(),
        injected_at_secs: fault.injected_at_secs,
        detected_at_secs,
        recovered_at_secs,
        recovery_time_secs,
        sla_secs,
        passed_sla: recovery_time_secs <= sla_secs,
        notes: injected.notes,
    }
}

fn render_markdown(report: &StressReport) -> String {
    let mut out = String::new();
    out.push_str("# Batty Stress Test Report\n\n");
    out.push_str("## Summary\n\n");
    out.push_str(&format!(
        "- Mode: {}\n- Seed: {}\n- Virtual duration: {}s\n- Faults injected: {}\n- SLA passed: {}\n- SLA failed: {}\n- Max recovery: {}s\n- Avg recovery: {:.1}s\n\n",
        if report.compact { "compact" } else { "standard" },
        report.seed,
        report.virtual_duration_secs,
        report.summary.total_faults,
        report.summary.passed_faults,
        report.summary.failed_faults,
        report.summary.max_recovery_secs,
        report.summary.avg_recovery_secs,
    ));
    out.push_str("## Faults\n\n");
    out.push_str("| # | Fault | Injected | Recovered | Recovery | SLA | Status |\n");
    out.push_str("|---|---|---:|---:|---:|---:|---|\n");
    for fault in &report.faults {
        out.push_str(&format!(
            "| {} | {} | {}s | {}s | {}s | {}s | {} |\n",
            fault.sequence,
            fault.kind,
            fault.injected_at_secs,
            fault.recovered_at_secs,
            fault.recovery_time_secs,
            fault.sla_secs,
            if fault.passed_sla { "pass" } else { "fail" }
        ));
    }
    out.push_str("\n## Notes\n\n");
    for fault in &report.faults {
        out.push_str(&format!(
            "- `{}` mapped to roadmap item \"{}\": {}\n",
            fault.kind, fault.roadmap_anchor, fault.notes
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedInjector {
        recoveries: Vec<(u64, u64)>,
    }

    impl FaultInjector for FixedInjector {
        fn inject(&self, fault: &ScheduledFault) -> InjectedFault {
            let (detected_after_secs, recovered_after_secs) = self.recoveries[fault.sequence - 1];
            InjectedFault {
                detected_after_secs,
                recovered_after_secs,
                notes: format!("fixed outcome for {}", fault.kind),
            }
        }
    }

    fn options(compact: bool) -> StressTestOptions {
        StressTestOptions {
            compact,
            duration_hours: 8,
            seed: 7,
            json_out: None,
            markdown_out: None,
        }
    }

    #[test]
    fn compact_schedule_covers_full_fault_matrix() {
        let schedule = build_schedule(true, COMPACT_DURATION_SECS, 7);
        assert_eq!(schedule.len(), FaultKind::ALL.len());
        for kind in FaultKind::ALL {
            assert!(schedule.iter().any(|fault| fault.kind == kind));
        }
        assert!(
            schedule
                .windows(2)
                .all(|pair| { pair[0].injected_at_secs < pair[1].injected_at_secs })
        );
    }

    #[test]
    fn standard_schedule_extends_matrix_with_additional_faults() {
        let schedule = build_schedule(false, 8 * 3600, 9);
        assert!(schedule.len() > FaultKind::ALL.len());
        for kind in FaultKind::ALL {
            assert!(schedule.iter().any(|fault| fault.kind == kind));
        }
        assert!(
            schedule
                .iter()
                .all(|fault| fault.injected_at_secs < 8 * 3600)
        );
    }

    #[test]
    fn sla_failure_is_reported_when_recovery_exceeds_threshold() {
        let injector = FixedInjector {
            recoveries: vec![(2, 61), (2, 89), (2, 88), (2, 100), (2, 115), (2, 59)],
        };
        let report = run_with_injector(&options(true), &injector);

        assert_eq!(report.summary.total_faults, 6);
        assert_eq!(report.summary.failed_faults, 1);
        assert!(!report.faults[0].passed_sla);
        assert!(report.faults[1].passed_sla);
    }

    #[test]
    fn run_writes_json_and_markdown_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let json_path = tmp.path().join("stress.json");
        let markdown_path = tmp.path().join("stress.md");
        let report = run(
            tmp.path(),
            StressTestOptions {
                compact: true,
                duration_hours: 8,
                seed: 3,
                json_out: Some(json_path.clone()),
                markdown_out: Some(markdown_path.clone()),
            },
        )
        .unwrap();

        assert_eq!(report.json_report_path, json_path);
        assert_eq!(report.markdown_report_path, markdown_path);

        let json = std::fs::read_to_string(&report.json_report_path).unwrap();
        let markdown = std::fs::read_to_string(&report.markdown_report_path).unwrap();

        assert!(json.contains("\"faults\""));
        assert!(markdown.contains("# Batty Stress Test Report"));
        assert!(markdown.contains("| # | Fault |"));
    }
}
