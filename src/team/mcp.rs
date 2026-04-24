//! Per-member MCP resource isolation helpers.
//!
//! Batty does not launch MCP servers directly for every backend. Agent CLIs
//! discover and spawn them from their own configs, so the daemon provides a
//! stable namespace contract through environment variables that those configs
//! can consume.

use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const MCP_PORT_BLOCK_SIZE: u16 = 20;
const MCP_PORT_MIN: u16 = 46000;
const MCP_PORT_BLOCK_COUNT: u16 = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct McpIsolation {
    pub(crate) namespace: String,
    pub(crate) resource_dir: PathBuf,
    pub(crate) shared_lock_dir: PathBuf,
    pub(crate) shared_lock: PathBuf,
    pub(crate) port_base: u16,
}

impl McpIsolation {
    pub(crate) fn for_member(project_root: &Path, member_name: &str) -> Self {
        let project_slug = project_root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "default".to_string());
        let namespace = format!(
            "{}-{}",
            sanitize_namespace_part(&project_slug),
            sanitize_namespace_part(member_name)
        );
        let resource_dir = project_root
            .join(".batty")
            .join("mcp")
            .join("namespaces")
            .join(member_name);
        let shared_lock_dir = project_root.join(".batty").join("mcp").join("shared-locks");
        let shared_lock = shared_lock_dir.join("mcp-shared.lock");
        let port_base = port_base_for(&namespace);

        Self {
            namespace,
            resource_dir,
            shared_lock_dir,
            shared_lock,
            port_base,
        }
    }

    pub(crate) fn create_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.resource_dir)?;
        std::fs::create_dir_all(&self.shared_lock_dir)?;
        Ok(())
    }

    pub(crate) fn apply_to_command(&self, command: &mut Command) {
        command.env("BATTY_MCP_NAMESPACE", &self.namespace);
        command.env("BATTY_MCP_RESOURCE_DIR", &self.resource_dir);
        command.env("BATTY_MCP_SHARED_LOCK_DIR", &self.shared_lock_dir);
        command.env("BATTY_MCP_SHARED_LOCK", &self.shared_lock);
        command.env("BATTY_MCP_PORT_BASE", self.port_base.to_string());
        command.env("BATTY_MCP_PORT_RANGE", MCP_PORT_BLOCK_SIZE.to_string());

        // Convenience aliases for MCP configs that support environment
        // expansion but should not need Batty-specific names.
        command.env("MCP_NAMESPACE", &self.namespace);
        command.env("MCP_RESOURCE_DIR", &self.resource_dir);
        command.env("MCP_SHARED_LOCK", &self.shared_lock);
        command.env("MCP_PORT_BASE", self.port_base.to_string());
    }

    pub(crate) fn shell_exports(&self) -> String {
        format!(
            "mcp_resource_dir={resource_dir}\n\
             mcp_shared_lock_dir={shared_lock_dir}\n\
             mkdir -p \"$mcp_resource_dir\" \"$mcp_shared_lock_dir\"\n\
             export BATTY_MCP_NAMESPACE={namespace}\n\
             export BATTY_MCP_RESOURCE_DIR=\"$mcp_resource_dir\"\n\
             export BATTY_MCP_SHARED_LOCK_DIR=\"$mcp_shared_lock_dir\"\n\
             export BATTY_MCP_SHARED_LOCK={shared_lock}\n\
             export BATTY_MCP_PORT_BASE='{port_base}'\n\
             export BATTY_MCP_PORT_RANGE='{port_range}'\n\
             export MCP_NAMESPACE=\"${{MCP_NAMESPACE:-$BATTY_MCP_NAMESPACE}}\"\n\
             export MCP_RESOURCE_DIR=\"${{MCP_RESOURCE_DIR:-$BATTY_MCP_RESOURCE_DIR}}\"\n\
             export MCP_SHARED_LOCK=\"${{MCP_SHARED_LOCK:-$BATTY_MCP_SHARED_LOCK}}\"\n\
             export MCP_PORT_BASE=\"${{MCP_PORT_BASE:-$BATTY_MCP_PORT_BASE}}\"\n",
            resource_dir = shell_quote(&self.resource_dir.to_string_lossy()),
            shared_lock_dir = shell_quote(&self.shared_lock_dir.to_string_lossy()),
            namespace = shell_quote(&self.namespace),
            shared_lock = shell_quote(&self.shared_lock.to_string_lossy()),
            port_base = self.port_base,
            port_range = MCP_PORT_BLOCK_SIZE,
        )
    }
}

fn sanitize_namespace_part(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            last_dash = false;
            ch.to_ascii_lowercase()
        } else if !last_dash {
            last_dash = true;
            '-'
        } else {
            continue;
        };
        out.push(normalized);
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "default".to_string()
    } else {
        trimmed
    }
}

fn port_base_for(namespace: &str) -> u16 {
    let block = stable_hash(namespace) % u64::from(MCP_PORT_BLOCK_COUNT);
    MCP_PORT_MIN + (block as u16 * MCP_PORT_BLOCK_SIZE)
}

fn stable_hash(input: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn shell_quote(input: &str) -> String {
    format!("'{}'", input.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_isolation_is_per_member() {
        let root = Path::new("/tmp/My Project");
        let eng_1 = McpIsolation::for_member(root, "eng-1-1");
        let eng_2 = McpIsolation::for_member(root, "eng-1-2");

        assert_eq!(eng_1.namespace, "my-project-eng-1-1");
        assert_eq!(
            eng_1.resource_dir,
            PathBuf::from("/tmp/My Project/.batty/mcp/namespaces/eng-1-1")
        );
        assert_ne!(eng_1.namespace, eng_2.namespace);
        assert_ne!(eng_1.resource_dir, eng_2.resource_dir);
        assert_ne!(eng_1.port_base, eng_2.port_base);
        assert_eq!(eng_1.shared_lock, eng_2.shared_lock);
    }

    #[test]
    fn shell_exports_include_namespace_and_serial_lock_contract() {
        let isolation = McpIsolation::for_member(Path::new("/tmp/repo"), "eng-1");
        let exports = isolation.shell_exports();

        assert!(exports.contains("export BATTY_MCP_NAMESPACE='repo-eng-1'"));
        assert!(exports.contains("export BATTY_MCP_RESOURCE_DIR=\"$mcp_resource_dir\""));
        assert!(exports.contains("export BATTY_MCP_SHARED_LOCK="));
        assert!(
            exports.contains("export MCP_PORT_BASE=\"${MCP_PORT_BASE:-$BATTY_MCP_PORT_BASE}\"")
        );
    }
}
