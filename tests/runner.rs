use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Runner {
    child: Child,
    log: PathBuf,
}
impl Runner {
    fn start(config: &Path) -> Self {
        let log = config.parent().unwrap().join("runner.log");
        let startup_log = log.clone();
        let child = Command::new(env!("CARGO_BIN_EXE_fev"))
            .arg("--config")
            .arg(config)
            .stdin(Stdio::null())
            .env("OUT", "inherited-output")
            .env("OUT_TMP", "inherited-temporary")
            .env("MATCH_GHOST", "inherited-capture")
            .env("EVENT", "inherited-event")
            .env("OLD_KEY", "inherited-old-key")
            .env("OLD_FILE", "inherited-old-file")
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        let mut runner = Self { child, log };
        runner.wait(|| {
            fs::read_to_string(&startup_log).is_ok_and(|value| value.contains("fev: watching "))
        });
        runner
    }
    fn wait(&mut self, mut predicate: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !predicate() {
            let status = self.child.try_wait().unwrap();
            assert!(
                status.is_none(),
                "runner exited {status:?}: {}",
                fs::read_to_string(&self.log).unwrap()
            );
            assert!(
                Instant::now() < deadline,
                "runner timed out: {}",
                fs::read_to_string(&self.log).unwrap()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn stop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "shutdown failed: {}",
                    fs::read_to_string(&self.log).unwrap()
                );
                return;
            }
            assert!(Instant::now() < deadline, "shutdown timed out");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn contains(path: &Path, expected: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|value| value == expected)
}
fn rule(id: &str, matcher: &str, command: &str) -> String {
    event_rule(id, "startup, created, updated", matcher, command)
}
fn event_rule(id: &str, events: &str, matcher: &str, command: &str) -> String {
    let command: String = command
        .lines()
        .map(|line| format!("          {line}\n"))
        .collect();
    format!(
        "      - id: {id}\n        events: [{events}]\n        match: '{matcher}'\n        run: |\n{command}"
    )
}
fn configure(base: &Path, rules: &str, concurrency: usize) -> (PathBuf, PathBuf) {
    let root = base.join("root");
    fs::create_dir(&root).unwrap();
    let config = base.join("rules.yaml");
    fs::write(&config, format!("settle: 80ms\nconcurrency: {concurrency}\nstate: ./state\nroots:\n  - id: source\n    path: ./root\n    rules:\n{rules}")).unwrap();
    (root, config)
}
fn successes(base: &Path, rule: &str) -> i64 {
    let db = base.join("state/ledger.sqlite3");
    if !db.exists() {
        return 0;
    }
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM successes WHERE rule_id = ?1",
            [rule],
            |row| row.get(0),
        )
        .unwrap_or(0)
}

const EVENT_PATHS: &str = r#"test "$FILE" = "$ROOT/$KEY" || exit 31
case "$EVENT" in
  renamed) test -n "$OLD_KEY" && test "$OLD_FILE" = "$ROOT/$OLD_KEY" || exit 32 ;;
  *) test -z "$OLD_KEY" && test -z "$OLD_FILE" || exit 33 ;;
esac"#;

fn trace_lines(path: &Path) -> Vec<String> {
    let mut lines: Vec<_> = fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

fn chain() -> String {
    rule(
        "first",
        r"^(?P<name>[^/.]+)\.txt$",
        "cat \"$FILE\" > \"${MATCH_NAME}.middle\"",
    ) + &rule(
        "second",
        r"^(?P<name>[^/.]+)\.middle$",
        "mkdir -p nested\ncat \"$FILE\" > \"nested/${MATCH_NAME}.done\"",
    )
}

#[test]
fn outputless_rule_records_success_and_skips_restart() {
    let base = tempfile::tempdir().unwrap();
    let rules = rule(
        "check",
        r"^item\.txt$",
        "test \"$KEY\" = item.txt && test \"$PWD\" = \"$ROOT\" && test \"$(cat \"$FILE\")\" = source && test -z \"${OUT+x}\" && test -z \"${OUT_TMP+x}\" && test -z \"${MATCH_GHOST+x}\" && printf executed >> ../calls",
    );
    let (root, config) = configure(base.path(), &rules, 2);
    fs::write(root.join("item.txt"), "source").unwrap();
    let mut runner = Runner::start(&config);
    runner.wait(|| successes(base.path(), "check") == 1);
    runner.stop();
    let files: Vec<_> = fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(files, ["item.txt"]);
    let mut runner = Runner::start(&config);
    thread::sleep(Duration::from_millis(400));
    runner.stop();
    assert!(contains(&base.path().join("calls"), "executed"));
    assert_eq!(successes(base.path(), "check"), 1);
}

#[test]
fn startup_chain_restart_skip_and_live_same_size_update() {
    let base = tempfile::tempdir().unwrap();
    let (root, config) = configure(base.path(), &chain(), 2);
    fs::write(root.join("item.txt"), "first").unwrap();
    let output = root.join("nested/item.done");
    let mut runner = Runner::start(&config);
    runner.wait(|| contains(&output, "first") && successes(base.path(), "second") == 1);
    runner.stop();
    let modified = fs::metadata(&output).unwrap().modified().unwrap();
    let mut runner = Runner::start(&config);
    thread::sleep(Duration::from_millis(400));
    assert_eq!(fs::metadata(&output).unwrap().modified().unwrap(), modified);
    fs::write(root.join("item.txt"), "other").unwrap();
    runner.wait(|| contains(&output, "other"));
    fs::write(root.join("fresh.txt"), "fresh").unwrap();
    runner.wait(|| contains(&root.join("nested/fresh.done"), "fresh"));
    assert!(root.join("item.txt").exists());
    assert!(root.join("item.middle").exists());
    runner.stop();
}

#[test]
fn failed_event_waits_for_change_or_restart() {
    let base = tempfile::tempdir().unwrap();
    let rules = rule(
        "fail",
        r"^item\.txt$",
        "test -f ../allow || exit 7\ncat \"$FILE\" > item.done",
    );
    let (root, config) = configure(base.path(), &rules, 1);
    fs::write(root.join("item.txt"), "first").unwrap();
    let mut runner = Runner::start(&config);
    runner.wait(|| {
        fs::read_to_string(base.path().join("runner.log")).is_ok_and(|log| log.contains("failed"))
    });
    fs::write(base.path().join("allow"), "").unwrap();
    thread::sleep(Duration::from_millis(350));
    assert!(!root.join("item.done").exists());
    assert_eq!(successes(base.path(), "fail"), 0);
    fs::write(root.join("item.txt"), "changed").unwrap();
    runner.wait(|| contains(&root.join("item.done"), "changed"));
    runner.stop();
    fs::remove_file(base.path().join("allow")).unwrap();
    fs::write(root.join("item.txt"), "again").unwrap();
    let mut runner = Runner::start(&config);
    runner.wait(|| {
        fs::read_to_string(base.path().join("runner.log")).is_ok_and(|log| log.contains("failed"))
    });
    runner.stop();
    fs::write(base.path().join("allow"), "").unwrap();
    let mut runner = Runner::start(&config);
    runner.wait(|| contains(&root.join("item.done"), "again"));
    runner.stop();
}

#[test]
fn run_array_orders_steps_and_stops_only_the_failed_rule() {
    let base = tempfile::tempdir().unwrap();
    let rules = "      - id: sequence\n        events: [startup]\n        match: '^(?P<name>[^/.]+)\\.txt$'\n        run:\n          - 'printf first > \"${MATCH_NAME}.trace\"'\n          - 'test \"$(cat \"${MATCH_NAME}.trace\")\" = first && printf second >> \"${MATCH_NAME}.trace\"'\n          - 'exit 9'\n          - 'printf wrong > should-not-exist'\n".to_owned()
        + &rule("sibling", r"^item\.txt$", "printf sibling > sibling.done");
    let (root, config) = configure(base.path(), &rules, 2);
    fs::write(root.join("item.txt"), "source").unwrap();
    let mut runner = Runner::start(&config);
    runner.wait(|| {
        contains(&root.join("item.trace"), "firstsecond") && successes(base.path(), "sibling") == 1
    });
    thread::sleep(Duration::from_millis(300));
    assert!(!root.join("should-not-exist").exists());
    assert_eq!(successes(base.path(), "sequence"), 0);
    runner.stop();
}

#[test]
fn matching_rules_run_in_parallel_and_have_independent_success_records() {
    let base = tempfile::tempdir().unwrap();
    let mut rules = String::new();
    for (id, other) in [("a", "b"), ("b", "a")] {
        rules.push_str(&rule(id, r"^item\.txt$", &format!("mkdir ../started-{id}\ni=0\nwhile [ ! -d ../started-{other} ] && [ \"$i\" -lt 100 ]; do i=$((i+1)); sleep 0.02; done\ntest -d ../started-{other} || exit 8\nprintf overlap > {id}.done")));
    }
    let (root, config) = configure(base.path(), &rules, 2);
    fs::write(root.join("item.txt"), "source").unwrap();
    let mut runner = Runner::start(&config);
    runner.wait(|| {
        contains(&root.join("a.done"), "overlap") && contains(&root.join("b.done"), "overlap")
    });
    runner.wait(|| successes(base.path(), "a") == 1 && successes(base.path(), "b") == 1);
    runner.stop();
}

#[test]
fn adding_matching_rule_does_not_rerun_successful_sibling() {
    let base = tempfile::tempdir().unwrap();
    let first = rule("a", r"^item\.txt$", "printf a >> ../calls-a");
    let (root, config) = configure(base.path(), &first, 2);
    fs::write(root.join("item.txt"), "source").unwrap();
    let mut runner = Runner::start(&config);
    runner.wait(|| successes(base.path(), "a") == 1);
    runner.stop();
    let mut yaml = fs::read_to_string(&config).unwrap();
    yaml.push_str(&rule("b", r"^item\.txt$", "printf b >> ../calls-b"));
    fs::write(&config, yaml).unwrap();
    let mut runner = Runner::start(&config);
    runner.wait(|| successes(base.path(), "b") == 1);
    runner.stop();
    assert!(contains(&base.path().join("calls-a"), "a"));
    assert!(contains(&base.path().join("calls-b"), "b"));
}

#[test]
fn same_key_updates_coalesce_per_rule_without_overlapping_instances() {
    let base = tempfile::tempdir().unwrap();
    let mut rules = String::new();
    for id in ["a", "b"] {
        rules.push_str(&rule(id, r"^item\.txt$", &format!("mkdir ../guard-{id} || exit 91\nvalue=$(cat \"$FILE\")\nprintf '%s\\n' \"$value\" >> ../calls-{id}\nsleep 0.5\nrmdir ../guard-{id}")));
    }
    let (root, config) = configure(base.path(), &rules, 4);
    fs::write(root.join("item.txt"), "initial").unwrap();
    let mut runner = Runner::start(&config);
    runner.wait(|| {
        contains(&base.path().join("calls-a"), "initial\n")
            && contains(&base.path().join("calls-b"), "initial\n")
    });
    fs::write(root.join("item.txt"), "intermediate").unwrap();
    thread::sleep(Duration::from_millis(20));
    fs::write(root.join("item.txt"), "latest").unwrap();
    runner.wait(|| successes(base.path(), "a") == 2 && successes(base.path(), "b") == 2);
    runner.stop();
    assert!(contains(&base.path().join("calls-a"), "initial\nlatest\n"));
    assert!(contains(&base.path().join("calls-b"), "initial\nlatest\n"));
}

#[test]
fn concurrency_limit_is_shared_across_roots() {
    let base = tempfile::tempdir().unwrap();
    fs::create_dir(base.path().join("active")).unwrap();
    let mut roots = Vec::new();
    let mut yaml = String::from("settle: 30ms\nconcurrency: 2\nstate: ./state\nroots:\n");
    for index in 0..2 {
        let root = base.path().join(format!("root{index}"));
        fs::create_dir(&root).unwrap();
        for number in 0..3 {
            fs::write(
                root.join(format!("id{index}{number}.txt")),
                format!("{index}:{number}"),
            )
            .unwrap();
        }
        yaml.push_str(&format!(
            "  - id: r{index}\n    path: ./root{index}\n    rules:\n"
        ));
        yaml.push_str(&rule("slow", r"^(?P<name>[^/.]+)\.txt$", "mkdir \"../active/$MATCH_NAME\"\nsleep 0.4\ncat \"$FILE\" > \"${MATCH_NAME}.done\"\nrmdir \"../active/$MATCH_NAME\""));
        roots.push(root);
    }
    let config = base.path().join("rules.yaml");
    fs::write(&config, yaml).unwrap();
    let mut runner = Runner::start(&config);
    let mut peak = 0;
    runner.wait(|| {
        let active = fs::read_dir(base.path().join("active")).unwrap().count();
        assert!(active <= 2, "global concurrency exceeded: {active}");
        peak = peak.max(active);
        roots.iter().enumerate().all(|(index, root)| {
            (0..3).all(|number| {
                contains(
                    &root.join(format!("id{index}{number}.done")),
                    &format!("{index}:{number}"),
                )
            })
        })
    });
    assert_eq!(peak, 2);
    runner.stop();
}

#[test]
fn settle_discards_deleted_inputs_and_tracks_growing_files() {
    let base = tempfile::tempdir().unwrap();
    let (root, config) = configure(base.path(), &chain(), 1);
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("80ms", "500ms");
    fs::write(&config, text).unwrap();
    let mut runner = Runner::start(&config);
    fs::write(root.join("vanish.txt"), "temporary").unwrap();
    fs::remove_file(root.join("vanish.txt")).unwrap();
    fs::write(root.join("grow.txt"), "a").unwrap();
    thread::sleep(Duration::from_millis(250));
    fs::write(root.join("grow.txt"), "complete").unwrap();
    runner.wait(|| contains(&root.join("nested/grow.done"), "complete"));
    assert!(!root.join("vanish.middle").exists());
    runner.stop();
}

#[test]
fn symlink_inputs_are_ignored() {
    let base = tempfile::tempdir().unwrap();
    let rules = rule("copy", r"^item\.txt$", "cat \"$FILE\" > item.done");
    let (root, config) = configure(base.path(), &rules, 2);
    fs::write(base.path().join("outside"), "outside").unwrap();
    std::os::unix::fs::symlink(base.path().join("outside"), root.join("item.txt")).unwrap();
    let mut runner = Runner::start(&config);
    thread::sleep(Duration::from_millis(300));
    assert!(!root.join("item.done").exists());
    assert_eq!(successes(base.path(), "copy"), 0);
    runner.stop();
}

#[test]
fn moved_in_subtree_is_processed_recursively() {
    let base = tempfile::tempdir().unwrap();
    let rules = rule(
        "copy",
        r"^(?P<name>.+)\.txt$",
        "cat \"$FILE\" > \"${MATCH_NAME}.done\"",
    );
    let (root, config) = configure(base.path(), &rules, 2);
    let incoming = base.path().join("incoming/deep");
    fs::create_dir_all(&incoming).unwrap();
    fs::write(incoming.join("item.txt"), "moved").unwrap();
    let mut runner = Runner::start(&config);
    fs::rename(base.path().join("incoming"), root.join("incoming")).unwrap();
    runner.wait(|| contains(&root.join("incoming/deep/item.done"), "moved"));
    runner.stop();
}

#[test]
fn capture_values_preserve_case_and_shell_syntax_as_data() {
    let base = tempfile::tempdir().unwrap();
    let rules = rule(
        "capture",
        r"^(?P<name>[^/]+)\.txt$",
        "printf '%s' \"$MATCH_NAME\" > captured",
    );
    let (root, config) = configure(base.path(), &rules, 2);
    let name = "Weekly '$(touch injected)' report";
    fs::write(root.join(format!("{name}.txt")), "source").unwrap();
    let mut runner = Runner::start(&config);
    runner.wait(|| contains(&root.join("captured"), name));
    assert!(!root.join("injected").exists());
    runner.stop();
}

#[test]
fn startup_created_and_updated_filters_distinguish_initial_files_and_live_changes() {
    let base = tempfile::tempdir().unwrap();
    let mut rules = String::new();
    for event in ["startup", "created", "updated"] {
        rules.push_str(&event_rule(
            event,
            event,
            r"^(startup|fresh)\.txt$",
            &format!(
                "{EVENT_PATHS}\nprintf '%s|%s|%s\\n' \"$EVENT\" \"$KEY\" \"$(cat \"$FILE\")\" >> ../{event}"
            ),
        ));
    }
    let (root, config) = configure(base.path(), &rules, 2);
    fs::write(root.join("startup.txt"), "first").unwrap();
    let mut runner = Runner::start(&config);
    runner.wait(|| successes(base.path(), "startup") == 1);
    fs::write(root.join("startup.txt"), "other").unwrap();
    runner.wait(|| successes(base.path(), "updated") == 1);
    fs::write(root.join("fresh.txt"), "new").unwrap();
    runner.wait(|| successes(base.path(), "created") == 1);
    fs::write(root.join("fresh.txt"), "changed").unwrap();
    runner.wait(|| successes(base.path(), "updated") == 2);
    runner.stop();
    assert!(contains(
        &base.path().join("startup"),
        "startup|startup.txt|first\n"
    ));
    assert!(contains(
        &base.path().join("created"),
        "created|fresh.txt|new\n"
    ));
    assert!(contains(
        &base.path().join("updated"),
        "updated|startup.txt|other\nupdated|fresh.txt|changed\n"
    ));
}

#[test]
fn live_only_rules_ignore_initial_files_with_fresh_retained_and_missing_state() {
    let base = tempfile::tempdir().unwrap();
    let rules = event_rule(
        "live",
        "created, updated",
        r"^(item|marker-[0-9]+)\.txt$",
        &format!("{EVENT_PATHS}\nprintf '%s|%s\\n' \"$EVENT\" \"$KEY\" >> ../live"),
    );
    let (root, config) = configure(base.path(), &rules, 1);
    fs::write(root.join("item.txt"), "preexisting").unwrap();
    let calls = base.path().join("live");
    let mut expected = String::new();
    for restart in 0..3 {
        if restart == 2 {
            fs::remove_dir_all(base.path().join("state")).unwrap();
        }
        let mut runner = Runner::start(&config);
        fs::write(root.join(format!("marker-{restart}.txt")), "live").unwrap();
        expected.push_str(&format!("created|marker-{restart}.txt\n"));
        runner.wait(|| contains(&calls, &expected));
        thread::sleep(Duration::from_millis(250));
        runner.stop();
        assert!(contains(&calls, &expected));
    }
}

#[test]
fn startup_only_processes_existing_files_not_live_creations_and_skips_unchanged_restart() {
    let base = tempfile::tempdir().unwrap();
    let rules = event_rule(
        "initial",
        "startup",
        r"^(item|new)\.txt$",
        &format!(
            "{EVENT_PATHS}\nprintf '%s|%s|%s\\n' \"$EVENT\" \"$KEY\" \"$(cat \"$FILE\")\" >> ../initial"
        ),
    ) + &event_rule(
        "marker",
        "created",
        r"^marker-[0-9]+\.txt$",
        "printf '%s\\n' \"$KEY\" >> ../markers",
    );
    let (root, config) = configure(base.path(), &rules, 2);
    fs::write(root.join("item.txt"), "existing").unwrap();
    let calls = base.path().join("initial");
    let markers = base.path().join("markers");
    let mut runner = Runner::start(&config);
    runner.wait(|| successes(base.path(), "initial") == 1);
    fs::write(root.join("new.txt"), "new").unwrap();
    fs::write(root.join("marker-0.txt"), "live").unwrap();
    runner.wait(|| successes(base.path(), "marker") == 1);
    thread::sleep(Duration::from_millis(250));
    assert!(contains(&calls, "startup|item.txt|existing\n"));
    fs::remove_file(root.join("new.txt")).unwrap();
    runner.stop();

    let mut runner = Runner::start(&config);
    fs::write(root.join("marker-1.txt"), "live again").unwrap();
    runner.wait(|| successes(base.path(), "marker") == 2);
    thread::sleep(Duration::from_millis(250));
    runner.stop();
    assert!(contains(&calls, "startup|item.txt|existing\n"));
    assert!(contains(&markers, "marker-0.txt\nmarker-1.txt\n"));
    assert_eq!(successes(base.path(), "initial"), 1);
}

#[test]
fn enabling_startup_preserves_success_identity_of_a_live_created_input() {
    let base = tempfile::tempdir().unwrap();
    let rules = event_rule(
        "process",
        "created",
        r"^item\.txt$",
        &format!(
            "{EVENT_PATHS}\nprintf '%s|%s\\n' \"$EVENT\" \"$(cat \"$FILE\")\" >> ../processed"
        ),
    ) + &event_rule(
        "marker",
        "created, updated",
        r"^marker\.txt$",
        "printf live > ../marker",
    );
    let (root, config) = configure(base.path(), &rules, 2);
    let mut runner = Runner::start(&config);
    fs::write(root.join("item.txt"), "created live").unwrap();
    runner.wait(|| successes(base.path(), "process") == 1);
    runner.stop();

    let yaml = fs::read_to_string(&config)
        .unwrap()
        .replace("events: [created]", "events: [startup, created]");
    fs::write(&config, yaml).unwrap();
    let mut runner = Runner::start(&config);
    fs::write(root.join("marker.txt"), "watcher is live").unwrap();
    runner.wait(|| successes(base.path(), "marker") == 1);
    thread::sleep(Duration::from_millis(250));
    runner.stop();
    assert!(contains(
        &base.path().join("processed"),
        "created|created live\n"
    ));
    assert!(contains(&base.path().join("marker"), "live"));
    assert_eq!(successes(base.path(), "process"), 1);
}

#[test]
fn startup_opt_out_keeps_existing_updates_eligible_immediately_after_readiness() {
    let base = tempfile::tempdir().unwrap();
    let rules = event_rule(
        "live",
        "created, updated",
        r"^item\.txt$",
        &format!("{EVENT_PATHS}\nprintf '%s|%s\\n' \"$EVENT\" \"$(cat \"$FILE\")\" >> ../live"),
    );
    let (root, config) = configure(base.path(), &rules, 1);
    let yaml = fs::read_to_string(&config).unwrap().replace("80ms", "2s");
    fs::write(&config, yaml).unwrap();
    fs::write(root.join("item.txt"), "initial").unwrap();
    let mut runner = Runner::start(&config);
    // The initial inventory is ready, but the initial content has not settled.
    fs::write(root.join("item.txt"), "changed immediately").unwrap();
    runner.wait(|| successes(base.path(), "live") == 1);
    runner.stop();
    assert!(contains(
        &base.path().join("live"),
        "updated|changed immediately\n"
    ));
}

#[test]
fn initial_settle_update_preserves_independent_filters_and_coalesces_combined_rule() {
    let base = tempfile::tempdir().unwrap();
    let mut rules = String::new();
    for (id, events) in [
        ("initial", "startup"),
        ("live", "updated"),
        ("combined", "startup, updated"),
    ] {
        rules.push_str(&event_rule(
            id,
            events,
            r"^item\.txt$",
            &format!("{EVENT_PATHS}\nprintf '%s|%s\\n' \"$EVENT\" \"$(cat \"$FILE\")\" >> ../{id}"),
        ));
    }
    let (root, config) = configure(base.path(), &rules, 3);
    let yaml = fs::read_to_string(&config).unwrap().replace("80ms", "2s");
    fs::write(&config, yaml).unwrap();
    fs::write(root.join("item.txt"), "initial").unwrap();
    let mut runner = Runner::start(&config);
    fs::write(root.join("item.txt"), "latest material").unwrap();
    runner.wait(|| {
        successes(base.path(), "initial") == 1
            && successes(base.path(), "live") == 1
            && successes(base.path(), "combined") == 1
    });
    thread::sleep(Duration::from_millis(250));
    runner.stop();
    assert!(contains(
        &base.path().join("initial"),
        "startup|latest material\n"
    ));
    assert!(contains(
        &base.path().join("live"),
        "updated|latest material\n"
    ));
    assert!(contains(
        &base.path().join("combined"),
        "updated|latest material\n"
    ));
}

#[test]
fn deletion_runs_with_absent_file_and_repeated_material_survives_restart() {
    let base = tempfile::tempdir().unwrap();
    let rules = event_rule(
        "deletion",
        "deleted",
        r"^item\.txt$",
        &format!(
            "{EVENT_PATHS}\ntest ! -e \"$FILE\" || exit 34\nprintf '%s|%s|%s\\n' \"$EVENT\" \"$KEY\" \"$OLD_KEY\" >> ../deleted"
        ),
    );
    let (root, config) = configure(base.path(), &rules, 2);
    let input = root.join("item.txt");
    fs::write(&input, "same material").unwrap();
    let modified = fs::metadata(&input).unwrap().modified().unwrap();
    let mut runner = Runner::start(&config);
    fs::remove_file(&input).unwrap();
    runner.wait(|| successes(base.path(), "deletion") == 1);
    runner.stop();

    // Startup reestablishes the inventory; the restored material identity is identical.
    fs::write(&input, "same material").unwrap();
    fs::File::options()
        .write(true)
        .open(&input)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let mut runner = Runner::start(&config);
    fs::remove_file(&input).unwrap();
    runner.wait(|| successes(base.path(), "deletion") == 2);
    runner.stop();
    assert!(contains(
        &base.path().join("deleted"),
        "deleted|item.txt|\ndeleted|item.txt|\n"
    ));
    let db = rusqlite::Connection::open(base.path().join("state/ledger.sqlite3")).unwrap();
    let identities: (i64, i64, i64) = db
        .query_row(
            "SELECT COUNT(DISTINCT size || ':' || modified_ns), COUNT(DISTINCT event_id), MIN(event_id)
             FROM successes WHERE rule_id = 'deletion' AND event_kind = 'deleted'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(identities.0, 1);
    assert_eq!(identities.1, 2);
    assert!(identities.2 > 0);
}

#[test]
fn renamed_matches_and_captures_new_key_and_exposes_old_paths() {
    let base = tempfile::tempdir().unwrap();
    let rules = event_rule(
        "new-name",
        "renamed",
        r"^new/(?P<name>[^/]+)\.txt$",
        &format!(
            "{EVENT_PATHS}\ntest \"$MATCH_NAME\" = Different || exit 35\ntest ! -e \"$OLD_FILE\" || exit 36\ncat \"$FILE\" > ../renamed-content\nprintf '%s|%s|%s|%s\\n' \"$EVENT\" \"$KEY\" \"$OLD_KEY\" \"$MATCH_NAME\" > ../renamed"
        ),
    ) + &event_rule(
        "old-name",
        "renamed",
        r"^old/(?P<name>[^/]+)\.txt$",
        "printf wrong > ../matched-old-key",
    );
    let (root, config) = configure(base.path(), &rules, 2);
    fs::create_dir(root.join("old")).unwrap();
    fs::create_dir(root.join("new")).unwrap();
    fs::write(root.join("old/Original.txt"), "preserved").unwrap();
    let mut runner = Runner::start(&config);
    fs::rename(
        root.join("old/Original.txt"),
        root.join("new/Different.txt"),
    )
    .unwrap();
    runner.wait(|| successes(base.path(), "new-name") == 1);
    runner.stop();
    assert!(contains(&base.path().join("renamed-content"), "preserved"));
    assert!(contains(
        &base.path().join("renamed"),
        "renamed|new/Different.txt|old/Original.txt|Different\n"
    ));
    assert!(!base.path().join("matched-old-key").exists());
    assert_eq!(successes(base.path(), "old-name"), 0);
}

#[test]
fn directory_rename_and_delete_expand_regular_descendants() {
    let base = tempfile::tempdir().unwrap();
    let rules = event_rule(
        "descendants",
        "renamed, deleted",
        r"^(?P<dir>before|after)/(?P<name>.+)\.txt$",
        &format!(
            r#"{EVENT_PATHS}
case "$EVENT" in
  renamed) value=$(cat "$FILE") || exit 37 ;;
  deleted) test ! -e "$FILE" || exit 38; value=absent ;;
  *) exit 39 ;;
esac
printf '%s|%s|%s|%s|%s|%s\n' "$EVENT" "$KEY" "$OLD_KEY" "$MATCH_DIR" "$MATCH_NAME" "$value" >> ../descendants"#
        ),
    );
    let (root, config) = configure(base.path(), &rules, 1);
    fs::create_dir_all(root.join("before/deep/empty")).unwrap();
    fs::write(root.join("before/a.txt"), "alpha").unwrap();
    fs::write(root.join("before/deep/b.txt"), "beta").unwrap();
    let mut runner = Runner::start(&config);
    fs::rename(root.join("before"), root.join("after")).unwrap();
    runner.wait(|| successes(base.path(), "descendants") == 2);
    assert_eq!(
        trace_lines(&base.path().join("descendants")),
        [
            "renamed|after/a.txt|before/a.txt|after|a|alpha",
            "renamed|after/deep/b.txt|before/deep/b.txt|after|deep/b|beta",
        ]
    );
    fs::remove_dir_all(root.join("after")).unwrap();
    runner.wait(|| successes(base.path(), "descendants") == 4);
    runner.stop();
    assert_eq!(
        trace_lines(&base.path().join("descendants")),
        [
            "deleted|after/a.txt||after|a|absent",
            "deleted|after/deep/b.txt||after|deep/b|absent",
            "renamed|after/a.txt|before/a.txt|after|a|alpha",
            "renamed|after/deep/b.txt|before/deep/b.txt|after|deep/b|beta",
        ]
    );
}

#[test]
fn queued_transitions_survive_disappearance_and_serialize_across_event_kinds() {
    let base = tempfile::tempdir().unwrap();
    let rules = event_rule(
        "primary",
        "startup, created, updated, renamed, deleted",
        r"^item\.txt$",
        &format!(
            r#"{EVENT_PATHS}
mkdir ../primary-active || {{ printf '%s\n' "$EVENT" >> ../overlap; exit 40; }}
printf 'start|%s|%s|%s\n' "$EVENT" "$KEY" "$OLD_KEY" >> ../primary
case "$EVENT" in
  startup)
    cat "$FILE" > ../initial-content || exit 41
    i=0
    while [ ! -e ../release-primary ] && [ "$i" -lt 3000 ]; do i=$((i+1)); sleep 0.02; done
    test -e ../release-primary || exit 47
    ;;
  renamed|deleted) test ! -e "$FILE" || exit 42 ;;
  *) exit 43 ;;
esac
printf 'end|%s|%s|%s\n' "$EVENT" "$KEY" "$OLD_KEY" >> ../primary
rmdir ../primary-active"#
        ),
    ) + &event_rule(
        "observer",
        "startup, created, updated, renamed, deleted",
        r"^(item|away)\.txt$",
        &format!(
            "{EVENT_PATHS}\nprintf '%s|%s|%s\\n' \"$EVENT\" \"$KEY\" \"$OLD_KEY\" >> ../observer"
        ),
    );
    let (root, config) = configure(base.path(), &rules, 4);
    fs::write(root.join("item.txt"), "initial").unwrap();
    let mut runner = Runner::start(&config);
    let observer = base.path().join("observer");
    let mut observed = String::from("startup|item.txt|\n");
    runner.wait(|| {
        contains(&observer, &observed)
            && contains(&base.path().join("primary"), "start|startup|item.txt|\n")
            && contains(&base.path().join("initial-content"), "initial")
            && successes(base.path(), "observer") == 1
    });

    // An independently executing rule acknowledges each callback before the next move.
    // The primary's settled update becomes stale, but its metadata transitions must survive.
    fs::write(root.join("item.txt"), "latest").unwrap();
    observed.push_str("updated|item.txt|\n");
    runner.wait(|| contains(&observer, &observed) && successes(base.path(), "observer") == 2);
    fs::remove_file(root.join("item.txt")).unwrap();
    observed.push_str("deleted|item.txt|\n");
    runner.wait(|| contains(&observer, &observed) && successes(base.path(), "observer") == 3);
    fs::write(root.join("item.txt"), "restored material").unwrap();
    observed.push_str("created|item.txt|\n");
    runner.wait(|| contains(&observer, &observed) && successes(base.path(), "observer") == 4);
    for cycle in 0..2 {
        fs::rename(root.join("item.txt"), root.join("away.txt")).unwrap();
        observed.push_str("renamed|away.txt|item.txt\n");
        runner.wait(|| {
            contains(&observer, &observed) && successes(base.path(), "observer") == 5 + cycle * 2
        });
        fs::rename(root.join("away.txt"), root.join("item.txt")).unwrap();
        observed.push_str("renamed|item.txt|away.txt\n");
        runner.wait(|| {
            contains(&observer, &observed) && successes(base.path(), "observer") == 6 + cycle * 2
        });
    }
    fs::remove_file(root.join("item.txt")).unwrap();
    observed.push_str("deleted|item.txt|\n");
    runner.wait(|| contains(&observer, &observed) && successes(base.path(), "observer") == 9);
    assert!(contains(
        &base.path().join("primary"),
        "start|startup|item.txt|\n"
    ));
    fs::write(base.path().join("release-primary"), "").unwrap();
    runner.wait(|| successes(base.path(), "primary") == 5);
    runner.stop();
    assert!(contains(
        &base.path().join("primary"),
        concat!(
            "start|startup|item.txt|\nend|startup|item.txt|\n",
            "start|deleted|item.txt|\nend|deleted|item.txt|\n",
            "start|renamed|item.txt|away.txt\nend|renamed|item.txt|away.txt\n",
            "start|renamed|item.txt|away.txt\nend|renamed|item.txt|away.txt\n",
            "start|deleted|item.txt|\nend|deleted|item.txt|\n",
        )
    ));
    assert!(!base.path().join("primary-active").exists());
    assert!(!base.path().join("overlap").exists());
}

#[test]
fn moves_between_watched_roots_are_deleted_and_created_not_renamed() {
    let base = tempfile::tempdir().unwrap();
    let mut yaml = String::from("settle: 80ms\nconcurrency: 2\nstate: ./state\nroots:\n");
    for id in ["left", "right"] {
        fs::create_dir(base.path().join(id)).unwrap();
        yaml.push_str(&format!("  - id: {id}\n    path: ./{id}\n    rules:\n"));
        yaml.push_str(&event_rule(
            id,
            "startup, created, updated, deleted, renamed",
            r"^item\.txt$",
            &format!(
                r#"{EVENT_PATHS}
case "$EVENT" in
  startup|created|updated) cat "$FILE" > ../{id}-content || exit 44 ;;
  deleted) test ! -e "$FILE" || exit 45 ;;
  *) exit 46 ;;
esac
printf '%s|%s|%s\n' "$EVENT" "$KEY" "$OLD_KEY" >> ../{id}-events"#
            ),
        ));
    }
    let config = base.path().join("rules.yaml");
    fs::write(&config, yaml).unwrap();
    fs::write(base.path().join("left/item.txt"), "cross-root").unwrap();
    let mut runner = Runner::start(&config);
    runner.wait(|| successes(base.path(), "left") == 1);
    fs::rename(
        base.path().join("left/item.txt"),
        base.path().join("right/item.txt"),
    )
    .unwrap();
    runner.wait(|| successes(base.path(), "left") == 2 && successes(base.path(), "right") == 1);
    runner.stop();
    assert!(contains(
        &base.path().join("left-events"),
        "startup|item.txt|\ndeleted|item.txt|\n"
    ));
    assert!(contains(
        &base.path().join("right-events"),
        "created|item.txt|\n"
    ));
    assert!(contains(&base.path().join("right-content"), "cross-root"));
}
