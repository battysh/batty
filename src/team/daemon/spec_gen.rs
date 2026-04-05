use anyhow::Result;

use super::*;

impl TeamDaemon {
    pub(super) fn sync_cleanroom_specs(&mut self) -> Result<()> {
        if !self.config.team_config.workflow_policy.clean_room_mode {
            return Ok(());
        }

        let specs = crate::team::spec_gen::load_behavior_specs(&self.config.project_root)?;
        for spec in &specs {
            let target_path = self.handoff_dir().join(&spec.relative_path);
            let current = std::fs::read_to_string(&target_path).ok();
            if current.as_deref() == Some(spec.content.as_str()) {
                continue;
            }
            self.write_handoff_artifact(
                "spec-writer",
                &spec.relative_path,
                spec.content.as_bytes(),
            )?;
        }

        crate::team::spec_gen::sync_specs_to_parity(&self.config.project_root, &specs)?;
        Ok(())
    }
}
