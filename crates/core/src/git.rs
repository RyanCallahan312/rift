use crate::{Error, Result};
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Source {
    PlainDirectory,
    Repository,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct GitDirs {
    pub(crate) git_dir: PathBuf,
    pub(crate) common_dir: PathBuf,
}

impl Source {
    pub(crate) fn is_repository(self) -> bool {
        matches!(self, Self::Repository)
    }
}

pub(crate) fn check_source(path: &Path) -> Result<Source> {
    let git = path.join(".git");
    if !git.exists() {
        return Ok(Source::PlainDirectory);
    }
    if !git.is_dir() {
        return Err(Error::UnsafeGit(
            "linked Git worktree sources are not supported".into(),
        ));
    }

    check_safe_state(path)?;
    Ok(Source::Repository)
}

pub(crate) fn check_source_allow_worktree(path: &Path) -> Result<Source> {
    if !path.join(".git").exists() {
        return Ok(Source::PlainDirectory);
    }
    resolve_dirs(path)?.ok_or_else(|| Error::UnsafeGit("invalid Git repository".into()))?;
    check_safe_state(path)?;
    Ok(Source::Repository)
}

pub(crate) fn resolve_dirs(path: &Path) -> Result<Option<GitDirs>> {
    if !path.join(".git").exists() {
        return Ok(None);
    }
    let git_dir = git_path(path, "--git-dir")?;
    let common_dir = git_path(path, "--git-common-dir")?;
    Ok(Some(GitDirs {
        git_dir,
        common_dir,
    }))
}

fn check_safe_state(path: &Path) -> Result<()> {
    let Some(dirs) = resolve_dirs(path)? else {
        return Ok(());
    };
    for state in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
        "rebase-merge",
        "rebase-apply",
        "index.lock",
        "HEAD.lock",
    ] {
        if dirs.git_dir.join(state).exists() {
            return Err(Error::UnsafeGit(format!("Git state in progress: {state}")));
        }
    }
    Ok(())
}

pub(crate) fn hide_marker(path: &Path) -> Result<()> {
    let Some(dirs) = resolve_dirs(path)? else {
        return Ok(());
    };
    let info = dirs.common_dir.join("info");
    fs::create_dir_all(&info)?;
    let exclude = info.join("exclude");
    let existing = match fs::read_to_string(&exclude) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    if existing.lines().any(|line| line.trim() == "/.rift") {
        return Ok(());
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(exclude, format!("{existing}{separator}/.rift\n"))?;
    Ok(())
}

pub(crate) fn detach_destination(path: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    // Avoid process startup when libgit2 understands the repository;
    // the Git CLI remains the authority for layouts it cannot resolve.
    if let Some(commit) = resolve_head_commit(path) {
        if let Some(dirs) = resolve_dirs(path)? {
            fs::write(dirs.git_dir.join("HEAD"), format!("{commit}\n"))?;
        }
        return Ok(());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .output()?;
    if !output.status.success() {
        return Ok(());
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if let Some(dirs) = resolve_dirs(path)? {
        fs::write(dirs.git_dir.join("HEAD"), format!("{commit}\n"))?;
    }
    Ok(())
}

fn git_path(path: &Path, flag: &str) -> Result<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--path-format=absolute", flag])
        .output()?;
    if !output.status.success() {
        return Err(Error::UnsafeGit(format!(
            "failed to resolve Git path with {flag}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if value.is_empty() {
        return Err(Error::UnsafeGit(format!(
            "Git returned an empty path for {flag}"
        )));
    }
    Ok(PathBuf::from(value))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn resolve_head_commit(path: &Path) -> Option<git2::Oid> {
    let repository = git2::Repository::open(path).ok()?;
    repository
        .head()
        .ok()?
        .peel_to_commit()
        .ok()
        .map(|commit| commit.id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn linked_worktree_marker_is_rejected() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(".git"), "gitdir: elsewhere").unwrap();

        assert!(matches!(
            check_source(temp.path()),
            Err(Error::UnsafeGit(_))
        ));
    }

    #[test]
    fn check_source_distinguishes_plain_and_git_directories() {
        let plain = TempDir::new().unwrap();
        assert_eq!(check_source(plain.path()).unwrap(), Source::PlainDirectory);

        let git = TempDir::new().unwrap();
        run(git.path(), &["init"]);
        assert_eq!(check_source(git.path()).unwrap(), Source::Repository);
    }

    #[test]
    fn hide_marker_creates_and_appends_exclude_cleanly() {
        let temp = TempDir::new().unwrap();
        run(temp.path(), &["init"]);

        hide_marker(temp.path()).unwrap();
        assert_marker_is_excluded(
            &fs::read_to_string(temp.path().join(".git/info/exclude")).unwrap(),
        );
        fs::write(temp.path().join(".git/info/exclude"), "existing").unwrap();
        hide_marker(temp.path()).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join(".git/info/exclude")).unwrap(),
            "existing\n/.rift\n"
        );
        hide_marker(temp.path()).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join(".git/info/exclude")).unwrap(),
            "existing\n/.rift\n"
        );
    }

    #[test]
    fn hide_marker_uses_common_dir_for_linked_worktrees() {
        let temp = TempDir::new().unwrap();
        let repo = temp.path().join("repo");
        let linked = temp.path().join("linked");
        fs::create_dir(&repo).unwrap();
        run(&repo, &["init"]);
        run(&repo, &["config", "user.email", "test@example.com"]);
        run(&repo, &["config", "user.name", "Test"]);
        fs::write(repo.join("file.txt"), "hello").unwrap();
        run(&repo, &["add", "file.txt"]);
        run(&repo, &["commit", "-m", "initial"]);
        run(
            &repo,
            &["worktree", "add", "--detach", linked.to_str().unwrap()],
        );

        hide_marker(&linked).unwrap();

        let common = resolve_dirs(&linked).unwrap().unwrap().common_dir;
        assert_marker_is_excluded(&fs::read_to_string(common.join("info/exclude")).unwrap());
    }

    #[test]
    fn detach_does_nothing_for_a_repository_without_a_commit() {
        let temp = TempDir::new().unwrap();
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(temp.path())
                .arg("init")
                .status()
                .unwrap()
                .success()
        );
        let head = fs::read_to_string(temp.path().join(".git/HEAD")).unwrap();

        detach_destination(temp.path()).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join(".git/HEAD")).unwrap(),
            head
        );
    }

    fn run(path: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .status()
                .unwrap()
                .success()
        );
    }

    fn assert_marker_is_excluded(contents: &str) {
        assert_eq!(
            contents
                .lines()
                .filter(|line| line.trim() == "/.rift")
                .count(),
            1
        );
    }
}
