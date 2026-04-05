//! Grafana monitoring: bundled dashboard template and CLI commands.
//!
//! The JSON is compiled into the binary via `include_str!()` so `batty init`
//! can write it without needing a network fetch or external file.
//!
//! CLI commands:
//! - `batty grafana setup` — install Grafana + SQLite plugin, start the service
//! - `batty grafana status` — check if the Grafana server is reachable
//! - `batty grafana open` — open the dashboard in the default browser

use anyhow::{Context, Result, bail};
use std::process::Command as ProcessCommand;

/// Raw JSON for the Grafana dashboard template.
pub const DASHBOARD_JSON: &str = include_str!("grafana/dashboard.json");

/// Default Grafana port.
pub const DEFAULT_PORT: u16 = 3000;

/// Expected row titles in the dashboard (used for validation).
pub const REQUIRED_ROWS: &[&str] = &[
    "Session Overview",
    "Pipeline Health",
    "Activity Over Time",
    "Agent Performance",
    "Event Breakdown",
    "Recent Activity",
    "Throughput Over Time",
];

/// Expected alert names in the dashboard (currently none — alerts are planned).
pub const REQUIRED_ALERTS: &[&str] = &[];

/// Write the bundled Grafana dashboard JSON to a file.
pub fn write_dashboard(path: &std::path::Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, DASHBOARD_JSON)?;
    Ok(())
}

/// Build the Grafana base URL from a port number.
pub fn grafana_url(port: u16) -> String {
    format!("http://localhost:{port}")
}

/// Install Grafana and the SQLite datasource plugin via Homebrew, then start
/// the service.
///
/// Steps:
/// 1. `brew install grafana`
/// 2. `grafana-cli plugins install frser-sqlite-datasource`
/// 3. `brew services start grafana`
pub fn setup(port: u16) -> Result<()> {
    println!("Installing Grafana via Homebrew...");
    run_cmd("brew", &["install", "grafana"])?;

    println!("Installing SQLite datasource plugin...");
    // Try homebrew plugin path first, fall back to grafana cli
    let plugin_result = ProcessCommand::new("grafana")
        .args([
            "cli",
            "--homepath",
            "/opt/homebrew/opt/grafana/share/grafana",
            "--pluginsDir",
            "/opt/homebrew/var/lib/grafana/plugins",
            "plugins",
            "install",
            "frser-sqlite-datasource",
        ])
        .status();
    if plugin_result.is_err() || !plugin_result.unwrap().success() {
        // Fallback: try grafana-cli directly
        let _ = run_cmd(
            "grafana-cli",
            &["plugins", "install", "frser-sqlite-datasource"],
        );
    }

    println!("Starting Grafana service...");
    run_cmd("brew", &["services", "start", "grafana"])?;

    // Wait for Grafana to become ready
    println!("Waiting for Grafana to start...");
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if check_health(&format!("{}/api/health", grafana_url(port))).is_ok() {
            break;
        }
    }

    println!("Grafana setup complete. Dashboard at {}", grafana_url(port));
    Ok(())
}

/// Provision a SQLite datasource and import the bundled dashboard for a project.
/// Call this after `setup()` to connect Grafana to the project's telemetry.db.
pub fn provision_dashboard(project_root: &std::path::Path, port: u16) -> Result<()> {
    let db_path = project_root.join(".batty").join("telemetry.db");
    if !db_path.exists() {
        bail!(
            "telemetry.db not found at {}. Run `batty start` first.",
            db_path.display()
        );
    }
    let base_url = grafana_url(port);

    // Flush WAL so Grafana can read the data
    let _ = ProcessCommand::new("sqlite3")
        .args([
            db_path.to_str().unwrap_or(""),
            "PRAGMA wal_checkpoint(TRUNCATE);",
        ])
        .status();

    // Create datasource
    println!("Creating SQLite datasource...");
    let ds_body = format!(
        r#"{{"name":"Batty Telemetry","uid":"batty-telemetry","type":"frser-sqlite-datasource","access":"proxy","jsonData":{{"path":"{}"}}}}"#,
        db_path.display()
    );
    let ds_result = ProcessCommand::new("curl")
        .args([
            "-sf",
            "-X",
            "POST",
            &format!("{base_url}/api/datasources"),
            "-H",
            "Content-Type: application/json",
            "-u",
            "admin:admin",
            "-d",
            &ds_body,
        ])
        .output();
    match ds_result {
        Ok(out) if out.status.success() => println!("Datasource created."),
        _ => println!("Datasource may already exist (continuing)."),
    }

    // Import dashboard
    println!("Importing dashboard...");
    let dashboard_payload = format!(
        r#"{{"dashboard":{},"overwrite":true,"folderId":0}}"#,
        DASHBOARD_JSON
    );
    let tmp_file = std::env::temp_dir().join("batty-grafana-import.json");
    std::fs::write(&tmp_file, &dashboard_payload)?;
    let import_result = ProcessCommand::new("curl")
        .args([
            "-sf",
            "-X",
            "POST",
            &format!("{base_url}/api/dashboards/db"),
            "-H",
            "Content-Type: application/json",
            "-u",
            "admin:admin",
            "-d",
            &format!("@{}", tmp_file.display()),
        ])
        .output();
    let _ = std::fs::remove_file(&tmp_file);
    match import_result {
        Ok(out) if out.status.success() => {
            let body = String::from_utf8_lossy(&out.stdout);
            if body.contains("\"status\":\"success\"") {
                println!("Dashboard imported successfully.");
            } else {
                println!("Dashboard import response: {body}");
            }
        }
        _ => {
            println!("Dashboard import may have failed. Check Grafana UI.");
        }
    }

    let url = format!("{base_url}/d/batty-project");
    println!("Dashboard at: {url}");
    Ok(())
}

/// Check whether the Grafana server is reachable by hitting `/api/health`.
pub fn status(port: u16) -> Result<()> {
    let url = format!("{}/api/health", grafana_url(port));
    match check_health(&url) {
        Ok(body) => {
            println!("Grafana is running at {}", grafana_url(port));
            println!("{body}");
            Ok(())
        }
        Err(e) => {
            bail!("Grafana is not reachable at {}: {e}", grafana_url(port));
        }
    }
}

/// Open the Grafana dashboard in the default browser.
pub fn open(port: u16) -> Result<()> {
    let url = grafana_url(port);
    open_browser(&url).context("failed to open browser")?;
    println!("Opened {url}");
    Ok(())
}

// --- internal helpers -------------------------------------------------------

fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let status = ProcessCommand::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {program} — is it installed?"))?;
    if !status.success() {
        bail!("{program} exited with status {status}");
    }
    Ok(())
}

fn check_health(url: &str) -> Result<String> {
    let output = ProcessCommand::new("curl")
        .args(["-sf", url])
        .output()
        .context("failed to run curl")?;
    if !output.status.success() {
        bail!("HTTP request failed (status {})", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(target_os = "linux")]
    let program = "xdg-open";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let program = "open";

    run_cmd(program, &[url])
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
        if REQUIRED_ALERTS.is_empty() {
            return; // no alerts expected yet
        }
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
            "Tasks Completed",
            "Auto-Merged",
            "Messages Routed",
            "Agent Metrics",
            "Event Type Distribution",
            "Recent Completions",
            "Events Per Hour",
            "Delivery Success Rate Per Hour (%)",
            "Narration Rejection Rate Per Hour (%)",
        ];
        for expected in required {
            assert!(
                titles.contains(&expected),
                "missing panel: {expected}. Found: {titles:?}"
            );
        }
    }

    #[test]
    fn dashboard_uses_consistent_datasource_uid() {
        // All datasource uids should reference "batty-telemetry"
        let parsed: serde_json::Value = serde_json::from_str(DASHBOARD_JSON).unwrap();
        let panels = parsed["panels"].as_array().unwrap();
        for panel in panels {
            if let Some(uid) = panel["datasource"]["uid"].as_str() {
                assert_eq!(
                    uid,
                    "batty-telemetry",
                    "panel '{}' has unexpected datasource uid: {uid}",
                    panel["title"].as_str().unwrap_or("?")
                );
            }
        }
    }

    #[test]
    fn dashboard_alert_count_matches_expected() {
        // Currently no alerts defined; update when alerts are added.
        assert_eq!(REQUIRED_ALERTS.len(), 0);
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
            non_row_panels.len() >= 16,
            "expected at least 16 data panels, got {}",
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
        assert!(
            parsed["title"].as_str().is_some(),
            "dashboard must have a title"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- CLI command tests ---

    #[test]
    fn grafana_url_default_port() {
        assert_eq!(grafana_url(3000), "http://localhost:3000");
    }

    #[test]
    fn grafana_url_custom_port() {
        assert_eq!(grafana_url(9090), "http://localhost:9090");
    }

    #[test]
    fn default_port_is_3000() {
        assert_eq!(DEFAULT_PORT, 3000);
    }

    #[test]
    fn check_health_unreachable() {
        // Port 1 is almost certainly not running Grafana
        let result = check_health("http://localhost:1/api/health");
        assert!(result.is_err());
    }
}
