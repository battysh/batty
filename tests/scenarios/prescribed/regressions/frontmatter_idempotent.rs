//! Regression for 0.10.8: `repair_task_frontmatter_compat` must return
//! `None` for already-canonical blocked tasks. Before the fix it was
//! re-normalizing every tick, rewriting files and hammering the
//! filesystem in a silent loop.
//!
//! Test: write a canonical blocked task file (status=blocked,
//! blocked=true, block_reason and blocked_on both matching), call
//! repair 3 times, assert each call returns false (no repair applied).

use super::super::super::scenarios_common::ScenarioFixture;

/// Canonical blocked task shape: `blocked: true`, `block_reason` and
/// `blocked_on` both set to the same value (matching what the
/// frontmatter normalizer produces after a clean repair).
const CANONICAL_BLOCKED_TASK: &str = r#"---
id: 999
title: already canonical blocked task
status: blocked
priority: high
class: standard
blocked: true
block_reason: waiting on dep
blocked_on: waiting on dep
---

Body.
"#;

#[test]
fn frontmatter_idempotent_repair_noop_on_canonical_blocked_task() {
    let mut fixture = ScenarioFixture::builder().with_engineers(1).build();

    let path = fixture.write_raw_task_file("999-canonical.md", CANONICAL_BLOCKED_TASK);

    for attempt in 0..3 {
        let repaired = fixture
            .daemon_mut()
            .scenario_hooks()
            .repair_task_frontmatter(&path);
        assert!(
            !repaired,
            "attempt {attempt}: canonical blocked task should not be repaired"
        );
    }
}
