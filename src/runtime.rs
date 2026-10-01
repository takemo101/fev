use crate::{
    config::Config,
    execution::{self, Ledger},
    model::{self, Identity},
};
use anyhow::{Context, Result, bail};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::Instant,
};
use walkdir::WalkDir;

type Location = (usize, String);
struct Settling {
    size: u64,
    deadline: Instant,
}
#[derive(Default)]
struct RuleState {
    ready: Option<Identity>,
    running: Option<Identity>,
    attempted: Option<Identity>,
}
#[derive(Default)]
struct Slot {
    settling: Option<Settling>,
    observed: Option<Identity>,
    rules: HashMap<usize, RuleState>,
}
impl Slot {
    fn discard_pending(&mut self) {
        self.settling = None;
        self.observed = None;
        for state in self.rules.values_mut() {
            state.ready = None;
        }
    }
}
struct Job {
    location: Location,
    rule: usize,
    input: Identity,
    captures: Vec<(String, String)>,
}
enum Message {
    Watch(notify::Result<Event>),
    Finished {
        location: Location,
        rule: usize,
        input: Identity,
        result: Result<()>,
    },
    Stop,
}
struct Scheduler {
    config: Arc<Config>,
    ledger: Ledger,
    slots: HashMap<Location, Slot>,
    queue: VecDeque<Job>,
    active: usize,
    stopping: bool,
    tx: Sender<Message>,
    rx: Receiver<Message>,
}

pub fn run(config: Config) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let config = Arc::new(config);
    let ledger = Ledger::open(&config.state)?;
    let watch_tx = tx.clone();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |event| {
        let _ = watch_tx.send(Message::Watch(event));
    })
    .context("creating native filesystem watcher")?;
    for root in &config.roots {
        watcher
            .watch(&root.path, RecursiveMode::Recursive)
            .with_context(|| format!("watching {}", root.path.display()))?;
    }
    let stop_tx = tx.clone();
    ctrlc::set_handler(move || {
        let _ = stop_tx.send(Message::Stop);
    })
    .context("installing termination handler")?;
    let mut scheduler = Scheduler {
        config,
        ledger,
        slots: HashMap::new(),
        queue: VecDeque::new(),
        active: 0,
        stopping: false,
        tx,
        rx,
    };
    for index in 0..scheduler.config.roots.len() {
        let root = scheduler.config.roots[index].path.clone();
        scheduler.scan(index, &root)?;
    }
    eprintln!(
        "fev: watching {} root(s), concurrency {}",
        scheduler.config.roots.len(),
        scheduler.config.concurrency
    );
    scheduler.event_loop()
}

impl Scheduler {
    fn scan(&mut self, root: usize, directory: &Path) -> Result<()> {
        for entry in WalkDir::new(directory).follow_links(false) {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error)
                    if error.io_error().is_some_and(|e| {
                        matches!(
                            e.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                        )
                    }) =>
                {
                    continue;
                }
                Err(error) => return Err(error).context("walking introduced files"),
            };
            if entry.file_type().is_file() {
                self.path_event(root, entry.path())?;
            }
        }
        Ok(())
    }

    fn path_event(&mut self, root: usize, path: &Path) -> Result<()> {
        let relative = match path.strip_prefix(&self.config.roots[root].path) {
            Ok(relative) => relative,
            Err(_) => return Ok(()),
        };
        let Some(key) = relative.to_str() else {
            eprintln!("fev: ignoring non-UTF-8 path {}", path.display());
            return Ok(());
        };
        if !model::valid_key(key) {
            return Ok(());
        }
        let location = (root, key.to_owned());
        if let Some(id) = model::identity(&self.config.roots[root].path, key)? {
            self.settle(location, id);
        } else if let Some(slot) = self.slots.get_mut(&location) {
            slot.discard_pending();
        }
        Ok(())
    }

    fn settle(&mut self, location: Location, id: Identity) {
        let slot = self.slots.entry(location).or_default();
        if slot.observed == Some(id)
            && slot.rules.values().all(|state| {
                state.ready == Some(id) || state.running == Some(id) || state.attempted == Some(id)
            })
        {
            return;
        }
        match &mut slot.settling {
            Some(pending) if pending.size == id.size => {}
            _ => {
                slot.settling = Some(Settling {
                    size: id.size,
                    deadline: Instant::now() + self.config.settle,
                })
            }
        }
        // A newer notification invalidates an older queued event, even before it settles.
        for state in slot.rules.values_mut() {
            state.ready = None;
        }
    }

    fn ready(&mut self, location: Location, id: Identity) {
        let slot = self.slots.entry(location.clone()).or_default();
        slot.settling = None;
        slot.observed = Some(id);
        for (index, rule) in self.config.roots[location.0].rules.iter().enumerate() {
            let Some(captures) = rule.environment(&location.1) else {
                continue;
            };
            let state = slot.rules.entry(index).or_default();
            if state.ready == Some(id) || state.attempted == Some(id) || state.running == Some(id) {
                continue;
            }
            state.ready = Some(id);
            self.queue.push_back(Job {
                location: location.clone(),
                rule: index,
                input: id,
                captures,
            });
        }
    }

    fn settle_due(&mut self) -> Result<()> {
        let now = Instant::now();
        let due: Vec<_> = self
            .slots
            .iter()
            .filter_map(|(location, slot)| {
                slot.settling
                    .as_ref()
                    .filter(|pending| pending.deadline <= now)
                    .map(|pending| (location.clone(), pending.size))
            })
            .collect();
        for (location, size) in due {
            match model::identity(&self.config.roots[location.0].path, &location.1)? {
                Some(id) if id.size == size => self.ready(location, id),
                Some(id) => {
                    self.slots.get_mut(&location).unwrap().settling = None;
                    self.settle(location, id);
                }
                None => {
                    let slot = self.slots.get_mut(&location).unwrap();
                    slot.discard_pending();
                }
            }
        }
        Ok(())
    }

    fn dispatch(&mut self) -> Result<()> {
        if self.stopping {
            return Ok(());
        }
        let count = self.queue.len();
        for _ in 0..count {
            if self.active >= self.config.concurrency {
                break;
            }
            let Some(job) = self.queue.pop_front() else {
                break;
            };
            let state = self
                .slots
                .get_mut(&job.location)
                .unwrap()
                .rules
                .get_mut(&job.rule)
                .unwrap();
            if state.ready != Some(job.input) {
                continue;
            }
            if state.running.is_some() {
                self.queue.push_back(job);
                continue;
            }
            state.ready = None;
            // Jobs can wait behind other rules; never run a stale input version.
            let current =
                model::identity(&self.config.roots[job.location.0].path, &job.location.1)?;
            if current != Some(job.input) {
                if let Some(current) = current {
                    self.settle(job.location, current);
                } else {
                    self.slots.get_mut(&job.location).unwrap().discard_pending();
                }
                continue;
            }
            if state.attempted == Some(job.input) {
                continue;
            }
            state.attempted = Some(job.input);
            let root = &self.config.roots[job.location.0];
            let rule = &root.rules[job.rule];
            if self
                .ledger
                .succeeded(&root.id, &job.location.1, &rule.id, job.input)?
            {
                continue;
            }
            eprintln!(
                "fev: running {}/{} rule {}",
                root.id, job.location.1, rule.id
            );
            state.running = Some(job.input);
            self.active += 1;
            let config = self.config.clone();
            let tx = self.tx.clone();
            thread::Builder::new()
                .name("fev-command".into())
                .spawn(move || {
                    let root = &config.roots[job.location.0];
                    let result = execution::execute(
                        &root.path,
                        &job.location.1,
                        &root.rules[job.rule].run,
                        &job.captures,
                    );
                    let _ = tx.send(Message::Finished {
                        location: job.location,
                        rule: job.rule,
                        input: job.input,
                        result,
                    });
                })
                .context("starting command worker")?;
        }
        Ok(())
    }

    fn discard(&mut self, root: usize, path: &Path) {
        let Ok(relative) = path.strip_prefix(&self.config.roots[root].path) else {
            return;
        };
        let Some(key) = relative.to_str() else {
            return;
        };
        for ((index, existing), slot) in &mut self.slots {
            if *index == root
                && (key.is_empty()
                    || existing == key
                    || existing
                        .strip_prefix(key)
                        .is_some_and(|suffix| suffix.starts_with('/')))
            {
                slot.discard_pending();
            }
        }
    }

    fn notification(&mut self, event: Event) -> Result<()> {
        if event.need_rescan() {
            bail!("native watcher lost events; restart to recover via startup scan");
        }
        if self.stopping
            || !matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
            )
        {
            return Ok(());
        }
        for (index, path) in event.paths.into_iter().enumerate() {
            let root = self
                .config
                .roots
                .iter()
                .position(|root| path.starts_with(&root.path));
            let Some(root) = root else {
                continue;
            };
            if matches!(
                event.kind,
                EventKind::Remove(_)
                    | EventKind::Modify(notify::event::ModifyKind::Name(
                        notify::event::RenameMode::From
                    ))
            ) || (index == 0
                && matches!(
                    event.kind,
                    EventKind::Modify(notify::event::ModifyKind::Name(
                        notify::event::RenameMode::Both
                    ))
                ))
            {
                self.discard(root, &path);
                continue;
            }
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    ) =>
                {
                    self.discard(root, &path);
                    continue;
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("reading notification {}", path.display()));
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                // Native backends can report a moved-in tree as a directory only.
                // Never rescan a root on generic directory modification notifications.
                if path != self.config.roots[root].path
                    && matches!(
                        event.kind,
                        EventKind::Create(_)
                            | EventKind::Modify(notify::event::ModifyKind::Name(_))
                    )
                {
                    self.scan(root, &path)?;
                }
            } else {
                self.path_event(root, &path)?;
            }
        }
        Ok(())
    }

    fn finished(
        &mut self,
        location: Location,
        rule_index: usize,
        input: Identity,
        result: Result<()>,
    ) -> Result<()> {
        self.active -= 1;
        self.slots
            .get_mut(&location)
            .unwrap()
            .rules
            .get_mut(&rule_index)
            .unwrap()
            .running = None;
        let root = &self.config.roots[location.0];
        let rule = &root.rules[rule_index];
        match result {
            Ok(()) => {
                self.ledger.record(&root.id, &location.1, &rule.id, input)?;
                eprintln!("fev: completed {}/{} rule {}", root.id, location.1, rule.id);
            }
            Err(error) => eprintln!(
                "fev: failed {}/{} rule {}: {error:#}",
                root.id, location.1, rule.id
            ),
        }
        if !self.stopping
            && let Some(current) =
                model::identity(&self.config.roots[location.0].path, &location.1)?
            && current != input
        {
            self.settle(location, current);
        }
        Ok(())
    }

    fn event_loop(&mut self) -> Result<()> {
        loop {
            if !self.stopping {
                self.settle_due()?;
                self.dispatch()?;
            } else if self.active == 0 {
                return Ok(());
            }
            let deadline = (!self.stopping)
                .then(|| {
                    self.slots
                        .values()
                        .filter_map(|slot| slot.settling.as_ref().map(|pending| pending.deadline))
                        .min()
                })
                .flatten();
            let message = match deadline {
                Some(deadline) => match self
                    .rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                {
                    Ok(message) => message,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => {
                        bail!("event channel disconnected")
                    }
                },
                None => self.rx.recv().context("waiting for filesystem events")?,
            };
            match message {
                Message::Watch(Ok(event)) => self.notification(event)?,
                Message::Watch(Err(error)) => {
                    return Err(error)
                        .context("native watcher failed; restart to recover via startup scan");
                }
                Message::Finished {
                    location,
                    input,
                    rule,
                    result,
                } => self.finished(location, rule, input, result)?,
                Message::Stop => {
                    self.stopping = true;
                    eprintln!("fev: stopping; waiting for {} command(s)", self.active);
                }
            }
        }
    }
}

#[cfg(test)]
mod regression_tests {
    use super::*;
    use crate::config::{Root, Rule};
    use notify::event::{CreateKind, RemoveKind};
    use std::{fs, time::Duration};

    pub(super) fn fixture(settle: Duration) -> (tempfile::TempDir, Scheduler) {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("root");
        let state = base.path().join("state");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&state).unwrap();
        let (tx, rx) = mpsc::channel();
        let ledger = Ledger::open(&state).unwrap();
        let config = Arc::new(Config {
            settle,
            concurrency: 2,
            state,
            roots: vec![Root {
                id: "source".into(),
                path: root,
                rules: vec![Rule {
                    id: "copy".into(),
                    matcher: regex::Regex::new(r"^(?P<name>.+)\.txt$").unwrap(),
                    run: vec!["cat \"$FILE\" > \"${MATCH_NAME}.done\"".into()],
                    capture_env: vec![(1, "MATCH_NAME".into())],
                }],
            }],
        });
        (
            base,
            Scheduler {
                config,
                ledger,
                slots: HashMap::new(),
                queue: VecDeque::new(),
                active: 0,
                stopping: false,
                tx,
                rx,
            },
        )
    }

    pub(super) fn complete(scheduler: &mut Scheduler) {
        let message = scheduler.rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let Message::Finished {
            location,
            input,
            rule,
            result,
        } = message
        else {
            panic!("unexpected message");
        };
        scheduler.finished(location, rule, input, result).unwrap();
    }

    fn removal_restarts_settle(directory: bool) {
        let (_base, mut scheduler) = fixture(Duration::from_millis(200));
        let root = scheduler.config.roots[0].path.clone();
        let key = if directory {
            "sub/item.txt"
        } else {
            "item.txt"
        };
        if directory {
            fs::create_dir(root.join("sub")).unwrap();
        }
        fs::write(root.join(key), "old").unwrap();
        scheduler.path_event(0, &root.join(key)).unwrap();
        thread::sleep(Duration::from_millis(100));
        let removed = if directory {
            root.join("sub")
        } else {
            root.join(key)
        };
        if directory {
            fs::remove_dir_all(&removed).unwrap();
        } else {
            fs::remove_file(&removed).unwrap();
        }
        scheduler
            .notification(Event::new(EventKind::Remove(RemoveKind::Any)).add_path(removed))
            .unwrap();
        if directory {
            fs::create_dir(root.join("sub")).unwrap();
        }
        fs::write(root.join(key), "new").unwrap();
        scheduler.path_event(0, &root.join(key)).unwrap();
        thread::sleep(Duration::from_millis(120));
        scheduler.settle_due().unwrap();
        scheduler.dispatch().unwrap();
        assert_eq!(
            scheduler.active, 0,
            "replacement was dispatched before its own settle interval"
        );
        thread::sleep(Duration::from_millis(100));
        scheduler.settle_due().unwrap();
        scheduler.dispatch().unwrap();
        assert_eq!(scheduler.active, 1);
        complete(&mut scheduler);
        assert_eq!(
            fs::read_to_string(root.join(key.replace(".txt", ".done"))).unwrap(),
            "new"
        );
    }

    #[test]
    fn regression_removed_file_gets_fresh_settle() {
        removal_restarts_settle(false);
    }

    #[test]
    fn regression_removed_directory_discards_descendant_settles() {
        removal_restarts_settle(true);
    }

    #[test]
    fn regression_vanished_parent_does_not_stop_other_inputs() {
        let (_base, mut scheduler) = fixture(Duration::ZERO);
        let root = scheduler.config.roots[0].path.clone();
        fs::write(root.join("sub"), "not a directory").unwrap();
        scheduler
            .notification(
                Event::new(EventKind::Create(CreateKind::File)).add_path(root.join("sub/item.txt")),
            )
            .unwrap();
        fs::write(root.join("valid.txt"), "valid").unwrap();
        scheduler.path_event(0, &root.join("valid.txt")).unwrap();
        scheduler.settle_due().unwrap();
        scheduler.dispatch().unwrap();
        complete(&mut scheduler);
        assert_eq!(
            fs::read_to_string(root.join("valid.done")).unwrap(),
            "valid"
        );
    }
}

#[cfg(test)]
mod queued_rule_regression {
    use super::*;
    use crate::config::Rule;
    use notify::event::RemoveKind;
    use std::{fs, time::Duration};

    fn queued_sibling() -> (tempfile::TempDir, Scheduler, std::path::PathBuf) {
        let (base, mut scheduler) = super::regression_tests::fixture(Duration::ZERO);
        let config = Arc::get_mut(&mut scheduler.config).unwrap();
        config.concurrency = 1;
        config.roots[0].rules.push(Rule {
            id: "second".into(),
            matcher: regex::Regex::new(r"^item\.txt$").unwrap(),
            run: vec!["printf second > second.done".into()],
            capture_env: Vec::new(),
        });
        let root = scheduler.config.roots[0].path.clone();
        fs::write(root.join("item.txt"), "source").unwrap();
        scheduler.path_event(0, &root.join("item.txt")).unwrap();
        scheduler.settle_due().unwrap();
        scheduler.dispatch().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !fs::read_to_string(root.join("item.done")).is_ok_and(|text| text == "source") {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        (base, scheduler, root)
    }

    fn finish_sibling(scheduler: &mut Scheduler, root: &Path) {
        scheduler.path_event(0, &root.join("item.txt")).unwrap();
        scheduler.settle_due().unwrap();
        super::regression_tests::complete(scheduler);
        scheduler.dispatch().unwrap();
        assert_eq!(
            scheduler.active, 1,
            "queued sibling was lost after input restoration"
        );
        super::regression_tests::complete(scheduler);
        assert_eq!(
            fs::read_to_string(root.join("second.done")).unwrap(),
            "second"
        );
    }

    #[test]
    fn restoring_unchanged_input_preserves_unattempted_sibling() {
        let (base, mut scheduler, root) = queued_sibling();
        let original = model::identity(&root, "item.txt").unwrap();
        fs::rename(root.join("item.txt"), base.path().join("away")).unwrap();
        scheduler
            .notification(
                Event::new(EventKind::Remove(RemoveKind::File)).add_path(root.join("item.txt")),
            )
            .unwrap();
        fs::rename(base.path().join("away"), root.join("item.txt")).unwrap();
        assert_eq!(model::identity(&root, "item.txt").unwrap(), original);
        finish_sibling(&mut scheduler, &root);
    }

    #[test]
    fn reverting_modification_preserves_unattempted_sibling() {
        let (_base, mut scheduler, root) = queued_sibling();
        let path = root.join("item.txt");
        let original = model::identity(&root, "item.txt").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        fs::write(&path, "changed input").unwrap();
        scheduler.path_event(0, &path).unwrap();
        fs::write(&path, "source").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified))
            .unwrap();
        assert_eq!(model::identity(&root, "item.txt").unwrap(), original);
        finish_sibling(&mut scheduler, &root);
    }
}
