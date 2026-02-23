//! Task dependency DAG utilities for kanban phase boards.
//!
//! The graph is rebuilt from task files and used by scheduling logic to:
//! - validate dependency integrity (missing IDs, cycles),
//! - compute the ready frontier from completed tasks,
//! - compute deterministic topological execution order.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::task::{self, Task};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DagTask {
    pub id: u32,
    pub status: String,
    pub depends_on: Vec<u32>,
}

#[derive(Debug, Clone, Default)]
pub struct TaskDag {
    tasks: BTreeMap<u32, DagTask>,
    adjacency: BTreeMap<u32, Vec<u32>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisitState {
    Visiting,
    Visited,
}

impl TaskDag {
    /// Build a dependency DAG from parsed task records.
    pub fn from_tasks(tasks: &[Task]) -> Result<Self> {
        let mut dag_tasks = BTreeMap::new();
        for task in tasks {
            let mut depends_on = task.depends_on.clone();
            depends_on.sort_unstable();
            depends_on.dedup();

            let dag_task = DagTask {
                id: task.id,
                status: task.status.clone(),
                depends_on,
            };

            if dag_tasks.insert(task.id, dag_task).is_some() {
                bail!("duplicate task id in board: #{}", task.id);
            }
        }

        let mut adjacency: BTreeMap<u32, Vec<u32>> =
            dag_tasks.keys().map(|id| (*id, Vec::new())).collect();

        for task in dag_tasks.values() {
            for dep in &task.depends_on {
                if !dag_tasks.contains_key(dep) {
                    bail!("task #{} depends on missing task #{}", task.id, dep);
                }
                if let Some(dependents) = adjacency.get_mut(dep) {
                    dependents.push(task.id);
                }
            }
        }
        for dependents in adjacency.values_mut() {
            dependents.sort_unstable();
            dependents.dedup();
        }

        let dag = Self {
            tasks: dag_tasks,
            adjacency,
        };
        dag.ensure_acyclic()?;
        Ok(dag)
    }

    /// Load task files from a board's `tasks/` directory and build a DAG.
    pub fn from_tasks_dir(tasks_dir: &Path) -> Result<Self> {
        let tasks = task::load_tasks_from_dir(tasks_dir)
            .with_context(|| format!("failed to load tasks from {}", tasks_dir.display()))?;
        Self::from_tasks(&tasks)
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    #[allow(dead_code)]
    pub fn adjacency_list(&self) -> &BTreeMap<u32, Vec<u32>> {
        &self.adjacency
    }

    /// Compute tasks ready to start given a set of completed task IDs.
    ///
    /// Ready task conditions:
    /// - task is not already completed,
    /// - task has not started yet (backlog/todo),
    /// - every dependency is completed.
    pub fn ready_set(&self, completed: &HashSet<u32>) -> Vec<u32> {
        let mut ready = Vec::new();

        for (id, task) in &self.tasks {
            if completed.contains(id) {
                continue;
            }
            if !is_not_started_status(&task.status) {
                continue;
            }
            if task.depends_on.iter().all(|dep| completed.contains(dep)) {
                ready.push(*id);
            }
        }

        ready
    }

    /// Return a deterministic topological sort of all task IDs.
    pub fn topological_sort(&self) -> Result<Vec<u32>> {
        let mut indegree: BTreeMap<u32, usize> = self
            .tasks
            .iter()
            .map(|(id, task)| (*id, task.depends_on.len()))
            .collect();
        let mut queue = VecDeque::new();
        for (id, degree) in &indegree {
            if *degree == 0 {
                queue.push_back(*id);
            }
        }

        let mut order = Vec::with_capacity(self.tasks.len());
        while let Some(task_id) = queue.pop_front() {
            order.push(task_id);
            if let Some(dependents) = self.adjacency.get(&task_id) {
                for dependent in dependents {
                    let degree = indegree
                        .get_mut(dependent)
                        .expect("dependent must exist in indegree map");
                    *degree -= 1;
                    if *degree == 0 {
                        queue.push_back(*dependent);
                    }
                }
            }
        }

        if order.len() != self.tasks.len() {
            bail!("dependency graph contains a cycle");
        }

        Ok(order)
    }

    fn ensure_acyclic(&self) -> Result<()> {
        let mut states: HashMap<u32, VisitState> = HashMap::new();
        let mut path = Vec::new();
        let mut path_index: HashMap<u32, usize> = HashMap::new();

        for task_id in self.tasks.keys() {
            if states.contains_key(task_id) {
                continue;
            }
            if let Some(cycle) =
                self.find_cycle(*task_id, &mut states, &mut path, &mut path_index)?
            {
                let rendered = cycle
                    .iter()
                    .map(|id| format!("#{id}"))
                    .collect::<Vec<_>>()
                    .join(" -> ");
                bail!("dependency cycle detected: {rendered}");
            }
        }

        Ok(())
    }

    fn find_cycle(
        &self,
        task_id: u32,
        states: &mut HashMap<u32, VisitState>,
        path: &mut Vec<u32>,
        path_index: &mut HashMap<u32, usize>,
    ) -> Result<Option<Vec<u32>>> {
        states.insert(task_id, VisitState::Visiting);
        path_index.insert(task_id, path.len());
        path.push(task_id);

        let task = self
            .tasks
            .get(&task_id)
            .with_context(|| format!("task #{task_id} missing from DAG"))?;
        for dep in &task.depends_on {
            match states.get(dep).copied() {
                None => {
                    if let Some(cycle) = self.find_cycle(*dep, states, path, path_index)? {
                        return Ok(Some(cycle));
                    }
                }
                Some(VisitState::Visiting) => {
                    let start = *path_index
                        .get(dep)
                        .with_context(|| format!("cycle path index missing for task #{dep}"))?;
                    let mut cycle = path[start..].to_vec();
                    cycle.push(*dep);
                    return Ok(Some(cycle));
                }
                Some(VisitState::Visited) => {}
            }
        }

        path.pop();
        path_index.remove(&task_id);
        states.insert(task_id, VisitState::Visited);
        Ok(None)
    }
}

fn is_not_started_status(status: &str) -> bool {
    matches!(status, "backlog" | "todo")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn mk_task(id: u32, status: &str, depends_on: Vec<u32>) -> Task {
        Task {
            id,
            title: format!("task-{id}"),
            status: status.to_string(),
            priority: "high".to_string(),
            tags: vec![],
            depends_on,
            description: format!("Task {id}"),
            batty_config: None,
            source_path: PathBuf::new(),
        }
    }

    #[test]
    fn builds_from_tasks_dir_and_adjacency() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(
            tmp.path().join("001-a.md"),
            r#"---
id: 1
title: A
status: done
priority: high
tags: []
depends_on: []
class: standard
---

A
"#,
        )
        .unwrap();
        fs::write(
            tmp.path().join("002-b.md"),
            r#"---
id: 2
title: B
status: backlog
priority: high
tags: []
depends_on:
  - 1
class: standard
---

B
"#,
        )
        .unwrap();

        let dag = TaskDag::from_tasks_dir(tmp.path()).unwrap();
        assert_eq!(dag.adjacency_list().get(&1).cloned(), Some(vec![2]));
        assert_eq!(dag.adjacency_list().get(&2).cloned(), Some(vec![]));
    }

    #[test]
    fn missing_dependency_is_error() {
        let dag = TaskDag::from_tasks(&[mk_task(1, "backlog", vec![99])]);
        let err = dag.unwrap_err().to_string();
        assert!(err.contains("task #1 depends on missing task #99"));
    }

    #[test]
    fn cycle_detection_names_cycle() {
        let dag = TaskDag::from_tasks(&[
            mk_task(1, "backlog", vec![2]),
            mk_task(2, "backlog", vec![3]),
            mk_task(3, "backlog", vec![1]),
        ]);
        let err = dag.unwrap_err().to_string();
        assert!(err.contains("dependency cycle detected"));
        assert!(err.contains("#1 -> #2 -> #3 -> #1") || err.contains("#2 -> #3 -> #1 -> #2"));
    }

    #[test]
    fn topological_sort_orders_dependencies_before_dependents() {
        let dag = TaskDag::from_tasks(&[
            mk_task(1, "done", vec![]),
            mk_task(2, "backlog", vec![1]),
            mk_task(3, "backlog", vec![1]),
            mk_task(4, "backlog", vec![2, 3]),
        ])
        .unwrap();

        let order = dag.topological_sort().unwrap();
        let pos = |id| order.iter().position(|x| *x == id).unwrap();
        assert!(pos(1) < pos(2));
        assert!(pos(1) < pos(3));
        assert!(pos(2) < pos(4));
        assert!(pos(3) < pos(4));
    }

    #[test]
    fn ready_set_filters_started_and_unsatisfied_tasks() {
        let dag = TaskDag::from_tasks(&[
            mk_task(1, "done", vec![]),
            mk_task(2, "backlog", vec![1]),
            mk_task(3, "todo", vec![1]),
            mk_task(4, "in-progress", vec![1]),
            mk_task(5, "backlog", vec![2]),
            mk_task(6, "review", vec![]),
        ])
        .unwrap();

        let completed = HashSet::from([1]);
        let ready = dag.ready_set(&completed);
        assert_eq!(ready, vec![2, 3]);
    }

    #[test]
    fn empty_graph_is_valid() {
        let dag = TaskDag::from_tasks(&[]).unwrap();
        assert!(dag.is_empty());
        assert_eq!(dag.topological_sort().unwrap(), Vec::<u32>::new());
        let ready = dag.ready_set(&HashSet::new());
        assert!(ready.is_empty());
    }

    #[test]
    fn synthetic_eight_task_dag_ready_progression_is_stable() {
        let dag = TaskDag::from_tasks(&[
            mk_task(1, "backlog", vec![]),
            mk_task(2, "backlog", vec![]),
            mk_task(3, "backlog", vec![1]),
            mk_task(4, "backlog", vec![1]),
            mk_task(5, "backlog", vec![2]),
            mk_task(6, "backlog", vec![3, 5]),
            mk_task(7, "backlog", vec![4]),
            mk_task(8, "backlog", vec![6, 7]),
        ])
        .unwrap();

        let ready0 = dag.ready_set(&HashSet::new());
        assert_eq!(ready0, vec![1, 2]);

        let ready1 = dag.ready_set(&HashSet::from([1, 2]));
        assert_eq!(ready1, vec![3, 4, 5]);

        let ready2 = dag.ready_set(&HashSet::from([1, 2, 3, 4, 5]));
        assert_eq!(ready2, vec![6, 7]);

        let ready3 = dag.ready_set(&HashSet::from([1, 2, 3, 4, 5, 6, 7]));
        assert_eq!(ready3, vec![8]);
    }
}
