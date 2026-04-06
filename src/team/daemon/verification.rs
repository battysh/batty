use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;

use crate::team::task_loop::run_tests_in_worktree;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VerificationRunResult {
    pub passed: bool,
    pub output: String,
    pub failures: Vec<String>,
    pub file_paths: Vec<String>,
}

pub(crate) fn run_automatic_verification(
    worktree_dir: &Path,
    test_command: Option<&str>,
) -> Result<VerificationRunResult> {
    let test_run = run_tests_in_worktree(worktree_dir, test_command)?;
    let (failures, file_paths) = parse_test_output(&test_run.output);
    Ok(VerificationRunResult {
        passed: test_run.passed,
        output: test_run.output,
        failures,
        file_paths,
    })
}

fn parse_test_output(output: &str) -> (Vec<String>, Vec<String>) {
    let mut failures = Vec::new();
    let mut file_paths = BTreeSet::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if trimmed.starts_with("test ") && trimmed.ends_with("FAILED") {
            failures.push(trimmed.to_string());
        } else if trimmed.starts_with("error:") || trimmed.contains("panicked at") {
            failures.push(trimmed.to_string());
        }

        for token in trimmed.split_whitespace() {
            let cleaned = token.trim_matches(|ch: char| {
                matches!(ch, '"' | '\'' | ',' | ':' | ';' | '(' | ')' | '[' | ']')
            });
            let normalized = normalize_path_token(cleaned);
            if looks_like_path(normalized) {
                file_paths.insert(normalized.to_string());
            }
        }
    }

    if failures.is_empty() && !output.trim().is_empty() {
        failures.push("test command failed without a parsed failure line".to_string());
    }

    (failures, file_paths.into_iter().collect())
}

fn normalize_path_token(token: &str) -> &str {
    let mut candidate = token;
    while let Some((head, tail)) = candidate.rsplit_once(':') {
        if tail.chars().all(|ch| ch.is_ascii_digit()) {
            candidate = head;
        } else {
            break;
        }
    }
    candidate
}

fn looks_like_path(token: &str) -> bool {
    let has_separator = token.contains('/') || token.contains('\\');
    let has_extension = token.rsplit_once('.').is_some_and(|(_, ext)| {
        !ext.is_empty() && ext.chars().all(|ch| ch.is_ascii_alphanumeric())
    });
    has_separator && has_extension
}

#[cfg(test)]
mod tests {
    use super::parse_test_output;

    #[test]
    fn parse_test_output_extracts_failures_and_paths() {
        let output = "\
test parser::it_works ... FAILED\n\
error: could not compile crate due to previous error\n\
src/parser.rs:12: failure here\n";
        let (failures, paths) = parse_test_output(output);
        assert!(failures.iter().any(|line| line.contains("FAILED")));
        assert!(failures.iter().any(|line| line.starts_with("error:")));
        assert!(paths.iter().any(|path| path == "src/parser.rs"));
    }
}
