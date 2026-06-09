mod config;
mod filter;
mod git;
mod hook;
mod id;
mod marker;
mod name;
mod registry;
mod strategy;
mod worktree;

#[cfg(all(test, target_os = "linux"))]
mod linux_filesystem_tests;
#[cfg(all(test, target_os = "linux"))]
mod test_support;

use id::RiftId;
use name::RiftName;
use registry::{MovedRecord, PathRecord, Record, Registry, SubtreeScope};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use strategy::{Strategy, StrategyInit};
use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Database(#[from] rusqlite::Error),
    #[error("{0}")]
    Walk(#[from] walkdir::Error),
    #[error("invalid path: {0}")]
    Path(String),
    #[error("copy-on-write cloning unavailable: {0}")]
    CowUnavailable(String),
    #[error("workspace requires initialization: {0}")]
    InitializationRequired(PathBuf),
    #[error("workspace is not initialized: {0}")]
    WorkspaceNotInitialized(PathBuf),
    #[error("rift marker is missing: {0}")]
    MissingMarker(PathBuf),
    #[error("unsupported filesystem entry: {0}")]
    UnsupportedEntry(PathBuf),
    #[error("unsafe Git source: {0}")]
    UnsafeGit(String),
    #[error("directory is not managed by rift: {0}")]
    NotManaged(PathBuf),
    #[error("rift marker does not match the registry at: {0}")]
    MarkerMismatch(PathBuf),
    #[error("rift marker belongs to an unknown registry entry at: {0}")]
    UnknownMarker(PathBuf),
    #[error("rift directory already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("cannot remove subtree while a recorded rift path is missing: {0}")]
    MissingRift(PathBuf),
    #[error("cannot copy a workspace into itself: {0}")]
    InsideSource(PathBuf),
    #[error("invalid rift config at {path}: {message}")]
    InvalidConfig { path: PathBuf, message: String },
    #[error("postcreate hook failed at {path}: `{command}` {message}")]
    HookFailed {
        path: PathBuf,
        command: String,
        message: String,
    },
}

pub struct Create {
    pub from: PathBuf,
    pub name: Option<String>,
    pub into: Option<PathBuf>,
}

impl Create {
    pub fn new(from: impl Into<PathBuf>) -> Self {
        Self {
            from: from.into(),
            name: None,
            into: None,
        }
    }

    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub fn with_name(mut self, name: Option<String>) -> Self {
        self.name = name;
        self
    }

    pub fn with_storage(mut self, into: Option<PathBuf>) -> Self {
        self.into = into;
        self
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CreateOptions {
    pub copy_mode: CopyMode,
    pub hook_mode: HookMode,
}

impl CreateOptions {
    pub fn copy_mode(mut self, copy_mode: CopyMode) -> Self {
        self.copy_mode = copy_mode;
        self
    }

    pub fn hook_mode(mut self, hook_mode: HookMode) -> Self {
        self.hook_mode = hook_mode;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CopyMode {
    Filtered,
    All,
}

impl Default for CopyMode {
    fn default() -> Self {
        Self::Filtered
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HookMode {
    Run,
    Skip,
}

impl Default for HookMode {
    fn default() -> Self {
        Self::Run
    }
}

#[derive(Clone, Debug)]
pub struct Init {
    pub at: PathBuf,
    pub worktrees: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitStorageMode {
    Inline,
    SharedWorktrees,
}

impl GitStorageMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::SharedWorktrees => "shared_worktrees",
        }
    }

    pub(crate) fn from_stored(value: String) -> Self {
        match value.as_str() {
            "shared_worktrees" => Self::SharedWorktrees,
            _ => Self::Inline,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitProgress {
    CreatingSubvolume,
    ImportingWorkspace,
    ImportedEntries { entries: u64 },
    ActivatingWorkspace,
    RemovingOriginal,
    RestoringMarker,
    RegisteringWorkspace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InitOutcome {
    Registered,
    AlreadyInitialized,
    Converted,
}

impl InitOutcome {
    pub fn is_converted(self) -> bool {
        matches!(self, Self::Converted)
    }
}

pub struct Manager {
    registry: Registry,
    strategy: Box<dyn Strategy>,
    repo_storage: PathBuf,
}

impl Manager {
    pub fn open_default() -> Result<Self> {
        let path = default_database_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Self::open(path)
    }

    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::with_strategy(path, strategy::default_strategy())
    }

    fn with_strategy(path: impl AsRef<Path>, strategy: Box<dyn Strategy>) -> Result<Self> {
        let registry = Registry::open(path)?;
        Ok(Self {
            registry,
            strategy,
            repo_storage: default_repo_storage()?,
        })
    }

    #[cfg(test)]
    fn with_strategy_and_repo_storage(
        path: impl AsRef<Path>,
        strategy: Box<dyn Strategy>,
        repo_storage: PathBuf,
    ) -> Result<Self> {
        let registry = Registry::open(path)?;
        Ok(Self {
            registry,
            strategy,
            repo_storage,
        })
    }

    pub fn create(&mut self, input: Create) -> Result<PathBuf> {
        self.create_with_options(input, CreateOptions::default())
    }

    pub fn create_with_options(
        &mut self,
        input: Create,
        options: CreateOptions,
    ) -> Result<PathBuf> {
        let requested = existing_directory(&input.from)?;
        let source = self.workspace_from(&requested)?;
        let from = source.path.clone();
        let root = self.root(&source)?;
        let git = match root.git_storage_mode {
            GitStorageMode::Inline => git::check_source(&from)?,
            GitStorageMode::SharedWorktrees => git::check_source_allow_worktree(&from)?,
        };
        let id = RiftId::new();
        let destination_parent = match input.into {
            Some(path) => absolute_path(&path)?,
            None => default_storage(&root.path)?,
        };
        let name = RiftName::from_optional(input.name)?;
        if destination_parent.join(name.as_str()).starts_with(&from) {
            return Err(Error::InsideSource(destination_parent.join(name.as_str())));
        }
        fs::create_dir_all(&destination_parent)?;
        let destination_parent = fs::canonicalize(destination_parent)?;
        let destination = destination_parent.join(name.as_str());
        if destination.starts_with(&from) {
            return Err(Error::InsideSource(destination));
        }
        if destination.exists() {
            return Err(Error::AlreadyExists(destination));
        }
        let config = match options.hook_mode {
            HookMode::Run => config::Config::load(&from)?,
            HookMode::Skip => config::Config::default(),
        };

        if let Err(error) = self
            .strategy
            .copy_directory(&from, &destination, options.copy_mode)
        {
            if destination.exists() {
                let _ = self.strategy.remove_directory(&destination);
            }
            return Err(error);
        }

        let mut git_worktree_dir = None;
        let result: Result<PathBuf> = (|| {
            match root.git_storage_mode {
                GitStorageMode::Inline => {
                    marker::write(&destination, &id)?;
                    if git.is_repository() {
                        git::hide_marker(&destination)?;
                        git::detach_destination(&destination)?;
                    }
                    if git.is_repository() {
                        git::hide_marker(&from)?;
                    }
                }
                GitStorageMode::SharedWorktrees => {
                    let shared_git_dir = root.shared_git_dir.as_ref().ok_or_else(|| {
                        Error::UnsafeGit("shared Git directory is missing from registry".into())
                    })?;
                    if git.is_repository() {
                        let registered =
                            worktree::register_cow_clone(shared_git_dir, &from, &destination, &id)?;
                        git_worktree_dir = Some(registered.git_dir);
                        git::hide_marker(&destination)?;
                        git::hide_marker(&from)?;
                    }
                    marker::write(&destination, &id)?;
                }
            }
            self.registry.insert_child(
                &id,
                &source.id,
                &destination,
                root.git_storage_mode,
                root.shared_git_dir.as_deref(),
                git_worktree_dir.as_deref(),
            )?;
            Ok(destination.clone())
        })();
        if result.is_err() {
            if let (Some(shared_git_dir), Some(git_worktree_dir)) =
                (root.shared_git_dir.as_ref(), git_worktree_dir.as_ref())
            {
                if worktree::validate_worktree_dir(shared_git_dir, git_worktree_dir).is_ok() {
                    let _ = worktree::remove_registered_worktree(git_worktree_dir);
                    let _ = worktree::prune(shared_git_dir);
                }
            }
            let _ = self.strategy.remove_directory(&destination);
        }
        result?;
        hook::run_postcreate(config.postcreate(), &from, &destination, &id, &source.id)?;
        Ok(destination)
    }

    pub fn init(&mut self, at: impl AsRef<Path>) -> Result<InitOutcome> {
        self.init_with_progress(at, |_| {})
    }

    pub fn init_options(&mut self, input: Init) -> Result<InitOutcome> {
        self.init_options_with_progress(input, |_| {})
    }

    pub fn init_with_progress(
        &mut self,
        at: impl AsRef<Path>,
        progress: impl FnMut(InitProgress),
    ) -> Result<InitOutcome> {
        self.init_options_with_progress(
            Init {
                at: at.as_ref().to_path_buf(),
                worktrees: false,
            },
            progress,
        )
    }

    pub fn init_options_with_progress(
        &mut self,
        input: Init,
        mut progress: impl FnMut(InitProgress),
    ) -> Result<InitOutcome> {
        let at = existing_directory(&input.at)?;
        if let Some(record) = self.registry.record_at(&at)? {
            if input.worktrees && record.git_storage_mode != GitStorageMode::SharedWorktrees {
                return Err(Error::UnsafeGit(
                    "workspace is already initialized without --worktrees".into(),
                ));
            }
            let git = match record.git_storage_mode {
                GitStorageMode::Inline => git::check_source(&at)?,
                GitStorageMode::SharedWorktrees => git::check_source_allow_worktree(&at)?,
            };
            if marker::read(&at)?.is_none() {
                progress(InitProgress::RestoringMarker);
                marker::write(&at, &record.id)?;
            } else {
                marker::verify(&record.path, &record.id)?;
            }
            let converted = self.strategy.initialize_directory(&at, &mut progress)?;
            if git.is_repository() {
                git::hide_marker(&at)?;
            }
            return Ok(match converted {
                StrategyInit::AlreadyNative => InitOutcome::AlreadyInitialized,
                StrategyInit::Converted => InitOutcome::Converted,
            });
        }
        let git = if input.worktrees {
            git::check_source_allow_worktree(&at)?
        } else {
            git::check_source(&at)?
        };
        if marker::read(&at)?.is_some() {
            return Err(Error::MarkerMismatch(at));
        }
        if input.worktrees && !git.is_repository() {
            return Err(Error::UnsafeGit(
                "rift init --worktrees requires a Git repository".into(),
            ));
        }
        if input.worktrees {
            worktree::validate_init_root(&at)?;
        }

        let converted = self.strategy.initialize_directory(&at, &mut progress)?;
        progress(InitProgress::RegisteringWorkspace);
        let id = RiftId::new();
        let shared_git = if input.worktrees {
            Some(worktree::initialize_root(&at, &id, &self.repo_storage)?.git_dir)
        } else {
            None
        };
        let git_storage_mode = if input.worktrees {
            GitStorageMode::SharedWorktrees
        } else {
            GitStorageMode::Inline
        };
        let result = (|| {
            marker::write(&at, &id)?;
            if git.is_repository() {
                git::hide_marker(&at)?;
            }
            self.registry
                .insert_root(&id, &at, git_storage_mode, shared_git.as_deref())?;
            Ok(match converted {
                StrategyInit::AlreadyNative => InitOutcome::Registered,
                StrategyInit::Converted => InitOutcome::Converted,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(marker::path(&at));
            if let Some(shared_git) = &shared_git {
                let _ = worktree::remove_shared_git_storage(shared_git, &id);
            }
        }
        result
    }

    pub fn remove(&mut self, at: impl AsRef<Path>) -> Result<()> {
        let record = self.workspace_at(at)?;
        if record.parent_id.is_none() {
            return self.unregister_root(&record);
        }
        marker::verify(&record.path, &record.id)?;
        let rows = self
            .registry
            .subtree(&record.id, SubtreeScope::IncludingRoot)?;
        self.trash_rows(&rows)?;
        Ok(())
    }

    pub fn remove_all(&mut self, at: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let record = self.workspace_at(at)?;
        marker::verify(&record.path, &record.id)?;
        let rows = self
            .registry
            .subtree(&record.id, SubtreeScope::DescendantsOnly)?;
        self.trash_rows(&rows)?;
        Ok(rows.into_iter().map(|record| record.path).collect())
    }

    fn unregister_root(&mut self, record: &Record) -> Result<()> {
        marker::verify(&record.path, &record.id)?;
        let rows = self
            .registry
            .subtree(&record.id, SubtreeScope::DescendantsOnly)?;
        let existing = rows
            .iter()
            .filter(|record| record.path.exists())
            .cloned()
            .collect::<Vec<_>>();
        let missing = rows
            .iter()
            .filter(|record| !record.path.exists())
            .cloned()
            .collect::<Vec<_>>();
        self.trash_rows(&existing)?;
        cleanup_worktrees(&missing)?;
        fs::remove_file(marker::path(&record.path))?;
        self.registry.delete_active(&record.id)?;
        if let Some(shared_git_dir) = &record.shared_git_dir {
            worktree::remove_shared_git_storage(shared_git_dir, &record.id)?;
        }
        Ok(())
    }

    fn trash_rows(&mut self, rows: &[PathRecord]) -> Result<()> {
        rows.iter().try_for_each(|row| -> Result<()> {
            row.path
                .exists()
                .then_some(())
                .ok_or_else(|| Error::MissingRift(row.path.clone()))?;
            marker::verify(&row.path, &row.id)?;
            Ok(())
        })?;
        let targets = rows
            .iter()
            .map(|row| {
                Ok(MovedRecord {
                    id: row.id.clone(),
                    original_path: row.path.clone(),
                    trash_path: trash_path(&row.id, &row.path)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        targets.iter().try_for_each(|target| {
            (!target.trash_path.exists())
                .then_some(())
                .ok_or_else(|| Error::AlreadyExists(target.trash_path.clone()))
        })?;
        let mut moved: Vec<MovedRecord> = Vec::with_capacity(rows.len());
        for target in targets {
            let trash_parent = target.trash_path.parent().ok_or_else(|| {
                Error::Path(format!(
                    "trash path has no parent: {}",
                    target.trash_path.display()
                ))
            })?;
            fs::create_dir_all(trash_parent)?;
            if let Err(error) = fs::rename(&target.original_path, &target.trash_path) {
                for record in moved.iter().rev() {
                    let _ = fs::rename(&record.trash_path, &record.original_path);
                }
                return Err(error.into());
            }
            moved.push(target);
        }
        let result = self.registry.trash_moved(&moved);
        if result.is_err() {
            for record in moved.iter().rev() {
                let _ = fs::rename(&record.trash_path, &record.original_path);
            }
            return result;
        }
        cleanup_worktrees(rows)
    }

    pub fn list(&self, of: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let record = self.workspace_at(of)?;
        self.registry.child_paths(&record.id)
    }

    pub fn ancestors(&self, of: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
        let record = self.workspace_at(of)?;
        let mut paths = Vec::new();
        let mut parent_id = record.parent_id;
        while let Some(id) = parent_id {
            let parent = self
                .registry
                .record_id(&id)?
                .ok_or_else(|| Error::NotManaged(record.path.clone()))?;
            paths.push(parent.path);
            parent_id = parent.parent_id;
        }
        Ok(paths)
    }

    pub fn gc(&mut self) -> Result<Vec<PathBuf>> {
        let trashed = self.registry.trashed_paths()?;
        cleanup_worktrees(&trashed)?;
        let removed = trashed
            .into_iter()
            .map(|row| -> Result<PathBuf> {
                if row.path.exists() {
                    self.strategy.remove_directory(&row.path)?;
                }
                self.registry.delete_trash(&row.id)?;
                Ok(row.path)
            })
            .collect::<Result<Vec<_>>>()?;

        let missing = self
            .registry
            .active_paths()?
            .into_iter()
            .filter(|row| !row.path.exists())
            .map(|row| {
                self.registry
                    .subtree(&row.id, SubtreeScope::DescendantsOnly)
                    .map(|descendants| {
                        (!descendants
                            .iter()
                            .any(|descendant| descendant.path.exists()))
                        .then_some(row)
                    })
            })
            .filter_map(Result::transpose)
            .collect::<Result<Vec<_>>>()?;
        cleanup_worktrees(&missing)?;
        self.registry.delete_active_records(&missing)?;
        Ok(removed
            .into_iter()
            .chain(missing.into_iter().map(|record| record.path))
            .collect())
    }

    pub fn workspace(&self, at: impl AsRef<Path>) -> Result<PathBuf> {
        Ok(self.workspace_at(at)?.path)
    }

    fn workspace_at(&self, path: impl AsRef<Path>) -> Result<Record> {
        let path = existing_directory(path.as_ref())?;
        self.workspace_from(&path)
    }

    fn workspace_from(&self, path: &Path) -> Result<Record> {
        self.workspace_from_optional(path)?
            .ok_or_else(|| Error::WorkspaceNotInitialized(path.to_path_buf()))
    }

    fn workspace_from_optional(&self, path: &Path) -> Result<Option<Record>> {
        for directory in path.ancestors() {
            if let Some(id) = marker::read(directory)? {
                let record = self
                    .registry
                    .record_id(&id)?
                    .ok_or_else(|| Error::UnknownMarker(directory.to_path_buf()))?;
                if record.path != directory {
                    return Err(Error::MarkerMismatch(directory.to_path_buf()));
                }
                return Ok(Some(record));
            }
            if self.registry.record_at(directory)?.is_some() {
                return Err(Error::MissingMarker(directory.to_path_buf()));
            }
        }
        Ok(None)
    }

    fn root(&self, record: &Record) -> Result<Record> {
        let mut current = record.clone();
        while let Some(id) = current.parent_id.clone() {
            current = self
                .registry
                .record_id(&id)?
                .ok_or_else(|| Error::NotManaged(record.path.clone()))?;
        }
        Ok(current)
    }
}

fn default_database_path() -> Result<PathBuf> {
    let base = dirs::data_local_dir()
        .ok_or_else(|| Error::Path("user data directory is unavailable".into()))?;
    Ok(base.join("rift").join("rift.sqlite"))
}

fn default_repo_storage() -> Result<PathBuf> {
    let base = dirs::data_local_dir()
        .ok_or_else(|| Error::Path("user data directory is unavailable".into()))?;
    Ok(base.join("rift").join("repos"))
}

fn existing_directory(path: &Path) -> Result<PathBuf> {
    let path = fs::canonicalize(path)?;
    if !path.is_dir() {
        return Err(Error::Path(format!("not a directory: {}", path.display())));
    }
    Ok(path)
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(std::env::current_dir()?.join(path))
}

fn default_storage(root: &Path) -> Result<PathBuf> {
    let parent = root
        .parent()
        .ok_or_else(|| Error::Path(format!("workspace has no parent: {}", root.display())))?;
    let name = root
        .file_name()
        .ok_or_else(|| Error::Path(format!("workspace has no name: {}", root.display())))?;
    Ok(parent.join(".rifts").join(name))
}

fn trash_path(id: &RiftId, path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::Path(format!("rift has no parent: {}", path.display())))?;
    let name = path
        .file_name()
        .ok_or_else(|| Error::Path(format!("rift has no name: {}", path.display())))?;
    Ok(parent
        .join(".trash")
        .join(format!("{id}-{}", name.to_string_lossy())))
}

fn cleanup_worktrees(rows: &[PathRecord]) -> Result<()> {
    let mut pruned = HashSet::<PathBuf>::new();
    for row in rows {
        let Some(shared_git_dir) = &row.shared_git_dir else {
            continue;
        };

        if let Some(git_dir) = &row.git_worktree_dir {
            worktree::validate_worktree_dir(shared_git_dir, git_dir)?;
            worktree::remove_registered_worktree(git_dir)?;
            if pruned.insert(shared_git_dir.clone()) && shared_git_dir.exists() {
                worktree::prune(shared_git_dir)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::{FailureStrategy, Strategy, TestStrategy};
    use std::cell::Cell;
    use std::process::Command;
    use std::rc::Rc;
    use tempfile::TempDir;
    use ulid::Ulid;

    fn manager(temp: &TempDir) -> Manager {
        Manager::with_strategy_and_repo_storage(
            temp.path().join("registry.sqlite"),
            Box::new(TestStrategy),
            temp.path().join("repos"),
        )
        .unwrap()
    }

    fn source(temp: &TempDir) -> PathBuf {
        let source = temp.path().join("app");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file.txt"), "hello").unwrap();
        fs::canonicalize(source).unwrap()
    }

    fn marker_id(path: &Path) -> RiftId {
        marker::read(path).unwrap().unwrap()
    }

    #[test]
    fn create_tracks_parentage_and_default_storage() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();

        let parent = source.parent().unwrap();
        assert_eq!(first, parent.join(".rifts/app/first"));
        assert_eq!(second, parent.join(".rifts/app/second"));
        assert_ne!(
            fs::read_to_string(source.join(".rift")).unwrap(),
            fs::read_to_string(first.join(".rift")).unwrap()
        );
        assert_eq!(manager.list(&source).unwrap(), vec![first.clone()]);
        assert_eq!(manager.ancestors(&second).unwrap(), vec![first, source]);
    }

    #[test]
    fn init_registers_a_root_workspace_without_creating_a_child() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);

        assert_eq!(manager.init(&source).unwrap(), InitOutcome::Registered);
        assert!(source.join(".rift").exists());
        assert!(manager.list(&source).unwrap().is_empty());
        assert_eq!(
            manager.init(&source).unwrap(),
            InitOutcome::AlreadyInitialized
        );
    }

    #[test]
    fn init_reports_structured_registration_progress_when_requested() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        let mut progress = Vec::new();

        manager
            .init_with_progress(&source, |event| progress.push(event))
            .unwrap();

        assert_eq!(progress, vec![InitProgress::RegisteringWorkspace]);
    }

    #[test]
    fn create_supports_custom_storage_and_rejects_invalid_destinations() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let custom = temp.path().join("custom");
        let child = manager
            .create(Create {
                from: source.clone(),
                name: Some("custom".into()),
                into: Some(custom.clone()),
            })
            .unwrap();
        assert_eq!(child, fs::canonicalize(&custom).unwrap().join("custom"));
        assert!(matches!(
            manager.create(Create {
                from: source.clone(),
                name: Some("custom".into()),
                into: Some(custom),
            }),
            Err(Error::AlreadyExists(_))
        ));
        assert!(matches!(
            manager.create(Create {
                from: source.clone(),
                name: Some("..".into()),
                into: None,
            }),
            Err(Error::Path(_))
        ));
        assert!(matches!(
            manager.create(Create {
                from: source.clone(),
                name: Some("inside".into()),
                into: Some(source.join("nested")),
            }),
            Err(Error::InsideSource(_))
        ));
        assert!(matches!(
            manager.create(Create {
                from: source.join("file.txt"),
                name: Some("file".into()),
                into: None,
            }),
            Err(Error::Path(_))
        ));
    }

    #[test]
    fn corrupt_and_unknown_markers_are_rejected() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let nested = source.join("nested");
        fs::create_dir(&nested).unwrap();
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        fs::write(source.join(".rift"), "unknown\n").unwrap();
        assert!(matches!(
            manager.list(&nested),
            Err(Error::UnknownMarker(_))
        ));

        let id = manager.registry.record_at(&source).unwrap().unwrap().id;
        fs::write(source.join(".rift"), format!("{id}\n")).unwrap();
        let other = temp.path().join("other");
        fs::create_dir(&other).unwrap();
        fs::write(other.join(".rift"), format!("{id}\n")).unwrap();
        assert!(matches!(
            manager.list(&other),
            Err(Error::MarkerMismatch(_))
        ));
    }

    #[test]
    fn removal_rejects_marker_mismatch_and_existing_trash_target() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let child = manager
            .create(Create {
                from: source.clone(),
                name: Some("child".into()),
                into: None,
            })
            .unwrap();
        let id = marker_id(&child);
        fs::write(child.join(".rift"), "wrong\n").unwrap();
        assert!(matches!(
            manager.remove(&child),
            Err(Error::UnknownMarker(_))
        ));
        fs::write(
            child.join(".rift"),
            fs::read_to_string(source.join(".rift")).unwrap(),
        )
        .unwrap();
        assert!(matches!(
            manager.remove(&child),
            Err(Error::MarkerMismatch(_))
        ));
        fs::write(child.join(".rift"), format!("{id}\n")).unwrap();
        let trash = trash_path(&id, &child).unwrap();
        fs::create_dir_all(&trash).unwrap();
        assert!(matches!(
            manager.remove(&child),
            Err(Error::AlreadyExists(_))
        ));
    }

    #[test]
    fn gc_forgets_a_trashed_path_already_removed_on_disk() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let child = manager
            .create(Create {
                from: source,
                name: Some("child".into()),
                into: None,
            })
            .unwrap();
        let id = marker_id(&child);
        let trash = trash_path(&id, &child).unwrap();
        manager.remove(&child).unwrap();
        fs::remove_dir_all(&trash).unwrap();

        assert_eq!(manager.gc().unwrap(), vec![trash]);
    }

    #[test]
    fn operations_use_the_nearest_ancestor_marker() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let nested = source.join("packages/app");
        fs::create_dir_all(&nested).unwrap();
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();

        let child = manager
            .create(Create {
                from: nested,
                name: Some("nested".into()),
                into: None,
            })
            .unwrap();
        fs::create_dir(child.join("deep")).unwrap();

        assert_eq!(
            manager.list(source.join("packages")).unwrap(),
            vec![child.clone()]
        );
        assert_eq!(manager.ancestors(child.join("deep")).unwrap(), vec![source]);
    }

    #[test]
    fn operations_without_a_marker_explain_how_to_initialize() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let nested = source.join("nested");
        fs::create_dir(&nested).unwrap();
        let manager = manager(&temp);

        assert!(matches!(
            manager.list(&nested),
            Err(Error::WorkspaceNotInitialized(path)) if path == nested
        ));
    }

    #[test]
    fn init_restores_a_deleted_marker_for_an_existing_workspace() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let nested = source.join("nested");
        fs::create_dir(&nested).unwrap();
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let id = fs::read_to_string(source.join(".rift")).unwrap();
        fs::remove_file(source.join(".rift")).unwrap();

        assert!(matches!(
            manager.list(&nested),
            Err(Error::MissingMarker(path)) if path == source
        ));

        manager.init(&source).unwrap();
        assert_eq!(fs::read_to_string(source.join(".rift")).unwrap(), id);
        assert!(manager.list(&nested).unwrap().is_empty());
    }

    struct InitializingStrategy {
        initialized: Rc<Cell<bool>>,
    }

    impl Strategy for InitializingStrategy {
        fn copy_directory(&self, _from: &Path, _to: &Path, _mode: CopyMode) -> Result<()> {
            unreachable!()
        }

        fn initialize_directory(
            &self,
            _path: &Path,
            _progress: &mut dyn FnMut(InitProgress),
        ) -> Result<StrategyInit> {
            self.initialized.set(true);
            Ok(StrategyInit::Converted)
        }
    }

    #[test]
    fn init_continues_initialization_after_restoring_a_marker() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut registered = manager(&temp);
        registered.init(&source).unwrap();
        fs::remove_file(source.join(".rift")).unwrap();
        drop(registered);
        let initialized = Rc::new(Cell::new(false));
        let mut manager = Manager::with_strategy(
            temp.path().join("registry.sqlite"),
            Box::new(InitializingStrategy {
                initialized: initialized.clone(),
            }),
        )
        .unwrap();

        assert_eq!(manager.init(&source).unwrap(), InitOutcome::Converted);
        assert!(initialized.get());
        assert!(source.join(".rift").exists());
    }

    #[test]
    fn init_registers_exactly_the_requested_directory() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let nested = source.join("nested");
        fs::create_dir(&nested).unwrap();
        run(&source, &["init"]);
        let mut manager = manager(&temp);

        manager.init(&nested).unwrap();
        assert!(!source.join(".rift").exists());
        assert!(nested.join(".rift").exists());
    }

    #[test]
    fn create_generates_readable_names_independent_of_ulid_identity() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();

        let destination = manager
            .create(Create {
                from: source,
                name: None,
                into: None,
            })
            .unwrap();
        let name = destination.file_name().unwrap().to_str().unwrap();
        let parts = name.split('-').collect::<Vec<_>>();
        let id = fs::read_to_string(destination.join(".rift")).unwrap();
        let id = id.trim();

        assert_eq!(parts.len(), 2);
        assert!(
            parts[0]
                .chars()
                .all(|character| character.is_ascii_lowercase())
        );
        assert!(
            parts[1]
                .chars()
                .all(|character| character.is_ascii_lowercase())
        );
        assert!(Ulid::from_string(id).is_ok());
        assert_ne!(name, id);
    }

    #[test]
    fn remove_trashes_a_full_subtree_and_gc_deletes_it() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();

        let first_id = marker_id(&first);
        let first_trash = trash_path(&first_id, &first).unwrap();
        let second_id = marker_id(&second);
        let second_trash = trash_path(&second_id, &second).unwrap();

        manager.remove(&first).unwrap();

        assert!(!first.exists());
        assert!(!second.exists());
        assert!(first_trash.exists());
        assert!(second_trash.exists());
        assert!(manager.list(&source).unwrap().is_empty());
        let deleted = manager.gc().unwrap();
        assert!(deleted.contains(&second_trash));
        assert!(deleted.contains(&first_trash));
        assert_eq!(deleted.len(), 2);
        assert!(!first_trash.exists());
        assert!(!second_trash.exists());
    }

    #[test]
    fn remove_on_a_registered_root_unregisters_it_and_trashes_descendants() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();
        let first_id = marker_id(&first);
        let first_trash = trash_path(&first_id, &first).unwrap();
        let second_id = marker_id(&second);
        let second_trash = trash_path(&second_id, &second).unwrap();

        manager.remove(&source).unwrap();

        assert!(source.exists());
        assert!(!source.join(".rift").exists());
        assert!(!first.exists());
        assert!(!second.exists());
        assert!(first_trash.exists());
        assert!(second_trash.exists());
        assert!(matches!(
            manager.list(&source),
            Err(Error::WorkspaceNotInitialized(_))
        ));
        let deleted = manager.gc().unwrap();
        assert!(deleted.contains(&first_trash));
        assert!(deleted.contains(&second_trash));
    }

    #[test]
    fn remove_on_a_registered_root_tolerates_missing_descendants() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        fs::remove_dir_all(&first).unwrap();

        manager.remove(&source).unwrap();

        assert!(source.exists());
        assert!(!source.join(".rift").exists());
        assert!(matches!(
            manager.list(&source),
            Err(Error::WorkspaceNotInitialized(_))
        ));
    }

    #[test]
    fn remove_all_deletes_descendants_and_preserves_the_selected_workspace() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();
        let sibling = manager
            .create(Create {
                from: source.clone(),
                name: Some("sibling".into()),
                into: None,
            })
            .unwrap();
        let first_id = marker_id(&first);
        let first_trash = trash_path(&first_id, &first).unwrap();

        let removed = manager.remove_all(&source).unwrap();
        assert_eq!(removed[0], second);
        assert!(removed.contains(&first));
        assert!(removed.contains(&sibling));
        assert_eq!(removed.len(), 3);
        assert!(source.exists());
        assert!(!first.exists());
        assert!(first_trash.exists());
        assert!(manager.list(&source).unwrap().is_empty());
    }

    #[test]
    fn remove_all_preserves_a_nested_selected_rift() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let first = manager
            .create(Create {
                from: source,
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();

        assert_eq!(manager.remove_all(&first).unwrap(), vec![second]);
        assert!(first.exists());
        assert!(manager.list(&first).unwrap().is_empty());
    }

    #[test]
    fn remove_refuses_a_subtree_with_an_unlinked_move() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();
        fs::rename(&second, temp.path().join("moved")).unwrap();

        assert!(matches!(manager.remove(&first), Err(Error::MissingRift(_))));
        assert!(first.exists());
    }

    #[test]
    fn gc_removes_trashed_entries() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();
        let first_id = marker_id(&first);
        let second_id = marker_id(&second);
        let first_trash = trash_path(&first_id, &first).unwrap();
        let second_trash = trash_path(&second_id, &second).unwrap();
        manager.remove(&first).unwrap();

        let deleted = manager.gc().unwrap();
        assert!(deleted.contains(&second_trash));
        assert!(deleted.contains(&first_trash));
        assert_eq!(deleted.len(), 2);
        assert!(manager.list(&source).unwrap().is_empty());
    }

    #[test]
    fn gc_has_no_effect_on_active_rifts() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();
        assert!(manager.gc().unwrap().is_empty());
        assert_eq!(manager.ancestors(&second).unwrap(), vec![first, source]);
    }

    #[test]
    fn gc_prunes_active_entries_deleted_outside_rift() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();
        fs::remove_dir_all(&first).unwrap();
        fs::remove_dir_all(&second).unwrap();

        let removed = manager.gc().unwrap();
        assert!(removed.contains(&first));
        assert!(removed.contains(&second));
        assert!(manager.list(&source).unwrap().is_empty());
    }

    #[test]
    fn gc_preserves_missing_active_parent_with_an_existing_descendant() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: first.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();
        fs::remove_dir_all(&first).unwrap();

        assert!(manager.gc().unwrap().is_empty());
        assert_eq!(manager.list(&source).unwrap(), vec![first]);
        assert_eq!(
            manager.ancestors(&second).unwrap(),
            vec![source.parent().unwrap().join(".rifts/app/first"), source]
        );
    }

    #[test]
    fn git_copy_detaches_head_and_preserves_dirty_state() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        run(&source, &["init"]);
        run(&source, &["config", "user.email", "test@example.com"]);
        run(&source, &["config", "user.name", "Test"]);
        run(&source, &["add", "file.txt"]);
        run(&source, &["commit", "-m", "initial"]);
        fs::write(source.join("file.txt"), "changed").unwrap();
        run(&source, &["add", "file.txt"]);
        fs::write(source.join("untracked.txt"), "new").unwrap();
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();

        let destination = manager
            .create(Create {
                from: source.clone(),
                name: Some("git".into()),
                into: None,
            })
            .unwrap();

        let source_commit = Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["rev-parse", "--verify", "HEAD^{commit}"])
            .output()
            .unwrap();
        assert!(
            !Command::new("git")
                .arg("-C")
                .arg(&destination)
                .args(["symbolic-ref", "-q", "HEAD"])
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(
            fs::read_to_string(destination.join(".git/HEAD")).unwrap(),
            format!(
                "{}\n",
                String::from_utf8_lossy(&source_commit.stdout).trim()
            )
        );
        let staged = Command::new("git")
            .arg("-C")
            .arg(&destination)
            .args(["diff", "--cached", "--name-only"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&staged.stdout).contains("file.txt"));
        assert!(destination.join("untracked.txt").exists());
        let status = Command::new("git")
            .arg("-C")
            .arg(&destination)
            .args(["status", "--porcelain", "--", ".rift"])
            .output()
            .unwrap();
        assert!(status.stdout.is_empty());
    }

    #[test]
    fn git_copy_peels_symbolic_tag_heads_to_commits() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        run(&source, &["init"]);
        run(&source, &["config", "user.email", "test@example.com"]);
        run(&source, &["config", "user.name", "Test"]);
        run(&source, &["add", "file.txt"]);
        run(&source, &["commit", "-m", "initial"]);
        run(&source, &["tag", "-a", "release", "-m", "release"]);
        run(&source, &["symbolic-ref", "HEAD", "refs/tags/release"]);
        let expected = Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["rev-parse", "--verify", "HEAD^{commit}"])
            .output()
            .unwrap();
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();

        let destination = manager
            .create(Create {
                from: source,
                name: Some("tag-head".into()),
                into: None,
            })
            .unwrap();

        assert_eq!(
            fs::read_to_string(destination.join(".git/HEAD")).unwrap(),
            format!("{}\n", String::from_utf8_lossy(&expected.stdout).trim())
        );
    }

    #[test]
    fn init_worktrees_leaves_the_root_git_directory_untouched() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        initialize_git_repo(&source);
        let mut manager = manager(&temp);

        manager
            .init_options(Init {
                at: source.clone(),
                worktrees: true,
            })
            .unwrap();

        let record = manager.registry.record_at(&source).unwrap().unwrap();
        assert_eq!(record.git_storage_mode, GitStorageMode::SharedWorktrees);
        let shared_git_dir = record.shared_git_dir.unwrap();
        assert!(shared_git_dir.is_dir());
        assert!(shared_git_dir.starts_with(temp.path().join("repos")));
        assert_eq!(shared_git_dir.file_name().unwrap(), "rift-manager");
        assert!(source.join(".git").is_dir());
        assert!(shared_git_dir.join("HEAD").exists());
        let bare = Command::new("git")
            .arg("--git-dir")
            .arg(&shared_git_dir)
            .args(["rev-parse", "--is-bare-repository"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&bare.stdout).trim(), "false");
        let configured_worktree = Command::new("git")
            .arg("--git-dir")
            .arg(&shared_git_dir)
            .args(["config", "--get", "core.worktree"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&configured_worktree.stdout).trim(),
            source.to_string_lossy()
        );
        let status = Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(status.status.success());
        let _ = fs::remove_dir_all(shared_git_dir.parent().unwrap());
    }

    #[test]
    fn init_worktrees_is_idempotent_without_flag() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        initialize_git_repo(&source);
        let mut manager = manager(&temp);
        manager
            .init_options(Init {
                at: source.clone(),
                worktrees: true,
            })
            .unwrap();

        assert_eq!(
            manager.init(&source).unwrap(),
            InitOutcome::AlreadyInitialized
        );
    }

    #[test]
    fn init_worktrees_rejects_unborn_repository_before_snapshotting_git() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        run(&source, &["init"]);
        let mut manager = manager(&temp);

        let error = manager
            .init_options(Init {
                at: source.clone(),
                worktrees: true,
            })
            .unwrap_err();

        assert!(matches!(error, Error::UnsafeGit(_)));
        assert!(source.join(".git").is_dir());
        assert!(manager.registry.record_at(&source).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn init_worktrees_rejects_git_symlink() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        initialize_git_repo(&source);
        fs::rename(source.join(".git"), source.join("actual.git")).unwrap();
        std::os::unix::fs::symlink(source.join("actual.git"), source.join(".git")).unwrap();
        let mut manager = manager(&temp);

        let error = manager
            .init_options(Init {
                at: source.clone(),
                worktrees: true,
            })
            .unwrap_err();

        assert!(matches!(error, Error::UnsafeGit(_)));
        assert!(source.join(".git").is_symlink());
        assert!(source.join("actual.git").is_dir());
    }

    #[test]
    fn create_from_worktree_root_registers_child_as_git_worktree() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        initialize_git_repo(&source);
        fs::write(source.join("file.txt"), "changed").unwrap();
        run(&source, &["add", "file.txt"]);
        fs::write(source.join("untracked.txt"), "new").unwrap();
        let mut manager = manager(&temp);
        manager
            .init_options(Init {
                at: source.clone(),
                worktrees: true,
            })
            .unwrap();

        let destination = manager
            .create(Create {
                from: source.clone(),
                name: Some("worktree".into()),
                into: None,
            })
            .unwrap();

        assert!(destination.join(".git").is_file());
        assert!(destination.join("untracked.txt").exists());
        let staged = Command::new("git")
            .arg("-C")
            .arg(&destination)
            .args(["diff", "--cached", "--name-only"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&staged.stdout).contains("file.txt"));
        assert!(
            !Command::new("git")
                .arg("-C")
                .arg(&destination)
                .args(["symbolic-ref", "-q", "HEAD"])
                .status()
                .unwrap()
                .success()
        );

        let root = manager
            .root(&manager.workspace_at(&source).unwrap())
            .unwrap();
        let shared_git_dir = root.shared_git_dir.unwrap();
        assert_eq!(shared_git_dir.file_name().unwrap(), "rift-manager");
        assert!(source.join(".git").is_dir());
        let worktrees = Command::new("git")
            .arg("--git-dir")
            .arg(&shared_git_dir)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        let worktrees = String::from_utf8_lossy(&worktrees.stdout);
        assert!(worktrees.contains(destination.to_str().unwrap()));

        manager.remove(&destination).unwrap();
        let worktrees = Command::new("git")
            .arg("--git-dir")
            .arg(&shared_git_dir)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&worktrees.stdout).contains(destination.to_str().unwrap())
        );
        let _ = fs::remove_dir_all(shared_git_dir.parent().unwrap());
    }

    #[test]
    fn create_from_worktree_root_assigns_distinct_git_dirs_per_rift() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        initialize_git_repo(&source);
        let mut manager = manager(&temp);
        manager
            .init_options(Init {
                at: source.clone(),
                worktrees: true,
            })
            .unwrap();

        let first = manager
            .create(Create {
                from: source.clone(),
                name: Some("first".into()),
                into: None,
            })
            .unwrap();
        let second = manager
            .create(Create {
                from: source.clone(),
                name: Some("second".into()),
                into: None,
            })
            .unwrap();

        let first_record = manager.workspace_at(&first).unwrap();
        let second_record = manager.workspace_at(&second).unwrap();
        let root = manager.root(&first_record).unwrap();
        let shared_git_dir = root.shared_git_dir.unwrap();
        let first_git_dir = manager
            .registry
            .subtree(&root.id, SubtreeScope::DescendantsOnly)
            .unwrap()
            .into_iter()
            .find(|record| record.id == first_record.id)
            .unwrap()
            .git_worktree_dir
            .unwrap();
        let second_git_dir = manager
            .registry
            .subtree(&root.id, SubtreeScope::DescendantsOnly)
            .unwrap()
            .into_iter()
            .find(|record| record.id == second_record.id)
            .unwrap()
            .git_worktree_dir
            .unwrap();

        assert_ne!(first_git_dir, second_git_dir);
        assert_eq!(
            fs::read_to_string(first.join(".git")).unwrap(),
            format!("gitdir: {}\n", first_git_dir.display())
        );
        assert_eq!(
            fs::read_to_string(second.join(".git")).unwrap(),
            format!("gitdir: {}\n", second_git_dir.display())
        );
        let worktrees = Command::new("git")
            .arg("--git-dir")
            .arg(&shared_git_dir)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        let worktrees = String::from_utf8_lossy(&worktrees.stdout);
        assert!(worktrees.contains(first.to_str().unwrap()));
        assert!(worktrees.contains(second.to_str().unwrap()));

        manager.remove(&first).unwrap();
        let worktrees = Command::new("git")
            .arg("--git-dir")
            .arg(&shared_git_dir)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        let worktrees = String::from_utf8_lossy(&worktrees.stdout);
        assert!(!worktrees.contains(first.to_str().unwrap()));
        assert!(worktrees.contains(second.to_str().unwrap()));
        assert!(second_git_dir.exists());
        let _ = fs::remove_dir_all(shared_git_dir.parent().unwrap());
    }

    #[test]
    fn create_from_worktree_root_imports_new_root_commits_into_snapshot() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        initialize_git_repo(&source);
        let mut manager = manager(&temp);
        manager
            .init_options(Init {
                at: source.clone(),
                worktrees: true,
            })
            .unwrap();

        fs::write(source.join("later.txt"), "later").unwrap();
        run(&source, &["add", "later.txt"]);
        run(&source, &["commit", "-m", "later"]);
        let expected = Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();

        let destination = manager
            .create(Create {
                from: source.clone(),
                name: Some("later-worktree".into()),
                into: None,
            })
            .unwrap();

        let head = Command::new("git")
            .arg("-C")
            .arg(&destination)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&head.stdout),
            String::from_utf8_lossy(&expected.stdout)
        );
        let branch = Command::new("git")
            .arg("-C")
            .arg(&destination)
            .args(["branch", "--show-current"])
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&branch.stdout).trim().is_empty());
    }

    #[test]
    fn remove_worktree_root_leaves_inline_git_directory_intact() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        initialize_git_repo(&source);
        let mut manager = manager(&temp);
        manager
            .init_options(Init {
                at: source.clone(),
                worktrees: true,
            })
            .unwrap();
        assert!(source.join(".git").is_dir());
        let shared_git_dir = manager
            .registry
            .record_at(&source)
            .unwrap()
            .unwrap()
            .shared_git_dir
            .unwrap();
        assert_eq!(shared_git_dir.file_name().unwrap(), "rift-manager");

        manager.remove(&source).unwrap();

        assert!(source.join(".git").is_dir());
        assert!(!shared_git_dir.parent().unwrap().exists());
        let status = Command::new("git")
            .arg("-C")
            .arg(&source)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(String::from_utf8_lossy(&status.stdout).is_empty());
    }

    #[test]
    fn remove_worktree_root_cleans_missing_child_git_metadata() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        initialize_git_repo(&source);
        let mut manager = manager(&temp);
        manager
            .init_options(Init {
                at: source.clone(),
                worktrees: true,
            })
            .unwrap();
        let child = manager
            .create(Create {
                from: source.clone(),
                name: Some("missing".into()),
                into: None,
            })
            .unwrap();
        let child_id = marker_id(&child);
        let shared_git_dir = manager
            .registry
            .record_at(&source)
            .unwrap()
            .unwrap()
            .shared_git_dir
            .unwrap();
        fs::remove_dir_all(&child).unwrap();

        manager.remove(&source).unwrap();

        assert!(source.join(".git").is_dir());
        assert!(
            !shared_git_dir.join("worktrees").join(child_id.as_str()).exists()
        );
    }

    #[test]
    fn create_requires_an_initialized_workspace() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = manager(&temp);

        assert!(matches!(
            manager.create(Create {
                from: source.clone(),
                name: Some("unsafe".into()),
                into: None,
            }),
            Err(Error::WorkspaceNotInitialized(_))
        ));
        assert!(!source.join(".rift").exists());
    }

    #[test]
    fn unsafe_git_source_is_rejected_after_initialization() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        run(&source, &["init"]);
        let mut manager = manager(&temp);
        manager.init(&source).unwrap();
        fs::write(source.join(".git/MERGE_HEAD"), "commit").unwrap();

        assert!(matches!(
            manager.create(Create {
                from: source,
                name: Some("unsafe".into()),
                into: None,
            }),
            Err(Error::UnsafeGit(_))
        ));
    }

    #[test]
    fn unavailable_cow_does_not_create_a_child() {
        let temp = TempDir::new().unwrap();
        let source = source(&temp);
        let mut manager = Manager::with_strategy(
            temp.path().join("registry.sqlite"),
            Box::new(FailureStrategy),
        )
        .unwrap();
        manager.init(&source).unwrap();

        assert!(matches!(
            manager.create(Create {
                from: source.clone(),
                name: Some("failure".into()),
                into: None,
            }),
            Err(Error::CowUnavailable(_))
        ));
        assert!(source.join(".rift").exists());
        assert!(manager.list(&source).unwrap().is_empty());
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

    fn initialize_git_repo(path: &Path) {
        run(path, &["init"]);
        run(path, &["config", "user.email", "test@example.com"]);
        run(path, &["config", "user.name", "Test"]);
        run(path, &["add", "file.txt"]);
        run(path, &["commit", "-m", "initial"]);
    }
}
