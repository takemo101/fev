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
        let child = Command::new(env!("CARGO_BIN_EXE_fev"))
            .arg("--config")
            .arg(config)
            .stdin(Stdio::null())
            .env("OUT", "inherited-output")
            .env("OUT_TMP", "inherited-temporary")
            .env("MATCH_GHOST", "inherited-capture")
            .stdout(Stdio::null())
            .stderr(fs::File::create(&log).unwrap())
            .spawn()
            .unwrap();
        Self { child, log }
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
    let command: String = command
        .lines()
        .map(|line| format!("          {line}\n"))
        .collect();
    format!(
        "      - id: {id}\n        events: [created]\n        match: '{matcher}'\n        run: |\n{command}"
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
    let rules = "      - id: sequence\n        events: [created]\n        match: '^(?P<name>[^/.]+)\\.txt$'\n        run:\n          - 'printf first > \"${MATCH_NAME}.trace\"'\n          - 'test \"$(cat \"${MATCH_NAME}.trace\")\" = first && printf second >> \"${MATCH_NAME}.trace\"'\n          - 'exit 9'\n          - 'printf wrong > should-not-exist'\n".to_owned()
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
    thread::sleep(Duration::from_millis(250));
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
    runner.wait(|| base.path().join("state/ledger.sqlite3").exists());
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
    runner.wait(|| base.path().join("state/ledger.sqlite3").exists());
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
