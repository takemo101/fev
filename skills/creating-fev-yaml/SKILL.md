---
name: creating-fev-yaml
description: Use when creating, changing, or troubleshooting fev YAML configurations for local file watching and shell-command workflows.
---

# Creating fev YAML

Define watched directories, input patterns, and shell commands. **Commands own their outputs; every matching rule runs.** This skill is specific to fev, not general YAML authoring.

Read the bundled [configuration reference](references/configuration.md) before authoring a configuration. It contains the configuration and execution contract; no external repository documentation is required. Start from [basic.yaml](templates/basic.yaml), resolving these resources relative to this skill's directory.

## 1. Establish requirements

Use the request and existing configuration to determine:

- Input directories, filename patterns, and subdirectory coverage.
- Commands, arguments, and installed external tools.
- Whether outputs are needed, their names, and downstream processing.
- Independent jobs versus commands that must execute in order.
- `settle`, global `concurrency`, state location, and permission to process existing inputs.

Ask only about unresolved requirements. Do not invent working external commands.

## 2. Copy and run

Requires macOS / Linux, `fev` on PATH, and standard `sh`, `tr`, `cat`, and `printf`. Run these commands **from the directory containing this SKILL.md**:

```sh
mkdir -p "$HOME/fev-example/work"
cp templates/basic.yaml "$HOME/fev-example/config.yaml"
fev --config "$HOME/fev-example/config.yaml"
```

Keep this terminal running. In another terminal:

```sh
printf 'hello fev\n' > "$HOME/fev-example/work/Weekly Report.txt"
# Wait for processing, then inspect both outputs.
cat "$HOME/fev-example/work/Weekly Report.upper.txt"
cat "$HOME/fev-example/work/Weekly Report.done.txt"
```

Both contain `HELLO FEV`. The original remains. Logs include `Observed: Weekly Report.txt` and `Uppercased: Weekly Report.txt`; their order is unspecified. If inspection is too early, wait and repeat it.

Change the input to trigger processing again. Stop with `Ctrl-C`; active commands drain before exit. This workspace retains state, so unchanged successful inputs are skipped on later runs.

If fev is not installed, obtain its executable first. From a source checkout, Rust/Cargo and a C compiler are required: run `cargo build --release` in that checkout, then use the resulting executable's **absolute path** instead of `fev` when following this guide.

## 3. Adapt the configuration

1. Resolve `path` and `state` relative to the YAML, not the launching terminal. Create roots first; keep state outside every root.
2. Assign nonempty, unique root ids and per-root rule ids.
3. Use `events: [created]` for creation **and updates**. Match the entire root-relative key using anchored regex, not glob syntax.
4. Access `(?P<name>...)` as `"${MATCH_NAME}"`. Only the variable name is uppercased; values retain spaces and case. There is no `{{ name }}` substitution.
5. Quote `"$FILE"` and output paths. Commands run inside the root. Use one `run` array for ordered commands; separate rules have no ordering guarantee, even with `concurrency: 1`.
6. Exclude generated files from input patterns, avoid competing writes, and leave final outputs unmatched.

The template's `[^/.]+` excludes dotted basenames and subdirectories, preventing its outputs from looping. Broaden it only after checking generated filenames.

Each array item is a separate `sh -c`; `cd`, variables, and `export` do not carry over. Use one `|` block when shell state must be shared, with appropriate failure handling.

## 4. Verify safely and deliver

There is **no `--check` or dry-run**. Startup processes existing inputs. Use trusted commands, isolated roots/state, and test services where needed.

Verify initial processing, updates, loop avoidance, expected outputs or external effects, and restart skipping. Restart after YAML changes; configuration is not hot-reloaded.

Deliver the complete YAML, destination, tool prerequisites, setup/start/input/inspection/stop commands, existing-input impact, and exercised verification. Do not claim untested external operations succeeded.

## Common mistakes

| Symptom | Fix |
| --- | --- |
| Missing root | Create it at the YAML-relative path |
| Unknown fields/events | Use `state`, `settle`, and `created`; not `state_dir`, `debounce`, `out`, or `updated` |
| YAML parse error | Use spaces, single-quoted regex, and `|` for complex commands |
| Nothing runs | Check full-key matching, regular-file eligibility, and success records |
| Missing capture | Use uppercase `MATCH_NAME`; unmatched optional groups are empty |
| Edited `run` does not replay | Update the input or intentionally change the rule id |
| Partial output triggers downstream work | Follow [output publication guidance](references/configuration.md#outputs-and-failures) |
