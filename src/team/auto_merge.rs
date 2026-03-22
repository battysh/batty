//! Auto-merge policy engine with confidence scoring.
//!
//! Evaluates completed task diffs and decides whether to auto-merge
//! or route to manual review based on configurable thresholds.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

use super::config::AutoMergePolicy;

const OVERRIDES_FILE: &str = ".batty/auto_merge_overrides.json";

/// Load per-task auto-merge overrides from disk.
pub fn load_overrides(project_root: &Path) -> HashMap<u32, bool> {
    let path = project_root.join(OVERRIDES_FILE);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

/// Save a per-task auto-merge override to disk.
pub fn save_override(project_root: &Path, task_id: u32, enabled: bool) -> Result<()> {
    let path = project_root.join(OVERRIDES_FILE);
    let mut overrides = load_overrides(project_root);
    overrides.insert(task_id, enabled);
    let content = serde_json::to_string_pretty(&overrides)
        .context("failed to serialize auto-merge overrides")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&path, content).context("failed to write auto-merge overrides file")?;
    Ok(())
}

/// Summary of a git diff between two refs.
#[derive(Debug, Clone)]
pub struct DiffSummary {
    pub files_changed: usize,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub modules_touched: HashSet<String>,
    pub sensitive_files: Vec<String>,
    pub has_unsafe: bool,
    pub has_conflicts: bool,
}

impl DiffSummary {
    pub fn total_lines(&self) -> usize {
        self.lines_added + self.lines_removed
    }
}

/// Decision returned by the policy engine.
#[derive(Debug, Clone, PartialEq)]
pub enum AutoMergeDecision {
    AutoMerge {
        confidence: f64,
    },
    ManualReview {
        confidence: f64,
        reasons: Vec<String>,
    },
}

/// Analyze the diff between `base` and `branch` in the given repo.
pub fn analyze_diff(repo: &Path, base: &str, branch: &str) -> Result<DiffSummary> {
    // Get --stat for file count and per-file changes
    let stat_output = Command::new("git")
        .args(["diff", "--numstat", &format!("{}...{}", base, branch)])
        .current_dir(repo)
        .output()
        .context("failed to run git diff --numstat")?;

    let stat_str = String::from_utf8_lossy(&stat_output.stdout);

    let mut files_changed = 0usize;
    let mut lines_added = 0usize;
    let mut lines_removed = 0usize;
    let mut modules_touched = HashSet::new();
    let mut changed_paths = Vec::new();

    for line in stat_str.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() < 3 {
            continue;
        }
        files_changed += 1;
        if let Ok(added) = parts[0].parse::<usize>() {
            lines_added += added;
        }
        if let Ok(removed) = parts[1].parse::<usize>() {
            lines_removed += removed;
        }
        let path = parts[2];
        changed_paths.push(path.to_string());

        // Extract top-level module (first component under src/)
        if let Some(rest) = path.strip_prefix("src/") {
            if let Some(module) = rest.split('/').next() {
                modules_touched.insert(module.to_string());
            }
        }
    }

    // Get full diff to check for unsafe blocks
    let diff_output = Command::new("git")
        .args(["diff", &format!("{}...{}", base, branch)])
        .current_dir(repo)
        .output()
        .context("failed to run git diff")?;

    let diff_str = String::from_utf8_lossy(&diff_output.stdout);
    let has_unsafe = diff_str.lines().any(|line| {
        line.starts_with('+') && (line.contains("unsafe {") || line.contains("unsafe fn"))
    });

    // Check if branch can merge cleanly into base
    let has_conflicts = check_has_conflicts(repo, base, branch);

    Ok(DiffSummary {
        files_changed,
        lines_added,
        lines_removed,
        modules_touched,
        sensitive_files: changed_paths, // filtered by caller via policy
        has_unsafe,
        has_conflicts,
    })
}

/// Check whether merging `branch` into `base` would produce conflicts.
fn check_has_conflicts(repo: &Path, base: &str, branch: &str) -> bool {
    let merge_base = Command::new("git")
        .args(["merge-base", base, branch])
        .current_dir(repo)
        .output();
    let merge_base_sha = match merge_base {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => return true, // Can't find merge base — treat as conflicting
    };

    let result = Command::new("git")
        .args(["merge-tree", &merge_base_sha, base, branch])
        .current_dir(repo)
        .output();
    match result {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.contains("<<<<<<") || stdout.contains("changed in both")
        }
        Err(_) => true, // merge-tree failed — assume conflicts
    }
}

/// Compute merge confidence score (0.0-1.0) from a diff summary and policy.
pub fn compute_merge_confidence(summary: &DiffSummary, policy: &AutoMergePolicy) -> f64 {
    let mut confidence = 1.0f64;

    // Subtract 0.1 per file over 3
    if summary.files_changed > 3 {
        confidence -= 0.1 * (summary.files_changed - 3) as f64;
    }

    // Subtract 0.2 per module touched over 1
    if summary.modules_touched.len() > 1 {
        confidence -= 0.2 * (summary.modules_touched.len() - 1) as f64;
    }

    // Subtract 0.3 if any sensitive path touched
    let touches_sensitive = summary
        .sensitive_files
        .iter()
        .any(|f| policy.sensitive_paths.iter().any(|s| f.contains(s)));
    if touches_sensitive {
        confidence -= 0.3;
    }

    // Subtract 0.1 per 50 lines over 100
    let total_lines = summary.total_lines();
    if total_lines > 100 {
        let excess = total_lines - 100;
        confidence -= 0.1 * (excess / 50) as f64;
    }

    // Subtract 0.4 if unsafe blocks or FFI
    if summary.has_unsafe {
        confidence -= 0.4;
    }

    // Subtract 0.5 if conflicts detected with main
    if summary.has_conflicts {
        confidence -= 0.5;
    }

    // Floor at 0.0
    confidence.max(0.0)
}

/// Decide whether to auto-merge or route to manual review.
///
/// `tests_passed` indicates whether the task's test suite passed. When
/// `policy.require_tests_pass` is true and tests haven't passed, the
/// decision is always manual review regardless of other criteria.
pub fn should_auto_merge(
    summary: &DiffSummary,
    policy: &AutoMergePolicy,
    tests_passed: bool,
) -> AutoMergeDecision {
    if !policy.enabled {
        return AutoMergeDecision::ManualReview {
            confidence: compute_merge_confidence(summary, policy),
            reasons: vec!["auto-merge disabled by policy".to_string()],
        };
    }

    let confidence = compute_merge_confidence(summary, policy);
    let mut reasons = Vec::new();

    if policy.require_tests_pass && !tests_passed {
        reasons.push("tests did not pass".to_string());
    }

    if summary.has_conflicts {
        reasons.push("conflicts with main".to_string());
    }

    if confidence < policy.confidence_threshold {
        reasons.push(format!(
            "confidence {:.2} below threshold {:.2}",
            confidence, policy.confidence_threshold
        ));
    }

    if summary.files_changed > policy.max_files_changed {
        reasons.push(format!(
            "{} files changed (max {})",
            summary.files_changed, policy.max_files_changed
        ));
    }

    let total_lines = summary.total_lines();
    if total_lines > policy.max_diff_lines {
        reasons.push(format!(
            "{} diff lines (max {})",
            total_lines, policy.max_diff_lines
        ));
    }

    if summary.modules_touched.len() > policy.max_modules_touched {
        reasons.push(format!(
            "{} modules touched (max {})",
            summary.modules_touched.len(),
            policy.max_modules_touched
        ));
    }

    let touches_sensitive = summary
        .sensitive_files
        .iter()
        .any(|f| policy.sensitive_paths.iter().any(|s| f.contains(s)));
    if touches_sensitive {
        reasons.push("touches sensitive paths".to_string());
    }

    if summary.has_unsafe {
        reasons.push("contains unsafe blocks".to_string());
    }

    if reasons.is_empty() {
        AutoMergeDecision::AutoMerge { confidence }
    } else {
        AutoMergeDecision::ManualReview {
            confidence,
            reasons,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_policy() -> AutoMergePolicy {
        AutoMergePolicy::default()
    }

    fn enabled_policy() -> AutoMergePolicy {
        AutoMergePolicy {
            enabled: true,
            ..AutoMergePolicy::default()
        }
    }

    fn make_summary(
        files: usize,
        added: usize,
        removed: usize,
        modules: Vec<&str>,
        sensitive: Vec<&str>,
        has_unsafe: bool,
    ) -> DiffSummary {
        DiffSummary {
            files_changed: files,
            lines_added: added,
            lines_removed: removed,
            modules_touched: modules.into_iter().map(String::from).collect(),
            sensitive_files: sensitive.into_iter().map(String::from).collect(),
            has_unsafe,
            has_conflicts: false,
        }
    }

    #[test]
    fn small_clean_diff_auto_merges() {
        let summary = make_summary(2, 30, 20, vec!["team"], vec![], false);
        let policy = enabled_policy();
        let decision = should_auto_merge(&summary, &policy, true);
        match decision {
            AutoMergeDecision::AutoMerge { confidence } => {
                assert!(
                    confidence >= 0.8,
                    "confidence should be >= 0.8, got {}",
                    confidence
                );
            }
            other => panic!("expected AutoMerge, got {:?}", other),
        }
    }

    #[test]
    fn large_diff_routes_to_review() {
        let summary = make_summary(3, 200, 100, vec!["team"], vec![], false);
        let policy = enabled_policy();
        let decision = should_auto_merge(&summary, &policy, true);
        match decision {
            AutoMergeDecision::ManualReview { reasons, .. } => {
                assert!(
                    reasons.iter().any(|r| r.contains("diff lines")),
                    "should mention diff lines: {:?}",
                    reasons
                );
            }
            other => panic!("expected ManualReview, got {:?}", other),
        }
    }

    #[test]
    fn sensitive_file_routes_to_review() {
        let summary = make_summary(2, 20, 10, vec!["team"], vec!["Cargo.toml"], false);
        let policy = enabled_policy();
        let decision = should_auto_merge(&summary, &policy, true);
        match decision {
            AutoMergeDecision::ManualReview { reasons, .. } => {
                assert!(
                    reasons.iter().any(|r| r.contains("sensitive")),
                    "should mention sensitive paths: {:?}",
                    reasons
                );
            }
            other => panic!("expected ManualReview, got {:?}", other),
        }
    }

    #[test]
    fn multi_module_reduces_confidence() {
        let summary = make_summary(
            4,
            40,
            10,
            vec!["team", "cli", "tmux", "agent"],
            vec![],
            false,
        );
        let policy = enabled_policy();
        let confidence = compute_merge_confidence(&summary, &policy);
        // 1.0 - 0.1*(4-3) - 0.2*(4-1) = 1.0 - 0.1 - 0.6 = 0.3
        assert!(
            confidence < policy.confidence_threshold,
            "confidence {} should be below threshold {}",
            confidence,
            policy.confidence_threshold
        );
    }

    #[test]
    fn confidence_floor_at_zero() {
        let summary = make_summary(
            20,
            2000,
            1000,
            vec!["team", "cli", "tmux", "agent", "config"],
            vec!["Cargo.toml", ".env"],
            true,
        );
        let policy = enabled_policy();
        let confidence = compute_merge_confidence(&summary, &policy);
        assert_eq!(confidence, 0.0, "confidence should be floored at 0.0");
    }

    #[test]
    fn disabled_policy_always_manual() {
        let summary = make_summary(1, 5, 2, vec!["team"], vec![], false);
        let policy = default_policy(); // enabled: false by default
        let decision = should_auto_merge(&summary, &policy, true);
        match decision {
            AutoMergeDecision::ManualReview { reasons, .. } => {
                assert!(
                    reasons.iter().any(|r| r.contains("disabled")),
                    "should mention disabled: {:?}",
                    reasons
                );
            }
            other => panic!("expected ManualReview, got {:?}", other),
        }
    }

    #[test]
    fn config_deserializes_with_defaults() {
        let yaml = "{}";
        let policy: AutoMergePolicy = serde_yaml::from_str(yaml).unwrap();
        assert!(!policy.enabled);
        assert_eq!(policy.max_diff_lines, 200);
        assert_eq!(policy.max_files_changed, 5);
        assert_eq!(policy.max_modules_touched, 2);
        assert_eq!(policy.confidence_threshold, 0.8);
        assert!(policy.require_tests_pass);
        assert!(policy.sensitive_paths.contains(&"Cargo.toml".to_string()));
    }

    #[test]
    fn unsafe_blocks_reduce_confidence() {
        let summary = make_summary(2, 30, 20, vec!["team"], vec![], true);
        let policy = enabled_policy();
        let confidence = compute_merge_confidence(&summary, &policy);
        // 1.0 - 0.4 = 0.6
        assert!(
            (confidence - 0.6).abs() < 0.001,
            "confidence should be 0.6, got {}",
            confidence
        );
    }

    #[test]
    fn tests_not_passed_routes_to_review() {
        let summary = make_summary(2, 30, 20, vec!["team"], vec![], false);
        let policy = enabled_policy();
        let decision = should_auto_merge(&summary, &policy, false);
        match decision {
            AutoMergeDecision::ManualReview { reasons, .. } => {
                assert!(
                    reasons.iter().any(|r| r.contains("tests did not pass")),
                    "should mention tests: {:?}",
                    reasons
                );
            }
            other => panic!("expected ManualReview, got {:?}", other),
        }
    }

    #[test]
    fn tests_not_required_allows_merge_without_passing() {
        let summary = make_summary(2, 30, 20, vec!["team"], vec![], false);
        let mut policy = enabled_policy();
        policy.require_tests_pass = false;
        let decision = should_auto_merge(&summary, &policy, false);
        match decision {
            AutoMergeDecision::AutoMerge { .. } => {}
            other => panic!(
                "expected AutoMerge when tests not required, got {:?}",
                other
            ),
        }
    }

    #[test]
    fn conflicts_reduce_confidence_and_route_to_review() {
        let mut summary = make_summary(2, 30, 20, vec!["team"], vec![], false);
        summary.has_conflicts = true;
        let policy = enabled_policy();
        let confidence = compute_merge_confidence(&summary, &policy);
        // 1.0 - 0.5 = 0.5
        assert!(
            (confidence - 0.5).abs() < 0.001,
            "confidence should be 0.5, got {}",
            confidence
        );
        let decision = should_auto_merge(&summary, &policy, true);
        match decision {
            AutoMergeDecision::ManualReview { reasons, .. } => {
                assert!(
                    reasons.iter().any(|r| r.contains("conflicts")),
                    "should mention conflicts: {:?}",
                    reasons
                );
            }
            other => panic!("expected ManualReview, got {:?}", other),
        }
    }

    #[test]
    fn override_persistence_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join(".batty")).unwrap();

        // No overrides file — returns empty
        assert!(load_overrides(root).is_empty());

        // Save override
        save_override(root, 42, true).unwrap();
        let overrides = load_overrides(root);
        assert_eq!(overrides.get(&42), Some(&true));

        // Save another override, first one persists
        save_override(root, 99, false).unwrap();
        let overrides = load_overrides(root);
        assert_eq!(overrides.get(&42), Some(&true));
        assert_eq!(overrides.get(&99), Some(&false));

        // Overwrite existing
        save_override(root, 42, false).unwrap();
        let overrides = load_overrides(root);
        assert_eq!(overrides.get(&42), Some(&false));
    }
}
