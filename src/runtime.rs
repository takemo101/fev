use crate::{
    config::Config,
    execution::{self, Ledger},
    model::{self, EventType, FileEvent, FileNode, FileSnapshot, Identity},
};
use anyhow::{Context, Result, bail};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};
use walkdir::WalkDir;

type Location = (usize, String);
struct Settling {
    size: u64,
    deadline: Instant,
    kind: EventType,
    startup: bool,
}
#[derive(Default)]
struct RuleState {
    ready: Option<u64>,
    running: Option<u64>,
    attempted: Option<Identity>,
}
#[derive(Default)]
struct Slot {
    settling: Option<Settling>,
    present: Option<FileSnapshot>,
    disappearing: Option<Instant>,
    explicitly_removed: bool,
    last_rename: Option<(String, FileNode)>,
    rules: HashMap<usize, RuleState>,
}
impl Slot {
    fn is_transition(event: &FileEvent) -> bool {
        matches!(event.kind, EventType::Deleted | EventType::Renamed)
    }
    fn discard_pending(&mut self) {
        self.settling = None;
        for state in self.rules.values_mut() {
            state.ready = None;
        }
    }
    fn mark_missing(&mut self, deadline: Instant, explicit: bool) {
        if self.present.is_none() {
            return;
        }
        self.explicitly_removed |= explicit;
        if self.disappearing.is_none() {
            self.discard_pending();
            self.disappearing = Some(deadline);
        }
    }
}
struct Job {
    location: Location,
    rule: usize,
    serial: u64,
    event: Arc<FileEvent>,
    captures: Vec<(String, String)>,
}
enum Message {
    Watch(notify::Result<Event>),
    Finished {
        location: Location,
        rule: usize,
        serial: u64,
        event: Arc<FileEvent>,
        result: Result<()>,
    },
    Stop,
}
struct NativeRename {
    root: usize,
    old: PathBuf,
    files: Vec<(String, FileSnapshot)>,
    deadline: Instant,
}
struct Scheduler {
    config: Arc<Config>,
    ledger: Ledger,
    slots: HashMap<Location, Slot>,
    nodes: HashMap<(usize, FileNode), HashSet<Location>>,
    native_renames: HashMap<usize, NativeRename>,
    last_native_tracker: Option<usize>,
    serial: u64,
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
        nodes: HashMap::new(),
        native_renames: HashMap::new(),
        last_native_tracker: None,
        serial: 0,
        queue: VecDeque::new(),
        active: 0,
        stopping: false,
        tx,
        rx,
    };
    for index in 0..scheduler.config.roots.len() {
        let root = scheduler.config.roots[index].path.clone();
        scheduler.scan(index, &root, EventType::Startup)?;
    }
    eprintln!(
        "fev: watching {} root(s), concurrency {}",
        scheduler.config.roots.len(),
        scheduler.config.concurrency
    );
    scheduler.event_loop()
}

impl Scheduler {
    fn scan(&mut self, root: usize, directory: &Path, source: EventType) -> Result<()> {
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
                let path = entry.path();
                let Some(key) = self.key(root, path) else {
                    continue;
                };
                let current = model::snapshot(&self.config.roots[root].path, &key)?;
                let location = (root, key);
                if source == EventType::Startup {
                    if let Some(current) = current {
                        self.set_present(&location, Some(current));
                        self.slots.get_mut(&location).unwrap().settling = Some(Settling {
                            size: current.identity.size,
                            deadline: Instant::now() + self.config.settle,
                            kind: EventType::Startup,
                            startup: true,
                        });
                    }
                } else {
                    self.observe(location, current, source == EventType::Renamed)?;
                }
            }
        }
        Ok(())
    }

    fn key(&self, root: usize, path: &Path) -> Option<String> {
        let key = path
            .strip_prefix(&self.config.roots[root].path)
            .ok()?
            .to_str()?;
        model::valid_key(key).then(|| key.to_owned())
    }

    fn path_event(&mut self, root: usize, path: &Path) -> Result<()> {
        self.observe_path(root, path, false)
    }

    fn observe_path(&mut self, root: usize, path: &Path, rename: bool) -> Result<()> {
        let Some(key) = self.key(root, path) else {
            return Ok(());
        };
        let current = model::snapshot(&self.config.roots[root].path, &key)?;
        self.observe((root, key), current, rename)
    }

    fn set_present(&mut self, location: &Location, current: Option<FileSnapshot>) {
        let slot = self.slots.entry(location.clone()).or_default();
        let old_node = slot.present.map(|file| file.node);
        let new_node = current.map(|file| file.node);
        if old_node != new_node {
            if let Some(node) = old_node {
                let index = (location.0, node);
                if let Some(keys) = self.nodes.get_mut(&index) {
                    keys.remove(location);
                    if keys.is_empty() {
                        self.nodes.remove(&index);
                    }
                }
            }
            if let Some(node) = new_node {
                self.nodes
                    .entry((location.0, node))
                    .or_default()
                    .insert(location.clone());
            }
        }
        slot.present = current;
        slot.disappearing = None;
        slot.explicitly_removed = false;
        if current.is_none() {
            slot.last_rename = None;
        }
    }

    fn observe(
        &mut self,
        location: Location,
        current: Option<FileSnapshot>,
        rename: bool,
    ) -> Result<()> {
        let Some(current) = current else {
            self.missing(&location);
            return Ok(());
        };
        // Only a vanished alias can be the source: existing hard links are not renames.
        let source = if rename {
            let mut source = None;
            if let Some(keys) = self.nodes.get(&(location.0, current.node)) {
                for candidate in keys {
                    if candidate != &location
                        && !self.slots[candidate].explicitly_removed
                        && model::snapshot(&self.config.roots[location.0].path, &candidate.1)?
                            .is_none_or(|snapshot| snapshot.node != current.node)
                    {
                        if source.is_some() {
                            source = None;
                            break;
                        }
                        source = Some(candidate);
                    }
                }
            }
            source.map(|candidate| candidate.1.clone())
        } else {
            None
        };
        if let Some(old_key) = source {
            return self.renamed(location, old_key, current);
        }
        if self
            .slots
            .get(&location)
            .is_some_and(|slot| slot.disappearing.is_some())
        {
            self.removed(&location)?;
        }
        let old = self.slots.get(&location).and_then(|slot| slot.present);
        if old == Some(current) {
            return Ok(());
        }
        let pending = self
            .slots
            .get(&location)
            .and_then(|slot| slot.settling.as_ref());
        let startup = pending.is_some_and(|pending| pending.startup);
        let kind = pending
            .map(|pending| pending.kind)
            .filter(|kind| *kind != EventType::Startup)
            .unwrap_or(if old.is_some() {
                EventType::Updated
            } else {
                EventType::Created
            });
        self.set_present(&location, Some(current));
        let slot = self.slots.get_mut(&location).unwrap();
        let deadline = slot
            .settling
            .as_ref()
            .filter(|pending| pending.size == current.identity.size)
            .map_or_else(
                || Instant::now() + self.config.settle,
                |pending| pending.deadline,
            );
        slot.discard_pending();
        if old.is_none_or(|previous| previous.node != current.node) {
            slot.last_rename = None;
        }
        slot.settling = Some(Settling {
            size: current.identity.size,
            deadline,
            kind,
            startup,
        });
        Ok(())
    }

    fn missing(&mut self, location: &Location) {
        if let Some(slot) = self.slots.get_mut(location) {
            // Retain vanished inode candidates while untracked FSEvents rename
            // paths can still arrive, including with settle: 0s.
            slot.mark_missing(
                Instant::now() + self.config.settle.max(Duration::from_millis(100)),
                false,
            );
        }
    }

    fn removed(&mut self, location: &Location) -> Result<()> {
        let previous = self.slots.get(location).and_then(|slot| slot.present);
        self.set_present(location, None);
        self.slots.get_mut(location).unwrap().discard_pending();
        if let Some(previous) = previous {
            self.ready(
                location.clone(),
                FileEvent {
                    kind: EventType::Deleted,
                    identity: previous.identity,
                    event_id: 0,
                    old_key: None,
                },
            )?;
        }
        Ok(())
    }

    fn renamed(
        &mut self,
        location: Location,
        old_key: String,
        current: FileSnapshot,
    ) -> Result<()> {
        if old_key == location.1 {
            return Ok(());
        }
        if self
            .slots
            .get(&location)
            .is_some_and(|slot| slot.explicitly_removed)
        {
            self.removed(&location)?;
        }
        if self.slots.get(&location).is_some_and(|slot| {
            slot.last_rename
                .as_ref()
                .is_some_and(|(old, node)| old == &old_key && *node == current.node)
        }) {
            return Ok(());
        }
        let source = (location.0, old_key.clone());
        if self.slots.get(&source).is_some_and(|slot| {
            !slot.explicitly_removed
                && slot
                    .present
                    .is_some_and(|previous| previous.node == current.node)
        }) {
            self.set_present(&source, None);
            self.slots.get_mut(&source).unwrap().discard_pending();
        }
        self.set_present(&location, Some(current));
        let slot = self.slots.get_mut(&location).unwrap();
        slot.discard_pending();
        slot.last_rename = Some((old_key.clone(), current.node));
        self.ready(
            location,
            FileEvent {
                kind: EventType::Renamed,
                identity: current.identity,
                event_id: 0,
                old_key: Some(old_key),
            },
        )
    }

    fn ready(&mut self, location: Location, event: FileEvent) -> Result<()> {
        let root = &self.config.roots[location.0];
        let slot = self.slots.entry(location.clone()).or_default();
        let transition = Slot::is_transition(&event);
        let mut shared = None;
        let kind = event.kind;
        let identity = event.identity;
        let mut event = Some(event);
        for (rule, handler) in root.rules.iter().enumerate() {
            if !handler.accepts(kind) {
                continue;
            }
            let Some(captures) = handler.environment(&location.1) else {
                continue;
            };
            let state = slot.rules.entry(rule).or_default();
            if !transition && state.attempted == Some(identity) {
                continue;
            }
            let event = match &shared {
                Some(event) => Arc::clone(event),
                None => {
                    let mut value = event.take().unwrap();
                    if transition {
                        value.event_id = self.ledger.next_event_id()?;
                    }
                    let value = Arc::new(value);
                    shared = Some(Arc::clone(&value));
                    value
                }
            };
            self.serial = self
                .serial
                .checked_add(1)
                .context("job sequence exhausted")?;
            let serial = self.serial;
            if !transition {
                state.ready = Some(serial);
            }
            self.queue.push_back(Job {
                location: location.clone(),
                rule,
                serial,
                event,
                captures,
            });
        }
        Ok(())
    }

    fn settle_due(&mut self) -> Result<()> {
        let now = Instant::now();
        self.native_renames
            .retain(|_, pending| pending.deadline > now);
        let due: Vec<_> = self
            .slots
            .iter()
            .filter(|(_, slot)| {
                slot.disappearing.is_some_and(|deadline| deadline <= now)
                    || slot
                        .settling
                        .as_ref()
                        .is_some_and(|pending| pending.deadline <= now)
            })
            .map(|(location, _)| location.clone())
            .collect();
        for location in due {
            let current = model::snapshot(&self.config.roots[location.0].path, &location.1)?;
            if self.slots[&location].disappearing.is_some() {
                if current.is_some() {
                    self.observe(location, current, false)?;
                } else {
                    self.removed(&location)?;
                }
                continue;
            }
            let pending = self
                .slots
                .get_mut(&location)
                .unwrap()
                .settling
                .take()
                .unwrap();
            match current {
                Some(current) => {
                    // Initial settling must not swallow changes to an existing
                    // file, even when its native update notification arrives late.
                    let kind = if pending.kind == EventType::Startup
                        && self.slots[&location]
                            .present
                            .is_some_and(|previous| previous.identity != current.identity)
                    {
                        EventType::Updated
                    } else {
                        pending.kind
                    };
                    self.set_present(&location, Some(current));
                    if current.identity.size != pending.size {
                        self.slots.get_mut(&location).unwrap().settling = Some(Settling {
                            size: current.identity.size,
                            deadline: now + self.config.settle,
                            kind,
                            startup: pending.startup,
                        });
                        continue;
                    }
                    if pending.startup && kind != EventType::Startup {
                        self.ready(
                            location.clone(),
                            FileEvent {
                                kind: EventType::Startup,
                                identity: current.identity,
                                event_id: 0,
                                old_key: None,
                            },
                        )?;
                    }
                    self.ready(
                        location,
                        FileEvent {
                            kind,
                            identity: current.identity,
                            event_id: 0,
                            old_key: None,
                        },
                    )?;
                }
                None => self.missing(&location),
            }
        }
        Ok(())
    }

    fn dispatch(&mut self) -> Result<()> {
        if self.stopping {
            return Ok(());
        }
        let candidates = self.queue.len();
        let mut deferred = 0;
        for _ in 0..candidates {
            if self.active >= self.config.concurrency {
                break;
            }
            let job = self.queue.pop_front().unwrap();
            let transition = Slot::is_transition(&job.event);
            let state = self
                .slots
                .get_mut(&job.location)
                .unwrap()
                .rules
                .get_mut(&job.rule)
                .unwrap();
            if !transition && state.ready != Some(job.serial) {
                continue;
            }
            if state.running.is_some() {
                self.queue.push_back(job);
                deferred += 1;
                continue;
            }
            if !transition {
                state.ready = None;
                let current =
                    model::snapshot(&self.config.roots[job.location.0].path, &job.location.1)?;
                if current.is_none_or(|file| file.identity != job.event.identity) {
                    self.observe(job.location, current, false)?;
                    continue;
                }
                let state = self
                    .slots
                    .get_mut(&job.location)
                    .unwrap()
                    .rules
                    .get_mut(&job.rule)
                    .unwrap();
                if state.attempted == Some(job.event.identity) {
                    continue;
                }
                state.attempted = Some(job.event.identity);
            }
            let root = &self.config.roots[job.location.0];
            let rule = &root.rules[job.rule];
            if self.ledger.succeeded(
                &root.id,
                &job.location.1,
                &rule.id,
                job.event.identity,
                job.event.event_id,
            )? {
                continue;
            }
            self.slots
                .get_mut(&job.location)
                .unwrap()
                .rules
                .get_mut(&job.rule)
                .unwrap()
                .running = Some(job.serial);
            self.active += 1;
            let config = Arc::clone(&self.config);
            let tx = self.tx.clone();
            thread::spawn(move || {
                let root = &config.roots[job.location.0];
                let result = execution::execute(
                    &root.path,
                    &job.location.1,
                    &job.event,
                    &root.rules[job.rule].run,
                    &job.captures,
                );
                let _ = tx.send(Message::Finished {
                    location: job.location,
                    rule: job.rule,
                    serial: job.serial,
                    event: job.event,
                    result,
                });
            });
        }
        // A capacity-limited pass leaves the deferred prefix behind the untouched
        // suffix. Restore that prefix so later transitions cannot overtake it.
        self.queue.rotate_right(deferred);
        Ok(())
    }

    fn discard(&mut self, root: usize, path: &Path, explicit: bool) {
        let Ok(relative) = path.strip_prefix(&self.config.roots[root].path) else {
            return;
        };
        let Some(prefix) = relative.to_str() else {
            return;
        };
        let deadline = Instant::now() + self.config.settle.max(Duration::from_millis(100));
        for ((index, key), slot) in &mut self.slots {
            if *index == root
                && (prefix.is_empty()
                    || key == prefix
                    || key
                        .strip_prefix(prefix)
                        .is_some_and(|tail| tail.starts_with('/')))
            {
                slot.mark_missing(deadline, explicit);
            }
        }
    }

    fn rename_sources(&self, root: usize, old: &Path) -> Vec<(String, FileSnapshot)> {
        let Ok(relative) = old.strip_prefix(&self.config.roots[root].path) else {
            return Vec::new();
        };
        let Some(prefix) = relative.to_str() else {
            return Vec::new();
        };
        let location = (root, prefix.to_owned());
        if let Some(slot) = self.slots.get(&location)
            && !slot.explicitly_removed
            && let Some(snapshot) = slot.present
        {
            return vec![(location.1, snapshot)];
        }
        self.slots
            .iter()
            .filter_map(|((index, key), slot)| {
                if *index != root
                    || slot.explicitly_removed
                    || !(key == prefix
                        || key
                            .strip_prefix(prefix)
                            .is_some_and(|tail| tail.starts_with('/')))
                {
                    return None;
                }
                slot.present.map(|snapshot| (key.clone(), snapshot))
            })
            .collect()
    }

    fn rename_pair(&mut self, root: usize, old: &Path, new: &Path) -> Result<()> {
        let files = self.rename_sources(root, old);
        self.rename_known(root, old, new, files)
    }

    fn rename_known(
        &mut self,
        root: usize,
        old: &Path,
        new: &Path,
        files: Vec<(String, FileSnapshot)>,
    ) -> Result<()> {
        let Some(old_key) = self.key(root, old) else {
            return Ok(());
        };
        let Some(new_key) = self.key(root, new) else {
            return Ok(());
        };
        if files.is_empty() {
            // A preceding untracked To may already have correlated this pair.
            if self
                .slots
                .get(&(root, new_key.clone()))
                .is_some_and(|slot| {
                    slot.last_rename
                        .as_ref()
                        .is_some_and(|(previous, _)| previous == &old_key)
                })
            {
                return Ok(());
            }
            if let Some(current) = model::snapshot(&self.config.roots[root].path, &new_key)? {
                return self.renamed((root, new_key), old_key, current);
            }
        } else {
            let directory = files.iter().any(|(key, _)| key != &old_key);
            for (source, previous) in files {
                let destination = if source == old_key {
                    new_key.clone()
                } else {
                    format!("{new_key}{}", source.strip_prefix(&old_key).unwrap())
                };
                // Native pairs describe a past move. A missing/reused destination
                // must not erase it or substitute an unrelated file's identity.
                let current = model::snapshot(&self.config.roots[root].path, &destination)?
                    .filter(|current| current.node == previous.node)
                    .unwrap_or(previous);
                self.renamed((root, destination), source, current)?;
            }
            if !directory {
                return Ok(());
            }
        }
        match std::fs::symlink_metadata(new) {
            Ok(metadata) if metadata.is_dir() => {
                // Only inventoried source descendants above are renames. Files
                // introduced after the directory move retain their creation work.
                self.scan(root, new, EventType::Created)?;
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) => {}
            Err(error) => return Err(error).context("inspecting renamed path"),
        }
        Ok(())
    }

    fn notification(&mut self, event: Event) -> Result<()> {
        use notify::event::{ModifyKind, RenameMode};
        if event.need_rescan() {
            bail!("filesystem notifications were lost; restart fev to rescan inputs");
        }
        if self.stopping {
            return Ok(());
        }
        if !matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) | EventKind::Any
        ) {
            return Ok(());
        }
        if matches!(event.kind, EventKind::Remove(_)) {
            for path in &event.paths {
                for root in 0..self.config.roots.len() {
                    if path.starts_with(&self.config.roots[root].path) {
                        self.discard(root, path, true);
                        // Remove is evidence about the old incarnation even if
                        // a replacement already occupies its historical path.
                        self.notification_path(root, path, false, true)?;
                    }
                }
            }
            return Ok(());
        }
        let tracker = event.tracker();
        if matches!(
            event.kind,
            EventKind::Modify(ModifyKind::Name(RenameMode::From))
        ) {
            for path in &event.paths {
                for root in 0..self.config.roots.len() {
                    if path.starts_with(&self.config.roots[root].path) {
                        if let Some(tracker) = tracker
                            && self.key(root, path).is_some()
                        {
                            self.native_renames.insert(
                                tracker,
                                NativeRename {
                                    root,
                                    old: path.clone(),
                                    files: self.rename_sources(root, path),
                                    deadline: Instant::now()
                                        + self.config.settle.max(Duration::from_millis(100)),
                                },
                            );
                        }
                        self.discard(root, path, false);
                    }
                }
            }
            return Ok(());
        }
        if matches!(
            event.kind,
            EventKind::Modify(ModifyKind::Name(RenameMode::To))
        ) && let Some(tracker) = tracker
            && event.paths.len() == 1
        {
            let new = &event.paths[0];
            let pending = self.native_renames.remove(&tracker);
            let destination = self
                .config
                .roots
                .iter()
                .position(|root| new.starts_with(&root.path));
            match (pending, destination) {
                (Some(pending), Some(root)) if pending.root == root => {
                    self.rename_known(root, &pending.old, new, pending.files)?;
                    self.last_native_tracker = Some(tracker);
                }
                (Some(pending), destination) => {
                    self.discard(pending.root, &pending.old, false);
                    if let Some(root) = destination {
                        self.notification_path(root, new, false, true)?;
                    }
                    self.last_native_tracker = Some(tracker);
                }
                (None, Some(root)) => {
                    // A tracked move-in without a watched From crosses our boundary;
                    // do not infer an unrelated same-inode rename from old inventory.
                    self.notification_path(root, new, false, true)?;
                    self.last_native_tracker = None;
                }
                (None, None) => {}
            }
            return Ok(());
        }
        let rename = matches!(event.kind, EventKind::Modify(ModifyKind::Name(_)));
        if matches!(
            event.kind,
            EventKind::Modify(ModifyKind::Name(RenameMode::Both))
        ) && event.paths.len() == 2
        {
            if tracker.is_some() && tracker == self.last_native_tracker {
                self.last_native_tracker = None;
                return Ok(());
            }
            let (old, new) = (&event.paths[0], &event.paths[1]);
            for root in 0..self.config.roots.len() {
                let base = &self.config.roots[root].path;
                match (old.starts_with(base), new.starts_with(base)) {
                    (true, true) => self.rename_pair(root, old, new)?,
                    (true, false) => self.discard(root, old, false),
                    (false, true) => self.notification_path(root, new, false, true)?,
                    (false, false) => {}
                }
            }
            return Ok(());
        }
        for path in &event.paths {
            for root in 0..self.config.roots.len() {
                if path.starts_with(&self.config.roots[root].path) {
                    self.notification_path(
                        root,
                        path,
                        rename,
                        matches!(event.kind, EventKind::Create(_) | EventKind::Any),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn notification_path(
        &mut self,
        root: usize,
        path: &Path,
        rename: bool,
        introduced: bool,
    ) -> Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() => self.observe_path(root, path, rename)?,
            Ok(metadata) if metadata.is_dir() => {
                if path != self.config.roots[root].path && (rename || introduced) {
                    self.scan(
                        root,
                        path,
                        if rename {
                            EventType::Renamed
                        } else {
                            EventType::Created
                        },
                    )?;
                }
            }
            Ok(_) => self.discard(root, path, false),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                ) =>
            {
                self.discard(root, path, false);
            }
            Err(error) => return Err(error).context("inspecting filesystem notification"),
        }
        Ok(())
    }

    fn finished(
        &mut self,
        location: Location,
        rule: usize,
        serial: u64,
        event: Arc<FileEvent>,
        result: Result<()>,
    ) -> Result<()> {
        let state = self
            .slots
            .get_mut(&location)
            .unwrap()
            .rules
            .get_mut(&rule)
            .unwrap();
        debug_assert_eq!(state.running, Some(serial));
        state.running = None;
        self.active -= 1;
        let root = &self.config.roots[location.0];
        let handler = &root.rules[rule];
        match result {
            Ok(()) => self.ledger.record(
                &root.id,
                &location.1,
                &handler.id,
                event.identity,
                event.event_id,
                event.kind,
            )?,
            Err(error) => eprintln!(
                "fev: {}:{} [{} / {}]: {error:#}",
                root.id,
                location.1,
                handler.id,
                event.kind.as_str()
            ),
        }
        if !self.stopping {
            let path = root.path.join(&location.1);
            self.path_event(location.0, &path)?;
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
            let deadline = self
                .slots
                .values()
                .filter_map(|slot| {
                    slot.settling
                        .as_ref()
                        .map(|pending| pending.deadline)
                        .into_iter()
                        .chain(slot.disappearing)
                        .min()
                })
                .min();
            let message = match (self.stopping, deadline) {
                (false, Some(deadline)) => {
                    match self
                        .rx
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(message) => message,
                        Err(mpsc::RecvTimeoutError::Timeout) => continue,
                        Err(mpsc::RecvTimeoutError::Disconnected) => {
                            bail!("scheduler channel disconnected")
                        }
                    }
                }
                _ => self.rx.recv().context("scheduler channel disconnected")?,
            };
            match message {
                Message::Watch(Ok(event)) => self.notification(event)?,
                Message::Watch(Err(error)) => {
                    return Err(error).context("filesystem watcher failed");
                }
                Message::Finished {
                    location,
                    rule,
                    serial,
                    event,
                    result,
                } => self.finished(location, rule, serial, event, result)?,
                Message::Stop => {
                    self.stopping = true;
                    self.queue.clear();
                    eprintln!("fev: stopping; waiting for {} active job(s)", self.active);
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
                    events: vec![EventType::Created, EventType::Updated],
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
                nodes: HashMap::new(),
                native_renames: HashMap::new(),
                last_native_tracker: None,
                serial: 0,
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
            serial,
            event,
            rule,
            result,
        } = message
        else {
            panic!("unexpected message");
        };
        scheduler
            .finished(location, rule, serial, event, result)
            .unwrap();
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

    fn event_fixture() -> (tempfile::TempDir, Scheduler) {
        let (base, mut scheduler) = fixture(Duration::ZERO);
        let config = Arc::get_mut(&mut scheduler.config).unwrap();
        config.concurrency = 1;
        config.roots[0].rules[0].events = vec![
            EventType::Created,
            EventType::Updated,
            EventType::Deleted,
            EventType::Renamed,
        ];
        config.roots[0].rules[0].run =
            vec!["printf '%s|%s|%s\\n' \"$EVENT\" \"$KEY\" \"$OLD_KEY\" >> ../events".into()];
        (base, scheduler)
    }

    fn create(scheduler: &mut Scheduler, key: &str) {
        let root = scheduler.config.roots[0].path.clone();
        fs::write(root.join(key), "source").unwrap();
        scheduler.path_event(0, &root.join(key)).unwrap();
        scheduler.settle_due().unwrap();
        scheduler.dispatch().unwrap();
        complete(scheduler);
    }

    #[test]
    fn from_to_and_both_report_one_rename_at_the_new_key() {
        use notify::event::{ModifyKind, RenameMode};
        let (base, mut scheduler) = event_fixture();
        let root = scheduler.config.roots[0].path.clone();
        create(&mut scheduler, "old.txt");
        fs::rename(root.join("old.txt"), root.join("new.txt")).unwrap();
        for mode in [RenameMode::From, RenameMode::To, RenameMode::Both] {
            let mut event = Event::new(EventKind::Modify(ModifyKind::Name(mode)));
            if mode != RenameMode::To {
                event = event.add_path(root.join("old.txt"));
            }
            if mode != RenameMode::From {
                event = event.add_path(root.join("new.txt"));
            }
            scheduler.notification(event).unwrap();
        }
        scheduler.settle_due().unwrap();
        scheduler.dispatch().unwrap();
        complete(&mut scheduler);
        assert_eq!(
            fs::read_to_string(base.path().join("events")).unwrap(),
            "created|old.txt|\nrenamed|new.txt|old.txt\n"
        );
    }

    #[test]
    fn existing_hardlink_is_not_a_rename_source() {
        use notify::event::{ModifyKind, RenameMode};
        let (base, mut scheduler) = event_fixture();
        let root = scheduler.config.roots[0].path.clone();
        create(&mut scheduler, "old.txt");
        fs::hard_link(root.join("old.txt"), root.join("alias.txt")).unwrap();
        scheduler
            .notification(
                Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Any)))
                    .add_path(root.join("alias.txt")),
            )
            .unwrap();
        scheduler.settle_due().unwrap();
        scheduler.dispatch().unwrap();
        complete(&mut scheduler);
        assert_eq!(
            fs::read_to_string(base.path().join("events")).unwrap(),
            "created|old.txt|\ncreated|alias.txt|\n"
        );
    }

    #[test]
    fn queued_rename_occurrences_survive_another_move_before_dispatch() {
        use notify::event::{ModifyKind, RenameMode};
        let (base, mut scheduler) = event_fixture();
        let root = scheduler.config.roots[0].path.clone();
        create(&mut scheduler, "a.txt");
        for (old, new) in [("a.txt", "b.txt"), ("b.txt", "c.txt")] {
            fs::rename(root.join(old), root.join(new)).unwrap();
            scheduler
                .notification(
                    Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
                        .add_path(root.join(old))
                        .add_path(root.join(new)),
                )
                .unwrap();
        }
        for _ in 0..2 {
            scheduler.dispatch().unwrap();
            complete(&mut scheduler);
        }
        assert_eq!(
            fs::read_to_string(base.path().join("events")).unwrap(),
            "created|a.txt|\nrenamed|b.txt|a.txt\nrenamed|c.txt|b.txt\n"
        );
    }

    #[test]
    fn repeated_deletions_of_same_identity_survive_running_content_job() {
        let (base, mut scheduler) = event_fixture();
        let root = scheduler.config.roots[0].path.clone();
        Arc::get_mut(&mut scheduler.config).unwrap().roots[0].rules[0].run = vec![
            "mkdir ../guard || exit 91\nprintf '%s|%s|%s\\n' \"$EVENT\" \"$KEY\" \"$OLD_KEY\" >> ../events\nif [ \"$EVENT\" = created ]; then while [ ! -f ../release ]; do sleep 0.01; done; fi\nrmdir ../guard".into()
        ];
        fs::write(root.join("item.txt"), "source").unwrap();
        scheduler.path_event(0, &root.join("item.txt")).unwrap();
        scheduler.settle_due().unwrap();
        scheduler.dispatch().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !fs::read_to_string(base.path().join("events"))
            .is_ok_and(|text| text == "created|item.txt|\n")
        {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        let original = model::identity(&root, "item.txt").unwrap();
        for occurrence in 0..2 {
            fs::rename(root.join("item.txt"), base.path().join("away")).unwrap();
            scheduler
                .notification(
                    Event::new(EventKind::Remove(RemoveKind::File)).add_path(root.join("item.txt")),
                )
                .unwrap();
            thread::sleep(Duration::from_millis(110));
            scheduler.settle_due().unwrap();
            scheduler.dispatch().unwrap();
            if occurrence == 0 {
                fs::rename(base.path().join("away"), root.join("item.txt")).unwrap();
                assert_eq!(model::identity(&root, "item.txt").unwrap(), original);
                scheduler.path_event(0, &root.join("item.txt")).unwrap();
                scheduler.settle_due().unwrap();
            }
        }
        fs::write(base.path().join("release"), "").unwrap();
        complete(&mut scheduler);
        for _ in 0..2 {
            scheduler.dispatch().unwrap();
            complete(&mut scheduler);
        }
        assert_eq!(
            fs::read_to_string(base.path().join("events")).unwrap(),
            "created|item.txt|\ndeleted|item.txt|\ndeleted|item.txt|\n"
        );
        assert!(!base.path().join("guard").exists());
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
            events: vec![EventType::Created, EventType::Updated],
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

#[cfg(test)]
mod transition_regressions {
    use super::*;
    use notify::event::{ModifyKind, RenameMode};
    use std::fs;

    fn fixture() -> (tempfile::TempDir, Scheduler) {
        let (base, mut scheduler) = super::regression_tests::fixture(Duration::ZERO);
        let config = Arc::get_mut(&mut scheduler.config).unwrap();
        config.concurrency = 1;
        config.roots[0].rules[0].events = vec![
            EventType::Created,
            EventType::Updated,
            EventType::Deleted,
            EventType::Renamed,
        ];
        config.roots[0].rules[0].run =
            vec!["printf '%s|%s|%s\\n' \"$EVENT\" \"$KEY\" \"$OLD_KEY\" >> ../events".into()];
        (base, scheduler)
    }

    fn drain(scheduler: &mut Scheduler) {
        loop {
            scheduler.dispatch().unwrap();
            if scheduler.active == 0 {
                break;
            }
            super::regression_tests::complete(scheduler);
        }
    }

    fn create(scheduler: &mut Scheduler, key: &str) {
        let root = scheduler.config.roots[0].path.clone();
        fs::write(root.join(key), "source").unwrap();
        scheduler.path_event(0, &root.join(key)).unwrap();
        scheduler.settle_due().unwrap();
        drain(scheduler);
    }

    fn notify(scheduler: &mut Scheduler, mode: RenameMode, paths: &[&str], tracker: Option<usize>) {
        let root = &scheduler.config.roots[0].path;
        let mut event = Event::new(EventKind::Modify(ModifyKind::Name(mode)));
        for path in paths {
            event = event.add_path(root.join(path));
        }
        if let Some(tracker) = tracker {
            event = event.set_tracker(tracker);
        }
        scheduler.notification(event).unwrap();
    }

    #[test]
    fn complete_native_pairs_preserve_intermediate_names_that_are_already_absent() {
        for tracked in [false, true] {
            let (base, mut scheduler) = fixture();
            let root = scheduler.config.roots[0].path.clone();
            create(&mut scheduler, "a.txt");
            fs::rename(root.join("a.txt"), root.join("b.txt")).unwrap();
            fs::rename(root.join("b.txt"), root.join("c.txt")).unwrap();
            for (cookie, old, new) in [(1, "a.txt", "b.txt"), (2, "b.txt", "c.txt")] {
                if tracked {
                    notify(&mut scheduler, RenameMode::From, &[old], Some(cookie));
                    notify(&mut scheduler, RenameMode::To, &[new], Some(cookie));
                }
                notify(
                    &mut scheduler,
                    RenameMode::Both,
                    &[old, new],
                    tracked.then_some(cookie),
                );
            }
            thread::sleep(Duration::from_millis(110));
            scheduler.settle_due().unwrap();
            drain(&mut scheduler);
            assert_eq!(
                fs::read_to_string(base.path().join("events")).unwrap(),
                "created|a.txt|\nrenamed|b.txt|a.txt\nrenamed|c.txt|b.txt\n",
                "tracked={tracked}"
            );
        }
    }

    #[test]
    fn a_reused_destination_does_not_replace_the_native_source_snapshot() {
        let (base, mut scheduler) = fixture();
        let root = scheduler.config.roots[0].path.clone();
        create(&mut scheduler, "a.txt");
        fs::rename(root.join("a.txt"), root.join("b.txt")).unwrap();
        fs::rename(root.join("b.txt"), root.join("c.txt")).unwrap();
        fs::write(root.join("b.txt"), "unrelated replacement").unwrap();
        for (cookie, old, new) in [(1, "a.txt", "b.txt"), (2, "b.txt", "c.txt")] {
            notify(&mut scheduler, RenameMode::From, &[old], Some(cookie));
            notify(&mut scheduler, RenameMode::To, &[new], Some(cookie));
            notify(&mut scheduler, RenameMode::Both, &[old, new], Some(cookie));
        }
        scheduler.path_event(0, &root.join("b.txt")).unwrap();
        thread::sleep(Duration::from_millis(110));
        scheduler.settle_due().unwrap();
        drain(&mut scheduler);
        assert_eq!(
            fs::read_to_string(base.path().join("events")).unwrap(),
            "created|a.txt|\nrenamed|b.txt|a.txt\nrenamed|c.txt|b.txt\ncreated|b.txt|\n"
        );
        let db = rusqlite::Connection::open(base.path().join("state/ledger.sqlite3")).unwrap();
        let sizes: Vec<String> = db
            .prepare("SELECT size FROM successes WHERE event_kind='renamed' ORDER BY event_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(sizes, ["6", "6"]);
    }

    #[test]
    fn a_new_directory_descendant_keeps_its_creation_instead_of_a_fabricated_rename() {
        let (base, mut scheduler) = fixture();
        let root = scheduler.config.roots[0].path.clone();
        fs::create_dir(root.join("before")).unwrap();
        create(&mut scheduler, "before/old.txt");
        fs::rename(root.join("before"), root.join("after")).unwrap();
        fs::write(root.join("after/fresh.txt"), "fresh").unwrap();
        notify(&mut scheduler, RenameMode::From, &["before"], Some(1));
        notify(&mut scheduler, RenameMode::To, &["after"], Some(1));
        notify(
            &mut scheduler,
            RenameMode::Both,
            &["before", "after"],
            Some(1),
        );
        scheduler
            .path_event(0, &root.join("after/fresh.txt"))
            .unwrap();
        scheduler.settle_due().unwrap();
        drain(&mut scheduler);
        assert_eq!(
            fs::read_to_string(base.path().join("events")).unwrap(),
            "created|before/old.txt|\nrenamed|after/old.txt|before/old.txt\ncreated|after/fresh.txt|\n"
        );
    }

    #[test]
    fn admitting_unrelated_work_does_not_reverse_queued_transitions_for_a_busy_key() {
        let (base, mut scheduler) = fixture();
        let root = scheduler.config.roots[0].path.clone();
        let config = Arc::get_mut(&mut scheduler.config).unwrap();
        config.concurrency = 2;
        config.roots[0].rules[0].matcher = regex::Regex::new(r"^(a|b)\.txt$").unwrap();
        config.roots[0].rules[0].capture_env.clear();
        config.roots[0].rules[0].run = vec![
            "printf '%s|%s|%s\\n' \"$EVENT\" \"$KEY\" \"$OLD_KEY\" >> ../events\nif [ \"$EVENT:$KEY\" = created:a.txt ]; then i=0; while [ ! -f ../release ] && [ \"$i\" -lt 300 ]; do i=$((i+1)); sleep 0.01; done; test -f ../release; fi".into()
        ];
        fs::write(root.join("a.txt"), "source").unwrap();
        scheduler.path_event(0, &root.join("a.txt")).unwrap();
        scheduler.settle_due().unwrap();
        scheduler.dispatch().unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while !fs::read_to_string(base.path().join("events"))
            .is_ok_and(|text| text == "created|a.txt|\n")
        {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(5));
        }
        for (old, new) in [("a.txt", "x.txt"), ("x.txt", "a.txt")] {
            fs::rename(root.join(old), root.join(new)).unwrap();
            notify(&mut scheduler, RenameMode::Both, &[old, new], None);
        }
        fs::write(root.join("b.txt"), "source").unwrap();
        scheduler.path_event(0, &root.join("b.txt")).unwrap();
        scheduler.settle_due().unwrap();
        for (old, new) in [("a.txt", "z.txt"), ("z.txt", "a.txt")] {
            fs::rename(root.join(old), root.join(new)).unwrap();
            notify(&mut scheduler, RenameMode::Both, &[old, new], None);
        }
        scheduler.dispatch().unwrap();
        super::regression_tests::complete(&mut scheduler);
        fs::write(base.path().join("release"), "").unwrap();
        super::regression_tests::complete(&mut scheduler);
        drain(&mut scheduler);
        assert_eq!(
            fs::read_to_string(base.path().join("events")).unwrap(),
            "created|a.txt|\ncreated|b.txt|\nrenamed|a.txt|x.txt\nrenamed|a.txt|z.txt\n"
        );
    }

    #[test]
    fn explicit_remove_followed_by_recreation_is_deleted_then_created() {
        use notify::event::{CreateKind, RemoveKind};
        let (base, mut scheduler) = fixture();
        let root = scheduler.config.roots[0].path.clone();
        create(&mut scheduler, "item.txt");
        fs::remove_file(root.join("item.txt")).unwrap();
        fs::write(root.join("item.txt"), "replacement material").unwrap();
        scheduler
            .notification(
                Event::new(EventKind::Remove(RemoveKind::File)).add_path(root.join("item.txt")),
            )
            .unwrap();
        scheduler
            .notification(
                Event::new(EventKind::Create(CreateKind::File)).add_path(root.join("item.txt")),
            )
            .unwrap();
        scheduler.settle_due().unwrap();
        drain(&mut scheduler);
        assert_eq!(
            fs::read_to_string(base.path().join("events")).unwrap(),
            "created|item.txt|\ndeleted|item.txt|\ncreated|item.txt|\n"
        );
    }

    #[test]
    fn deleted_child_is_not_reintroduced_by_a_later_directory_pair() {
        use notify::event::RemoveKind;
        let (base, mut scheduler) = fixture();
        let root = scheduler.config.roots[0].path.clone();
        fs::create_dir(root.join("before")).unwrap();
        create(&mut scheduler, "before/item.txt");
        fs::remove_file(root.join("before/item.txt")).unwrap();
        scheduler
            .notification(
                Event::new(EventKind::Remove(RemoveKind::File))
                    .add_path(root.join("before/item.txt")),
            )
            .unwrap();
        fs::rename(root.join("before"), root.join("after")).unwrap();
        notify(&mut scheduler, RenameMode::From, &["before"], Some(1));
        notify(&mut scheduler, RenameMode::To, &["after"], Some(1));
        notify(
            &mut scheduler,
            RenameMode::Both,
            &["before", "after"],
            Some(1),
        );
        thread::sleep(Duration::from_millis(110));
        scheduler.settle_due().unwrap();
        drain(&mut scheduler);
        assert_eq!(
            fs::read_to_string(base.path().join("events")).unwrap(),
            "created|before/item.txt|\ndeleted|before/item.txt|\n"
        );
    }

    #[test]
    fn a_rename_into_a_deleted_key_preserves_the_old_incarnations_deletion() {
        use notify::event::RemoveKind;
        let (base, mut scheduler) = fixture();
        let root = scheduler.config.roots[0].path.clone();
        create(&mut scheduler, "a.txt");
        create(&mut scheduler, "b.txt");
        fs::remove_file(root.join("a.txt")).unwrap();
        scheduler
            .notification(
                Event::new(EventKind::Remove(RemoveKind::File)).add_path(root.join("a.txt")),
            )
            .unwrap();
        fs::rename(root.join("b.txt"), root.join("a.txt")).unwrap();
        notify(&mut scheduler, RenameMode::From, &["b.txt"], Some(1));
        notify(&mut scheduler, RenameMode::To, &["a.txt"], Some(1));
        notify(
            &mut scheduler,
            RenameMode::Both,
            &["b.txt", "a.txt"],
            Some(1),
        );
        thread::sleep(Duration::from_millis(110));
        scheduler.settle_due().unwrap();
        drain(&mut scheduler);
        assert_eq!(
            fs::read_to_string(base.path().join("events")).unwrap(),
            "created|a.txt|\ncreated|b.txt|\ndeleted|a.txt|\nrenamed|a.txt|b.txt\n"
        );
    }
}

#[cfg(test)]
mod startup_regression {
    use super::*;
    use std::fs;

    #[test]
    fn initial_settle_detects_same_size_updates_before_native_callback_arrives() {
        let (_base, mut scheduler) = super::regression_tests::fixture(Duration::ZERO);
        Arc::get_mut(&mut scheduler.config).unwrap().roots[0].rules[0].events =
            vec![EventType::Updated];
        let root = scheduler.config.roots[0].path.clone();
        let input = root.join("item.txt");
        fs::write(&input, "old").unwrap();
        let modified = fs::metadata(&input).unwrap().modified().unwrap();
        scheduler.scan(0, &root, EventType::Startup).unwrap();
        fs::write(&input, "new").unwrap();
        fs::File::options()
            .write(true)
            .open(&input)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(modified + Duration::from_secs(1)))
            .unwrap();
        scheduler.settle_due().unwrap();
        scheduler.dispatch().unwrap();
        super::regression_tests::complete(&mut scheduler);
        assert_eq!(fs::read_to_string(root.join("item.done")).unwrap(), "new");
    }
}
