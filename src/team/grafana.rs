//! Grafana monitoring: bundled dashboard template, alert provisioning, and CLI commands.
//!
//! The JSON is compiled into the binary via `include_str!()` so `batty init`
//! can write it without needing a network fetch or external file.

use anyhow::{Context, Result, bail};
use serde_json::json;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::time::Duration;

/// Raw JSON for the Grafana dashboard template.
pub const DASHBOARD_JSON: &str = include_str!("grafana/dashboard.json");

/// Default Grafana port.
pub const DEFAULT_PORT: u16 = 3000;
/// Local webhook receiver used by the file-backed contact point.
pub const ALERT_WEBHOOK_PORT: u16 = 8787;

/// Expected row titles in the dashboard (used for validation).
pub const REQUIRED_ROWS: &[&str] = &[
    "Session Overview",
    "Pipeline Health",
    "Activity Over Time",
    "Agent Performance",
    "Event Breakdown",
    "Recent Activity",
    "Throughput Over Time",
    "Tact Engine",
    "Health Signals",
    "Board Health",
];

/// Expected alert names provisioned alongside the dashboard.
pub const REQUIRED_ALERTS: &[&str] = &[
    "Zero Activity",
    "Crash Spike",
    "Dispatch Starvation",
    "Merge Queue Depth",
    "Non-Engineer Stall SLO",
];

const ALERT_RULE_GROUP: &str = "batty-operational-anomalies";
const ALERT_FOLDER: &str = "Batty";
const ALERT_CONTACT_POINT: &str = "batty-file-log";

#[derive(Clone, Copy)]
struct AlertRuleDefinition {
    uid: &'static str,
    title: &'static str,
    severity: &'static str,
    window_secs: u64,
    for_duration: &'static str,
    threshold: f64,
    sql: &'static str,
    description: &'static str,
}

const ALERT_RULES: &[AlertRuleDefinition] = &[
    AlertRuleDefinition {
        uid: "batty_zero_activity",
        title: "Zero Activity",
        severity: "warning",
        window_secs: 30 * 60,
        for_duration: "5m",
        threshold: 0.5,
        sql: "SELECT CASE WHEN COUNT(*) = 0 THEN 1 ELSE 0 END AS value \
              FROM events \
              WHERE event_type IN ('task_assigned', 'task_completed') \
                AND timestamp BETWEEN $__from / 1000 AND $__to / 1000;",
        description: "No task assignment or completion activity in the last 30 minutes.",
    },
    AlertRuleDefinition {
        uid: "batty_crash_spike",
        title: "Crash Spike",
        severity: "critical",
        window_secs: 10 * 60,
        for_duration: "2m",
        threshold: 3.0,
        sql: "SELECT COUNT(*) AS value \
              FROM events \
              WHERE event_type = 'member_crashed' \
                AND timestamp BETWEEN $__from / 1000 AND $__to / 1000;",
        description: "More than three agent crashes in the last 10 minutes.",
    },
    AlertRuleDefinition {
        uid: "batty_dispatch_starvation",
        title: "Dispatch Starvation",
        severity: "warning",
        window_secs: 10 * 60,
        for_duration: "2m",
        threshold: 0.5,
        sql: "SELECT CASE WHEN COUNT(*) > 0 THEN 1 ELSE 0 END AS value \
              FROM events \
              WHERE event_type = 'pipeline_starvation_detected' \
                AND timestamp BETWEEN $__from / 1000 AND $__to / 1000;",
        description: "Idle engineers outnumber runnable work and dispatch is starving the lane.",
    },
    AlertRuleDefinition {
        uid: "batty_merge_queue_depth",
        title: "Merge Queue Depth",
        severity: "warning",
        window_secs: 60 * 60,
        for_duration: "2m",
        threshold: 3.0,
        sql: "SELECT COUNT(*) AS value \
              FROM task_metrics tm \
              WHERE tm.completed_at IS NOT NULL \
                AND NOT EXISTS ( \
                    SELECT 1 \
                    FROM events e \
                    WHERE e.task_id = tm.task_id \
                      AND e.event_type IN ('task_auto_merged', 'task_manual_merged', 'task_reworked') \
                      AND e.timestamp >= tm.completed_at \
                );",
        description: "More than three completed tasks are still waiting for merge or rework.",
    },
    AlertRuleDefinition {
        uid: "batty_non_engineer_stall_slo",
        title: "Non-Engineer Stall SLO",
        severity: "warning",
        window_secs: 15 * 60,
        for_duration: "2m",
        threshold: 0.5,
        sql: "SELECT CASE WHEN COUNT(*) > 0 THEN 1 ELSE 0 END AS value \
              FROM non_engineer_stall_metrics \
              WHERE last_seen_at BETWEEN $__from / 1000 AND $__to / 1000;",
        description: "Architect, manager, or daemon SLO stall pressure was recorded recently.",
    },
];

/// Write the bundled Grafana dashboard JSON to a file.
pub fn write_dashboard(path: &Path) -> anyhow::Result<()> {
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

fn alert_webhook_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/grafana-alerts")
}

fn alerts_log_path(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("alerts.log")
}

fn orchestrator_alert_log_path(project_root: &Path) -> PathBuf {
    crate::team::orchestrator_log_path(project_root)
}

fn orchestrator_alert_ansi_log_path(project_root: &Path) -> PathBuf {
    crate::team::orchestrator_ansi_log_path(project_root)
}

fn project_alerting_dir(project_root: &Path) -> PathBuf {
    project_root.join(".batty").join("grafana").join("alerting")
}

fn grafana_provisioning_dir() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("BATTY_GRAFANA_PROVISIONING_DIR") {
        return Some(PathBuf::from(dir));
    }

    [
        "/opt/homebrew/etc/grafana/provisioning",
        "/usr/local/etc/grafana/provisioning",
        "/etc/grafana/provisioning",
    ]
    .iter()
    .map(PathBuf::from)
    .find(|path| path.exists())
}

fn provisioning_target_dir(project_root: &Path) -> PathBuf {
    grafana_provisioning_dir().unwrap_or_else(|| project_root.join(".batty").join("grafana"))
}

/// Install Grafana, provision alerting resources, and start the service.
pub fn setup(project_root: &Path, port: u16) -> Result<()> {
    println!("Installing Grafana via Homebrew...");
    run_cmd("brew", &["install", "grafana"])?;

    println!("Installing SQLite datasource plugin...");
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
        let _ = run_cmd(
            "grafana-cli",
            &["plugins", "install", "frser-sqlite-datasource"],
        );
    }

    println!("Provisioning alert rules and notification policy...");
    provision_alerting(project_root, ALERT_WEBHOOK_PORT)?;
    ensure_alert_webhook_running(project_root, ALERT_WEBHOOK_PORT)?;

    println!("Starting Grafana service...");
    run_cmd("brew", &["services", "start", "grafana"])?;

    println!("Waiting for Grafana to start...");
    for _ in 0..10 {
        std::thread::sleep(Duration::from_secs(1));
        if check_health(&format!("{}/api/health", grafana_url(port))).is_ok() {
            break;
        }
    }

    let _ = reload_alerting_provisioning(port);

    let db_path = project_root.join(".batty").join("telemetry.db");
    if db_path.exists() {
        provision_dashboard(project_root, port)?;
    } else {
        println!(
            "telemetry.db not found at {}. Alerting files were written, but dashboard import \
will complete after `batty start` creates the database.",
            db_path.display()
        );
    }

    println!("Grafana setup complete. Dashboard at {}", grafana_url(port));
    Ok(())
}

/// Provision a SQLite datasource and import the bundled dashboard for a project.
pub fn provision_dashboard(project_root: &Path, port: u16) -> Result<()> {
    let db_path = project_root.join(".batty").join("telemetry.db");
    if !db_path.exists() {
        bail!(
            "telemetry.db not found at {}. Run `batty start` first.",
            db_path.display()
        );
    }
    let base_url = grafana_url(port);

    let _ = ProcessCommand::new("sqlite3")
        .args([
            db_path.to_str().unwrap_or(""),
            "PRAGMA wal_checkpoint(TRUNCATE);",
        ])
        .status();

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
        Err(e) => bail!("Grafana is not reachable at {}: {e}", grafana_url(port)),
    }
}

/// Open the Grafana dashboard in the default browser.
pub fn open(port: u16) -> Result<()> {
    let url = grafana_url(port);
    open_browser(&url).context("failed to open browser")?;
    println!("Opened {url}");
    Ok(())
}

/// Run the local webhook receiver used by Grafana's webhook contact point.
pub fn run_alert_webhook(project_root: &Path, port: u16) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("failed to bind Grafana alert webhook on port {port}"))?;
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(error) = handle_alert_webhook(stream, project_root) {
                    eprintln!("grafana alert webhook error: {error}");
                }
            }
            Err(error) => eprintln!("grafana alert webhook accept failed: {error}"),
        }
    }
    Ok(())
}

pub fn render_alert_rules_yaml() -> Result<String> {
    let rules = ALERT_RULES
        .iter()
        .map(|rule| {
            json!({
                "uid": rule.uid,
                "title": rule.title,
                "condition": "B",
                "data": [
                    {
                        "refId": "A",
                        "datasourceUid": "batty-telemetry",
                        "relativeTimeRange": {
                            "from": rule.window_secs,
                            "to": 0
                        },
                        "model": {
                            "datasource": {
                                "type": "frser-sqlite-datasource",
                                "uid": "batty-telemetry"
                            },
                            "intervalMs": 1000,
                            "maxDataPoints": 43200,
                            "queryText": rule.sql,
                            "queryType": "table",
                            "rawQueryText": rule.sql,
                            "rawSql": rule.sql,
                            "refId": "A"
                        }
                    },
                    {
                        "refId": "B",
                        "datasourceUid": "__expr__",
                        "relativeTimeRange": {
                            "from": rule.window_secs,
                            "to": 0
                        },
                        "model": {
                            "conditions": [
                                {
                                    "evaluator": {
                                        "params": [rule.threshold],
                                        "type": "gt"
                                    },
                                    "operator": {
                                        "type": "and"
                                    },
                                    "query": {
                                        "params": ["A"]
                                    },
                                    "reducer": {
                                        "type": "last"
                                    },
                                    "type": "query"
                                }
                            ],
                            "datasource": {
                                "type": "__expr__",
                                "uid": "__expr__"
                            },
                            "expression": "A",
                            "intervalMs": 1000,
                            "maxDataPoints": 43200,
                            "refId": "B",
                            "type": "classic_conditions"
                        }
                    }
                ],
                "noDataState": "OK",
                "execErrState": "Alerting",
                "for": rule.for_duration,
                "annotations": {
                    "summary": rule.description
                },
                "labels": {
                    "severity": rule.severity,
                    "team": "batty"
                }
            })
        })
        .collect::<Vec<_>>();

    serde_yaml::to_string(&json!({
        "apiVersion": 1,
        "groups": [
            {
                "orgId": 1,
                "name": ALERT_RULE_GROUP,
                "folder": ALERT_FOLDER,
                "interval": "60s",
                "rules": rules
            }
        ]
    }))
    .context("failed to render Grafana alert rules YAML")
}

fn render_contact_points_yaml(port: u16) -> Result<String> {
    serde_yaml::to_string(&json!({
        "apiVersion": 1,
        "contactPoints": [
            {
                "orgId": 1,
                "name": ALERT_CONTACT_POINT,
                "receivers": [
                    {
                        "uid": "batty_file_log",
                        "type": "webhook",
                        "disableResolveMessage": false,
                        "settings": {
                            "url": alert_webhook_url(port),
                            "httpMethod": "POST",
                            "title": "{{ template \"default.title\" . }}",
                            "message": "{{ template \"default.message\" . }}"
                        }
                    }
                ]
            }
        ]
    }))
    .context("failed to render Grafana contact points YAML")
}

fn render_notification_policies_yaml() -> Result<String> {
    serde_yaml::to_string(&json!({
        "apiVersion": 1,
        "policies": [
            {
                "orgId": 1,
                "receiver": ALERT_CONTACT_POINT,
                "group_by": ["alertname", "severity"],
                "group_wait": "30s",
                "group_interval": "5m",
                "repeat_interval": "4h"
            }
        ]
    }))
    .context("failed to render Grafana notification policy YAML")
}

fn provision_alerting(project_root: &Path, port: u16) -> Result<()> {
    let project_dir = project_alerting_dir(project_root);
    std::fs::create_dir_all(&project_dir)?;
    if let Some(parent) = alerts_log_path(project_root).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = OpenOptions::new()
        .create(true)
        .append(true)
        .open(alerts_log_path(project_root))?;

    let rules_yaml = render_alert_rules_yaml()?;
    let contact_points_yaml = render_contact_points_yaml(port)?;
    let policies_yaml = render_notification_policies_yaml()?;

    write_alerting_file(&project_dir.join("rules.yaml"), &rules_yaml)?;
    write_alerting_file(
        &project_dir.join("contact-points.yaml"),
        &contact_points_yaml,
    )?;
    write_alerting_file(&project_dir.join("policies.yaml"), &policies_yaml)?;

    let target_dir = provisioning_target_dir(project_root).join("alerting");
    std::fs::create_dir_all(&target_dir)?;
    write_alerting_file(&target_dir.join("batty-rules.yaml"), &rules_yaml)?;
    write_alerting_file(
        &target_dir.join("batty-contact-points.yaml"),
        &contact_points_yaml,
    )?;
    write_alerting_file(&target_dir.join("batty-policies.yaml"), &policies_yaml)?;
    Ok(())
}

fn write_alerting_file(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, content)
        .with_context(|| format!("failed to write alerting file {}", path.display()))
}

fn ensure_alert_webhook_running(project_root: &Path, port: u16) -> Result<()> {
    if TcpStream::connect(("127.0.0.1", port)).is_ok() {
        return Ok(());
    }

    let current_exe = std::env::current_exe().context("failed to resolve current batty binary")?;
    ProcessCommand::new(current_exe)
        .args([
            "grafana-webhook",
            "--project-root",
            &project_root.display().to_string(),
            "--port",
            &port.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to start Grafana webhook receiver")?;
    std::thread::sleep(Duration::from_millis(250));
    Ok(())
}

fn handle_alert_webhook(mut stream: TcpStream, project_root: &Path) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let request = String::from_utf8_lossy(&buf);
    let body = request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default();
    let line = format_alert_notification(body);
    append_alert_logs(project_root, &line)?;
    if let Err(error) = maybe_send_telegram_alert(&line) {
        eprintln!("grafana alert telegram delivery failed: {error}");
    }
    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")?;
    Ok(())
}

fn append_alert_logs(project_root: &Path, line: &str) -> Result<()> {
    if let Some(parent) = alerts_log_path(project_root).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut alerts = OpenOptions::new()
        .create(true)
        .append(true)
        .open(alerts_log_path(project_root))
        .with_context(|| {
            format!(
                "failed to open alert log at {}",
                alerts_log_path(project_root).display()
            )
        })?;
    writeln!(alerts, "{line}")?;

    let orchestrator_line = format!("alert: {line}");
    let plain_path = orchestrator_alert_log_path(project_root);
    let ansi_path = orchestrator_alert_ansi_log_path(project_root);
    let mut plain = crate::team::open_log_for_append(&plain_path)?;
    writeln!(plain, "{orchestrator_line}")?;
    let mut ansi = crate::team::open_log_for_append(&ansi_path)?;
    writeln!(ansi, "{orchestrator_line}")?;
    Ok(())
}

fn maybe_send_telegram_alert(message: &str) -> Result<()> {
    let bot_token = match std::env::var("BATTY_TELEGRAM_BOT_TOKEN") {
        Ok(token) => token,
        Err(_) => return Ok(()),
    };
    let chat_id = match std::env::var("BATTY_GRAFANA_ALERT_CHAT_ID")
        .or_else(|_| std::env::var("BATTY_TELEGRAM_ALERT_CHAT_ID"))
    {
        Ok(chat_id) => chat_id,
        Err(_) => return Ok(()),
    };

    let bot = crate::team::telegram::TelegramBot::new(bot_token, Vec::new());
    bot.send_message(&chat_id, message)
        .context("failed to send Grafana alert to Telegram")
}

fn format_alert_notification(body: &str) -> String {
    let ts = crate::team::now_unix();
    if let Ok(payload) = serde_json::from_str::<serde_json::Value>(body) {
        let status = payload
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let alerts = payload
            .get("alerts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let names = alerts
            .iter()
            .filter_map(|alert| {
                alert
                    .get("labels")
                    .and_then(|labels| labels.get("alertname"))
                    .and_then(|name| name.as_str())
            })
            .collect::<Vec<_>>();
        if !names.is_empty() {
            return format!(
                "[{ts}] status={status} alerts={} raw={}",
                names.join(","),
                body.trim()
            );
        }
    }
    format!("[{ts}] raw={}", body.trim())
}

fn reload_alerting_provisioning(port: u16) -> Result<()> {
    let output = ProcessCommand::new("curl")
        .args([
            "-sf",
            "-X",
            "POST",
            &format!(
                "{}/api/admin/provisioning/alerting/reload",
                grafana_url(port)
            ),
            "-u",
            "admin:admin",
        ])
        .output()
        .context("failed to run curl for alerting reload")?;
    if !output.status.success() {
        bail!(
            "Grafana alerting reload failed with status {}",
            output.status
        );
    }
    Ok(())
}

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
    fn required_alerts_match_rule_definitions() {
        let titles: Vec<&str> = ALERT_RULES.iter().map(|rule| rule.title).collect();
        for expected in REQUIRED_ALERTS {
            assert!(titles.contains(expected), "missing alert rule {expected}");
        }
        assert_eq!(titles.len(), REQUIRED_ALERTS.len());
    }

    #[test]
    fn alert_rules_yaml_contains_expected_rules() {
        let yaml = render_alert_rules_yaml().unwrap();
        for expected in REQUIRED_ALERTS {
            assert!(yaml.contains(expected), "missing {expected} from YAML");
        }
        assert!(yaml.contains("pipeline_starvation_detected"));
        assert!(yaml.contains("task_auto_merged"));
        assert!(yaml.contains("task_manual_merged"));
        assert!(yaml.contains("task_reworked"));
    }

    #[test]
    fn alert_contact_points_yaml_uses_local_webhook() {
        let yaml = render_contact_points_yaml(ALERT_WEBHOOK_PORT).unwrap();
        assert!(yaml.contains("type: webhook"));
        assert!(yaml.contains("http://127.0.0.1:8787/grafana-alerts"));
        assert!(yaml.contains(ALERT_CONTACT_POINT));
    }

    #[test]
    fn notification_formatting_extracts_alert_names() {
        let body = r#"{"status":"firing","alerts":[{"labels":{"alertname":"Crash Spike"}},{"labels":{"alertname":"Zero Activity"}}]}"#;
        let formatted = format_alert_notification(body);
        assert!(formatted.contains("status=firing"));
        assert!(formatted.contains("Crash Spike,Zero Activity"));
    }

    #[test]
    fn provision_alerting_writes_expected_files() {
        let tmp = tempfile::tempdir().unwrap();
        provision_alerting(tmp.path(), 9911).unwrap();

        assert!(tmp.path().join(".batty/alerts.log").exists());
        assert!(
            tmp.path()
                .join(".batty/grafana/alerting/rules.yaml")
                .exists()
        );
        assert!(
            tmp.path()
                .join(".batty/grafana/alerting/contact-points.yaml")
                .exists()
        );
        assert!(
            tmp.path()
                .join(".batty/grafana/alerting/policies.yaml")
                .exists()
        );
    }

    #[test]
    fn append_alert_logs_writes_alert_and_orchestrator_logs() {
        let tmp = tempfile::tempdir().unwrap();

        append_alert_logs(tmp.path(), "[1] status=firing alerts=Crash Spike").unwrap();

        let alerts = std::fs::read_to_string(tmp.path().join(".batty/alerts.log")).unwrap();
        assert!(alerts.contains("Crash Spike"));

        let orchestrator =
            std::fs::read_to_string(tmp.path().join(".batty/orchestrator.log")).unwrap();
        assert!(orchestrator.contains("alert: [1] status=firing alerts=Crash Spike"));

        let orchestrator_ansi =
            std::fs::read_to_string(tmp.path().join(".batty/orchestrator.ansi.log")).unwrap();
        assert!(orchestrator_ansi.contains("alert: [1] status=firing alerts=Crash Spike"));
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
            "Discord Events Sent",
            "Merge Queue Depth",
            "Notification Delivery Latency",
            "Verification Pass Rate Over Time",
            "Agent Metrics",
            "Event Type Distribution",
            "Recent Completions",
            "Events Per Hour",
            "Average Cycle and Lead Time Per Hour",
            "Tasks Completed Per Hour",
            "Narration Rejection Rate Per Hour (%)",
            "Planning Cycles Triggered Per Hour",
            "Tasks Created Per Planning Cycle",
            "Planning Cycle Latency",
            "Planning Cycle Success vs Failure",
            "Narration Events Detected Per Agent",
            "Context Pressure Warnings Per Agent",
            "Board Task Count by Status Over Time",
            "Tasks Archived Per Hour",
            "Review Queue Depth",
            "Oldest Review Age",
            "Review Over SLO",
            "Review Disposition Latency",
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
        assert_eq!(REQUIRED_ALERTS.len(), ALERT_RULES.len());
    }

    #[test]
    fn dashboard_panel_count() {
        let parsed: serde_json::Value = serde_json::from_str(DASHBOARD_JSON).unwrap();
        let panels = parsed["panels"].as_array().unwrap();
        let non_row_panels: Vec<_> = panels
            .iter()
            .filter(|p| p["type"].as_str() != Some("row"))
            .collect();
        assert!(
            non_row_panels.len() >= 28,
            "expected at least 28 data panels, got {}",
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
        let result = check_health("http://localhost:1/api/health");
        assert!(result.is_err());
    }
}
