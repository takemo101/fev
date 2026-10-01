use crate::model::valid_key;
use anyhow::{Context, Result, bail, ensure};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

#[derive(Debug)]
pub struct Config {
    pub settle: Duration,
    pub concurrency: usize,
    pub state: PathBuf,
    pub roots: Vec<Root>,
}

#[derive(Debug)]
pub struct Root {
    pub id: String,
    pub path: PathBuf,
    pub rules: Vec<Rule>,
}

#[derive(Debug)]
pub struct Rule {
    pub id: String,
    pub matcher: Regex,
    pub run: String,
    pub out: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default = "default_settle")]
    settle: String,
    #[serde(default = "default_concurrency")]
    concurrency: usize,
    #[serde(default = "default_state")]
    state: String,
    roots: Vec<RawRoot>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoot {
    id: String,
    path: String,
    rules: Vec<RawRule>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    id: String,
    events: Vec<String>,
    #[serde(rename = "match")]
    pattern: String,
    run: String,
    out: String,
}

fn default_settle() -> String {
    "2s".to_owned()
}

fn default_concurrency() -> usize {
    2
}

fn default_state() -> String {
    "state".to_owned()
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let yaml = fs::read_to_string(path)
            .with_context(|| format!("reading configuration {}", path.display()))?;
        let raw: RawConfig = serde_saphyr::from_str(&yaml).context("parsing configuration YAML")?;
        let settle = humantime::parse_duration(&raw.settle).context("invalid settle duration")?;
        ensure!(raw.concurrency >= 1, "concurrency must be at least 1");
        let absolute_config = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        let parent = absolute_config
            .parent()
            .context("configuration has no parent directory")?;
        let parent = fs::canonicalize(parent).context("resolving configuration directory")?;
        let mut root_ids = HashSet::new();
        let mut roots: Vec<Root> = Vec::with_capacity(raw.roots.len());
        for raw_root in raw.roots {
            ensure!(!raw_root.id.is_empty(), "root id must not be empty");
            ensure!(
                root_ids.insert(raw_root.id.clone()),
                "duplicate root id {:?}",
                raw_root.id
            );
            let root_path = expand_path(&raw_root.path, &parent)?;
            let root_path = fs::canonicalize(&root_path)
                .with_context(|| format!("resolving root {}", root_path.display()))?;
            ensure!(
                fs::metadata(&root_path)?.is_dir(),
                "root {} is not a directory",
                root_path.display()
            );
            for previous in &roots {
                ensure!(
                    !root_path.starts_with(&previous.path)
                        && !previous.path.starts_with(&root_path),
                    "roots {:?} and {:?} overlap",
                    previous.id,
                    raw_root.id
                );
            }
            let mut rule_ids = HashSet::new();
            let mut rules = Vec::with_capacity(raw_root.rules.len());
            for raw_rule in raw_root.rules {
                ensure!(
                    rule_ids.insert(raw_rule.id.clone()),
                    "duplicate rule id {:?} in root {:?}",
                    raw_rule.id,
                    raw_root.id
                );
                rules.push(
                    Rule::from_raw(raw_rule)
                        .with_context(|| format!("invalid rule in root {:?}", raw_root.id))?,
                );
            }
            roots.push(Root {
                id: raw_root.id,
                path: root_path,
                rules,
            });
        }

        // Resolve and validate before creating any directory: an invalid state
        // location must not leave new directories inside a watched root.
        let requested_state = expand_path(&raw.state, &parent)?;
        let (state, device) = resolve_state(&requested_state)?;
        validate_state(&state, device, &roots)?;
        fs::create_dir_all(&state)
            .with_context(|| format!("creating state directory {}", state.display()))?;
        let state = fs::canonicalize(&state).context("canonicalizing state directory")?;
        let metadata = fs::metadata(&state)?;
        ensure!(metadata.is_dir(), "state is not a directory");
        // Recheck the actual created directory, including its device, rather
        // than trusting a previously resolved nonexistent path.
        validate_state(&state, metadata.dev(), &roots)?;
        Ok(Self {
            settle,
            concurrency: raw.concurrency,
            state,
            roots,
        })
    }
}

impl Rule {
    fn from_raw(raw: RawRule) -> Result<Self> {
        ensure!(
            !raw.events.is_empty() && raw.events.iter().all(|event| event == "created"),
            "rule {:?}: events must be a nonempty list containing only created",
            raw.id
        );
        ensure!(
            raw.pattern.starts_with('^') && raw.pattern.ends_with('$'),
            "rule {:?}: match must start with ^ and end with $",
            raw.id
        );
        // Literal outer anchors alone do not constrain every regex alternative.
        // Absolute anchors around the complete expression also ensure that a
        // partial first alternative cannot hide a later whole-key match.
        let matcher = Regex::new(&format!(r"\A(?:{})\z", raw.pattern))
            .with_context(|| format!("rule {:?}: invalid regular expression", raw.id))?;
        ensure!(
            !raw.out.contains("{name}") || matcher.capture_names().any(|name| name == Some("name")),
            "rule {:?}: out uses {{name}} without a name capture",
            raw.id
        );
        let probe = raw.out.replace("{name}", "probe");
        ensure!(
            valid_key(&probe) && !probe.contains(".."),
            "rule {:?}: out is not a safe relative key",
            raw.id
        );
        ensure!(
            !matcher.is_match(&probe),
            "rule {:?}: out matches the same rule",
            raw.id
        );
        Ok(Self {
            id: raw.id,
            matcher,
            run: raw.run,
            out: raw.out,
        })
    }
}

impl Root {
    pub fn select(&self, key: &str) -> Result<Option<(&Rule, String)>> {
        let mut selected = None;
        for rule in &self.rules {
            let Some(captures) = rule.matcher.captures(key) else {
                continue;
            };
            let Some(full) = captures.get(0) else {
                continue;
            };
            if full.start() != 0 || full.end() != key.len() {
                continue;
            }
            ensure!(
                selected.is_none(),
                "ambiguous rules for root {:?}, key {:?}",
                self.id,
                key
            );
            selected = Some((rule, captures));
        }
        let Some((rule, captures)) = selected else {
            return Ok(None);
        };
        let out = if rule.out.contains("{name}") {
            let name = captures.name("name").with_context(|| {
                format!(
                    "rule {:?}: name capture did not match key {:?}",
                    rule.id, key
                )
            })?;
            rule.out.replace("{name}", name.as_str())
        } else {
            rule.out.clone()
        };
        ensure!(
            valid_key(&out) && !out.contains(".."),
            "rule {:?}: rendered output {:?} is not a safe relative key",
            rule.id,
            out
        );
        Ok(Some((rule, out)))
    }
}

fn expand_path(value: &str, parent: &Path) -> Result<PathBuf> {
    let path = if value == "~" || value.starts_with("~/") {
        let home = std::env::var_os("HOME").context("HOME is required to expand ~ paths")?;
        let mut path = PathBuf::from(home);
        if value != "~" {
            path.push(&value[2..]);
        }
        path
    } else {
        PathBuf::from(value)
    };
    Ok(if path.is_absolute() {
        path
    } else {
        parent.join(path)
    })
}

fn resolve_state(path: &Path) -> Result<(PathBuf, u64)> {
    let mut resolved = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                match fs::symlink_metadata(&resolved) {
                    Ok(_) => {
                        resolved = fs::canonicalize(&resolved).with_context(|| {
                            format!("resolving state component {}", resolved.display())
                        })?;
                        ensure!(
                            fs::metadata(&resolved)?.is_dir(),
                            "state component {} is not a directory",
                            resolved.display()
                        );
                    }
                    Err(error) if error.kind() == ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!("inspecting state component {}", resolved.display())
                        });
                    }
                }
            }
            Component::Prefix(_) => bail!("unsupported state path prefix"),
        }
    }
    let mut ancestor = resolved.as_path();
    loop {
        match fs::metadata(ancestor) {
            Ok(metadata) => {
                ensure!(metadata.is_dir(), "state ancestor is not a directory");
                return Ok((resolved, metadata.dev()));
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                ancestor = ancestor
                    .parent()
                    .context("state has no existing ancestor")?;
            }
            Err(error) => return Err(error).context("inspecting state ancestor"),
        }
    }
}

fn validate_state(state: &Path, device: u64, roots: &[Root]) -> Result<()> {
    for root in roots {
        ensure!(
            !state.starts_with(&root.path),
            "state {} is inside root {:?}",
            state.display(),
            root.id
        );
        ensure!(
            fs::metadata(&root.path)?.dev() == device,
            "state and root {:?} are on different volumes",
            root.id
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    struct Fixture {
        dir: TempDir,
        config: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join("root")).unwrap();
            let config = dir.path().join("config.yaml");
            Self { dir, config }
        }

        fn load(&self, yaml: &str) -> Result<Config> {
            fs::write(&self.config, yaml).unwrap();
            Config::load(&self.config)
        }

        fn rule_yaml(&self, matcher: &str, out: &str) -> String {
            format!(
                "roots:\n  - id: articles\n    path: root\n    rules:\n      - id: convert\n        events: [created]\n        match: '{matcher}'\n        run: 'cp \"$FILE\" \"$OUT_TMP\"'\n        out: '{out}'\n"
            )
        }
    }

    #[test]
    fn selects_named_capture_and_ignores_nonmatching_input() {
        let fixture = Fixture::new();
        let config = fixture
            .load(&fixture.rule_yaml(r"^(?P<name>[^/.]+)\.md$", "processed/{name}.txt"))
            .unwrap();
        let root = &config.roots[0];
        let (rule, out) = root.select("weekly.md").unwrap().unwrap();
        assert_eq!(rule.id, "convert");
        assert_eq!(out, "processed/weekly.txt");
        assert!(root.select("weekly.txt").unwrap().is_none());
    }

    #[test]
    fn alternation_cannot_escape_whole_key_matching() {
        let fixture = Fixture::new();
        let config = fixture
            .load(&fixture.rule_yaml("^in|input$", "finished"))
            .unwrap();
        let root = &config.roots[0];
        assert!(root.select("input").unwrap().is_some());
        assert!(root.select("in-more").unwrap().is_none());
        assert!(root.select("some-input").unwrap().is_none());
    }

    #[test]
    fn self_match_probe_uses_entire_match_not_partial_alternative() {
        let fixture = Fixture::new();
        let config = fixture
            .load(&fixture.rule_yaml("^in|other$", "in-more"))
            .unwrap();
        assert_eq!(config.roots[0].select("in").unwrap().unwrap().1, "in-more");
        assert!(config.roots[0].select("in-more").unwrap().is_none());
    }

    #[test]
    fn overlapping_rules_are_ambiguous_instead_of_ordered() {
        let fixture = Fixture::new();
        let mut yaml = fixture.rule_yaml("^input$", "first");
        yaml.push_str("      - id: another\n        events: [created]\n        match: '^input$'\n        run: 'true'\n        out: second\n");
        let config = fixture.load(&yaml).unwrap();
        let error = config.roots[0].select("input").unwrap_err();
        assert!(error.to_string().contains("ambiguous"));
    }

    #[test]
    fn rendered_capture_must_still_be_a_safe_key() {
        let fixture = Fixture::new();
        let config = fixture
            .load(&fixture.rule_yaml(r"^prefix(?P<name>.*)\.in$", "{name}"))
            .unwrap();
        let root = &config.roots[0];
        for key in [
            "prefix../escape.in",
            "prefix/absolute.in",
            "prefix.in",
            "prefix./dot.in",
        ] {
            assert!(
                root.select(key).is_err(),
                "accepted unsafe output for {key}"
            );
        }
        assert_eq!(root.select("prefixsafe.in").unwrap().unwrap().1, "safe");
    }

    #[test]
    fn unmatched_required_output_capture_is_rejected() {
        let fixture = Fixture::new();
        let config = fixture
            .load(&fixture.rule_yaml(r"^(?P<name>x)?input$", "{name}.done"))
            .unwrap();
        assert!(config.roots[0].select("input").is_err());
        assert_eq!(
            config.roots[0].select("xinput").unwrap().unwrap().1,
            "x.done"
        );
    }

    #[test]
    fn rejects_rule_validation_boundaries_before_creating_state() {
        let invalid = [
            ("^input$", ""),
            ("^input$", "/outside"),
            ("^input$", "nested/../outside"),
            ("^input$", "a..b"),
            ("^input$", "./output"),
            ("^input$", "{name}.done"),
            ("input$", "output"),
            ("^input", "output"),
            ("^[$", "output"),
            ("^input$", "input"),
            ("^(?P<name>[^/.]+)\\.md$", "{name}.md"),
        ];
        for (matcher, out) in invalid {
            let fixture = Fixture::new();
            assert!(
                fixture.load(&fixture.rule_yaml(matcher, out)).is_err(),
                "accepted {matcher} / {out}"
            );
            assert!(!fixture.dir.path().join("state").exists());
        }
    }

    #[test]
    fn rejects_invalid_events_ids_concurrency_and_unknown_fields() {
        let cases = [
            ("events: [created]", "events: []"),
            ("events: [created]", "events: [created, deleted]"),
            ("id: articles", "id: ''"),
            ("roots:", "concurrency: 0\nroots:"),
            ("roots:", "concurrency: -1\nroots:"),
            ("roots:", "settle: nonsense\nroots:"),
            ("roots:", "typo: value\nroots:"),
            ("    path: root", "    path: root\n    typo: value"),
            (
                "        out: 'finished'",
                "        out: 'finished'\n        typo: value",
            ),
        ];
        for (from, to) in cases {
            let fixture = Fixture::new();
            let yaml = fixture.rule_yaml("^input$", "finished").replace(from, to);
            assert!(fixture.load(&yaml).is_err(), "accepted replacement {to}");
        }
    }

    #[test]
    fn rejects_duplicate_root_and_rule_ids() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.dir.path().join("second")).unwrap();
        let mut yaml = fixture.rule_yaml("^input$", "finished");
        yaml.push_str("  - id: articles\n    path: second\n    rules: []\n");
        assert!(fixture.load(&yaml).is_err());
        let mut yaml = fixture.rule_yaml("^input$", "finished");
        yaml.push_str("      - id: convert\n        events: [created]\n        match: '^different$'\n        run: 'true'\n        out: other\n");
        assert!(fixture.load(&yaml).is_err());
    }

    #[test]
    fn rejects_file_missing_nested_and_identical_roots() {
        for path in ["missing", "file"] {
            let fixture = Fixture::new();
            fs::write(fixture.dir.path().join("file"), "not a directory").unwrap();
            let yaml = fixture
                .rule_yaml("^input$", "finished")
                .replace("path: root", &format!("path: {path}"));
            assert!(fixture.load(&yaml).is_err());
        }
        for path in ["root", "root/child"] {
            let fixture = Fixture::new();
            fs::create_dir(fixture.dir.path().join("root/child")).unwrap();
            let mut yaml = fixture.rule_yaml("^input$", "finished");
            yaml.push_str(&format!(
                "  - id: second\n    path: {path}\n    rules: []\n"
            ));
            assert!(fixture.load(&yaml).is_err());
        }
    }

    #[test]
    fn state_in_root_is_rejected_without_creating_any_component() {
        for state in ["root", "root/new/state", "root/new/../state"] {
            let fixture = Fixture::new();
            let yaml = format!(
                "state: {state}\n{}",
                fixture.rule_yaml("^input$", "finished")
            );
            assert!(fixture.load(&yaml).is_err());
            assert!(!fixture.dir.path().join("root/new").exists());
            assert!(!fixture.dir.path().join("root/state").exists());
        }
    }

    #[test]
    fn nonexisting_state_dotdot_is_normalized_without_creating_discarded_paths() {
        let fixture = Fixture::new();
        let yaml = format!(
            "state: root/../new/../safe/state\n{}",
            fixture.rule_yaml("^input$", "finished")
        );
        let config = fixture.load(&yaml).unwrap();
        assert_eq!(
            config.state,
            fs::canonicalize(fixture.dir.path().join("safe/state")).unwrap()
        );
        assert!(!fixture.dir.path().join("new").exists());
        assert!(!fixture.dir.path().join("root/state").exists());
    }

    #[test]
    fn state_ancestor_of_root_is_allowed() {
        let fixture = Fixture::new();
        let yaml = format!("state: .\n{}", fixture.rule_yaml("^input$", "finished"));
        let config = fixture.load(&yaml).unwrap();
        assert_eq!(config.state, fs::canonicalize(fixture.dir.path()).unwrap());
    }

    #[test]
    fn existing_state_file_is_rejected() {
        let fixture = Fixture::new();
        fs::write(fixture.dir.path().join("state"), "not a directory").unwrap();
        assert!(
            fixture
                .load(&fixture.rule_yaml("^input$", "finished"))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_cannot_bypass_root_state_overlap() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new();
        symlink(
            fixture.dir.path().join("root"),
            fixture.dir.path().join("alias"),
        )
        .unwrap();
        let yaml = format!(
            "state: alias/new/state\n{}",
            fixture.rule_yaml("^input$", "finished")
        );
        assert!(fixture.load(&yaml).is_err());
        assert!(!fixture.dir.path().join("root/new").exists());
        let mut yaml = fixture.rule_yaml("^input$", "finished");
        yaml.push_str("  - id: second\n    path: alias\n    rules: []\n");
        assert!(fixture.load(&yaml).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn different_volume_is_rejected_before_state_creation_when_available() {
        use std::os::unix::fs::MetadataExt;
        let fixture = Fixture::new();
        let root_device = fs::metadata(fixture.dir.path()).unwrap().dev();
        let other = ["/dev/shm", "/dev", "/Volumes"]
            .into_iter()
            .map(Path::new)
            .find(|path| {
                fs::metadata(path)
                    .is_ok_and(|metadata| metadata.is_dir() && metadata.dev() != root_device)
            });
        let Some(other) = other else { return };
        let state = other.join(fixture.dir.path().file_name().unwrap());
        let yaml = format!(
            "state: '{}'\n{}",
            state.display(),
            fixture.rule_yaml("^input$", "finished")
        );
        let error = fixture.load(&yaml).unwrap_err();
        assert!(error.to_string().contains("volume"), "{error:#}");
        assert!(!state.exists());
    }
}
