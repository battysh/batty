use std::path::Path;

use anyhow::{Context, Result};

pub fn load_project_env(project_root: &Path) -> Result<()> {
    load_env_file(&project_root.join(".env"))
}

pub fn upsert_env_var(path: &Path, key: &str, value: &str) -> Result<()> {
    let mut lines = if path.exists() {
        std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?
            .lines()
            .map(ToOwned::to_owned)
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };

    let rendered = format!("{key}={}", render_env_value(value));
    let mut replaced = false;
    for line in &mut lines {
        if env_key(line).is_some_and(|existing| existing == key) {
            *line = rendered.clone();
            replaced = true;
            break;
        }
    }

    if !replaced {
        if !lines.is_empty() && !lines.last().is_some_and(String::is_empty) {
            lines.push(String::new());
        }
        lines.push(rendered);
    }

    let output = if lines.is_empty() {
        String::new()
    } else {
        format!("{}\n", lines.join("\n"))
    };
    std::fs::write(path, output).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn load_env_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    for line in content.lines() {
        let Some((key, value)) = parse_env_assignment(line) else {
            continue;
        };
        if std::env::var_os(&key).is_none() {
            // SAFETY: batty mutates process env during single-threaded CLI startup,
            // before it spawns workers or background threads.
            unsafe {
                std::env::set_var(key, value);
            }
        }
    }

    Ok(())
}

fn env_key(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
    let (key, _) = trimmed.split_once('=')?;
    let key = key.trim();
    if key.is_empty() { None } else { Some(key) }
}

fn parse_env_assignment(line: &str) -> Option<(String, String)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let trimmed = trimmed.strip_prefix("export ").unwrap_or(trimmed);
    let (key, value) = trimmed.split_once('=')?;
    let key = key.trim();
    if key.is_empty() {
        return None;
    }

    let value = value.trim();
    let value = match (
        value.strip_prefix('"').and_then(|v| v.strip_suffix('"')),
        value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')),
    ) {
        (Some(unquoted), _) => unquoted.to_string(),
        (_, Some(unquoted)) => unquoted.to_string(),
        _ => value.to_string(),
    };

    Some((key.to_string(), value.to_string()))
}

fn render_env_value(value: &str) -> String {
    if value.contains(char::is_whitespace) || value.contains('#') {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvVarGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvVarGuard {
        fn unset(key: &'static str) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: tests mutate process env in a bounded scope and restore it on drop.
            unsafe {
                std::env::remove_var(key);
            }
            Self { key, original }
        }

        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            // SAFETY: tests mutate process env in a bounded scope and restore it on drop.
            unsafe {
                std::env::set_var(key, value);
            }
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(value) => {
                    // SAFETY: tests restore the original value before exiting.
                    unsafe {
                        std::env::set_var(self.key, value);
                    }
                }
                None => {
                    // SAFETY: tests restore the original absence before exiting.
                    unsafe {
                        std::env::remove_var(self.key);
                    }
                }
            }
        }
    }

    #[test]
    fn load_env_file_sets_missing_vars() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".env");
        std::fs::write(
            &path,
            "BATTY_TEST_FIRST=alpha\nexport BATTY_TEST_SECOND=\"beta value\"\n# comment\nBATTY_TEST_THIRD='gamma'\n",
        )
        .unwrap();

        let _first = EnvVarGuard::unset("BATTY_TEST_FIRST");
        let _second = EnvVarGuard::unset("BATTY_TEST_SECOND");
        let _third = EnvVarGuard::unset("BATTY_TEST_THIRD");

        load_env_file(&path).unwrap();

        assert_eq!(std::env::var("BATTY_TEST_FIRST").unwrap(), "alpha");
        assert_eq!(std::env::var("BATTY_TEST_SECOND").unwrap(), "beta value");
        assert_eq!(std::env::var("BATTY_TEST_THIRD").unwrap(), "gamma");
    }

    #[test]
    fn load_env_file_does_not_override_existing_vars() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".env");
        std::fs::write(&path, "BATTY_TEST_EXISTING=from-file\n").unwrap();

        let _guard = EnvVarGuard::set("BATTY_TEST_EXISTING", "from-shell");
        load_env_file(&path).unwrap();

        assert_eq!(std::env::var("BATTY_TEST_EXISTING").unwrap(), "from-shell");
    }

    #[test]
    fn upsert_env_var_replaces_existing_assignment() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".env");
        std::fs::write(&path, "FIRST=alpha\nSECOND=beta\n").unwrap();

        upsert_env_var(&path, "SECOND", "updated").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "FIRST=alpha\nSECOND=updated\n"
        );
    }

    #[test]
    fn upsert_env_var_appends_new_assignment() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".env");
        std::fs::write(&path, "FIRST=alpha\n").unwrap();

        upsert_env_var(&path, "SECOND", "beta value").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "FIRST=alpha\n\nSECOND=\"beta value\"\n"
        );
    }
}
