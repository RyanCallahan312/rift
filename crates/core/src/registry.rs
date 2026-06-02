use crate::{Error, GitStorageMode, Result, id::RiftId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use std::path::{Path, PathBuf};

#[derive(Clone)]
pub(crate) struct Record {
    pub(crate) id: RiftId,
    pub(crate) parent_id: Option<RiftId>,
    pub(crate) path: PathBuf,
    pub(crate) git_storage_mode: GitStorageMode,
    pub(crate) shared_git_dir: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PathRecord {
    pub(crate) id: RiftId,
    pub(crate) path: PathBuf,
    pub(crate) shared_git_dir: Option<PathBuf>,
    pub(crate) git_worktree_dir: Option<PathBuf>,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct MovedRecord {
    pub(crate) id: RiftId,
    pub(crate) original_path: PathBuf,
    pub(crate) trash_path: PathBuf,
}

#[derive(Clone, Copy)]
pub(crate) enum SubtreeScope {
    IncludingRoot,
    DescendantsOnly,
}

impl SubtreeScope {
    fn min_depth(self) -> u8 {
        match self {
            Self::IncludingRoot => 0,
            Self::DescendantsOnly => 1,
        }
    }
}

pub(crate) struct Registry {
    database: Connection,
}

impl Registry {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self> {
        let database = Connection::open(path)?;
        database.execute_batch(
            "PRAGMA busy_timeout = 2000;
             PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
               CREATE TABLE IF NOT EXISTS rift (
                id TEXT PRIMARY KEY,
                parent_id TEXT REFERENCES rift(id) ON DELETE CASCADE,
                path TEXT NOT NULL UNIQUE,
                created_at INTEGER NOT NULL,
                git_storage_mode TEXT NOT NULL DEFAULT 'inline',
                shared_git_dir TEXT,
                git_worktree_dir TEXT
               );
              CREATE INDEX IF NOT EXISTS rift_parent_id_idx ON rift(parent_id);
               CREATE TABLE IF NOT EXISTS trash (
                id TEXT PRIMARY KEY,
                path TEXT NOT NULL UNIQUE,
                removed_at INTEGER NOT NULL,
                shared_git_dir TEXT,
                git_worktree_dir TEXT
              );",
        )?;
        migrate(&database)?;
        Ok(Self { database })
    }

    pub(crate) fn insert_root(
        &self,
        id: &RiftId,
        path: &Path,
        git_storage_mode: GitStorageMode,
        shared_git_dir: Option<&Path>,
    ) -> Result<()> {
        self.database.execute(
            "INSERT INTO rift (id, parent_id, path, created_at, git_storage_mode, shared_git_dir, git_worktree_dir) VALUES (?1, NULL, ?2, ?3, ?4, ?5, NULL)",
            params![
                id.as_str(),
                path_text(path)?,
                timestamp(),
                git_storage_mode.as_str(),
                optional_path_text(shared_git_dir)?,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn insert_child(
        &self,
        id: &RiftId,
        parent_id: &RiftId,
        path: &Path,
        git_storage_mode: GitStorageMode,
        shared_git_dir: Option<&Path>,
        git_worktree_dir: Option<&Path>,
    ) -> Result<()> {
        self.database.execute(
            "INSERT INTO rift (id, parent_id, path, created_at, git_storage_mode, shared_git_dir, git_worktree_dir) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id.as_str(),
                parent_id.as_str(),
                path_text(path)?,
                timestamp(),
                git_storage_mode.as_str(),
                optional_path_text(shared_git_dir)?,
                optional_path_text(git_worktree_dir)?,
            ],
        )?;
        Ok(())
    }

    pub(crate) fn record_at(&self, path: &Path) -> Result<Option<Record>> {
        self.database
            .query_row(
                "SELECT id, parent_id, path, git_storage_mode, shared_git_dir, git_worktree_dir FROM rift WHERE path = ?1",
                [path_text(path)?],
                record_from_row,
            )
            .optional()
            .map_err(Error::from)
    }

    pub(crate) fn record_id(&self, id: &RiftId) -> Result<Option<Record>> {
        self.database
            .query_row(
                "SELECT id, parent_id, path, git_storage_mode, shared_git_dir, git_worktree_dir FROM rift WHERE id = ?1",
                [id.as_str()],
                record_from_row,
            )
            .optional()
            .map_err(Error::from)
    }

    pub(crate) fn subtree(&self, id: &RiftId, scope: SubtreeScope) -> Result<Vec<PathRecord>> {
        let mut statement = self.database.prepare(
            "WITH RECURSIVE subtree(id, path, shared_git_dir, git_worktree_dir, depth) AS (
               SELECT id, path, shared_git_dir, git_worktree_dir, 0 FROM rift WHERE id = ?1
               UNION ALL
               SELECT rift.id, rift.path, rift.shared_git_dir, rift.git_worktree_dir, subtree.depth + 1
               FROM rift JOIN subtree ON rift.parent_id = subtree.id
             ) SELECT id, path, shared_git_dir, git_worktree_dir FROM subtree WHERE depth >= ?2 ORDER BY depth DESC, id",
        )?;
        let rows = statement
            .query_map(params![id.as_str(), scope.min_depth()], |row| {
                Ok(PathRecord {
                    id: RiftId::from_stored(row.get(0)?),
                    path: PathBuf::from(row.get::<_, String>(1)?),
                    shared_git_dir: optional_path(row.get(2)?),
                    git_worktree_dir: optional_path(row.get(3)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub(crate) fn child_paths(&self, parent_id: &RiftId) -> Result<Vec<PathBuf>> {
        let mut statement = self
            .database
            .prepare("SELECT path FROM rift WHERE parent_id = ?1 ORDER BY created_at, id")?;
        Ok(statement
            .query_map([parent_id.as_str()], |row| {
                Ok(PathBuf::from(row.get::<_, String>(0)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub(crate) fn delete_active(&self, id: &RiftId) -> Result<()> {
        self.database
            .execute("DELETE FROM rift WHERE id = ?1", [id.as_str()])?;
        Ok(())
    }

    pub(crate) fn trash_moved(&mut self, moved: &[MovedRecord]) -> Result<()> {
        let transaction = self.database.transaction()?;
        moved.iter().try_for_each(|record| -> Result<()> {
            transaction.execute(
                "INSERT INTO trash (id, path, removed_at, shared_git_dir, git_worktree_dir)
                 SELECT id, ?2, ?3, shared_git_dir, git_worktree_dir FROM rift WHERE id = ?1",
                params![
                    record.id.as_str(),
                    path_text(&record.trash_path)?,
                    timestamp()
                ],
            )?;
            transaction.execute("DELETE FROM rift WHERE id = ?1", [record.id.as_str()])?;
            Ok(())
        })?;
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn trashed_paths(&self) -> Result<Vec<PathRecord>> {
        let mut statement = self.database.prepare(
            "SELECT id, path, shared_git_dir, git_worktree_dir FROM trash ORDER BY removed_at, id",
        )?;
        Ok(statement
            .query_map([], |row| {
                Ok(PathRecord {
                    id: RiftId::from_stored(row.get(0)?),
                    path: PathBuf::from(row.get::<_, String>(1)?),
                    shared_git_dir: optional_path(row.get(2)?),
                    git_worktree_dir: optional_path(row.get(3)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub(crate) fn delete_trash(&self, id: &RiftId) -> Result<()> {
        self.database
            .execute("DELETE FROM trash WHERE id = ?1", [id.as_str()])?;
        Ok(())
    }

    pub(crate) fn active_paths(&self) -> Result<Vec<PathRecord>> {
        let mut statement = self
            .database
            .prepare("SELECT id, path, shared_git_dir, git_worktree_dir FROM rift")?;
        Ok(statement
            .query_map([], |row| {
                Ok(PathRecord {
                    id: RiftId::from_stored(row.get(0)?),
                    path: PathBuf::from(row.get::<_, String>(1)?),
                    shared_git_dir: optional_path(row.get(2)?),
                    git_worktree_dir: optional_path(row.get(3)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub(crate) fn delete_active_records(&mut self, rows: &[PathRecord]) -> Result<()> {
        let transaction = self.database.transaction()?;
        rows.iter().try_for_each(|record| -> Result<()> {
            transaction.execute("DELETE FROM rift WHERE id = ?1", [record.id.as_str()])?;
            Ok(())
        })?;
        transaction.commit()?;
        Ok(())
    }
}

fn record_from_row(row: &Row<'_>) -> rusqlite::Result<Record> {
    Ok(Record {
        id: RiftId::from_stored(row.get(0)?),
        parent_id: row.get::<_, Option<String>>(1)?.map(RiftId::from_stored),
        path: PathBuf::from(row.get::<_, String>(2)?),
        git_storage_mode: GitStorageMode::from_stored(row.get::<_, String>(3)?),
        shared_git_dir: optional_path(row.get(4)?),
    })
}

fn migrate(database: &Connection) -> Result<()> {
    ensure_column(
        database,
        "git_storage_mode",
        "ALTER TABLE rift ADD COLUMN git_storage_mode TEXT NOT NULL DEFAULT 'inline'",
    )?;
    ensure_column(
        database,
        "shared_git_dir",
        "ALTER TABLE rift ADD COLUMN shared_git_dir TEXT",
    )?;
    ensure_column(
        database,
        "git_worktree_dir",
        "ALTER TABLE rift ADD COLUMN git_worktree_dir TEXT",
    )?;
    ensure_table_column(
        database,
        "trash",
        "shared_git_dir",
        "ALTER TABLE trash ADD COLUMN shared_git_dir TEXT",
    )?;
    ensure_table_column(
        database,
        "trash",
        "git_worktree_dir",
        "ALTER TABLE trash ADD COLUMN git_worktree_dir TEXT",
    )?;
    Ok(())
}

fn ensure_column(database: &Connection, column: &str, statement: &str) -> Result<()> {
    ensure_table_column(database, "rift", column, statement)
}

fn ensure_table_column(
    database: &Connection,
    table: &str,
    column: &str,
    statement: &str,
) -> Result<()> {
    let pragma = format!("PRAGMA table_info({table})");
    let exists = database
        .prepare(&pragma)?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .iter()
        .any(|name| name == column);
    if !exists {
        database.execute(statement, [])?;
    }
    Ok(())
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| Error::Path(format!("path is not valid UTF-8: {}", path.display())))
}

fn optional_path_text(path: Option<&Path>) -> Result<Option<String>> {
    path.map(path_text).transpose()
}

fn optional_path(path: Option<String>) -> Option<PathBuf> {
    path.map(PathBuf::from)
}

fn timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn registry() -> (TempDir, Registry) {
        let temp = TempDir::new().unwrap();
        let registry = Registry::open(temp.path().join("registry.sqlite")).unwrap();
        (temp, registry)
    }

    #[test]
    fn uses_wal_and_busy_timeout() {
        let (_temp, registry) = registry();
        let journal_mode: String = registry
            .database
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        let busy_timeout: i32 = registry
            .database
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .unwrap();

        assert_eq!(journal_mode, "wal");
        assert_eq!(busy_timeout, 2000);
    }

    #[test]
    fn subtree_returns_descendants_before_ancestors() {
        let (temp, registry) = registry();
        let root = temp.path().join("root");
        let child = temp.path().join("child");
        let sibling = temp.path().join("sibling");
        let grandchild = temp.path().join("grandchild");
        let root_id = id("root");
        let child_id = id("child");
        let sibling_id = id("sibling");
        let grandchild_id = id("grandchild");
        registry
            .insert_root(&root_id, &root, GitStorageMode::Inline, None)
            .unwrap();
        registry
            .insert_child(
                &child_id,
                &root_id,
                &child,
                GitStorageMode::Inline,
                None,
                None,
            )
            .unwrap();
        registry
            .insert_child(
                &sibling_id,
                &root_id,
                &sibling,
                GitStorageMode::Inline,
                None,
                None,
            )
            .unwrap();
        registry
            .insert_child(
                &grandchild_id,
                &child_id,
                &grandchild,
                GitStorageMode::Inline,
                None,
                None,
            )
            .unwrap();

        let subtree = registry
            .subtree(&root_id, SubtreeScope::IncludingRoot)
            .unwrap()
            .into_iter()
            .map(|record| record.id.to_string())
            .collect::<Vec<_>>();
        let descendants = registry
            .subtree(&root_id, SubtreeScope::DescendantsOnly)
            .unwrap()
            .into_iter()
            .map(|record| record.id.to_string())
            .collect::<Vec<_>>();

        assert_eq!(subtree, vec!["grandchild", "child", "sibling", "root"]);
        assert_eq!(descendants, vec!["grandchild", "child", "sibling"]);
        assert_eq!(
            registry.child_paths(&root_id).unwrap(),
            vec![child, sibling]
        );
    }

    #[test]
    fn trash_moved_transfers_records_from_active_tree_to_trash() {
        let (temp, mut registry) = registry();
        let root = temp.path().join("root");
        let child = temp.path().join("child");
        let trash = temp.path().join(".trash/child");
        let root_id = id("root");
        let child_id = id("child");
        registry
            .insert_root(&root_id, &root, GitStorageMode::Inline, None)
            .unwrap();
        registry
            .insert_child(
                &child_id,
                &root_id,
                &child,
                GitStorageMode::Inline,
                None,
                None,
            )
            .unwrap();

        registry
            .trash_moved(&[MovedRecord {
                id: child_id.clone(),
                original_path: child.clone(),
                trash_path: trash.clone(),
            }])
            .unwrap();

        assert!(registry.record_id(&root_id).unwrap().is_some());
        assert!(registry.record_id(&child_id).unwrap().is_none());
        assert_eq!(
            registry.trashed_paths().unwrap(),
            vec![PathRecord {
                id: child_id,
                path: trash,
                shared_git_dir: None,
                git_worktree_dir: None,
            }]
        );
    }

    #[test]
    fn trash_moved_preserves_git_metadata() {
        let (temp, mut registry) = registry();
        let root = temp.path().join("root");
        let child = temp.path().join("child");
        let trash = temp.path().join(".trash/child");
        let root_git = temp.path().join("rift/repos/root/git");
        let child_git = root_git.join("worktrees/child");
        let root_id = id("root");
        let child_id = id("child");
        registry
            .insert_root(
                &root_id,
                &root,
                GitStorageMode::SharedWorktrees,
                Some(&root_git),
            )
            .unwrap();
        registry
            .insert_child(
                &child_id,
                &root_id,
                &child,
                GitStorageMode::SharedWorktrees,
                Some(&root_git),
                Some(&child_git),
            )
            .unwrap();

        registry
            .trash_moved(&[MovedRecord {
                id: child_id.clone(),
                original_path: child,
                trash_path: trash.clone(),
            }])
            .unwrap();

        assert_eq!(
            registry.trashed_paths().unwrap(),
            vec![PathRecord {
                id: child_id,
                path: trash,
                shared_git_dir: Some(root_git),
                git_worktree_dir: Some(child_git),
            }]
        );
    }

    #[test]
    fn git_storage_metadata_round_trips() {
        let (temp, registry) = registry();
        let root = temp.path().join("root");
        let child = temp.path().join("child");
        let root_git = temp.path().join("rift/repos/root/git");
        let child_git = root_git.join("worktrees/child");
        let root_id = id("root");
        let child_id = id("child");

        registry
            .insert_root(
                &root_id,
                &root,
                GitStorageMode::SharedWorktrees,
                Some(&root_git),
            )
            .unwrap();
        registry
            .insert_child(
                &child_id,
                &root_id,
                &child,
                GitStorageMode::SharedWorktrees,
                Some(&root_git),
                Some(&child_git),
            )
            .unwrap();

        let root_record = registry.record_id(&root_id).unwrap().unwrap();
        let child_record = registry.record_id(&child_id).unwrap().unwrap();

        assert_eq!(
            root_record.git_storage_mode,
            GitStorageMode::SharedWorktrees
        );
        assert_eq!(root_record.shared_git_dir, Some(root_git.clone()));
        assert_eq!(
            child_record.git_storage_mode,
            GitStorageMode::SharedWorktrees
        );
        assert_eq!(child_record.shared_git_dir, Some(root_git.clone()));

        let subtree = registry
            .subtree(&root_id, SubtreeScope::IncludingRoot)
            .unwrap();
        assert!(subtree.iter().any(|record| {
            record.id == child_id
                && record.shared_git_dir == Some(root_git.clone())
                && record.git_worktree_dir == Some(child_git.clone())
        }));
    }

    fn id(value: &str) -> RiftId {
        RiftId::from_stored(value.to_owned())
    }
}
