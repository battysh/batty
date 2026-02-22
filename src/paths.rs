use std::path::{Path, PathBuf};

/// Resolve the kanban root directory for a project.
///
/// Prefers `.batty/kanban/` (new layout) if it exists, otherwise falls back
/// to `kanban/` (legacy layout). When neither exists, returns the preferred
/// `.batty/kanban/` path so new projects get the consolidated layout.
pub fn resolve_kanban_root(base: &Path) -> PathBuf {
    let preferred = base.join(".batty").join("kanban");
    if preferred.is_dir() {
        preferred
    } else {
        let legacy = base.join("kanban");
        if legacy.is_dir() {
            legacy
        } else {
            preferred
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_batty_kanban_when_it_exists() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".batty").join("kanban")).unwrap();
        std::fs::create_dir_all(tmp.path().join("kanban")).unwrap();

        let result = resolve_kanban_root(tmp.path());
        assert_eq!(result, tmp.path().join(".batty").join("kanban"));
    }

    #[test]
    fn falls_back_to_legacy_kanban() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("kanban")).unwrap();

        let result = resolve_kanban_root(tmp.path());
        assert_eq!(result, tmp.path().join("kanban"));
    }

    #[test]
    fn returns_preferred_when_neither_exists() {
        let tmp = tempfile::tempdir().unwrap();

        let result = resolve_kanban_root(tmp.path());
        assert_eq!(result, tmp.path().join(".batty").join("kanban"));
    }
}
