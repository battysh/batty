use std::collections::{BTreeMap, HashMap};

use super::events::TeamEvent;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct QualityMetrics {
    pub narration_ratio: f64,
    pub commit_frequency: f64,
    pub first_pass_test_rate: f64,
    pub retry_rate: f64,
    pub time_to_completion_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentMetric {
    pub backend: String,
    pub quality: QualityMetrics,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompletionQualityMetrics {
    pub backend: String,
    pub role: String,
    pub task_id: String,
    pub narration_ratio: f64,
    pub commit_frequency: f64,
    pub first_pass_test_rate: f64,
    pub retry_rate: f64,
    pub time_to_completion_secs: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackendQualityStats {
    pub backend: String,
    pub samples: u32,
    pub narration_ratio: f64,
    pub commit_frequency: f64,
    pub first_pass_test_rate: f64,
    pub retry_rate: f64,
    pub time_to_completion_secs: f64,
}

pub const BACKEND_COMPARISON_SQL: &str = "SELECT \
  json_extract(payload, '$.backend') AS backend, \
  ROUND(AVG(json_extract(payload, '$.narration_ratio')), 4) AS narration_ratio, \
  ROUND(AVG(json_extract(payload, '$.commit_frequency')), 4) AS commit_frequency, \
  ROUND(AVG(json_extract(payload, '$.first_pass_test_rate')), 4) AS first_pass_test_rate, \
  ROUND(AVG(json_extract(payload, '$.retry_rate')), 4) AS retry_rate, \
  ROUND(AVG(json_extract(payload, '$.time_to_completion_secs')), 2) AS time_to_completion_secs, \
  COUNT(*) AS samples \
FROM events \
WHERE event_type='quality_metrics_recorded' \
GROUP BY backend \
ORDER BY backend;";

pub const QUALITY_TRENDS_SQL: &str = "WITH RECURSIVE hours(h) AS ( \
  SELECT COALESCE(MIN(timestamp) / 3600, strftime('%s', 'now') / 3600) FROM events WHERE event_type='quality_metrics_recorded' \
  UNION ALL \
  SELECT h + 1 FROM hours WHERE h < (SELECT COALESCE(MAX(timestamp) / 3600, strftime('%s', 'now') / 3600) FROM events WHERE event_type='quality_metrics_recorded') \
) \
SELECT \
  h * 3600 AS time, \
  json_extract(e.payload, '$.backend') AS backend, \
  ROUND(AVG(json_extract(e.payload, '$.first_pass_test_rate')), 4) AS first_pass_test_rate, \
  ROUND(AVG(json_extract(e.payload, '$.retry_rate')), 4) AS retry_rate \
FROM hours \
LEFT JOIN events e \
  ON e.event_type='quality_metrics_recorded' \
 AND e.timestamp / 3600 = hours.h \
  GROUP BY time, backend \
ORDER BY time, backend;";

pub fn calculate_narration_ratio(output: &str) -> f64 {
    narration_ratio(output)
}

pub fn narration_ratio(output: &str) -> f64 {
    let mut explanation_lines = 0_u32;
    let mut code_or_tool_lines = 0_u32;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if is_code_or_tool_line(trimmed) {
            code_or_tool_lines += 1;
        } else {
            explanation_lines += 1;
        }
    }

    ratio(explanation_lines, code_or_tool_lines)
}

pub fn ratio(explanation_lines: u32, code_or_tool_lines: u32) -> f64 {
    let total = explanation_lines + code_or_tool_lines;
    if total == 0 {
        return 0.0;
    }
    explanation_lines as f64 / total as f64
}

pub fn commit_frequency(commits: u32, time_to_completion_secs: u64) -> f64 {
    if commits == 0 || time_to_completion_secs == 0 {
        return 0.0;
    }
    commits as f64 / (time_to_completion_secs as f64 / 3600.0)
}

pub fn build_completion_quality_metrics(
    backend: impl Into<String>,
    role: impl Into<String>,
    task_id: u32,
    output: &str,
    commits: u32,
    retries_before_success: u32,
    started_at: Option<u64>,
    completed_at: u64,
) -> CompletionQualityMetrics {
    let time_to_completion_secs = started_at
        .map(|started| completed_at.saturating_sub(started))
        .unwrap_or(0);
    CompletionQualityMetrics {
        backend: backend.into(),
        role: role.into(),
        task_id: task_id.to_string(),
        narration_ratio: narration_ratio(output),
        commit_frequency: commit_frequency(commits, time_to_completion_secs),
        first_pass_test_rate: if retries_before_success == 0 {
            1.0
        } else {
            0.0
        },
        retry_rate: retries_before_success as f64,
        time_to_completion_secs,
    }
}

pub fn aggregate_by_backend(metrics: &[AgentMetric]) -> HashMap<String, QualityMetrics> {
    let mut grouped: HashMap<String, Vec<&AgentMetric>> = HashMap::new();
    for metric in metrics {
        grouped.entry(metric.backend.clone()).or_default().push(metric);
    }

    grouped
        .into_iter()
        .map(|(backend, samples)| {
            let count = samples.len() as f64;
            let avg = |f: fn(&AgentMetric) -> f64| -> f64 {
                if samples.is_empty() {
                    return 0.0;
                }
                samples.iter().map(|sample| f(sample)).sum::<f64>() / count
            };
            let avg_u64 = |f: fn(&AgentMetric) -> u64| -> u64 {
                if samples.is_empty() {
                    return 0;
                }
                (samples.iter().map(|sample| f(sample) as f64).sum::<f64>() / count).round() as u64
            };

            (
                backend,
                QualityMetrics {
                    narration_ratio: avg(|sample| sample.quality.narration_ratio),
                    commit_frequency: avg(|sample| sample.quality.commit_frequency),
                    first_pass_test_rate: avg(|sample| sample.quality.first_pass_test_rate),
                    retry_rate: avg(|sample| sample.quality.retry_rate),
                    time_to_completion_secs: avg_u64(|sample| sample.quality.time_to_completion_secs),
                },
            )
        })
        .collect()
}

pub fn aggregate_completion_metrics_by_backend(
    metrics: &[CompletionQualityMetrics],
) -> BTreeMap<String, BackendQualityStats> {
    let mut grouped: BTreeMap<String, Vec<&CompletionQualityMetrics>> = BTreeMap::new();
    for metric in metrics {
        grouped
            .entry(metric.backend.clone())
            .or_default()
            .push(metric);
    }

    grouped
        .into_iter()
        .map(|(backend, samples)| {
            let count = samples.len() as u32;
            let avg = |f: fn(&CompletionQualityMetrics) -> f64| -> f64 {
                if samples.is_empty() {
                    return 0.0;
                }
                samples.iter().map(|sample| f(sample)).sum::<f64>() / samples.len() as f64
            };

            (
                backend.clone(),
                BackendQualityStats {
                    backend,
                    samples: count,
                    narration_ratio: avg(|sample| sample.narration_ratio),
                    commit_frequency: avg(|sample| sample.commit_frequency),
                    first_pass_test_rate: avg(|sample| sample.first_pass_test_rate),
                    retry_rate: avg(|sample| sample.retry_rate),
                    time_to_completion_secs: avg(|sample| sample.time_to_completion_secs as f64),
                },
            )
        })
        .collect()
}

pub fn assignment_started_at(events: &[TeamEvent], role: &str, task_id: u32) -> Option<u64> {
    let task_id = task_id.to_string();
    events
        .iter()
        .filter(|event| event.event == "task_assigned")
        .filter(|event| event.role.as_deref() == Some(role))
        .filter(|event| event.task.as_deref() == Some(task_id.as_str()))
        .map(|event| event.ts)
        .min()
}

fn is_code_or_tool_line(line: &str) -> bool {
    line.starts_with("```")
        || line.starts_with("$ ")
        || line.starts_with("> ")
        || line.starts_with("Command:")
        || line.starts_with("Output:")
        || line.starts_with("diff --git")
        || line.starts_with("+++ ")
        || line.starts_with("--- ")
        || line.starts_with("@@")
        || line.starts_with('{')
        || line.starts_with('[')
        || line.starts_with("fn ")
        || line.starts_with("let ")
        || line.starts_with("use ")
        || line.starts_with("pub ")
        || line.contains("::")
        || line.contains('{')
        || line.contains('}')
        || (line.contains('(') && line.contains(')') && line.ends_with(';'))
        || line.contains("();")
        || line.starts_with("test ")
        || line.starts_with("running ")
        || line.starts_with("error[")
        || line.starts_with("warning:")
        || line.starts_with("Compiling ")
        || line.starts_with("Finished ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narration_ratio_is_zero_for_all_code_lines() {
        let output = "```rust\nfn main() {\n    println!(\"hi\");\n}\n```\n$ cargo test";
        assert_eq!(calculate_narration_ratio(output), 0.0);
    }

    #[test]
    fn narration_ratio_is_one_for_all_text_lines() {
        let output =
            "I inspected the failure.\nNext I will patch the parser.\nThen I will rerun tests.";
        assert_eq!(calculate_narration_ratio(output), 1.0);
    }

    #[test]
    fn narration_ratio_handles_mixed_text_and_code() {
        let output = "I found the issue.\nfn main() {}\nI patched the bug.\n$ cargo test";
        assert_eq!(calculate_narration_ratio(output), 0.5);
    }

    #[test]
    fn first_pass_rate_aggregation_handles_multiple_completions() {
        let stats = aggregate_by_backend(&[
            AgentMetric {
                backend: "codex".into(),
                quality: QualityMetrics {
                    first_pass_test_rate: 1.0,
                    retry_rate: 0.0,
                    ..QualityMetrics::default()
                },
            },
            AgentMetric {
                backend: "codex".into(),
                quality: QualityMetrics {
                    first_pass_test_rate: 0.0,
                    retry_rate: 2.0,
                    ..QualityMetrics::default()
                },
            },
            AgentMetric {
                backend: "codex".into(),
                quality: QualityMetrics {
                    first_pass_test_rate: 1.0,
                    retry_rate: 0.0,
                    ..QualityMetrics::default()
                },
            },
        ]);

        let codex = stats.get("codex").unwrap();
        assert!((codex.first_pass_test_rate - (2.0 / 3.0)).abs() < 0.0001);
        assert!((codex.retry_rate - (2.0 / 3.0)).abs() < 0.0001);
    }

    #[test]
    fn backend_grouping_produces_correct_per_backend_stats() {
        let stats = aggregate_by_backend(&[
            AgentMetric {
                backend: "claude".into(),
                quality: QualityMetrics {
                    narration_ratio: 0.5,
                    commit_frequency: 1.0,
                    first_pass_test_rate: 1.0,
                    retry_rate: 0.0,
                    time_to_completion_secs: 3600,
                },
            },
            AgentMetric {
                backend: "codex".into(),
                quality: QualityMetrics {
                    narration_ratio: 0.0,
                    commit_frequency: 4.0,
                    first_pass_test_rate: 0.0,
                    retry_rate: 1.0,
                    time_to_completion_secs: 1800,
                },
            },
            AgentMetric {
                backend: "codex".into(),
                quality: QualityMetrics {
                    narration_ratio: 0.5,
                    commit_frequency: 3.0,
                    first_pass_test_rate: 1.0,
                    retry_rate: 0.0,
                    time_to_completion_secs: 3600,
                },
            },
        ]);

        assert!((stats.get("claude").unwrap().first_pass_test_rate - 1.0).abs() < 0.0001);
        assert!((stats.get("codex").unwrap().first_pass_test_rate - 0.5).abs() < 0.0001);
        assert_eq!(stats.get("codex").unwrap().time_to_completion_secs, 2700);
    }

    #[test]
    fn empty_data_is_handled_gracefully() {
        assert!(aggregate_by_backend(&[]).is_empty());
        assert_eq!(assignment_started_at(&[], "eng-1", 42), None);
        assert_eq!(calculate_narration_ratio(""), 0.0);
        assert_eq!(commit_frequency(3, 0), 0.0);
    }
}
