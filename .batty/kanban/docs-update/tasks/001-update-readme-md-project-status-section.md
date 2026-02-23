---
id: 1
title: Update README.md project status section
status: done
priority: high
created: 2026-02-22T14:45:31.439468652-05:00
updated: 2026-02-22T14:51:29.828106896-05:00
started: 2026-02-22T14:50:04.829583865-05:00
completed: 2026-02-22T14:51:29.828106509-05:00
tags:
    - docs
    - readme
class: standard
---

## Problem

The Project Status section at line 330 says:
> Phase 1 complete. Phase 2 complete. Phase 2.4 (supervision harness validation) is next. Phase 2.5 (runtime hardening + dogfood) follows.

This is stale. Phases 2.4, 2.5, 2.6 are all DONE. Phase 2.7 is in-progress.

## Also Fix

- The `batty work all` and `batty work all --parallel 3` examples at lines 213-216 advertise unimplemented features (Phase 3A/4). Add a note that these are planned, or remove them from the quick-reference section and mention them only under a "Roadmap" heading.
- Verify the Links section URLs (batty.sh, github.com/battysh/batty, discord, twitter, bluesky) — confirm they're real or mark as TBD.

## Acceptance Criteria

- Project Status reflects current reality (phases 1-2.6 done, 2.7 in-progress, 3A+ planned)
- Unimplemented commands are not presented as working features
- Links section is accurate

[[2026-02-22]] Sun 14:51
Updated README status to phases 1-2.6 done / 2.7 in progress, moved work-all examples to planned-not-implemented note, and set non-verified public links to TBD while keeping verified GitHub URL.
