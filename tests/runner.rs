use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct Runner {
    child: Child,
}
impl Runner {
    fn start(config: &Path) -> Self {
        Self {
            child: Command::new(env!("CARGO_BIN_EXE_fev"))
                .arg("--config")
                .arg(config)
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        }
    }
    fn stop(&mut self) {
        unsafe {
            libc::kill(self.child.id() as i32, libc::SIGTERM);
        }
        wait_for(|| self.child.try_wait().unwrap().is_some());
    }
}
impl Drop for Runner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn wait_for(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "runner did not reach expected state"
        );
        thread::sleep(Duration::from_millis(20));
    }
}
fn contains(path: &Path, expected: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|value| value == expected)
}
fn configure(base: &Path, rules: &str, concurrency: usize) -> (PathBuf, PathBuf) {
    let root = base.join("root");
    fs::create_dir(&root).unwrap();
    let config = base.join("rules.yaml");
    fs::write(&config, format!("settle: 80ms\nconcurrency: {concurrency}\nstate: ./state\nroots:\n  - id: source\n    path: ./root\n    rules:\n{rules}")).unwrap();
    (root, config)
}
const CHAIN: &str = "      - id: first\n        events: [created]\n        match: '^(?P<name>[^/.]+)\\.txt$'\n        run: 'cat \"$FILE\" > \"$OUT_TMP\"'\n        out: '{name}.middle'\n      - id: second\n        events: [created]\n        match: '^(?P<name>[^/.]+)\\.middle$'\n        run: 'cat \"$FILE\" > \"$OUT_TMP\"'\n        out: 'nested/{name}.done'\n";

#[test]
fn startup_chain_restart_skip_and_live_same_size_update() {
    let base = tempfile::tempdir().unwrap();
    let (root, config) = configure(base.path(), CHAIN, 2);
    fs::write(root.join("item.txt"), "first").unwrap();
    let output = root.join("nested/item.done");
    let mut runner = Runner::start(&config);
    wait_for(|| contains(&output, "first"));
    runner.stop();
    let modified = fs::metadata(&output).unwrap().modified().unwrap();
    let mut runner = Runner::start(&config);
    thread::sleep(Duration::from_millis(500));
    assert_eq!(fs::metadata(&output).unwrap().modified().unwrap(), modified);
    fs::write(root.join("item.txt"), "other").unwrap();
    wait_for(|| contains(&output, "other"));
    fs::write(root.join("fresh.txt"), "fresh").unwrap();
    wait_for(|| contains(&root.join("nested/fresh.done"), "fresh"));
    assert!(root.join("item.txt").exists());
    assert!(root.join("item.middle").exists());
    runner.stop();
}

#[test]
fn failed_event_waits_for_change_or_restart() {
    let base = tempfile::tempdir().unwrap();
    let rules = "      - id: fail\n        events: [created]\n        match: '^item\\.txt$'\n        run: 'cat \"$FILE\" > \"$OUT_TMP\"; test -f ../allow'\n        out: 'item.done'\n";
    let (root, config) = configure(base.path(), rules, 1);
    fs::write(root.join("item.txt"), "first").unwrap();
    let mut runner = Runner::start(&config);
    thread::sleep(Duration::from_millis(700));
    fs::write(base.path().join("allow"), "").unwrap();
    thread::sleep(Duration::from_millis(500));
    assert!(!root.join("item.done").exists());
    fs::write(root.join("item.txt"), "changed").unwrap();
    wait_for(|| contains(&root.join("item.done"), "changed"));
    runner.stop();
    fs::remove_file(base.path().join("allow")).unwrap();
    fs::write(root.join("item.txt"), "again").unwrap();
    let mut runner = Runner::start(&config);
    thread::sleep(Duration::from_millis(500));
    runner.stop();
    fs::write(base.path().join("allow"), "").unwrap();
    let mut runner = Runner::start(&config);
    wait_for(|| contains(&root.join("item.done"), "again"));
    runner.stop();
}

#[test]
fn settle_discards_deleted_inputs_and_tracks_growing_files() {
    let base = tempfile::tempdir().unwrap();
    let (root, config) = configure(base.path(), CHAIN, 1);
    let text = fs::read_to_string(&config)
        .unwrap()
        .replace("80ms", "500ms");
    fs::write(&config, text).unwrap();
    let mut runner = Runner::start(&config);
    thread::sleep(Duration::from_millis(300));
    fs::write(root.join("vanish.txt"), "temporary").unwrap();
    fs::remove_file(root.join("vanish.txt")).unwrap();
    fs::write(root.join("grow.txt"), "a").unwrap();
    thread::sleep(Duration::from_millis(250));
    fs::write(root.join("grow.txt"), "complete").unwrap();
    wait_for(|| contains(&root.join("nested/grow.done"), "complete"));
    assert!(!root.join("vanish.middle").exists());
    runner.stop();
}

#[test]
fn in_flight_changes_are_coalesced_without_same_key_overlap() {
    let base = tempfile::tempdir().unwrap();
    let rules = "      - id: slow\n        events: [created]\n        match: '^item\\.txt$'\n        run: 'cat \"$FILE\" > \"$OUT_TMP\"; sleep 1'\n        out: 'item.done'\n";
    let (root, config) = configure(base.path(), rules, 4);
    fs::write(root.join("item.txt"), "initial").unwrap();
    let mut runner = Runner::start(&config);
    wait_for(|| {
        fs::read_dir(base.path().join("state"))
            .is_ok_and(|entries| entries.flatten().any(|entry| entry.path().is_dir()))
    });
    thread::sleep(Duration::from_millis(150));
    fs::write(root.join("item.txt"), "intermediate").unwrap();
    thread::sleep(Duration::from_millis(40));
    fs::write(root.join("item.txt"), "latest").unwrap();
    wait_for(|| contains(&root.join("item.done"), "initial"));
    let first = Instant::now();
    wait_for(|| contains(&root.join("item.done"), "latest"));
    assert!(
        first.elapsed() >= Duration::from_millis(700),
        "same-key commands overlapped"
    );
    let db = rusqlite::Connection::open(base.path().join("state/ledger.sqlite3")).unwrap();
    let count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM successes WHERE key = 'item.txt'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
    runner.stop();
}

#[test]
fn ignores_ambiguous_events_and_symlink_inputs() {
    let base = tempfile::tempdir().unwrap();
    let rules = "      - id: a\n        events: [created]\n        match: '^item\\.txt$'\n        run: 'cat \"$FILE\" > \"$OUT_TMP\"'\n        out: 'a.done'\n      - id: b\n        events: [created]\n        match: '^item\\.txt$'\n        run: 'cat \"$FILE\" > \"$OUT_TMP\"'\n        out: 'b.done'\n";
    let (root, config) = configure(base.path(), rules, 2);
    fs::write(root.join("item.txt"), "input").unwrap();
    let mut runner = Runner::start(&config);
    thread::sleep(Duration::from_millis(500));
    assert!(!root.join("a.done").exists());
    assert!(!root.join("b.done").exists());
    runner.stop();
    fs::remove_file(root.join("item.txt")).unwrap();
    fs::write(base.path().join("outside"), "outside").unwrap();
    std::os::unix::fs::symlink(base.path().join("outside"), root.join("item.txt")).unwrap();
    let config_text = fs::read_to_string(&config).unwrap();
    fs::write(&config, config_text.split("      - id: b").next().unwrap()).unwrap();
    let mut runner = Runner::start(&config);
    thread::sleep(Duration::from_millis(500));
    assert!(!root.join("a.done").exists());
    runner.stop();
}

#[test]
fn concurrency_limit_is_shared_across_roots() {
    let base = tempfile::tempdir().unwrap();
    let mut roots = Vec::new();
    let mut yaml = String::from("settle: 30ms\nconcurrency: 2\nstate: ./state\nroots:\n");
    for index in 0..2 {
        let root = base.path().join(format!("root{index}"));
        fs::create_dir(&root).unwrap();
        for number in 0..3 {
            fs::write(
                root.join(format!("{number}.txt")),
                format!("{index}:{number}"),
            )
            .unwrap();
        }
        yaml.push_str(&format!("  - id: r{index}\n    path: ./root{index}\n    rules:\n      - id: slow\n        events: [created]\n        match: '^(?P<name>[0-9]+)\\.txt$'\n        run: 'cat \"$FILE\" > \"$OUT_TMP\"; sleep 1'\n        out: '{{name}}.done'\n"));
        roots.push(root);
    }
    let config = base.path().join("rules.yaml");
    fs::write(&config, yaml).unwrap();
    let mut runner = Runner::start(&config);
    let mut peak = 0;
    wait_for(|| {
        let active = fs::read_dir(base.path().join("state"))
            .map(|entries| {
                entries
                    .flatten()
                    .filter(|entry| entry.path().is_dir())
                    .count()
            })
            .unwrap_or(0);
        assert!(active <= 2, "global concurrency exceeded: {active}");
        peak = peak.max(active);
        roots.iter().enumerate().all(|(index, root)| {
            (0..3).all(|number| {
                contains(
                    &root.join(format!("{number}.done")),
                    &format!("{index}:{number}"),
                )
            })
        })
    });
    assert_eq!(peak, 2);
    runner.stop();
}

#[test]
fn moved_in_subtree_is_processed_recursively() {
    let base = tempfile::tempdir().unwrap();
    let rules = "      - id: copy\n        events: [created]\n        match: '^(?P<name>.+)\\.txt$'\n        run: 'cat \"$FILE\" > \"$OUT_TMP\"'\n        out: '{name}.done'\n";
    let (root, config) = configure(base.path(), rules, 2);
    let incoming = base.path().join("incoming/deep");
    fs::create_dir_all(&incoming).unwrap();
    fs::write(incoming.join("item.txt"), "moved").unwrap();
    let mut runner = Runner::start(&config);
    thread::sleep(Duration::from_millis(300));
    fs::rename(base.path().join("incoming"), root.join("incoming")).unwrap();
    wait_for(|| contains(&root.join("incoming/deep/item.done"), "moved"));
    runner.stop();
}
