//! Dependency graph visualization for board tasks.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Result, bail};

use crate::task::{Task, load_tasks_from_dir};

/// Output format for dependency graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepsFormat {
    Tree,
    Flat,
    Dot,
}

/// A node in the dependency graph.
#[derive(Debug)]
struct TaskNode {
    id: u32,
    title: String,
    status: String,
    priority: String,
    depends_on: Vec<u32>,
}

impl TaskNode {
    fn from_task(task: &Task) -> Self {
        Self {
            id: task.id,
            title: task.title.clone(),
            status: task.status.clone(),
            priority: task.priority.clone(),
            depends_on: task.depends_on.clone(),
        }
    }

    fn truncated_title(&self, max_len: usize) -> String {
        if self.title.len() <= max_len {
            self.title.clone()
        } else {
            format!("{}...", &self.title[..max_len - 3])
        }
    }

    fn status_indicator(&self) -> &str {
        match self.status.as_str() {
            "done" => "[x]",
            "in-progress" => "[>]",
            "review" => "[R]",
            "todo" => "[ ]",
            "backlog" => "[-]",
            "blocked" => "[!]",
            _ => "[?]",
        }
    }

    fn is_incomplete(&self) -> bool {
        self.status != "done"
    }
}

/// Build and render the dependency graph.
pub fn render_deps(board_dir: &Path, format: DepsFormat) -> Result<String> {
    let tasks_dir = board_dir.join("tasks");
    if !tasks_dir.is_dir() {
        bail!("no tasks directory found at {}", tasks_dir.display());
    }

    let tasks = load_tasks_from_dir(&tasks_dir)?;
    let nodes: BTreeMap<u32, TaskNode> = tasks
        .iter()
        .map(|t| (t.id, TaskNode::from_task(t)))
        .collect();

    // Check for cycles before rendering
    if let Some(cycle) = detect_cycle(&nodes) {
        let cycle_str = cycle
            .iter()
            .map(|id| format!("#{id}"))
            .collect::<Vec<_>>()
            .join(" -> ");
        bail!("dependency cycle detected: {cycle_str}");
    }

    match format {
        DepsFormat::Tree => render_tree(&nodes),
        DepsFormat::Flat => render_flat(&nodes),
        DepsFormat::Dot => render_dot(&nodes),
    }
}

pub(crate) fn detect_cycle_for_tasks(tasks: &[Task]) -> Option<Vec<u32>> {
    let nodes: BTreeMap<u32, TaskNode> = tasks
        .iter()
        .map(|task| (task.id, TaskNode::from_task(task)))
        .collect();
    detect_cycle(&nodes)
}

/// Detect cycles using DFS. Returns the cycle path if found.
fn detect_cycle(nodes: &BTreeMap<u32, TaskNode>) -> Option<Vec<u32>> {
    let mut visited = HashSet::new();
    let mut in_stack = HashSet::new();
    let mut path = Vec::new();

    for &id in nodes.keys() {
        if !visited.contains(&id) {
            if let Some(cycle) = dfs_cycle(id, nodes, &mut visited, &mut in_stack, &mut path) {
                return Some(cycle);
            }
        }
    }
    None
}

fn dfs_cycle(
    id: u32,
    nodes: &BTreeMap<u32, TaskNode>,
    visited: &mut HashSet<u32>,
    in_stack: &mut HashSet<u32>,
    path: &mut Vec<u32>,
) -> Option<Vec<u32>> {
    visited.insert(id);
    in_stack.insert(id);
    path.push(id);

    if let Some(node) = nodes.get(&id) {
        for &dep in &node.depends_on {
            if !nodes.contains_key(&dep) {
                continue; // skip references to non-existent tasks
            }
            if in_stack.contains(&dep) {
                // Found a cycle — extract it
                let cycle_start = path.iter().position(|&x| x == dep).unwrap();
                let mut cycle = path[cycle_start..].to_vec();
                cycle.push(dep);
                return Some(cycle);
            }
            if !visited.contains(&dep) {
                if let Some(cycle) = dfs_cycle(dep, nodes, visited, in_stack, path) {
                    return Some(cycle);
                }
            }
        }
    }

    path.pop();
    in_stack.remove(&id);
    None
}

/// Render tree format: roots at top, children indented below.
fn render_tree(nodes: &BTreeMap<u32, TaskNode>) -> Result<String> {
    // Build reverse map: parent -> children (tasks that depend on parent)
    let mut children: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
    let mut has_parent = HashSet::new();

    for node in nodes.values() {
        for &dep in &node.depends_on {
            if nodes.contains_key(&dep) {
                children.entry(dep).or_default().insert(node.id);
                has_parent.insert(node.id);
            }
        }
    }

    // Roots are tasks with no dependencies (or deps pointing to non-existent tasks)
    let roots: Vec<u32> = nodes
        .keys()
        .filter(|id| !has_parent.contains(id))
        .copied()
        .collect();

    if roots.is_empty() && !nodes.is_empty() {
        // All tasks have parents — shouldn't happen without cycles
        return Ok("(no root tasks found)\n".to_string());
    }

    let mut out = String::new();

    // Show blocked chains header
    let blocked = find_blocked_chains(nodes);
    if !blocked.is_empty() {
        writeln!(out, "Blocked chains:").unwrap();
        for chain in &blocked {
            let parts: Vec<String> = chain
                .iter()
                .map(|&id| {
                    let node = &nodes[&id];
                    format!("#{} {}", id, node.truncated_title(30))
                })
                .collect();
            writeln!(out, "  {} (waiting)", parts.join(" -> ")).unwrap();
        }
        writeln!(out).unwrap();
    }

    // Show critical path
    let critical = find_critical_path(nodes);
    if critical.len() > 1 {
        let parts: Vec<String> = critical.iter().map(|&id| format!("#{id}")).collect();
        writeln!(
            out,
            "Critical path ({}): {}",
            critical.len(),
            parts.join(" -> ")
        )
        .unwrap();
        writeln!(out).unwrap();
    }

    // Render tree
    writeln!(out, "Dependency tree:").unwrap();
    for &root in &roots {
        render_tree_node(root, nodes, &children, &mut out, "", true);
    }

    Ok(out)
}

fn render_tree_node(
    id: u32,
    nodes: &BTreeMap<u32, TaskNode>,
    children: &BTreeMap<u32, BTreeSet<u32>>,
    out: &mut String,
    prefix: &str,
    is_last: bool,
) {
    let Some(node) = nodes.get(&id) else { return };

    let connector = if prefix.is_empty() {
        ""
    } else if is_last {
        "└── "
    } else {
        "├── "
    };

    let priority_tag = if node.priority.is_empty() {
        String::new()
    } else {
        format!(" ({})", node.priority)
    };

    writeln!(
        out,
        "{prefix}{connector}{} #{} {}{}",
        node.status_indicator(),
        node.id,
        node.truncated_title(50),
        priority_tag,
    )
    .unwrap();

    let child_ids: Vec<u32> = children
        .get(&id)
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default();

    let child_prefix = if prefix.is_empty() {
        String::new()
    } else if is_last {
        format!("{prefix}    ")
    } else {
        format!("{prefix}│   ")
    };

    for (i, &child) in child_ids.iter().enumerate() {
        let child_is_last = i == child_ids.len() - 1;
        render_tree_node(child, nodes, children, out, &child_prefix, child_is_last);
    }
}

/// Render flat format: just dependency pairs.
fn render_flat(nodes: &BTreeMap<u32, TaskNode>) -> Result<String> {
    let mut out = String::new();
    let mut has_deps = false;

    for node in nodes.values() {
        for &dep in &node.depends_on {
            if nodes.contains_key(&dep) {
                writeln!(out, "#{} -> #{}", node.id, dep).unwrap();
                has_deps = true;
            }
        }
    }

    if !has_deps {
        writeln!(out, "(no dependencies)").unwrap();
    }
    Ok(out)
}

/// Render graphviz DOT format.
fn render_dot(nodes: &BTreeMap<u32, TaskNode>) -> Result<String> {
    let mut out = String::new();
    writeln!(out, "digraph deps {{").unwrap();
    writeln!(out, "  rankdir=BT;").unwrap();
    writeln!(out, "  node [shape=box];").unwrap();

    for node in nodes.values() {
        if node.depends_on.is_empty() && !nodes.values().any(|n| n.depends_on.contains(&node.id)) {
            continue; // skip isolated nodes
        }

        let color = match node.status.as_str() {
            "done" => "green",
            "in-progress" => "yellow",
            "review" => "orange",
            "todo" | "backlog" => "white",
            _ => "gray",
        };

        writeln!(
            out,
            "  t{} [label=\"#{} {}\" style=filled fillcolor={}];",
            node.id,
            node.id,
            node.truncated_title(30).replace('"', "\\\""),
            color,
        )
        .unwrap();
    }

    writeln!(out).unwrap();

    for node in nodes.values() {
        for &dep in &node.depends_on {
            if nodes.contains_key(&dep) {
                writeln!(out, "  t{} -> t{};", node.id, dep).unwrap();
            }
        }
    }

    writeln!(out, "}}").unwrap();
    Ok(out)
}

/// Find chains where an incomplete task is blocked on another incomplete task.
fn find_blocked_chains(nodes: &BTreeMap<u32, TaskNode>) -> Vec<Vec<u32>> {
    let mut chains = Vec::new();

    for node in nodes.values() {
        if !node.is_incomplete() {
            continue;
        }
        for &dep in &node.depends_on {
            if let Some(dep_node) = nodes.get(&dep) {
                if dep_node.is_incomplete() {
                    chains.push(vec![node.id, dep]);
                }
            }
        }
    }

    chains.sort();
    chains
}

/// Find the longest chain of incomplete tasks (critical path).
fn find_critical_path(nodes: &BTreeMap<u32, TaskNode>) -> Vec<u32> {
    let mut memo: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut best = Vec::new();

    for &id in nodes.keys() {
        let path = longest_path(id, nodes, &mut memo);
        if path.len() > best.len() {
            best = path;
        }
    }

    best
}

fn longest_path(
    id: u32,
    nodes: &BTreeMap<u32, TaskNode>,
    memo: &mut HashMap<u32, Vec<u32>>,
) -> Vec<u32> {
    if let Some(cached) = memo.get(&id) {
        return cached.clone();
    }

    let Some(node) = nodes.get(&id) else {
        return Vec::new();
    };

    if !node.is_incomplete() {
        memo.insert(id, Vec::new());
        return Vec::new();
    }

    let mut best_child = Vec::new();
    for &dep in &node.depends_on {
        if nodes.contains_key(&dep) {
            let child_path = longest_path(dep, nodes, memo);
            if child_path.len() > best_child.len() {
                best_child = child_path;
            }
        }
    }

    let mut path = vec![id];
    path.extend(best_child);
    memo.insert(id, path.clone());
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_task(
        dir: &Path,
        id: u32,
        title: &str,
        status: &str,
        priority: &str,
        depends_on: &[u32],
    ) {
        let deps = if depends_on.is_empty() {
            "depends_on: []".to_string()
        } else {
            let items: Vec<String> = depends_on.iter().map(|d| format!("  - {d}")).collect();
            format!("depends_on:\n{}", items.join("\n"))
        };
        let content = format!(
            "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: {priority}\n{deps}\nclass: standard\n---\n\nDescription.\n"
        );
        fs::write(dir.join(format!("{id:03}-task.md")), content).unwrap();
    }

    fn setup_board(tasks: &[(u32, &str, &str, &str, &[u32])]) -> TempDir {
        let tmp = TempDir::new().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();
        for &(id, title, status, priority, deps) in tasks {
            write_task(&tasks_dir, id, title, status, priority, deps);
        }
        tmp
    }

    #[test]
    fn empty_board() {
        let tmp = TempDir::new().unwrap();
        let tasks_dir = tmp.path().join("tasks");
        fs::create_dir_all(&tasks_dir).unwrap();

        let output = render_deps(tmp.path(), DepsFormat::Tree).unwrap();
        assert!(output.contains("Dependency tree:"));

        let flat = render_deps(tmp.path(), DepsFormat::Flat).unwrap();
        assert!(flat.contains("(no dependencies)"));
    }

    #[test]
    fn no_dependencies() {
        let tmp = setup_board(&[
            (1, "Task A", "todo", "high", &[]),
            (2, "Task B", "todo", "medium", &[]),
        ]);

        let flat = render_deps(tmp.path(), DepsFormat::Flat).unwrap();
        assert!(flat.contains("(no dependencies)"));

        let tree = render_deps(tmp.path(), DepsFormat::Tree).unwrap();
        assert!(tree.contains("#1"));
        assert!(tree.contains("#2"));
    }

    #[test]
    fn linear_chain() {
        let tmp = setup_board(&[
            (1, "Foundation", "done", "high", &[]),
            (2, "Build on foundation", "in-progress", "high", &[1]),
            (3, "Final step", "todo", "medium", &[2]),
        ]);

        let tree = render_deps(tmp.path(), DepsFormat::Tree).unwrap();
        assert!(tree.contains("#1"));
        assert!(tree.contains("#2"));
        assert!(tree.contains("#3"));
        assert!(tree.contains("Foundation"));

        let flat = render_deps(tmp.path(), DepsFormat::Flat).unwrap();
        assert!(flat.contains("#2 -> #1"));
        assert!(flat.contains("#3 -> #2"));
    }

    #[test]
    fn diamond_dependencies() {
        let tmp = setup_board(&[
            (1, "Root", "done", "high", &[]),
            (2, "Left", "todo", "medium", &[1]),
            (3, "Right", "todo", "medium", &[1]),
            (4, "Join", "todo", "low", &[2, 3]),
        ]);

        let flat = render_deps(tmp.path(), DepsFormat::Flat).unwrap();
        assert!(flat.contains("#2 -> #1"));
        assert!(flat.contains("#3 -> #1"));
        assert!(flat.contains("#4 -> #2"));
        assert!(flat.contains("#4 -> #3"));
    }

    #[test]
    fn cycle_detection() {
        let tmp = setup_board(&[
            (1, "Task A", "todo", "high", &[2]),
            (2, "Task B", "todo", "high", &[1]),
        ]);

        let result = render_deps(tmp.path(), DepsFormat::Tree);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("cycle"));
    }

    #[test]
    fn blocked_chains_shown() {
        let tmp = setup_board(&[
            (1, "Blocker", "todo", "high", &[]),
            (2, "Blocked task", "todo", "medium", &[1]),
        ]);

        let tree = render_deps(tmp.path(), DepsFormat::Tree).unwrap();
        assert!(tree.contains("Blocked chains:"));
        assert!(tree.contains("#2"));
        assert!(tree.contains("#1"));
    }

    #[test]
    fn done_deps_not_blocked() {
        let tmp = setup_board(&[
            (1, "Done task", "done", "high", &[]),
            (2, "Ready task", "todo", "medium", &[1]),
        ]);

        let tree = render_deps(tmp.path(), DepsFormat::Tree).unwrap();
        assert!(!tree.contains("Blocked chains:"));
    }

    #[test]
    fn critical_path_shown() {
        let tmp = setup_board(&[
            (1, "Step 1", "todo", "high", &[]),
            (2, "Step 2", "todo", "high", &[1]),
            (3, "Step 3", "todo", "high", &[2]),
        ]);

        let tree = render_deps(tmp.path(), DepsFormat::Tree).unwrap();
        assert!(tree.contains("Critical path (3)"));
        assert!(tree.contains("#3 -> #2 -> #1"));
    }

    #[test]
    fn dot_format() {
        let tmp = setup_board(&[
            (1, "Root", "done", "high", &[]),
            (2, "Child", "todo", "medium", &[1]),
        ]);

        let dot = render_deps(tmp.path(), DepsFormat::Dot).unwrap();
        assert!(dot.contains("digraph deps {"));
        assert!(dot.contains("t2 -> t1;"));
        assert!(dot.contains("fillcolor=green"));
        assert!(dot.contains("fillcolor=white"));
        assert!(dot.contains("}"));
    }

    #[test]
    fn title_truncation() {
        let node = TaskNode {
            id: 1,
            title: "A very long task title that should be truncated to fit within limits"
                .to_string(),
            status: "todo".to_string(),
            priority: "high".to_string(),
            depends_on: vec![],
        };
        let truncated = node.truncated_title(50);
        assert!(truncated.len() <= 50);
        assert!(truncated.ends_with("..."));
    }

    #[test]
    fn status_indicators() {
        let statuses = vec![
            ("done", "[x]"),
            ("in-progress", "[>]"),
            ("review", "[R]"),
            ("todo", "[ ]"),
            ("backlog", "[-]"),
            ("blocked", "[!]"),
            ("unknown", "[?]"),
        ];

        for (status, expected) in statuses {
            let node = TaskNode {
                id: 1,
                title: "Test".to_string(),
                status: status.to_string(),
                priority: "".to_string(),
                depends_on: vec![],
            };
            assert_eq!(node.status_indicator(), expected, "status={status}");
        }
    }

    #[test]
    fn missing_tasks_dir_errors() {
        let tmp = TempDir::new().unwrap();
        let result = render_deps(tmp.path(), DepsFormat::Tree);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("no tasks directory")
        );
    }

    #[test]
    fn deps_to_nonexistent_tasks_ignored() {
        let tmp = setup_board(&[(1, "Task", "todo", "high", &[999])]);

        // Should not error — just skip the nonexistent dep
        let flat = render_deps(tmp.path(), DepsFormat::Flat).unwrap();
        assert!(flat.contains("(no dependencies)"));
    }

    #[test]
    fn three_node_cycle_detected() {
        let tmp = setup_board(&[
            (1, "A", "todo", "high", &[3]),
            (2, "B", "todo", "high", &[1]),
            (3, "C", "todo", "high", &[2]),
        ]);

        let result = render_deps(tmp.path(), DepsFormat::Flat);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("cycle"));
    }
}
