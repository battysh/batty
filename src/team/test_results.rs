use regex::Regex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestFailure {
    pub test_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TestResults {
    pub framework: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<u32>,
    pub passed: u32,
    pub failed: u32,
    pub ignored: u32,
    #[serde(default)]
    pub failures: Vec<TestFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl TestResults {
    pub fn failure_summary(&self) -> String {
        if self.failed == 0 || self.failures.is_empty() {
            return "tests failed".to_string();
        }

        let details = self
            .failures
            .iter()
            .take(3)
            .map(TestFailure::summary)
            .collect::<Vec<_>>()
            .join("; ");
        let more = self.failures.len().saturating_sub(3);
        if more > 0 {
            format!("{} tests failed: {}; +{} more", self.failed, details, more)
        } else {
            format!("{} tests failed: {}", self.failed, details)
        }
    }
}

impl TestFailure {
    fn summary(&self) -> String {
        let mut out = self.test_name.clone();
        if let Some(message) = self
            .message
            .as_deref()
            .filter(|message| !message.is_empty())
        {
            out.push_str(" (");
            out.push_str(message);
            if let Some(location) = self
                .location
                .as_deref()
                .filter(|location| !location.is_empty())
            {
                out.push_str(" at ");
                out.push_str(location);
            }
            out.push(')');
            return out;
        }
        if let Some(location) = self
            .location
            .as_deref()
            .filter(|location| !location.is_empty())
        {
            out.push_str(" (at ");
            out.push_str(location);
            out.push(')');
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestRunOutput {
    pub passed: bool,
    pub output: String,
    pub results: TestResults,
}

pub fn parse(command_text: &str, output: &str, passed: bool) -> TestResults {
    let command = command_text.to_ascii_lowercase();
    if command.contains("pytest") {
        parse_pytest(output, passed)
    } else if command.contains("jest") {
        parse_jest(output, passed)
    } else {
        parse_cargo_test(output, passed)
    }
}

pub fn parse_cargo_test(output: &str, passed: bool) -> TestResults {
    let summary_re = Regex::new(
        r"test result:\s+(?:ok|FAILED)\.\s+(\d+)\s+passed;\s+(\d+)\s+failed;\s+(\d+)\s+ignored;",
    )
    .expect("valid regex");
    let block_header_re = Regex::new(r"^----\s+(.+?)\s+stdout\s+----$").expect("valid regex");

    let mut passed_count = 0;
    let mut failed_count = if passed { 0 } else { 1 };
    let mut ignored_count = 0;
    let mut total = None;
    let mut summary = None;

    if let Some(captures) = summary_re.captures(output) {
        passed_count = captures[1].parse().unwrap_or(0);
        failed_count = captures[2].parse().unwrap_or(0);
        ignored_count = captures[3].parse().unwrap_or(0);
        total = Some(passed_count + failed_count + ignored_count);
        summary = captures.get(0).map(|match_| match_.as_str().to_string());
    }

    let mut failures = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_message: Option<String> = None;
    let mut current_location: Option<String> = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(captures) = block_header_re.captures(trimmed) {
            if let Some(name) = current_name.take() {
                failures.push(TestFailure {
                    test_name: name,
                    message: current_message.take(),
                    location: current_location.take(),
                });
            }
            current_name = Some(captures[1].trim().to_string());
            continue;
        }

        if current_name.is_none() {
            continue;
        }

        if let Some((location, message)) = parse_cargo_panic_line(trimmed) {
            current_location = Some(location);
            if current_message.is_none() {
                current_message = message;
            }
            continue;
        }

        if current_message.is_none()
            && !trimmed.is_empty()
            && !trimmed.starts_with("stack backtrace:")
            && !trimmed.starts_with("note:")
            && !trimmed.starts_with("failures:")
        {
            current_message = Some(normalize_message(trimmed));
        }
    }

    if let Some(name) = current_name.take() {
        failures.push(TestFailure {
            test_name: name,
            message: current_message.take(),
            location: current_location.take(),
        });
    }

    TestResults {
        framework: "cargo".to_string(),
        total,
        passed: passed_count,
        failed: failed_count.max(failures.len() as u32),
        ignored: ignored_count,
        failures,
        summary,
    }
}

fn parse_cargo_panic_line(line: &str) -> Option<(String, Option<String>)> {
    let panic_marker = "panicked at ";
    let panic_start = line.find(panic_marker)?;
    let rest = line[panic_start + panic_marker.len()..].trim();

    if let Some((message, location)) = rest.rsplit_once(", ") {
        let normalized_message = normalize_message(message.trim_matches('\''));
        if !normalized_message.is_empty() && looks_like_location(location) {
            return Some((location.trim().to_string(), Some(normalized_message)));
        }
    }

    let location = rest.strip_suffix(':').unwrap_or(rest).trim();
    if looks_like_location(location) {
        return Some((location.to_string(), None));
    }

    None
}

fn looks_like_location(value: &str) -> bool {
    let mut segments = value.rsplit(':');
    let Some(column) = segments.next() else {
        return false;
    };
    let Some(line) = segments.next() else {
        return false;
    };
    column.chars().all(|c| c.is_ascii_digit()) && line.chars().all(|c| c.is_ascii_digit())
}

pub fn parse_pytest(output: &str, passed: bool) -> TestResults {
    let summary_re =
        Regex::new(r"=+\s+((?:\d+\s+\w+(?:,\s*)?)+)\s+in\s+[0-9.]+s\s+=+").expect("valid regex");
    let failure_re = Regex::new(r"^_{2,}\s+(.+?)\s+_{2,}$").expect("valid regex");
    let location_re = Regex::new(r"^(.+?):(\d+):\s+(?:AssertionError|E\s+)").expect("valid regex");

    let mut results = TestResults {
        framework: "pytest".to_string(),
        total: None,
        passed: 0,
        failed: if passed { 0 } else { 1 },
        ignored: 0,
        failures: Vec::new(),
        summary: None,
    };

    if let Some(captures) = summary_re.captures(output) {
        let summary_text = captures[1].to_string();
        results.summary = Some(summary_text.clone());
        for chunk in summary_text.split(',') {
            let parts = chunk.split_whitespace().collect::<Vec<_>>();
            if parts.len() < 2 {
                continue;
            }
            let count = parts[0].parse::<u32>().unwrap_or(0);
            match parts[1] {
                "passed" => results.passed = count,
                "failed" => results.failed = count,
                "skipped" | "xfailed" | "xpassed" => results.ignored += count,
                _ => {}
            }
        }
        results.total = Some(results.passed + results.failed + results.ignored);
    }

    let mut current: Option<TestFailure> = None;
    for line in output.lines() {
        let trimmed = line.trim_end();
        if let Some(captures) = failure_re.captures(trimmed) {
            if let Some(failure) = current.take() {
                results.failures.push(failure);
            }
            current = Some(TestFailure {
                test_name: captures[1].trim().to_string(),
                message: None,
                location: None,
            });
            continue;
        }

        let Some(failure) = current.as_mut() else {
            continue;
        };

        if failure.location.is_none()
            && let Some(captures) = location_re.captures(trimmed)
        {
            failure.location = Some(format!("{}:{}", captures[1].trim(), &captures[2]));
            continue;
        }

        if failure.message.is_none() && trimmed.starts_with("E ") {
            failure.message = Some(normalize_message(trimmed.trim_start_matches("E ")));
        }
    }

    if let Some(failure) = current.take() {
        results.failures.push(failure);
    }

    results.failed = results.failed.max(results.failures.len() as u32);
    results
}

pub fn parse_jest(output: &str, passed: bool) -> TestResults {
    let summary_re = Regex::new(
        r"Tests:\s+(\d+)\s+failed(?:,\s+(\d+)\s+skipped)?(?:,\s+(\d+)\s+passed)?(?:,\s+(\d+)\s+total)?",
    )
    .expect("valid regex");
    let fail_line_re = Regex::new(r"^\s*[xX]\s+(.+?)\s+\((\d+)\s*ms\)\s*$").expect("valid regex");
    let suite_re = Regex::new(r"^FAIL\s+(.+)$").expect("valid regex");

    let mut results = TestResults {
        framework: "jest".to_string(),
        total: None,
        passed: 0,
        failed: if passed { 0 } else { 1 },
        ignored: 0,
        failures: Vec::new(),
        summary: None,
    };

    if let Some(captures) = summary_re.captures(output) {
        results.failed = captures[1].parse().unwrap_or(results.failed);
        results.ignored = captures
            .get(2)
            .and_then(|match_| match_.as_str().parse().ok())
            .unwrap_or(0);
        results.passed = captures
            .get(3)
            .and_then(|match_| match_.as_str().parse().ok())
            .unwrap_or(0);
        results.total = captures
            .get(4)
            .and_then(|match_| match_.as_str().parse().ok())
            .or(Some(results.passed + results.failed + results.ignored));
        results.summary = captures.get(0).map(|match_| match_.as_str().to_string());
    }

    let mut current_suite: Option<String> = None;
    for line in output.lines() {
        let trimmed = line.trim_end();
        if let Some(captures) = suite_re.captures(trimmed) {
            current_suite = Some(captures[1].trim().to_string());
            continue;
        }
        if let Some(captures) = fail_line_re.captures(trimmed) {
            let test_name = match current_suite.as_deref() {
                Some(suite) => format!("{suite} :: {}", captures[1].trim()),
                None => captures[1].trim().to_string(),
            };
            results.failures.push(TestFailure {
                test_name,
                message: None,
                location: None,
            });
        }
    }

    results.failed = results.failed.max(results.failures.len() as u32);
    results
}

fn normalize_message(message: &str) -> String {
    message.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_failures_with_message_and_location() {
        let output = r#"
running 2 tests
test tests::passes ... ok
test tests::fails ... FAILED

failures:

---- tests::fails stdout ----
thread 'tests::fails' panicked at src/lib.rs:12:9:
assertion `left == right` failed
  left: 4
 right: 5

failures:
    tests::fails

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
"#;

        let results = parse_cargo_test(output, false);
        assert_eq!(results.framework, "cargo");
        assert_eq!(results.total, Some(2));
        assert_eq!(results.passed, 1);
        assert_eq!(results.failed, 1);
        assert_eq!(results.failures.len(), 1);
        assert_eq!(results.failures[0].test_name, "tests::fails");
        assert_eq!(
            results.failures[0].message.as_deref(),
            Some("assertion `left == right` failed")
        );
        assert_eq!(
            results.failures[0].location.as_deref(),
            Some("src/lib.rs:12:9")
        );
    }

    #[test]
    fn parses_pytest_summary_and_failure() {
        let output = r#"
__________________________ test_dispatch_guard ___________________________

tmp/test_dispatch.py:14: AssertionError
E   assert 2 == 3

=========================== short test summary info ============================
FAILED tmp/test_dispatch.py::test_dispatch_guard - assert 2 == 3
========================= 1 failed, 2 passed in 0.12s =========================
"#;

        let results = parse_pytest(output, false);
        assert_eq!(results.framework, "pytest");
        assert_eq!(results.total, Some(3));
        assert_eq!(results.passed, 2);
        assert_eq!(results.failed, 1);
        assert_eq!(results.failures[0].test_name, "test_dispatch_guard");
        assert_eq!(
            results.failures[0].location.as_deref(),
            Some("tmp/test_dispatch.py:14")
        );
        assert_eq!(
            results.failures[0].message.as_deref(),
            Some("assert 2 == 3")
        );
    }

    #[test]
    fn parses_jest_summary_and_failures() {
        let output = r#"
FAIL src/app.test.ts
  feature
    x renders details (5 ms)

Tests:       1 failed, 2 passed, 3 total
"#;

        let results = parse_jest(output, false);
        assert_eq!(results.framework, "jest");
        assert_eq!(results.total, Some(3));
        assert_eq!(results.passed, 2);
        assert_eq!(results.failed, 1);
        assert_eq!(
            results.failures[0].test_name,
            "src/app.test.ts :: renders details"
        );
    }

    #[test]
    fn formats_failure_summary() {
        let results = TestResults {
            framework: "cargo".to_string(),
            total: Some(3),
            passed: 1,
            failed: 2,
            ignored: 0,
            failures: vec![
                TestFailure {
                    test_name: "a::fails".to_string(),
                    message: Some("expected 2, got 3".to_string()),
                    location: Some("src/a.rs:10".to_string()),
                },
                TestFailure {
                    test_name: "b::fails".to_string(),
                    message: None,
                    location: None,
                },
            ],
            summary: None,
        };

        assert_eq!(
            results.failure_summary(),
            "2 tests failed: a::fails (expected 2, got 3 at src/a.rs:10); b::fails"
        );
    }

    #[test]
    fn parses_legacy_cargo_panic_message_and_location() {
        let output = r#"
running 1 test
test tests::fails ... FAILED

failures:

---- tests::fails stdout ----
thread 'tests::fails' panicked at 'assertion `left == right` failed', src/lib.rs:12:9

failures:
    tests::fails

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
"#;

        let results = parse_cargo_test(output, false);
        assert_eq!(
            results.failures[0].message.as_deref(),
            Some("assertion `left == right` failed")
        );
        assert_eq!(
            results.failures[0].location.as_deref(),
            Some("src/lib.rs:12:9")
        );
    }
}
