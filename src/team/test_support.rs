use std::path::{Path, PathBuf};
use std::process::{Command, Output};

pub(crate) fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|error| panic!("git {:?} failed to run: {error}", args))
}

pub(crate) fn git_ok(dir: &Path, args: &[&str]) {
    let output = git(dir, args);
    assert!(
        output.status.success(),
        "git {:?} failed:\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(crate) fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = git(dir, args);
    assert!(
        output.status.success(),
        "git {:?} failed:\nstdout={}\nstderr={}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

pub(crate) fn init_git_repo(tmp: &tempfile::TempDir, package_name: &str) -> PathBuf {
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("src")).unwrap();
    std::fs::create_dir_all(repo.join(".batty").join("team_config")).unwrap();
    std::fs::write(
        repo.join("Cargo.toml"),
        format!("[package]\nname = \"{package_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    )
    .unwrap();
    std::fs::write(
        repo.join("src").join("lib.rs"),
        "pub fn smoke() -> bool { true }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn smoke_test() {\n        assert!(smoke());\n    }\n}\n",
    )
    .unwrap();
    git_ok(tmp.path(), &["init", "-b", "main", repo.to_str().unwrap()]);
    git_ok(&repo, &["config", "user.email", "batty@example.com"]);
    git_ok(&repo, &["config", "user.name", "Batty Tests"]);
    git_ok(&repo, &["add", "."]);
    git_ok(&repo, &["commit", "-m", "initial"]);
    repo
}

pub(crate) fn write_owned_task_file(
    project_root: &Path,
    id: u32,
    title: &str,
    status: &str,
    claimed_by: &str,
) {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join(format!("{id:03}-{title}.md")),
        format!(
            "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: critical\nclaimed_by: {claimed_by}\nclass: standard\n---\n\nTask description.\n"
        ),
    )
    .unwrap();
}

pub(crate) fn write_owned_task_file_with_context(
    project_root: &Path,
    id: u32,
    title: &str,
    status: &str,
    claimed_by: &str,
    branch: &str,
    worktree_path: &str,
) {
    let tasks_dir = project_root
        .join(".batty")
        .join("team_config")
        .join("board")
        .join("tasks");
    std::fs::create_dir_all(&tasks_dir).unwrap();
    std::fs::write(
        tasks_dir.join(format!("{id:03}-{title}.md")),
        format!(
            "---\nid: {id}\ntitle: {title}\nstatus: {status}\npriority: critical\nclaimed_by: {claimed_by}\nbranch: {branch}\nworktree_path: {worktree_path}\nclass: standard\n---\n\nTask description.\n"
        ),
    )
    .unwrap();
}

pub(crate) fn setup_fake_claude(tmp: &tempfile::TempDir, member_name: &str) -> (PathBuf, PathBuf) {
    let project_slug = tmp
        .path()
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "default".to_string());
    let fake_bin = std::env::temp_dir().join(format!("batty-bin-{project_slug}-{member_name}"));
    let _ = std::fs::remove_dir_all(&fake_bin);
    std::fs::create_dir_all(&fake_bin).unwrap();

    let fake_log = tmp.path().join(format!("{member_name}-fake-claude.log"));
    let fake_claude = fake_bin.join("claude");
    std::fs::write(
        &fake_claude,
        format!(
            "#!/bin/bash\nprintf '%s\\n' \"$*\" >> '{}'\nsleep 5\n",
            fake_log.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    (fake_bin, fake_log)
}
