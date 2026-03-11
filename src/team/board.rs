//! Board management — kanban.md rotation of done items to archive.

use std::path::Path;

use anyhow::{Context, Result};
use tracing::info;

/// Rotate done items from kanban.md to kanban-archive.md when the count
/// exceeds `threshold`.
///
/// Done items are lines under the `## Done` section. When the count exceeds
/// the threshold, the oldest items (first in the list) are moved to the
/// archive file.
pub fn rotate_done_items(kanban_path: &Path, archive_path: &Path, threshold: u32) -> Result<u32> {
    let content = std::fs::read_to_string(kanban_path)
        .with_context(|| format!("failed to read {}", kanban_path.display()))?;

    let (before_done, done_items, after_done) = split_done_section(&content);

    if done_items.len() <= threshold as usize {
        return Ok(0);
    }

    // Move excess items (oldest = first in list) to archive
    let keep_count = threshold as usize;
    let to_archive = &done_items[..done_items.len() - keep_count];
    let to_keep = &done_items[done_items.len() - keep_count..];
    let rotated = to_archive.len() as u32;

    // Rebuild kanban
    let mut new_kanban = before_done.to_string();
    new_kanban.push_str("## Done\n");
    for item in to_keep {
        new_kanban.push_str(item);
        new_kanban.push('\n');
    }
    if !after_done.is_empty() {
        new_kanban.push_str(after_done);
    }

    std::fs::write(kanban_path, &new_kanban)
        .with_context(|| format!("failed to write {}", kanban_path.display()))?;

    // Append to archive
    let mut archive_content = if archive_path.exists() {
        std::fs::read_to_string(archive_path)
            .with_context(|| format!("failed to read {}", archive_path.display()))?
    } else {
        "# Kanban Archive\n".to_string()
    };

    if !archive_content.ends_with('\n') {
        archive_content.push('\n');
    }
    for item in to_archive {
        archive_content.push_str(item);
        archive_content.push('\n');
    }

    std::fs::write(archive_path, &archive_content)
        .with_context(|| format!("failed to write {}", archive_path.display()))?;

    info!(rotated, threshold, "rotated done items to archive");
    Ok(rotated)
}

/// Split kanban content into (before_done, done_items, after_done).
fn split_done_section(content: &str) -> (&str, Vec<&str>, &str) {
    let done_marker = "## Done";
    let Some(done_start) = content.find(done_marker) else {
        return (content, Vec::new(), "");
    };

    let before_done = &content[..done_start];
    let after_marker = &content[done_start + done_marker.len()..];

    // Skip the newline after "## Done"
    let items_start = after_marker
        .find('\n')
        .map(|i| i + 1)
        .unwrap_or(after_marker.len());
    let items_section = &after_marker[items_start..];

    // Find the next section header (## Something)
    let mut done_items = Vec::new();
    let mut remaining_start = items_section.len();

    for (i, line) in items_section.lines().enumerate() {
        if line.starts_with("## ") && i > 0 {
            // Found next section — compute byte offset
            remaining_start = items_section
                .find(&format!("\n{line}"))
                .map(|pos| pos + 1)
                .unwrap_or(items_section.len());
            break;
        }
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            done_items.push(line);
        }
    }

    let after_done = &items_section[remaining_start..];
    (before_done, done_items, after_done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_done_section_basic() {
        let content =
            "# Board\n\n## Backlog\n\n## In Progress\n\n## Done\n- item 1\n- item 2\n- item 3\n";
        let (before, items, after) = split_done_section(content);
        assert!(before.contains("## In Progress"));
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], "- item 1");
        assert!(after.is_empty());
    }

    #[test]
    fn split_done_section_with_following_section() {
        let content = "## Done\n- a\n- b\n## Archive\nstuff\n";
        let (_, items, after) = split_done_section(content);
        assert_eq!(items.len(), 2);
        assert!(after.contains("## Archive"));
    }

    #[test]
    fn split_done_section_empty() {
        let content = "## Done\n\n## Other\n";
        let (_, items, _) = split_done_section(content);
        assert!(items.is_empty());
    }

    #[test]
    fn split_done_section_no_done_header() {
        let content = "# Board\n## Backlog\n- task\n";
        let (before, items, _) = split_done_section(content);
        assert_eq!(before, content);
        assert!(items.is_empty());
    }

    #[test]
    fn rotate_moves_excess_items() {
        let tmp = tempfile::tempdir().unwrap();
        let kanban = tmp.path().join("kanban.md");
        let archive = tmp.path().join("archive.md");

        std::fs::write(
            &kanban,
            "## Backlog\n\n## In Progress\n\n## Done\n- old 1\n- old 2\n- old 3\n- new 1\n- new 2\n",
        )
        .unwrap();

        let rotated = rotate_done_items(&kanban, &archive, 2).unwrap();
        assert_eq!(rotated, 3);

        let kanban_content = std::fs::read_to_string(&kanban).unwrap();
        assert!(kanban_content.contains("- new 1"));
        assert!(kanban_content.contains("- new 2"));
        assert!(!kanban_content.contains("- old 1"));

        let archive_content = std::fs::read_to_string(&archive).unwrap();
        assert!(archive_content.contains("- old 1"));
        assert!(archive_content.contains("- old 2"));
        assert!(archive_content.contains("- old 3"));
    }

    #[test]
    fn rotate_does_nothing_under_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let kanban = tmp.path().join("kanban.md");
        let archive = tmp.path().join("archive.md");

        std::fs::write(&kanban, "## Done\n- item 1\n- item 2\n").unwrap();

        let rotated = rotate_done_items(&kanban, &archive, 5).unwrap();
        assert_eq!(rotated, 0);
        assert!(!archive.exists());
    }

    #[test]
    fn rotate_appends_to_existing_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let kanban = tmp.path().join("kanban.md");
        let archive = tmp.path().join("archive.md");

        std::fs::write(&archive, "# Kanban Archive\n- previous\n").unwrap();
        std::fs::write(&kanban, "## Done\n- a\n- b\n- c\n").unwrap();

        let rotated = rotate_done_items(&kanban, &archive, 1).unwrap();
        assert_eq!(rotated, 2);

        let archive_content = std::fs::read_to_string(&archive).unwrap();
        assert!(archive_content.contains("- previous"));
        assert!(archive_content.contains("- a"));
        assert!(archive_content.contains("- b"));
    }
}
