//! Bundled Grafana dashboard template for per-project monitoring.
//!
//! The JSON is compiled into the binary via `include_str!()` so `batty init`
//! can write it without needing a network fetch or external file.

/// Raw JSON for the Grafana dashboard template.
pub const DASHBOARD_JSON: &str = include_str!("grafana/dashboard.json");

/// Expected row titles in the dashboard (used for validation).
pub const REQUIRED_ROWS: &[&str] = &[
    "Session Overview",
    "Pipeline Health",
    "Agent Performance",
    "Delivery & Communication",
    "Task Lifecycle",
    "Recent Activity",
];

/// Expected alert names in the dashboard.
pub const REQUIRED_ALERTS: &[&str] = &[
    "Agent Stall",
    "Delivery Failure Spike",
    "Pipeline Starvation",
    "High Failure Rate",
    "Context Exhaustion",
    "Session Idle",
];

/// Write the bundled Grafana dashboard JSON to a file.
pub fn write_dashboard(path: &std::path::Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, DASHBOARD_JSON)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_json_valid() {
        let parsed: serde_json::Value =
            serde_json::from_str(DASHBOARD_JSON).expect("dashboard.json must be valid JSON");
        assert!(parsed.is_object(), "root must be an object");
        assert!(parsed["panels"].is_array(), "panels must be an array");
        assert!(parsed["alerts"].is_array(), "alerts must be an array");
    }

    #[test]
    fn dashboard_has_all_rows() {
        let parsed: serde_json::Value = serde_json::from_str(DASHBOARD_JSON).unwrap();
        let panels = parsed["panels"].as_array().unwrap();
        let row_titles: Vec<&str> = panels
            .iter()
            .filter(|p| p["type"].as_str() == Some("row"))
            .filter_map(|p| p["title"].as_str())
            .collect();

        for expected in REQUIRED_ROWS {
            assert!(
                row_titles.contains(expected),
                "missing row: {expected}. Found: {row_titles:?}"
            );
        }
    }

    #[test]
    fn dashboard_has_all_alerts() {
        let parsed: serde_json::Value = serde_json::from_str(DASHBOARD_JSON).unwrap();
        let alerts = parsed["alerts"].as_array().unwrap();
        let alert_names: Vec<&str> = alerts.iter().filter_map(|a| a["name"].as_str()).collect();

        for expected in REQUIRED_ALERTS {
            assert!(
                alert_names.contains(expected),
                "missing alert: {expected}. Found: {alert_names:?}"
            );
        }
    }

    #[test]
    fn dashboard_has_required_panels() {
        let parsed: serde_json::Value = serde_json::from_str(DASHBOARD_JSON).unwrap();
        let panels = parsed["panels"].as_array().unwrap();
        let titles: Vec<&str> = panels.iter().filter_map(|p| p["title"].as_str()).collect();

        let required = [
            "Total Events",
            "Uptime (hrs)",
            "Tasks Completed",
            "In Progress",
            "Engineers Active",
            "Throughput (tasks/day)",
            "Delivery Failures",
            "Delivery Success Rate",
            "Cycle Time by Engineer",
            "Burndown",
            "Last 50 Events",
            "Recent Completions",
            "Escalations",
            "Top Message Routes",
            "Event Type Breakdown",
        ];
        for expected in required {
            assert!(
                titles.contains(&expected),
                "missing panel: {expected}. Found: {titles:?}"
            );
        }
    }

    #[test]
    fn dashboard_uses_datasource_variable() {
        // All datasource uids should reference the template variable, not hardcoded uids
        let parsed: serde_json::Value = serde_json::from_str(DASHBOARD_JSON).unwrap();
        let panels = parsed["panels"].as_array().unwrap();
        for panel in panels {
            if let Some(uid) = panel["datasource"]["uid"].as_str() {
                assert_eq!(
                    uid,
                    "${datasource}",
                    "panel '{}' has hardcoded datasource uid: {uid}",
                    panel["title"].as_str().unwrap_or("?")
                );
            }
        }
    }

    #[test]
    fn dashboard_has_6_alerts() {
        let parsed: serde_json::Value = serde_json::from_str(DASHBOARD_JSON).unwrap();
        let alerts = parsed["alerts"].as_array().unwrap();
        assert_eq!(alerts.len(), 6, "expected 6 alert rules");
    }

    #[test]
    fn dashboard_panel_count() {
        let parsed: serde_json::Value = serde_json::from_str(DASHBOARD_JSON).unwrap();
        let panels = parsed["panels"].as_array().unwrap();
        // 6 rows + data panels
        let non_row_panels: Vec<_> = panels
            .iter()
            .filter(|p| p["type"].as_str() != Some("row"))
            .collect();
        assert!(
            non_row_panels.len() >= 15,
            "expected at least 15 data panels, got {}",
            non_row_panels.len()
        );
    }

    #[test]
    fn write_dashboard_creates_file() {
        let dir = std::env::temp_dir().join("batty_grafana_test");
        let path = dir.join("dashboard.json");
        let _ = std::fs::remove_dir_all(&dir);

        write_dashboard(&path).expect("write_dashboard should succeed");
        assert!(path.exists());

        let content = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(parsed["title"].as_str(), Some("Batty — Project Dashboard"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
