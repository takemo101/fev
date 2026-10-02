use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Identity {
    pub size: u64,
    pub modified_ns: i128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventType {
    Startup,
    Created,
    Updated,
    Deleted,
    Renamed,
}

impl EventType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Deleted => "deleted",
            Self::Renamed => "renamed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileNode {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSnapshot {
    pub identity: Identity,
    pub node: FileNode,
}

#[derive(Debug, Clone)]
pub struct FileEvent {
    pub kind: EventType,
    pub identity: Identity,
    pub event_id: i64,
    pub old_key: Option<String>,
}

pub fn valid_key(key: &str) -> bool {
    !key.is_empty()
        && !key.starts_with('/')
        && !key.contains('\0')
        && key
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

pub fn identity(root: &Path, key: &str) -> Result<Option<Identity>> {
    Ok(snapshot(root, key)?.map(|file| file.identity))
}

pub fn snapshot(root: &Path, key: &str) -> Result<Option<FileSnapshot>> {
    let EventPathMetadata::Present(metadata) = event_path_metadata(root, key)? else {
        return Ok(None);
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    Ok(Some(FileSnapshot {
        identity: Identity {
            size: metadata.len(),
            modified_ns: i128::from(metadata.mtime()) * 1_000_000_000
                + i128::from(metadata.mtime_nsec()),
        },
        node: FileNode {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
    }))
}

pub fn safe_event_path(root: &Path, key: &str) -> Result<bool> {
    Ok(!matches!(
        event_path_metadata(root, key)?,
        EventPathMetadata::Unsafe
    ))
}

enum EventPathMetadata {
    Unsafe,
    Missing,
    Present(Metadata),
}

fn event_path_metadata(root: &Path, key: &str) -> Result<EventPathMetadata> {
    if !valid_key(key) {
        bail!("invalid key: {key:?}");
    }
    let root_metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(EventPathMetadata::Unsafe);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading root {}", root.display()));
        }
    };
    if !root_metadata.is_dir() {
        return Ok(EventPathMetadata::Unsafe);
    }
    let mut path = root.to_path_buf();
    let mut components = key.split('/').peekable();
    while let Some(component) = components.next() {
        path.push(component);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(EventPathMetadata::Missing);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
                return Ok(EventPathMetadata::Unsafe);
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        if metadata.file_type().is_symlink() {
            return Ok(EventPathMetadata::Unsafe);
        }
        if components.peek().is_none() {
            return Ok(EventPathMetadata::Present(metadata));
        }
        if !metadata.is_dir() {
            return Ok(EventPathMetadata::Unsafe);
        }
    }
    Ok(EventPathMetadata::Unsafe)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn rejects_traversal_and_symlink_components() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("input"), "outside").unwrap();
        symlink(outside.path(), root.path().join("link")).unwrap();
        assert!(identity(root.path(), "link/input").unwrap().is_none());
        assert!(!valid_key("../input"));
        assert!(!valid_key("a/../input"));
        assert!(!valid_key("/input"));
        assert!(!valid_key("a/./input"));

        let replaced = outside.path().join("replaced-root");
        symlink(root.path(), &replaced).unwrap();
        std::fs::write(root.path().join("input"), "inside").unwrap();
        assert!(identity(&replaced, "input").unwrap().is_none());
    }

    #[test]
    fn distinguishes_rewrites_and_ignores_nonfiles() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("input"), "first").unwrap();
        let first = identity(root.path(), "input").unwrap().unwrap();
        std::fs::write(root.path().join("input"), "longer input").unwrap();
        let second = identity(root.path(), "input").unwrap().unwrap();
        assert_ne!(first, second);
        assert!(identity(root.path(), "missing").unwrap().is_none());
        std::fs::create_dir(root.path().join("directory")).unwrap();
        assert!(identity(root.path(), "directory").unwrap().is_none());
    }

    #[test]
    fn missing_deleted_paths_remain_safe_under_real_directories() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("parent")).unwrap();
        std::fs::write(root.path().join("parent/input"), "source").unwrap();
        std::fs::remove_file(root.path().join("parent/input")).unwrap();
        assert!(safe_event_path(root.path(), "parent/input").unwrap());
        std::fs::remove_dir(root.path().join("parent")).unwrap();
        assert!(safe_event_path(root.path(), "parent/input").unwrap());
        assert!(snapshot(root.path(), "parent/input").unwrap().is_none());

        // A historical path may have been reused by a nonsymlink object.
        std::fs::create_dir(root.path().join("input")).unwrap();
        assert!(safe_event_path(root.path(), "input").unwrap());
        std::fs::remove_dir(root.path().join("input")).unwrap();
        std::fs::write(root.path().join("input"), "replacement").unwrap();
        assert!(safe_event_path(root.path(), "input").unwrap());
    }

    #[test]
    fn deleted_paths_reject_symlinks_non_directory_parents_and_invalid_roots() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("parent")).unwrap();
        symlink(outside.path().join("missing"), root.path().join("input")).unwrap();
        assert!(!safe_event_path(root.path(), "parent/missing").unwrap());
        assert!(!safe_event_path(root.path(), "input").unwrap());
        std::fs::write(root.path().join("file"), "not a directory").unwrap();
        assert!(!safe_event_path(root.path(), "file/missing").unwrap());
        assert!(snapshot(root.path(), "file/missing").unwrap().is_none());

        let root_alias = outside.path().join("root");
        symlink(root.path(), &root_alias).unwrap();
        assert!(!safe_event_path(&root_alias, "missing").unwrap());
        assert!(!safe_event_path(&root.path().join("file"), "missing").unwrap());
        assert!(!safe_event_path(&root.path().join("absent"), "missing").unwrap());
        for key in [
            "",
            "../input",
            "a/../input",
            "/input",
            "a/./input",
            "a//input",
            "input/",
            "a\0b",
        ] {
            assert!(safe_event_path(root.path(), key).is_err());
            assert!(snapshot(root.path(), key).is_err());
        }
    }

    #[test]
    fn snapshots_preserve_node_identity_across_hardlinks_renames_and_rewrites() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input");
        std::fs::write(&input, "first").unwrap();
        let original = snapshot(root.path(), "input").unwrap().unwrap();

        std::fs::hard_link(&input, root.path().join("hardlink")).unwrap();
        assert_eq!(snapshot(root.path(), "hardlink").unwrap(), Some(original));
        std::fs::rename(&input, root.path().join("renamed")).unwrap();
        assert!(snapshot(root.path(), "input").unwrap().is_none());
        assert_eq!(snapshot(root.path(), "renamed").unwrap(), Some(original));

        std::fs::write(root.path().join("renamed"), "longer content").unwrap();
        let changed = snapshot(root.path(), "renamed").unwrap().unwrap();
        assert_eq!(changed.node, original.node);
        assert_ne!(changed.identity, original.identity);
        assert_eq!(snapshot(root.path(), "hardlink").unwrap(), Some(changed));

        std::fs::write(root.path().join("independent"), "longer content").unwrap();
        let independent = snapshot(root.path(), "independent").unwrap().unwrap();
        assert_ne!(independent.node, changed.node);
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;

    #[test]
    fn regression_input_names_can_contain_double_dots() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("draft..txt"), "source").unwrap();
        assert_eq!(
            identity(root.path(), "draft..txt").unwrap().unwrap().size,
            6
        );
    }
}
