use crate::model::{Identity, identity, valid_key};
use anyhow::{Context, Result, bail, ensure};
use rusqlite::{Connection, params};
use std::ffi::CString;
use std::fs::{self, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Ledger {
    connection: Connection,
}

impl Ledger {
    pub fn open(state: &Path) -> Result<Self> {
        fs::create_dir_all(state)
            .with_context(|| format!("creating state directory {}", state.display()))?;
        let connection =
            Connection::open(state.join("ledger.sqlite3")).context("opening success ledger")?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS successes (
                root_id TEXT NOT NULL,
                key TEXT NOT NULL,
                size TEXT NOT NULL,
                modified_ns TEXT NOT NULL,
                out_key TEXT NOT NULL,
                succeeded_ns TEXT NOT NULL,
                PRIMARY KEY (root_id, key, size, modified_ns)
            ) WITHOUT ROWID;",
        )?;
        Ok(Self { connection })
    }

    pub fn succeeded(&self, root: &str, key: &str, id: Identity) -> Result<bool> {
        let mut statement = self.connection.prepare_cached(
            "SELECT EXISTS(
                SELECT 1 FROM successes
                WHERE root_id = ?1 AND key = ?2 AND size = ?3 AND modified_ns = ?4
            )",
        )?;
        Ok(statement.query_row(
            params![root, key, id.size.to_string(), id.modified_ns.to_string()],
            |row| row.get(0),
        )?)
    }

    pub fn record(&self, root: &str, key: &str, id: Identity, out: &str) -> Result<()> {
        let succeeded_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock precedes Unix epoch")?
            .as_nanos()
            .to_string();
        self.connection.execute(
            "INSERT INTO successes (root_id, key, size, modified_ns, out_key, succeeded_ns)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (root_id, key, size, modified_ns) DO NOTHING",
            params![
                root,
                key,
                id.size.to_string(),
                id.modified_ns.to_string(),
                out,
                succeeded_ns
            ],
        )?;
        Ok(())
    }
}

pub fn execute(root: &Path, state: &Path, run: &str, key: &str, out: &str) -> Result<Identity> {
    ensure!(valid_key(key), "unsafe input key: {key}");
    ensure!(
        valid_key(out) && !out.contains(".."),
        "unsafe output key: {out}"
    );
    let root_directory =
        open_directory_at(libc::AT_FDCWD, &path_string(root)?).with_context(|| {
            format!(
                "opening root without following symlinks: {}",
                root.display()
            )
        })?;
    ensure!(
        identity(root, key)?.is_some(),
        "input is not an existing regular file: {key}"
    );

    let canonical_root = root.canonicalize().context("resolving root directory")?;
    let canonical_state = state.canonicalize().context("resolving state directory")?;
    ensure!(
        !canonical_state.starts_with(&canonical_root),
        "temporary output state must be outside root"
    );
    // A new child cannot coincide with an existing root when state is its ancestor.
    let temporary = tempfile::Builder::new()
        .prefix(".fev-execution-")
        .tempdir_in(&canonical_state)
        .context("creating temporary output directory")?;
    ensure!(
        !temporary.path().starts_with(&canonical_root),
        "temporary output directory must be outside root"
    );
    let temporary_directory = open_directory_at(libc::AT_FDCWD, &path_string(temporary.path())?)?;
    let temporary_output = temporary.path().join("output");
    let status = Command::new("sh")
        .arg("-c")
        .arg(run)
        .current_dir(root)
        .env("ROOT", root)
        .env("KEY", key)
        .env("FILE", root.join(key))
        .env("OUT_TMP", &temporary_output)
        .env("OUT", out)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .context("running command with sh -c")?;
    ensure!(status.success(), "command failed with {status}");

    let temporary_name = CString::new("output")?;
    // Checking metadata does not require read permission on the output file.
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let found = unsafe {
        libc::fstatat(
            temporary_directory.as_raw_fd(),
            temporary_name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if found != 0 {
        return Err(io::Error::last_os_error()).context("reading temporary output metadata");
    }
    // Successful fstatat initialized the complete value.
    let metadata = unsafe { metadata.assume_init() };
    ensure!(
        metadata.st_mode & libc::S_IFMT == libc::S_IFREG,
        "temporary output is not a regular, non-symlink file"
    );

    let mut components = out.split('/').peekable();
    let mut parent = root_directory;
    let destination = loop {
        let component = components.next().context("empty output key")?;
        let name = CString::new(component)?;
        if components.peek().is_none() {
            break name;
        }
        parent = create_directory_at(&parent, &name)
            .with_context(|| format!("opening safe output directory component {component}"))?;
    };
    reject_unsafe_destination(&parent, &destination)?;
    // Both ends are anchored to owned directory descriptors. renameat replaces a
    // regular destination atomically without following any destination symlink.
    let renamed = unsafe {
        libc::renameat(
            temporary_directory.as_raw_fd(),
            temporary_name.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
        )
    };
    if renamed != 0 {
        return Err(io::Error::last_os_error()).context("atomically publishing output");
    }
    Ok(Identity {
        size: u64::try_from(metadata.st_size).context("negative output size")?,
        modified_ns: i128::from(metadata.st_mtime) * 1_000_000_000
            + i128::from(metadata.st_mtime_nsec),
    })
}

fn path_string(path: &Path) -> Result<CString> {
    // Component reconstruction removes a trailing slash, which otherwise lets
    // some kernels follow a final symlink despite O_NOFOLLOW.
    let normalized: std::path::PathBuf = path.components().collect();
    Ok(CString::new(normalized.as_os_str().as_bytes())?)
}

fn open_at(directory: RawFd, path: &CString, flags: libc::c_int) -> Result<File> {
    // No O_CREAT is used here, so openat does not require a mode argument.
    let descriptor = unsafe { libc::openat(directory, path.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error()).context("opening file descriptor");
    }
    // A successful openat returns a new descriptor owned exclusively here.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

fn open_directory_at(directory: RawFd, path: &CString) -> Result<File> {
    open_at(
        directory,
        path,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )
}

fn create_directory_at(parent: &File, component: &CString) -> Result<File> {
    let created = unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o755) };
    if created != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error).context("creating output parent directory");
        }
    }
    open_directory_at(parent.as_raw_fd(), component)
}

fn reject_unsafe_destination(parent: &File, name: &CString) -> Result<()> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    let found = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if found != 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(error).context("checking existing publication destination");
    }
    // fstatat initialized the stat value on its successful return.
    let metadata = unsafe { metadata.assume_init() };
    let kind = metadata.st_mode & libc::S_IFMT;
    if kind != libc::S_IFREG {
        bail!("publication destination exists but is not a regular, non-symlink file");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use tempfile::{TempDir, tempdir};

    fn fixture() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let base = tempdir().unwrap();
        let root = base.path().join("root");
        let state = base.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        let root = root.canonicalize().unwrap();
        let state = state.canonicalize().unwrap();
        fs::write(root.join("input"), "source").unwrap();
        (base, root, state)
    }

    fn assert_clean(state: &Path) {
        assert_eq!(fs::read_dir(state).unwrap().count(), 0);
    }

    #[test]
    fn ledger_remembers_multiple_identities_across_reopen() {
        let state = tempdir().unwrap();
        let first = Identity {
            size: u64::MAX,
            modified_ns: i128::MAX,
        };
        let second = Identity {
            size: 4,
            modified_ns: -1,
        };
        {
            let ledger = Ledger::open(state.path()).unwrap();
            assert!(!ledger.succeeded("root", "input", first).unwrap());
            ledger.record("root", "input", first, "first").unwrap();
            ledger.record("root", "input", second, "second").unwrap();
            ledger.record("root", "input", first, "first").unwrap();
        }
        let ledger = Ledger::open(state.path()).unwrap();
        assert!(ledger.succeeded("root", "input", first).unwrap());
        assert!(ledger.succeeded("root", "input", second).unwrap());
        assert!(!ledger.succeeded("other", "input", first).unwrap());
        assert!(!ledger.succeeded("root", "other", first).unwrap());
        assert!(
            !ledger
                .succeeded("root", "input", Identity { size: 5, ..second })
                .unwrap()
        );
    }

    #[test]
    fn publishes_nested_output_and_replaces_only_on_success() {
        let (_base, root, state) = fixture();
        fs::create_dir_all(root.join("nested/deep")).unwrap();
        let destination = root.join("nested/deep/output");
        fs::write(&destination, "old").unwrap();
        let old = fs::File::open(&destination).unwrap();
        let published = execute(&root, &state,
            "test ! -e \"$OUT_TMP\" && test \"$KEY\" = input && test \"$OUT\" = nested/deep/output && test \"$PWD\" = \"$ROOT\" && cat \"$FILE\" > \"$OUT_TMP\"",
            "input", "nested/deep/output").unwrap();
        assert_eq!(fs::read_to_string(&destination).unwrap(), "source");
        use std::io::Read;
        let mut previous = String::new();
        (&old).read_to_string(&mut previous).unwrap();
        assert_eq!(previous, "old");
        assert_eq!(
            published,
            crate::model::identity(&root, "nested/deep/output")
                .unwrap()
                .unwrap()
        );
        assert_clean(&state);
        execute(
            &root,
            &state,
            "printf fresh > \"$OUT_TMP\"",
            "input",
            "new/parents/output",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("new/parents/output")).unwrap(),
            "fresh"
        );
        assert_clean(&state);
    }

    #[test]
    fn unsuccessful_or_missing_output_does_not_publish_and_cleans_temp() {
        let (_base, root, state) = fixture();
        fs::write(root.join("output"), "original").unwrap();
        for command in [
            "true",
            "printf bad > \"$OUT_TMP\"; exit 7",
            "mkdir \"$OUT_TMP\"",
        ] {
            assert!(execute(&root, &state, command, "input", "output").is_err());
            assert_eq!(fs::read_to_string(root.join("output")).unwrap(), "original");
            assert_clean(&state);
        }
    }

    #[test]
    fn input_symlinks_and_parent_escapes_never_run_command() {
        let (base, root, state) = fixture();
        let outside = base.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("input"), "outside").unwrap();
        symlink(outside.join("input"), root.join("linked")).unwrap();
        symlink(&outside, root.join("linked-parent")).unwrap();
        for key in ["linked", "linked-parent/input", "../outside/input"] {
            assert!(
                execute(
                    &root,
                    &state,
                    "printf ran > marker; printf data > \"$OUT_TMP\"",
                    key,
                    "output"
                )
                .is_err()
            );
            assert!(!root.join("marker").exists());
            assert!(!root.join("output").exists());
            assert_clean(&state);
        }
    }

    #[test]
    fn output_symlinks_directories_and_parent_escapes_are_refused() {
        let (base, root, state) = fixture();
        let outside = base.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("target"), "safe").unwrap();
        symlink(outside.join("target"), root.join("linked")).unwrap();
        symlink(&outside, root.join("linked-parent")).unwrap();
        fs::create_dir(root.join("directory")).unwrap();
        for out in ["linked", "linked-parent/new", "directory", "../outside/new"] {
            assert!(
                execute(
                    &root,
                    &state,
                    "printf dangerous > \"$OUT_TMP\"",
                    "input",
                    out
                )
                .is_err()
            );
            assert_eq!(fs::read_to_string(outside.join("target")).unwrap(), "safe");
            assert!(!outside.join("new").exists());
            assert_clean(&state);
        }
        assert!(
            fs::symlink_metadata(root.join("linked"))
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn temporary_symlink_and_fifo_outputs_are_refused_and_cleaned() {
        let (_base, root, state) = fixture();
        for command in ["ln -s \"$FILE\" \"$OUT_TMP\"", "mkfifo \"$OUT_TMP\""] {
            assert!(execute(&root, &state, command, "input", "output").is_err());
            assert!(!root.join("output").exists());
            assert_clean(&state);
        }
    }

    #[test]
    fn root_symlink_is_refused() {
        let (base, root, state) = fixture();
        let alias = base.path().join("alias");
        symlink(&root, &alias).unwrap();
        assert!(
            execute(
                &alias,
                &state,
                "printf data > \"$OUT_TMP\"",
                "input",
                "output"
            )
            .is_err()
        );
        assert!(!root.join("output").exists());
        assert_clean(&state);
    }

    #[test]
    fn temporary_directory_can_live_in_root_ancestor_but_not_in_root() {
        let (base, root, state) = fixture();
        let command = "case \"$OUT_TMP\" in \"$ROOT\"/*) exit 3;; esac; printf safe > \"$OUT_TMP\"";
        execute(&root, base.path(), command, "input", "output").unwrap();
        assert_eq!(fs::read_to_string(root.join("output")).unwrap(), "safe");
        assert_eq!(fs::read_dir(base.path()).unwrap().count(), 2);
        assert_clean(&state);
        assert!(execute(&root, &root, command, "input", "other").is_err());
        assert!(!root.join("other").exists());
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn regression_publishes_unreadable_regular_output() {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        let state = base.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        fs::write(root.join("input"), "source").unwrap();
        let id = execute(
            &root,
            &state,
            "printf locked > \"$OUT_TMP\"; chmod 000 \"$OUT_TMP\"",
            "input",
            "result",
        )
        .unwrap();
        assert_eq!(id.size, 6);
        let metadata = fs::symlink_metadata(root.join("result")).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.mode() & 0o777, 0);
    }
}
