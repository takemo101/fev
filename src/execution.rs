use crate::model::{EventType, FileEvent, Identity, identity, safe_event_path, valid_key};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, TransactionBehavior, params};
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
            .transaction_with_behavior(TransactionBehavior::Immediate)
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
        let pre_events: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'successes')
                AND NOT EXISTS(SELECT 1 FROM pragma_table_info('successes') WHERE name = 'event_id')",
            [],
            |row| row.get(0),
        )?;
        if pre_events {
            transaction
                .execute_batch("ALTER TABLE successes RENAME TO successes_before_events;")
                .context("preserving per-rule successes during event migration")?;
        }
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS successes (
                root_id TEXT NOT NULL,
                key TEXT NOT NULL,
                rule_id TEXT NOT NULL,
                size TEXT NOT NULL,
                modified_ns TEXT NOT NULL,
                succeeded_ns TEXT NOT NULL,
                event_id INTEGER NOT NULL,
                event_kind TEXT NOT NULL,
                PRIMARY KEY (root_id, key, rule_id, size, modified_ns, event_id)
            ) WITHOUT ROWID;",
        )?;
        if pre_events {
            transaction.execute_batch(
                "INSERT INTO successes
                    (root_id, key, rule_id, size, modified_ns, succeeded_ns, event_id, event_kind)
                 SELECT root_id, key, rule_id, size, modified_ns, succeeded_ns, 0, 'created'
                 FROM successes_before_events;
                 DROP TABLE successes_before_events;",
            )?;
        }
        transaction.execute_batch(
            "CREATE TABLE IF NOT EXISTS event_sequence (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                last_event_id INTEGER NOT NULL
                    CHECK (typeof(last_event_id) = 'integer' AND last_event_id >= 0)
            );
            INSERT OR IGNORE INTO event_sequence (singleton, last_event_id) VALUES (1, 0);",
        )?;
        transaction
            .commit()
            .context("committing ledger migration")?;
        Ok(Self { connection })
    }

    pub fn succeeded(
        &self,
        root: &str,
        key: &str,
        rule: &str,
        id: Identity,
        event_id: i64,
    ) -> Result<bool> {
        let mut statement = self.connection.prepare_cached(
            "SELECT EXISTS(
                SELECT 1 FROM successes
                WHERE root_id = ?1 AND key = ?2 AND rule_id = ?3 AND size = ?4 AND modified_ns = ?5
                    AND event_id = ?6
            )",
        )?;
        Ok(statement.query_row(
            params![
                root,
                key,
                rule,
                id.size.to_string(),
                id.modified_ns.to_string(),
                event_id
            ],
            |row| row.get(0),
        )?)
    }

    pub fn record(
        &self,
        root: &str,
        key: &str,
        rule: &str,
        id: Identity,
        event_id: i64,
        kind: EventType,
    ) -> Result<()> {
        let succeeded_ns = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("system clock precedes Unix epoch")?
            .as_nanos()
            .to_string();
        self.connection.execute(
            "INSERT INTO successes
                (root_id, key, rule_id, size, modified_ns, succeeded_ns, event_id, event_kind)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT (root_id, key, rule_id, size, modified_ns, event_id) DO NOTHING",
            params![
                root,
                key,
                rule,
                id.size.to_string(),
                id.modified_ns.to_string(),
                succeeded_ns,
                event_id,
                kind.as_str()
            ],
        )?;
        Ok(())
    }

    pub fn next_event_id(&self) -> Result<i64> {
        let mut statement = self.connection.prepare_cached(
            "UPDATE event_sequence SET last_event_id = last_event_id + 1
             WHERE singleton = 1 RETURNING last_event_id",
        )?;
        statement
            .query_row([], |row| row.get(0))
            .context("reserving filesystem event identity")
    }
}

pub fn execute(
    root: &Path,
    key: &str,
    event: &FileEvent,
    run: &[String],
    captures: &[(String, String)],
) -> Result<()> {
    ensure!(valid_key(key), "unsafe input key: {key}");
    if matches!(event.kind, EventType::Deleted | EventType::Renamed) {
        ensure!(
            safe_event_path(root, key)?,
            "unsafe event input path: {key}"
        );
    } else {
        ensure!(
            identity(root, key)?.is_some(),
            "input is not an existing regular file: {key}"
        );
    }
    let (old_key, old_file) = if event.kind == EventType::Renamed {
        let old_key = event
            .old_key
            .as_deref()
            .context("renamed event is missing its previous key")?;
        ensure!(
            safe_event_path(root, old_key)?,
            "unsafe previous input path: {old_key}"
        );
        (old_key, root.join(old_key))
    } else {
        ("", Default::default())
    };
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
            .env("EVENT", event.kind.as_str())
            .env("OLD_KEY", old_key)
            .env("OLD_FILE", &old_file)
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

    fn event(kind: EventType) -> FileEvent {
        FileEvent {
            kind,
            identity: Identity {
                size: 6,
                modified_ns: 0,
            },
            event_id: if matches!(kind, EventType::Deleted | EventType::Renamed) {
                1
            } else {
                0
            },
            old_key: (kind == EventType::Renamed).then(|| "old-input".to_owned()),
        }
    }

    #[test]
    fn deleted_command_runs_when_input_and_its_parents_are_absent() {
        let (_base, root) = fixture();
        execute(
            &root,
            "missing-parent/removed",
            &event(EventType::Deleted),
            &commands(&[
                "test \"$EVENT\" = deleted && test \"$KEY\" = missing-parent/removed && test \"$FILE\" = \"$ROOT/$KEY\" && test ! -e \"$FILE\" && test -z \"$OLD_KEY\" && test -z \"$OLD_FILE\" && printf removed > deletion",
            ]),
            &[],
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("deletion")).unwrap(),
            "removed"
        );
        assert!(!root.join("missing-parent").exists());
    }

    #[test]
    fn renamed_command_uses_new_input_and_captures_with_safe_old_path() {
        let (_base, root) = fixture();
        fs::rename(root.join("input"), root.join("new-input")).unwrap();
        let mut renamed = event(EventType::Renamed);
        renamed.old_key = Some("input".to_owned());
        execute(
            &root,
            "new-input",
            &renamed,
            &commands(&[
                "test \"$EVENT\" = renamed && test \"$KEY\" = new-input && test \"$FILE\" = \"$ROOT/new-input\" && test \"$OLD_KEY\" = input && test \"$OLD_FILE\" = \"$ROOT/input\" && test ! -e \"$OLD_FILE\" && test \"$MATCH_NAME\" = new && cp \"$FILE\" \"processed-$MATCH_NAME\"",
            ]),
            &[("MATCH_NAME".to_owned(), "new".to_owned())],
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("processed-new")).unwrap(),
            "source"
        );
        assert!(!root.join("input").exists());
    }

    #[test]
    fn queued_rename_runs_after_destination_moves_again() {
        let (_base, root) = fixture();
        fs::rename(root.join("input"), root.join("next-input")).unwrap();
        let mut renamed = event(EventType::Renamed);
        renamed.old_key = Some("input".to_owned());
        execute(
            &root,
            "intermediate-input",
            &renamed,
            &commands(&[
                "test ! -e \"$FILE\" && printf '%s:%s:%s' \"$EVENT\" \"$OLD_KEY\" \"$KEY\" > transitions",
            ]),
            &[],
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("transitions")).unwrap(),
            "renamed:input:intermediate-input"
        );
    }

    #[test]
    fn unsafe_historical_paths_never_start_commands() {
        let (base, root) = fixture();
        let outside = base.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("input"), "outside").unwrap();
        symlink(&outside, root.join("linked-parent")).unwrap();
        symlink(outside.join("input"), root.join("linked")).unwrap();
        for key in [
            "../outside/input",
            "/input",
            "input/../input",
            "linked",
            "linked-parent/missing",
            "input/child",
            "",
            "input\0suffix",
        ] {
            for kind in [EventType::Deleted, EventType::Renamed] {
                assert!(
                    execute(
                        &root,
                        key,
                        &event(kind),
                        &commands(&["printf ran > marker"]),
                        &[],
                    )
                    .is_err(),
                    "{kind:?} {key:?}"
                );
            }
            let mut renamed = event(EventType::Renamed);
            renamed.old_key = Some(key.to_owned());
            assert!(
                execute(
                    &root,
                    "input",
                    &renamed,
                    &commands(&["printf ran > marker"]),
                    &[],
                )
                .is_err(),
                "{key:?}"
            );
            assert!(!root.join("marker").exists(), "{key:?}");
        }
        let mut renamed = event(EventType::Renamed);
        renamed.old_key = None;
        assert!(
            execute(
                &root,
                "input",
                &renamed,
                &commands(&["printf ran > marker"]),
                &[],
            )
            .is_err()
        );
        assert!(!root.join("marker").exists());
    }

    #[test]
    fn outputless_commands_receive_input_and_closed_stdin() {
        let (_base, root) = fixture();
        execute(&root, "input", &event(EventType::Created), &commands(&[
            "test \"$PWD\" = \"$ROOT\" && test \"$KEY\" = input && test \"$FILE\" = \"$ROOT/$KEY\" && test \"$(cat \"$FILE\")\" = source && ! read -r value",
        ]), &[])
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
            &event(EventType::Created),
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
        execute(&root, "input", &event(EventType::Created), &commands(&[
            "export MATCH_NAME=changed; cd nested; printf persisted > shared",
            "test \"$PWD\" = \"$ROOT\" && test \"$MATCH_NAME\" = original && test \"$(cat nested/shared)\" = persisted",
        ]), &[("MATCH_NAME".to_owned(), "original".to_owned())])
        .unwrap();
    }

    #[test]
    fn captures_are_data_even_with_hostile_shell_contents() {
        let (_base, root) = fixture();
        let hostile = "spaces ' \" $HOME $(touch injected) `touch injected` ;\n* ? [abc] \\";
        execute(
            &root,
            "input",
            &event(EventType::Created),
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
            .env("EVENT", "inherited-event")
            .env("OLD_KEY", "inherited-key")
            .env("OLD_FILE", "inherited-file")
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
        for kind in [
            EventType::Startup,
            EventType::Created,
            EventType::Updated,
            EventType::Deleted,
        ] {
            execute(
                &root,
                "input",
                &event(kind),
                &commands(&[&format!(
                    "test \"$EVENT\" = {} && test -z \"$OLD_KEY\" && test -z \"$OLD_FILE\" && test \"${{OUT+x}}\" != x && test \"${{OUT_TMP+x}}\" != x && test \"${{MATCH_STALE+x}}\" != x && test \"${{MATCH_lowercase+x}}\" != x && test \"$MATCH_NAME\" = fresh && printf '%s:%s' \"$MATCH_NAME\" \"$FEV_PRESERVED_ENV\" > environment",
                    kind.as_str()
                )]),
                &[("MATCH_NAME".to_owned(), "fresh".to_owned())],
            )
            .unwrap();
        }
        fs::rename(root.join("input"), root.join("new-input")).unwrap();
        let mut renamed = event(EventType::Renamed);
        renamed.old_key = Some("input".to_owned());
        execute(
            &root,
            "new-input",
            &renamed,
            &commands(&[
                "test \"$EVENT\" = renamed && test \"$OLD_KEY\" = input && test \"$OLD_FILE\" = \"$ROOT/input\" && cp \"$FILE\" inherited-rename",
            ]),
            &[],
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("inherited-rename")).unwrap(),
            "source"
        );
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
            for kind in [EventType::Startup, EventType::Created, EventType::Updated] {
                assert!(
                    execute(
                        &root,
                        key,
                        &event(kind),
                        &commands(&["printf ran > marker"]),
                        &[],
                    )
                    .is_err(),
                    "{kind:?} {key:?}"
                );
                assert!(!root.join("marker").exists(), "{kind:?} {key:?}");
            }
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
                    .succeeded("root", "input", "first-rule", first, 0)
                    .unwrap()
            );
            ledger
                .record("root", "input", "first-rule", first, 0, EventType::Created)
                .unwrap();
            ledger
                .record("root", "input", "first-rule", second, 0, EventType::Created)
                .unwrap();
            ledger
                .record("root", "input", "first-rule", first, 0, EventType::Created)
                .unwrap();
            assert!(
                !ledger
                    .succeeded("root", "input", "second-rule", first, 0)
                    .unwrap()
            );
            ledger
                .record("root", "input", "second-rule", first, 0, EventType::Created)
                .unwrap();
        }
        let ledger = Ledger::open(state.path()).unwrap();
        assert!(
            ledger
                .succeeded("root", "input", "first-rule", first, 0)
                .unwrap()
        );
        assert!(
            ledger
                .succeeded("root", "input", "first-rule", second, 0)
                .unwrap()
        );
        assert!(
            ledger
                .succeeded("root", "input", "second-rule", first, 0)
                .unwrap()
        );
        assert!(
            !ledger
                .succeeded("root", "input", "second-rule", second, 0)
                .unwrap()
        );
        assert!(
            !ledger
                .succeeded("other", "input", "first-rule", first, 0)
                .unwrap()
        );
        assert!(
            !ledger
                .succeeded("root", "other", "first-rule", first, 0)
                .unwrap()
        );
        assert!(
            !ledger
                .succeeded(
                    "root",
                    "input",
                    "first-rule",
                    Identity { size: 5, ..second },
                    0,
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
                    },
                    0,
                )
                .unwrap()
        );
    }

    #[test]
    fn content_kinds_share_identity_dedup_but_transitions_remain_distinct_after_reopen() {
        let state = tempdir().unwrap();
        let id = event(EventType::Created).identity;
        let first;
        {
            let ledger = Ledger::open(state.path()).unwrap();
            ledger
                .record("root", "input", "rule", id, 0, EventType::Created)
                .unwrap();
            assert!(ledger.succeeded("root", "input", "rule", id, 0).unwrap());
            ledger
                .record("root", "input", "rule", id, 0, EventType::Updated)
                .unwrap();
            ledger
                .record("root", "input", "rule", id, 0, EventType::Startup)
                .unwrap();
            first = ledger.next_event_id().unwrap();
            assert!(first > 0);
            assert!(
                !ledger
                    .succeeded("root", "input", "rule", id, first)
                    .unwrap()
            );
            ledger
                .record("root", "input", "rule", id, first, EventType::Deleted)
                .unwrap();
            // Reservation must survive a restart even if no success was recorded.
            let reserved = ledger.next_event_id().unwrap();
            assert!(reserved > first);
        }
        let ledger = Ledger::open(state.path()).unwrap();
        let other = Ledger::open(state.path()).unwrap();
        let second = ledger.next_event_id().unwrap();
        assert!(second > first + 1);
        let third = other.next_event_id().unwrap();
        assert!(third > second);
        for (event_id, kind) in [(second, EventType::Deleted), (third, EventType::Renamed)] {
            assert!(
                !ledger
                    .succeeded("root", "input", "rule", id, event_id)
                    .unwrap()
            );
            ledger
                .record("root", "input", "rule", id, event_id, kind)
                .unwrap();
        }
        drop(ledger);
        drop(other);
        let ledger = Ledger::open(state.path()).unwrap();
        for event_id in [0, first, second, third] {
            assert!(
                ledger
                    .succeeded("root", "input", "rule", id, event_id)
                    .unwrap()
            );
        }
        let records: Vec<(i64, String)> = ledger
            .connection
            .prepare("SELECT event_id, event_kind FROM successes ORDER BY event_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            records,
            vec![
                (0, "created".to_owned()),
                (first, "deleted".to_owned()),
                (second, "deleted".to_owned()),
                (third, "renamed".to_owned()),
            ]
        );
        assert!(ledger.next_event_id().unwrap() > third);
    }

    fn pre_events_state() -> TempDir {
        let state = tempdir().unwrap();
        let connection = Connection::open(state.path().join("ledger.sqlite3")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE successes (
                    root_id TEXT NOT NULL,
                    key TEXT NOT NULL,
                    rule_id TEXT NOT NULL,
                    size TEXT NOT NULL,
                    modified_ns TEXT NOT NULL,
                    succeeded_ns TEXT NOT NULL,
                    PRIMARY KEY (root_id, key, rule_id, size, modified_ns)
                ) WITHOUT ROWID;
                INSERT INTO successes VALUES ('root', 'input', 'first', '6', '-1', '123');
                INSERT INTO successes VALUES ('root', 'input', 'second', '6', '-1', '456');
                CREATE TABLE legacy_successes (history TEXT NOT NULL);
                INSERT INTO legacy_successes VALUES ('older-history');",
            )
            .unwrap();
        state
    }

    #[test]
    fn migration_preserves_per_rule_successes_and_existing_legacy_history() {
        let state = pre_events_state();
        let id = Identity {
            size: 6,
            modified_ns: -1,
        };
        for _ in 0..2 {
            let ledger = Ledger::open(state.path()).unwrap();
            for rule in ["first", "second"] {
                assert!(ledger.succeeded("root", "input", rule, id, 0).unwrap());
            }
            assert!(!ledger.succeeded("root", "input", "other", id, 0).unwrap());
            let records: Vec<(String, String, i64, String)> = ledger
                .connection
                .prepare("SELECT rule_id, succeeded_ns, event_id, event_kind FROM successes ORDER BY rule_id")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(
                records,
                vec![
                    (
                        "first".to_owned(),
                        "123".to_owned(),
                        0,
                        "created".to_owned()
                    ),
                    (
                        "second".to_owned(),
                        "456".to_owned(),
                        0,
                        "created".to_owned()
                    ),
                ]
            );
            assert_eq!(
                ledger
                    .connection
                    .query_row("SELECT history FROM legacy_successes", [], |row| row
                        .get::<_, String>(0))
                    .unwrap(),
                "older-history"
            );
        }
    }

    #[test]
    fn conflicting_per_rule_archive_refuses_migration_without_destroying_data() {
        let state = pre_events_state();
        let connection = Connection::open(state.path().join("ledger.sqlite3")).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE successes_before_events (history TEXT NOT NULL);
                 INSERT INTO successes_before_events VALUES ('existing-history');",
            )
            .unwrap();
        assert!(Ledger::open(state.path()).is_err());
        let records: Vec<(String, String, String, String, String, String)> = connection
            .prepare(
                "SELECT root_id, key, rule_id, size, modified_ns, succeeded_ns
                 FROM successes ORDER BY rule_id",
            )
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            records,
            vec![
                (
                    "root".to_owned(),
                    "input".to_owned(),
                    "first".to_owned(),
                    "6".to_owned(),
                    "-1".to_owned(),
                    "123".to_owned()
                ),
                (
                    "root".to_owned(),
                    "input".to_owned(),
                    "second".to_owned(),
                    "6".to_owned(),
                    "-1".to_owned(),
                    "456".to_owned()
                ),
            ]
        );
        for (table, expected) in [
            ("successes_before_events", "existing-history"),
            ("legacy_successes", "older-history"),
        ] {
            assert_eq!(
                connection
                    .query_row(&format!("SELECT history FROM {table}"), [], |row| row
                        .get::<_, String>(0),)
                    .unwrap(),
                expected
            );
        }
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
            assert!(!ledger.succeeded("root", "input", "rule", id, 0).unwrap());
            ledger
                .record("root", "input", "rule", id, 0, EventType::Created)
                .unwrap();
        }
        for _ in 0..2 {
            let ledger = Ledger::open(state.path()).unwrap();
            assert!(ledger.succeeded("root", "input", "rule", id, 0).unwrap());
            assert!(
                !ledger
                    .succeeded("root", "input", "other-rule", id, 0)
                    .unwrap()
            );
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
