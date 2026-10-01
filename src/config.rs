use anyhow::{Context, Result, bail, ensure};
use regex::Regex;
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::io::ErrorKind;
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
    pub run: Vec<String>,
    pub capture_env: Vec<(usize, String)>,
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
    run: RawRun,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawRun {
    Command(String),
    Commands(Vec<String>),
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
                ensure!(!raw_rule.id.is_empty(), "rule id must not be empty");
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
        let state = resolve_state(&requested_state)?;
        validate_state(&state, &roots)?;
        fs::create_dir_all(&state)
            .with_context(|| format!("creating state directory {}", state.display()))?;
        let state = fs::canonicalize(&state).context("canonicalizing state directory")?;
        let metadata = fs::metadata(&state)?;
        ensure!(metadata.is_dir(), "state is not a directory");
        // Recheck the actual created directory rather than trusting a
        // previously resolved nonexistent path.
        validate_state(&state, &roots)?;
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
        let mut capture_env = Vec::new();
        let mut capture_keys = HashSet::new();
        for (index, name) in matcher.capture_names().enumerate() {
            let Some(name) = name else {
                continue;
            };
            ensure!(
                name.bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
                "rule {:?}: capture name {:?} must contain only ASCII letters, digits, or underscores",
                raw.id,
                name
            );
            let key = format!("MATCH_{}", name.to_ascii_uppercase());
            ensure!(
                capture_keys.insert(key.clone()),
                "rule {:?}: capture name {:?} collides after uppercasing",
                raw.id,
                name
            );
            capture_env.push((index, key));
        }
        let run = match raw.run {
            RawRun::Command(command) => vec![command],
            RawRun::Commands(commands) => commands,
        };
        ensure!(
            !run.is_empty() && run.iter().all(|command| !command.trim().is_empty()),
            "rule {:?}: run must be a command or a nonempty array of nonblank commands",
            raw.id
        );
        Ok(Self {
            id: raw.id,
            matcher,
            run,
            capture_env,
        })
    }

    pub fn environment(&self, key: &str) -> Option<Vec<(String, String)>> {
        let captures = self.matcher.captures(key)?;
        let full = captures.get(0)?;
        if full.start() != 0 || full.end() != key.len() {
            return None;
        }
        Some(
            self.capture_env
                .iter()
                .map(|(index, name)| {
                    let value = captures.get(*index).map_or("", |capture| capture.as_str());
                    (name.clone(), value.to_owned())
                })
                .collect(),
        )
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

fn resolve_state(path: &Path) -> Result<PathBuf> {
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
    Ok(resolved)
}

fn validate_state(state: &Path, roots: &[Root]) -> Result<()> {
    for root in roots {
        ensure!(
            !state.starts_with(&root.path),
            "state {} is inside root {:?}",
            state.display(),
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

        fn rule_yaml(&self, matcher: &str, run: &str) -> String {
            format!(
                "roots:\n  - id: articles\n    path: root\n    rules:\n      - id: convert\n        events: [created]\n        match: '{matcher}'\n        run: {run}\n"
            )
        }
    }

    #[test]
    fn captures_use_uppercase_environment_names_and_preserve_values() {
        let fixture = Fixture::new();
        let config = fixture
            .load(&fixture.rule_yaml(r"^(?P<name>.+)\.(?P<ext>[a-z]+)$", "'true'"))
            .unwrap();
        let rule = &config.roots[0].rules[0];
        assert_eq!(
            rule.environment("MiXeD $(touch pwned);../x.md").unwrap(),
            vec![
                (
                    "MATCH_NAME".to_owned(),
                    "MiXeD $(touch pwned);../x".to_owned()
                ),
                ("MATCH_EXT".to_owned(), "md".to_owned()),
            ]
        );
        assert!(rule.environment("weekly").is_none());
    }

    #[test]
    fn unnamed_groups_do_not_shift_named_capture_values() {
        let fixture = Fixture::new();
        let config = fixture
            .load(&fixture.rule_yaml(r"^(prefix)(?P<name>[^.]+)\.in$", "'true'"))
            .unwrap();
        assert_eq!(
            config.roots[0].rules[0]
                .environment("prefixvalue.in")
                .unwrap(),
            vec![("MATCH_NAME".to_owned(), "value".to_owned())]
        );
    }

    #[test]
    fn alternation_cannot_escape_whole_key_matching() {
        let fixture = Fixture::new();
        let config = fixture
            .load(&fixture.rule_yaml("^in|input$", "'true'"))
            .unwrap();
        let rule = &config.roots[0].rules[0];
        assert_eq!(rule.environment("input"), Some(Vec::new()));
        assert!(rule.environment("in-more").is_none());
        assert!(rule.environment("some-input").is_none());
        assert!(rule.environment("input\n").is_none());
    }

    #[test]
    fn environment_rejects_partial_matches_even_with_an_unanchored_matcher() {
        let rule = Rule {
            id: "unanchored".to_owned(),
            matcher: Regex::new("input").unwrap(),
            run: vec!["true".to_owned()],
            capture_env: Vec::new(),
        };
        assert_eq!(rule.environment("input"), Some(Vec::new()));
        assert!(rule.environment("input-more").is_none());
        assert!(rule.environment("some-input").is_none());
    }

    #[test]
    fn overlapping_rules_match_independently() {
        let fixture = Fixture::new();
        let mut yaml = fixture.rule_yaml("^(?P<name>input)$", "'true'");
        yaml.push_str("      - id: another\n        events: [created]\n        match: '^(?P<value>input)$'\n        run: 'true'\n");
        let config = fixture.load(&yaml).unwrap();
        let rules = &config.roots[0].rules;
        assert_eq!(
            rules[0].environment("input"),
            Some(vec![("MATCH_NAME".to_owned(), "input".to_owned())])
        );
        assert_eq!(
            rules[1].environment("input"),
            Some(vec![("MATCH_VALUE".to_owned(), "input".to_owned())])
        );
    }

    #[test]
    fn optional_capture_exports_empty_string_when_unmatched() {
        let fixture = Fixture::new();
        let config = fixture
            .load(&fixture.rule_yaml(r"^(?P<name>x)?input$", "'true'"))
            .unwrap();
        let rule = &config.roots[0].rules[0];
        assert_eq!(
            rule.environment("input"),
            Some(vec![("MATCH_NAME".to_owned(), String::new())])
        );
        assert_eq!(
            rule.environment("xinput"),
            Some(vec![("MATCH_NAME".to_owned(), "x".to_owned())])
        );
    }

    #[test]
    fn rejects_invalid_capture_names_and_uppercase_collisions() {
        for matcher in [
            "^(?P<name>x)(?P<NAME>y)$",
            "^(?P<naïve>x)$",
            "^(?P<with.dot>x)$",
            "^(?P<with[bracket>x)$",
        ] {
            let fixture = Fixture::new();
            let error = fixture
                .load(&fixture.rule_yaml(matcher, "'true'"))
                .unwrap_err();
            let message = format!("{error:#}");
            assert!(message.contains("capture"), "{message}");
            assert!(!fixture.dir.path().join("state").exists());
        }
    }

    #[test]
    fn ascii_capture_names_allow_underscores_and_digits() {
        let fixture = Fixture::new();
        let config = fixture
            .load(&fixture.rule_yaml("^(?P<_name2>input)$", "'true'"))
            .unwrap();
        assert_eq!(
            config.roots[0].rules[0].environment("input"),
            Some(vec![("MATCH__NAME2".to_owned(), "input".to_owned())])
        );
    }

    #[test]
    fn run_accepts_a_string_or_an_ordered_command_array() {
        let fixture = Fixture::new();
        let single = fixture
            .load(&fixture.rule_yaml("^input$", "'printf first'"))
            .unwrap();
        assert_eq!(single.roots[0].rules[0].run, vec!["printf first"]);
        let array = fixture
            .load(&fixture.rule_yaml("^input$", "['printf first', 'printf second']"))
            .unwrap();
        assert_eq!(
            array.roots[0].rules[0].run,
            vec!["printf first", "printf second"]
        );
    }

    #[test]
    fn rejects_empty_or_invalid_run_before_creating_state() {
        for run in [
            "''",
            "'   '",
            "[]",
            "['true', '']",
            "['true', '   ']",
            "null",
            "42",
            "{}",
        ] {
            let fixture = Fixture::new();
            assert!(
                fixture.load(&fixture.rule_yaml("^input$", run)).is_err(),
                "accepted run: {run}"
            );
            assert!(!fixture.dir.path().join("state").exists());
        }
    }

    #[test]
    fn rejects_rule_validation_boundaries_before_creating_state() {
        for matcher in ["input$", "^input", "^[$"] {
            let fixture = Fixture::new();
            assert!(
                fixture.load(&fixture.rule_yaml(matcher, "'true'")).is_err(),
                "accepted {matcher}"
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
            ("id: convert", "id: ''"),
            ("roots:", "concurrency: 0\nroots:"),
            ("roots:", "concurrency: -1\nroots:"),
            ("roots:", "settle: nonsense\nroots:"),
            ("roots:", "typo: value\nroots:"),
            ("    path: root", "    path: root\n    typo: value"),
            (
                "        run: 'true'",
                "        run: 'true'\n        typo: value",
            ),
            (
                "        run: 'true'",
                "        run: 'true'\n        out: finished",
            ),
        ];
        for (from, to) in cases {
            let fixture = Fixture::new();
            let yaml = fixture.rule_yaml("^input$", "'true'").replace(from, to);
            assert!(fixture.load(&yaml).is_err(), "accepted replacement {to}");
        }
    }

    #[test]
    fn rejects_duplicate_root_and_rule_ids() {
        let fixture = Fixture::new();
        fs::create_dir(fixture.dir.path().join("second")).unwrap();
        let mut yaml = fixture.rule_yaml("^input$", "'true'");
        yaml.push_str("  - id: articles\n    path: second\n    rules: []\n");
        assert!(fixture.load(&yaml).is_err());
        let mut yaml = fixture.rule_yaml("^input$", "'true'");
        yaml.push_str("      - id: convert\n        events: [created]\n        match: '^different$'\n        run: 'true'\n");
        assert!(fixture.load(&yaml).is_err());
    }

    #[test]
    fn rejects_file_missing_nested_and_identical_roots() {
        for path in ["missing", "file"] {
            let fixture = Fixture::new();
            fs::write(fixture.dir.path().join("file"), "not a directory").unwrap();
            let yaml = fixture
                .rule_yaml("^input$", "'true'")
                .replace("path: root", &format!("path: {path}"));
            assert!(fixture.load(&yaml).is_err());
        }
        for path in ["root", "root/child"] {
            let fixture = Fixture::new();
            fs::create_dir(fixture.dir.path().join("root/child")).unwrap();
            let mut yaml = fixture.rule_yaml("^input$", "'true'");
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
            let yaml = format!("state: {state}\n{}", fixture.rule_yaml("^input$", "'true'"));
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
            fixture.rule_yaml("^input$", "'true'")
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
        let yaml = format!("state: .\n{}", fixture.rule_yaml("^input$", "'true'"));
        let config = fixture.load(&yaml).unwrap();
        assert_eq!(config.state, fs::canonicalize(fixture.dir.path()).unwrap());
    }

    #[test]
    fn existing_state_file_is_rejected() {
        let fixture = Fixture::new();
        fs::write(fixture.dir.path().join("state"), "not a directory").unwrap();
        assert!(
            fixture
                .load(&fixture.rule_yaml("^input$", "'true'"))
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
            fixture.rule_yaml("^input$", "'true'")
        );
        assert!(fixture.load(&yaml).is_err());
        assert!(!fixture.dir.path().join("root/new").exists());
        let mut yaml = fixture.rule_yaml("^input$", "'true'");
        yaml.push_str("  - id: second\n    path: alias\n    rules: []\n");
        assert!(fixture.load(&yaml).is_err());
    }
}
