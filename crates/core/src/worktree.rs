use crate::git;
use crate::id::RiftId;
use crate::{Error, Result};
use std::fs;
use std::path::Component;
use std::path::{Path, PathBuf};
use std::process::Command;

const SHARED_GIT_DIR_NAME: &str = "rift-manager";

pub(crate) struct SharedGitRoot {
    pub(crate) git_dir: PathBuf,
}

pub(crate) struct RegisteredWorktree {
    pub(crate) git_dir: PathBuf,
}

pub(crate) fn shared_git_dir_for(repo_storage: &Path, root_id: &RiftId) -> PathBuf {
    repo_storage.join(root_id.as_str()).join(SHARED_GIT_DIR_NAME)
}

pub(crate) fn initialize_root(
    path: &Path,
    root_id: &RiftId,
    repo_storage: &Path,
    copy_directory: impl FnOnce(&Path, &Path) -> Result<()>,
) -> Result<SharedGitRoot> {
    let inline_git = path.join(".git");
    if !real_directory(&inline_git)? {
        return Err(Error::UnsafeGit(
            "rift init --worktrees requires a repository with an inline .git directory".into(),
        ));
    }
    ensure_toplevel(path)?;

    let final_git = shared_git_dir_for(repo_storage, root_id);
    let final_parent = final_git
        .parent()
        .ok_or_else(|| Error::Path(format!("Git dir has no parent: {}", final_git.display())))?;
    let final_parent = final_parent.to_path_buf();
    let staging_parent = final_parent.with_extension("tmp");
    let staging_git = staging_parent.join(SHARED_GIT_DIR_NAME);
    if final_parent.exists() || staging_parent.exists() {
        return Err(Error::AlreadyExists(final_parent.to_path_buf()));
    }
    let repos = final_parent.parent().ok_or_else(|| {
        Error::Path(format!(
            "repository storage has no parent: {}",
            final_parent.display()
        ))
    })?;
    let result = (|| {
        fs::create_dir_all(repos)?;
        fs::create_dir_all(&staging_parent)?;
        copy_directory(&inline_git, &staging_git)?;
        configure_snapshot_root(&staging_git, path)?;
        validate_snapshot_root(&staging_git)?;
        fs::rename(&staging_parent, &final_parent)?;
        configure_snapshot_root(&final_git, path)?;
        validate_snapshot_root(&final_git)?;
        Ok(SharedGitRoot {
            git_dir: final_git.clone(),
        })
    })();

    if result.is_err() {
        rollback_initialize_root(&staging_parent, &final_parent);
    }
    result
}

pub(crate) fn ensure_head_commit(path: &Path) -> Result<()> {
    match head_state(path)? {
        HeadState::Commit(_) => Ok(()),
        HeadState::Symbolic(_) => Err(Error::UnsafeGit(
            "rift init --worktrees requires a repository with at least one commit".into(),
        )),
    }
}

pub(crate) fn register_cow_clone(
    root_git_dir: &Path,
    source: &Path,
    destination: &Path,
    id: &RiftId,
) -> Result<RegisteredWorktree> {
    let head = head_state(source)?;
    import_source_head(root_git_dir, source, &head)?;
    let git_dir = root_git_dir.join("worktrees").join(id.as_str());
    if git_dir.exists() {
        return Err(Error::AlreadyExists(git_dir));
    }
    let result = (|| {
        fs::create_dir_all(&git_dir)?;
        remove_copied_git_entry(destination)?;
        write_git_pointer(&destination.join(".git"), &git_dir)?;
        fs::write(
            git_dir.join("gitdir"),
            format!("{}\n", destination.join(".git").display()),
        )?;
        fs::write(git_dir.join("commondir"), "../..\n")?;
        fs::write(git_dir.join("HEAD"), head.contents())?;
        if !copy_source_index(source, &git_dir)? {
            if let HeadState::Commit(commit) = &head {
                run_worktree(
                    destination,
                    &["reset", "--mixed", "--quiet", commit.as_str()],
                )?;
            }
        }
        validate_worktree(destination)?;
        validate_worktree_list(root_git_dir)?;
        Ok(RegisteredWorktree {
            git_dir: git_dir.clone(),
        })
    })();
    if result.is_err() {
        let _ = remove_registered_worktree(&git_dir);
    }
    result
}

pub(crate) fn remove_registered_worktree(git_dir: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(git_dir) {
        if metadata.is_dir() {
            fs::remove_dir_all(git_dir)?;
        } else {
            fs::remove_file(git_dir)?;
        }
    }
    Ok(())
}

pub(crate) fn remove_shared_git_storage(shared_git_dir: &Path, root_id: &RiftId) -> Result<()> {
    let Some(parent) = shared_git_dir.parent() else {
        return Err(Error::Path(format!(
            "shared Git dir has no parent: {}",
            shared_git_dir.display()
        )));
    };
    let unsafe_components = shared_git_dir
        .components()
        .any(|component| matches!(component, Component::ParentDir));
    if unsafe_components
        || shared_git_dir.file_name() != Some(std::ffi::OsStr::new(SHARED_GIT_DIR_NAME))
        || parent.file_name() != Some(std::ffi::OsStr::new(root_id.as_str()))
    {
        return Err(Error::UnsafeGit(format!(
            "refusing to remove shared Git storage outside Rift repository storage: {}",
            shared_git_dir.display()
        )));
    }
    if parent.exists() {
        fs::remove_dir_all(parent)?;
    }
    Ok(())
}

pub(crate) fn prune(root_git_dir: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(root_git_dir)
        .args(["worktree", "prune"])
        .output()?;
    if !output.status.success() {
        return Err(Error::UnsafeGit(format!(
            "failed to prune Git worktrees: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

pub(crate) fn validate_worktree_dir(root_git_dir: &Path, git_dir: &Path) -> Result<()> {
    let worktrees = root_git_dir.join("worktrees");
    let unsafe_components = root_git_dir
        .components()
        .chain(git_dir.components())
        .any(|component| matches!(component, Component::ParentDir));
    if unsafe_components || git_dir.parent() != Some(worktrees.as_path()) {
        return Err(Error::UnsafeGit(format!(
            "refusing to remove Git metadata outside Rift worktree storage: {}",
            git_dir.display()
        )));
    }
    Ok(())
}

fn ensure_toplevel(path: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !output.status.success() {
        return Err(Error::UnsafeGit(format!(
            "failed to resolve Git root: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let top = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim().to_owned());
    if fs::canonicalize(path)? != fs::canonicalize(top)? {
        return Err(Error::UnsafeGit(
            "rift init --worktrees must run at the repository root".into(),
        ));
    }
    Ok(())
}

fn configure_snapshot_root(git_dir: &Path, worktree: &Path) -> Result<()> {
    run_git(
        git_dir,
        &["config", "core.worktree", &worktree.to_string_lossy()],
    )?;
    run_git(git_dir, &["config", "core.bare", "false"])
}

fn validate_worktree(path: &Path) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["status", "--porcelain"])
        .output()?;
    if !output.status.success() {
        return Err(Error::UnsafeGit(format!(
            "Git worktree validation failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn validate_snapshot_root(git_dir: &Path) -> Result<()> {
    let bare = git_output(git_dir, &["rev-parse", "--is-bare-repository"])?;
    if bare != "false" {
        return Err(Error::UnsafeGit(format!(
            "Git snapshot root is unexpectedly bare: {}",
            git_dir.display()
        )));
    }
    run_git(git_dir, &["worktree", "list", "--porcelain"])
}

fn validate_worktree_list(root_git_dir: &Path) -> Result<()> {
    run_git(root_git_dir, &["worktree", "list", "--porcelain"])
}

fn import_source_head(root_git_dir: &Path, source: &Path, head: &HeadState) -> Result<()> {
    let Some(source_dirs) = git::resolve_dirs(source)? else {
        return Err(Error::UnsafeGit("source is not a Git repository".into()));
    };
    if source_dirs.common_dir == root_git_dir {
        return Ok(());
    }
    let HeadState::Commit(commit) = head else {
        return Ok(());
    };
    if snapshot_has_commit(root_git_dir, commit)? {
        return Ok(());
    }
    let source_repo = source.to_string_lossy().into_owned();
    run_git(root_git_dir, &["fetch", "--quiet", "--no-tags", &source_repo, commit])
}

fn run_worktree(worktree: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(Error::UnsafeGit(format!(
            "Git command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

fn run_git(git_dir: &Path, args: &[&str]) -> Result<()> {
    let _ = git_output(git_dir, args)?;
    Ok(())
}

fn git_output(git_dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .output()?;
    if !output.status.success() {
        return Err(Error::UnsafeGit(format!(
            "Git command failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn snapshot_has_commit(git_dir: &Path, commit: &str) -> Result<bool> {
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .output()?;
    Ok(output.status.success())
}

enum HeadState {
    Commit(String),
    Symbolic(String),
}

impl HeadState {
    fn contents(&self) -> String {
        match self {
            Self::Commit(commit) => format!("{commit}\n"),
            Self::Symbolic(contents) => contents.clone(),
        }
    }
}

fn head_state(path: &Path) -> Result<HeadState> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .output()?;
    if output.status.success() {
        return Ok(HeadState::Commit(
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        ));
    }
    let Some(dirs) = git::resolve_dirs(path)? else {
        return Err(Error::UnsafeGit("source is not a Git repository".into()));
    };
    let contents = fs::read_to_string(dirs.git_dir.join("HEAD"))?;
    if contents.starts_with("ref: ") {
        return Ok(HeadState::Symbolic(contents));
    }
    Err(Error::UnsafeGit(
        "cannot resolve Git HEAD for worktree registration".into(),
    ))
}

fn remove_copied_git_entry(destination: &Path) -> Result<()> {
    let git = destination.join(".git");
    let metadata = fs::symlink_metadata(&git)?;
    if metadata.is_dir() {
        fs::remove_dir_all(git)?;
    } else {
        fs::remove_file(git)?;
    }
    Ok(())
}

fn copy_source_index(source: &Path, git_dir: &Path) -> Result<bool> {
    let Some(source_dirs) = git::resolve_dirs(source)? else {
        return Err(Error::UnsafeGit("source is not a Git repository".into()));
    };
    let source_index = source_dirs.git_dir.join("index");
    if source_index.exists() {
        fs::copy(source_index, git_dir.join("index"))?;
        return Ok(true);
    }
    Ok(false)
}

fn write_git_pointer(path: &Path, git_dir: &Path) -> Result<()> {
    fs::write(path, format!("gitdir: {}\n", git_dir.display()))?;
    Ok(())
}

fn rollback_initialize_root(staging_parent: &Path, final_parent: &Path) {
    let _ = fs::remove_dir_all(staging_parent);
    let _ = fs::remove_dir_all(final_parent);
}

fn real_directory(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir() && !metadata.file_type().is_symlink()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}
