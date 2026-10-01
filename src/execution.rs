use crate::model::{Identity, identity, valid_key};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, params};
use std::fs;
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
        let mut connection =
            Connection::open(state.join("ledger.sqlite3")).context("opening success ledger")?;
        let transaction = connection
            .transaction()
            .context("starting ledger migration")?;
        let legacy: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'successes')
                AND NOT EXISTS(SELECT 1 FROM pragma_table_info('successes') WHERE name = 'rule_id')",
            [],
            |row| row.get(0),
        )?;
        if legacy {
            // Old successes have no rule identity and cannot suppress current jobs.
            // A conflicting archive makes ALTER fail, preserving both tables.
            transaction
                .execute_batch("ALTER TABLE successes RENAME TO legacy_successes;")
                .context("archiving legacy success ledger")?;
        }
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS successes (
                root_id TEXT NOT NULL,
                key TEXT NOT NULL,
                rule_id TEXT NOT NULL,
                size TEXT NOT NULL,
                modified_ns TEXT NOT NULL,
                succeeded_ns TEXT NOT NULL,
                PRIMARY KEY (root_id, key, rule_id, size, modified_ns)
            ) WITHOUT ROWID;",
        )?;
        transaction
            .commit()
            .context("committing ledger migration")?;
        Ok(Self { connection })
    }

    pub fn succeeded(&self, root: &str, key: &str, rule: &str, id: Identity) -> Result<bool> {
        let mut statement = self.connection.prepare_cached(
            "SELECT EXISTS(
                SELECT 1 FROM successes
                WHERE root_id = ?1 AND key = ?2 AND rule_id = ?3 AND size = ?4 AND modified_ns = ?5
            )",
        )?;
        Ok(statement.query_row(
            params![
                root,
                key,
                rule,
                id.size.to_string(),
                id.modified_ns.to_string()
            ],
            |row| row.get(0),
        )?)
    }

    pub fn record(&self, root: &str, key: &str, rule: &str, id: Identity) -> Result<()> {
        let succeeded_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock precedes Unix epoch")?
            .as_nanos()
            .to_string();
        self.connection.execute(
            "INSERT INTO successes (root_id, key, rule_id, size, modified_ns, succeeded_ns)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (root_id, key, rule_id, size, modified_ns) DO NOTHING",
            params![
                root,
                key,
                rule,
                id.size.to_string(),
                id.modified_ns.to_string(),
                succeeded_ns
            ],
        )?;
        Ok(())
    }
}

pub fn execute(
    root: &Path,
    key: &str,
    run: &[String],
    captures: &[(String, String)],
) -> Result<()> {
    ensure!(valid_key(key), "unsafe input key: {key}");
    ensure!(
        identity(root, key)?.is_some(),
        "input is not an existing regular file: {key}"
    );
    let inherited_captures: Vec<_> = std::env::vars_os()
        .filter_map(|(name, _)| name.as_bytes().starts_with(b"MATCH_").then_some(name))
        .collect();
    let input = root.join(key);
    for (index, run) in run.iter().enumerate() {
        let step = index + 1;
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(run)
            .current_dir(root)
            .env_remove("OUT")
            .env_remove("OUT_TMP");
        for name in &inherited_captures {
            command.env_remove(name);
        }
        let status = command
            .env("ROOT", root)
            .env("KEY", key)
            .env("FILE", &input)
            .envs(captures.iter().map(|(name, value)| (name, value)))
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("running command step {step} with sh -c"))?;
        ensure!(status.success(), "command step {step} failed with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use tempfile::{TempDir, tempdir};

    fn fixture() -> (TempDir, PathBuf) {
        let base = tempdir().unwrap();
        let root = base.path().join("root");
        fs::create_dir(&root).unwrap();
        let root = root.canonicalize().unwrap();
        fs::write(root.join("input"), "source").unwrap();
        (base, root)
    }

    fn commands(steps: &[&str]) -> Vec<String> {
        steps.iter().map(|step| (*step).to_owned()).collect()
    }

    #[test]
    fn outputless_commands_receive_input_and_closed_stdin() {
        let (_base, root) = fixture();
        execute(
            &root,
            "input",
            &commands(&[
                "test \"$PWD\" = \"$ROOT\" && test \"$KEY\" = input && test \"$FILE\" = \"$ROOT/$KEY\" && test \"$(cat \"$FILE\")\" = source && ! read -r value",
            ]),
            &[],
        )
        .unwrap();
        assert_eq!(
            fs::read_dir(&root)
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<Vec<_>>(),
            vec![std::ffi::OsString::from("input")]
        );
    }

    #[test]
    fn commands_run_in_order_and_stop_at_first_failure() {
        let (_base, root) = fixture();
        execute(
            &root,
            "input",
            &commands(&[
                "printf first > order",
                "test \"$(cat order)\" = first && printf second >> order",
                "printf failed >> order; exit 7",
                "printf unexpected >> order",
            ]),
            &[],
        )
        .unwrap_err();
        assert_eq!(
            fs::read_to_string(root.join("order")).unwrap(),
            "firstsecondfailed"
        );
    }

    #[test]
    fn each_step_has_independent_shell_state_but_shared_files() {
        let (_base, root) = fixture();
        fs::create_dir(root.join("nested")).unwrap();
        execute(
            &root,
            "input",
            &commands(&[
                "export MATCH_NAME=changed; cd nested; printf persisted > shared",
                "test \"$PWD\" = \"$ROOT\" && test \"$MATCH_NAME\" = original && test \"$(cat nested/shared)\" = persisted",
            ]),
            &[("MATCH_NAME".to_owned(), "original".to_owned())],
        )
        .unwrap();
    }

    #[test]
    fn captures_are_data_even_with_hostile_shell_contents() {
        let (_base, root) = fixture();
        let hostile = "spaces ' \" $HOME $(touch injected) `touch injected` ;\n* ? [abc] \\";
        execute(
            &root,
            "input",
            &commands(&[
                "printf '%s' \"$MATCH_NAME\" > captured; printf '%s' \"$MATCH_EMPTY\" > optional",
            ]),
            &[
                ("MATCH_NAME".to_owned(), hostile.to_owned()),
                ("MATCH_EMPTY".to_owned(), String::new()),
            ],
        )
        .unwrap();
        assert_eq!(fs::read_to_string(root.join("captured")).unwrap(), hostile);
        assert_eq!(fs::read_to_string(root.join("optional")).unwrap(), "");
        assert!(!root.join("injected").exists());
    }

    #[test]
    fn inherited_output_and_capture_variables_are_removed() {
        let (_base, root) = fixture();
        let status = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "execution::tests::inherited_environment_subprocess",
                "--ignored",
                "--nocapture",
            ])
            .env("FEV_EXECUTION_TEST_ROOT", &root)
            .env("OUT", "old-output")
            .env("OUT_TMP", "old-temporary")
            .env("MATCH_STALE", "old-capture")
            .env("MATCH_NAME", "old-name")
            .env("MATCH_lowercase", "also-stale")
            .env("FEV_PRESERVED_ENV", "keep-me")
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            fs::read_to_string(root.join("environment")).unwrap(),
            "fresh:keep-me"
        );
    }

    #[test]
    #[ignore = "invoked in a subprocess with an isolated inherited environment"]
    fn inherited_environment_subprocess() {
        let root = PathBuf::from(std::env::var_os("FEV_EXECUTION_TEST_ROOT").unwrap());
        execute(
            &root,
            "input",
            &commands(&[
                "test \"${OUT+x}\" != x && test \"${OUT_TMP+x}\" != x && test \"${MATCH_STALE+x}\" != x && test \"${MATCH_lowercase+x}\" != x && test \"$MATCH_NAME\" = fresh && printf '%s:%s' \"$MATCH_NAME\" \"$FEV_PRESERVED_ENV\" > environment",
            ]),
            &[("MATCH_NAME".to_owned(), "fresh".to_owned())],
        )
        .unwrap();
    }

    #[test]
    fn unsafe_or_nonregular_inputs_never_start_commands() {
        let (base, root) = fixture();
        let outside = base.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("input"), "outside").unwrap();
        symlink(outside.join("input"), root.join("linked")).unwrap();
        symlink(&outside, root.join("linked-parent")).unwrap();
        fs::create_dir(root.join("directory")).unwrap();
        for key in [
            "linked",
            "linked-parent/input",
            "../outside/input",
            "/input",
            "input/../input",
            "directory",
            "missing",
            "",
            "input\0suffix",
        ] {
            assert!(
                execute(&root, key, &commands(&["printf ran > marker"]), &[]).is_err(),
                "{key:?}"
            );
            assert!(!root.join("marker").exists(), "{key:?}");
        }
    }

    #[test]
    fn ledger_remembers_multiple_identities_and_separates_rules_across_reopen() {
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
            assert!(
                !ledger
                    .succeeded("root", "input", "first-rule", first)
                    .unwrap()
            );
            ledger.record("root", "input", "first-rule", first).unwrap();
            ledger
                .record("root", "input", "first-rule", second)
                .unwrap();
            ledger.record("root", "input", "first-rule", first).unwrap();
            assert!(
                !ledger
                    .succeeded("root", "input", "second-rule", first)
                    .unwrap()
            );
            ledger
                .record("root", "input", "second-rule", first)
                .unwrap();
        }
        let ledger = Ledger::open(state.path()).unwrap();
        assert!(
            ledger
                .succeeded("root", "input", "first-rule", first)
                .unwrap()
        );
        assert!(
            ledger
                .succeeded("root", "input", "first-rule", second)
                .unwrap()
        );
        assert!(
            ledger
                .succeeded("root", "input", "second-rule", first)
                .unwrap()
        );
        assert!(
            !ledger
                .succeeded("root", "input", "second-rule", second)
                .unwrap()
        );
        assert!(
            !ledger
                .succeeded("other", "input", "first-rule", first)
                .unwrap()
        );
        assert!(
            !ledger
                .succeeded("root", "other", "first-rule", first)
                .unwrap()
        );
        assert!(
            !ledger
                .succeeded(
                    "root",
                    "input",
                    "first-rule",
                    Identity { size: 5, ..second }
                )
                .unwrap()
        );
        assert!(
            !ledger
                .succeeded(
                    "root",
                    "input",
                    "first-rule",
                    Identity {
                        modified_ns: 0,
                        ..second
                    }
                )
                .unwrap()
        );
    }

    fn legacy_state() -> TempDir {
        let state = tempdir().unwrap();
        let connection = Connection::open(state.path().join("ledger.sqlite3")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE successes (
                    root_id TEXT NOT NULL,
                    key TEXT NOT NULL,
                    size TEXT NOT NULL,
                    modified_ns TEXT NOT NULL,
                    out_key TEXT NOT NULL,
                    succeeded_ns TEXT NOT NULL,
                    PRIMARY KEY (root_id, key, size, modified_ns)
                ) WITHOUT ROWID;
                INSERT INTO successes VALUES ('root', 'input', '6', '-1', 'old-output', '123');",
            )
            .unwrap();
        state
    }

    #[test]
    fn migration_preserves_history_without_suppressing_new_rules_and_reopens() {
        let state = legacy_state();
        let id = Identity {
            size: 6,
            modified_ns: -1,
        };
        {
            let ledger = Ledger::open(state.path()).unwrap();
            assert!(!ledger.succeeded("root", "input", "rule", id).unwrap());
            ledger.record("root", "input", "rule", id).unwrap();
        }
        for _ in 0..2 {
            let ledger = Ledger::open(state.path()).unwrap();
            assert!(ledger.succeeded("root", "input", "rule", id).unwrap());
            assert!(!ledger.succeeded("root", "input", "other-rule", id).unwrap());
            let historical: (String, String, String, String, String, String) = ledger
                .connection
                .query_row(
                    "SELECT root_id, key, size, modified_ns, out_key, succeeded_ns FROM legacy_successes",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
                )
                .unwrap();
            assert_eq!(
                historical,
                (
                    "root".to_owned(),
                    "input".to_owned(),
                    "6".to_owned(),
                    "-1".to_owned(),
                    "old-output".to_owned(),
                    "123".to_owned(),
                )
            );
        }
    }

    #[test]
    fn conflicting_archive_refuses_migration_without_destroying_data() {
        let state = legacy_state();
        let connection = Connection::open(state.path().join("ledger.sqlite3")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE legacy_successes (history TEXT NOT NULL);
                 INSERT INTO legacy_successes VALUES ('existing-history');",
            )
            .unwrap();
        assert!(Ledger::open(state.path()).is_err());
        assert_eq!(
            connection
                .query_row("SELECT out_key FROM successes", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "old-output"
        );
        assert_eq!(
            connection
                .query_row("SELECT history FROM legacy_successes", [], |row| row
                    .get::<_, String>(0))
                .unwrap(),
            "existing-history"
        );
    }
}
