use anyhow::{Context, Result, bail};
use std::os::unix::fs::MetadataExt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Identity {
    pub size: u64,
    pub modified_ns: i128,
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
            return Ok(None);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("reading root {}", root.display()));
        }
    };
    if !root_metadata.is_dir() {
        return Ok(None);
    }
    let mut path = root.to_path_buf();
    let mut components = key.split('/').peekable();
    while let Some(component) = components.next() {
        path.push(component);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        if metadata.file_type().is_symlink() {
            return Ok(None);
        }
        if components.peek().is_none() {
            return Ok(metadata.is_file().then_some(Identity {
                size: metadata.len(),
                modified_ns: i128::from(metadata.mtime()) * 1_000_000_000
                    + i128::from(metadata.mtime_nsec()),
            }));
        }
        if !metadata.is_dir() {
            return Ok(None);
        }
    }
    Ok(None)
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
