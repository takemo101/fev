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
struct Slot {
    settling: Option<Settling>,
    ready: Option<Identity>,
    running: Option<Identity>,
    attempted: Option<Identity>,
}
enum Message {
    Watch(notify::Result<Event>),
    Finished {
        location: Location,
        input: Identity,
        out: String,
        result: Result<Identity>,
    },
    Stop,
}
struct Scheduler {
    config: Arc<Config>,
    ledger: Ledger,
    slots: HashMap<Location, Slot>,
    queue: VecDeque<Location>,
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
            slot.settling = None;
            slot.ready = None;
        }
        Ok(())
    }

    fn settle(&mut self, location: Location, id: Identity) {
        let slot = self.slots.entry(location).or_default();
        if slot.running == Some(id) || slot.ready == Some(id) || slot.attempted == Some(id) {
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
        slot.ready = None;
    }

    fn ready(&mut self, location: Location, id: Identity) {
        let slot = self.slots.entry(location.clone()).or_default();
        if slot.attempted == Some(id) || slot.running == Some(id) {
            return;
        }
        slot.settling = None;
        slot.ready = Some(id);
        self.queue.push_back(location);
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
                    slot.settling = None;
                    slot.ready = None;
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
            let Some(location) = self.queue.pop_front() else {
                break;
            };
            let slot = self.slots.get_mut(&location).unwrap();
            if slot.running.is_some() {
                if slot.ready.is_some() {
                    self.queue.push_back(location);
                }
                continue;
            }
            let Some(id) = slot.ready.take() else {
                continue;
            };
            // A ready event can wait behind other jobs; do not run a stale version.
            let current = model::identity(&self.config.roots[location.0].path, &location.1)?;
            if current != Some(id) {
                if let Some(current) = current {
                    self.settle(location, current);
                }
                continue;
            }
            if slot.attempted == Some(id) {
                continue;
            }
            slot.attempted = Some(id);
            let root = &self.config.roots[location.0];
            if self.ledger.succeeded(&root.id, &location.1, id)? {
                continue;
            }
            let selected = match root.select(&location.1) {
                Ok(selected) => selected,
                Err(error) => {
                    eprintln!("fev: {}/{}: {error:#}", root.id, location.1);
                    continue;
                }
            };
            let Some((rule, out)) = selected else {
                continue;
            };
            let run = rule.run.clone();
            eprintln!("fev: running {}/{} rule {}", root.id, location.1, rule.id);
            slot.running = Some(id);
            self.active += 1;
            let config = self.config.clone();
            let tx = self.tx.clone();
            thread::Builder::new()
                .name("fev-command".into())
                .spawn(move || {
                    let result = execution::execute(
                        &config.roots[location.0].path,
                        &config.state,
                        &run,
                        &location.1,
                        &out,
                    );
                    let _ = tx.send(Message::Finished {
                        location,
                        input: id,
                        out,
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
                slot.settling = None;
                slot.ready = None;
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
        input: Identity,
        out: String,
        result: Result<Identity>,
    ) -> Result<()> {
        self.active -= 1;
        self.slots.get_mut(&location).unwrap().running = None;
        let root = &self.config.roots[location.0];
        match result {
            Ok(output_id) => {
                self.ledger.record(&root.id, &location.1, input, &out)?;
                eprintln!("fev: published {}/{}", root.id, out);
                if !self.stopping {
                    self.ready((location.0, out), output_id);
                }
            }
            Err(error) => eprintln!("fev: failed {}/{}: {error:#}", root.id, location.1),
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
                    out,
                    result,
                } => self.finished(location, input, out, result)?,
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

    fn fixture(settle: Duration) -> (tempfile::TempDir, Scheduler) {
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
                    run: "cat \"$FILE\" > \"$OUT_TMP\"".into(),
                    out: "{name}.done".into(),
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

    fn complete(scheduler: &mut Scheduler) {
        let message = scheduler.rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let Message::Finished {
            location,
            input,
            out,
            result,
        } = message
        else {
            panic!("unexpected message");
        };
        scheduler.finished(location, input, out, result).unwrap();
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

    #[test]
    fn regression_duplicate_publication_preserves_newer_change() {
        let (_base, mut scheduler) = fixture(Duration::ZERO);
        let root = scheduler.config.roots[0].path.clone();
        fs::write(root.join("item.txt"), "old").unwrap();
        let old = model::identity(&root, "item.txt").unwrap().unwrap();
        scheduler.path_event(0, &root.join("item.txt")).unwrap();
        scheduler.settle_due().unwrap();
        scheduler.dispatch().unwrap();
        complete(&mut scheduler);
        fs::write(root.join("item.txt"), "new content").unwrap();
        scheduler.path_event(0, &root.join("item.txt")).unwrap();
        scheduler.ready((0, "item.txt".into()), old);
        scheduler.settle_due().unwrap();
        scheduler.dispatch().unwrap();
        assert_eq!(scheduler.active, 1, "newer external change was lost");
        complete(&mut scheduler);
        assert_eq!(
            fs::read_to_string(root.join("item.done")).unwrap(),
            "new content"
        );
    }
}
