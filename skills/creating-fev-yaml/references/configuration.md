# fev YAML Configuration Reference

This bundled reference contains the configuration and execution contract needed to create and use fev YAML. It is self-contained: no repository specification or source files are needed. The runnable example is [basic.yaml](../templates/basic.yaml); the workflow is [SKILL.md](../SKILL.md).

## Configuration fields

A configuration is one YAML mapping. Indent with spaces, not tabs. Quote regular expressions with single quotes to preserve backslashes. Use quoted strings or a `|` block for shell commands containing YAML-sensitive punctuation. Commands must be strings; for example, write `run: 'true'`, not the YAML boolean `run: true`.

| Field | Required / default | Type and constraints |
| --- | --- | --- |
| `settle` | Optional; `2s` | Duration string such as `200ms` or `2s`; wait for file size to stabilize |
| `concurrency` | Optional; `2` | Integer at least 1; maximum simultaneous rule jobs across all roots |
| `state` | Optional; `./state` | Success-ledger directory outside every watched root; created if missing |
| `roots` | Required | Array of root mappings |
| `roots[].id` | Required | Nonempty string, unique across the configuration |
| `roots[].path` | Required | String naming an existing directory to watch |
| `roots[].rules` | Required | Array of rules belonging to this root |
| `rules[].id` | Required | Nonempty string, unique within its root |
| `rules[].events` | Required | Nonempty array containing only `created` |
| `rules[].match` | Required | Regex string beginning with `^` and ending with `$` |
| `rules[].run` | Required | Command string or nonempty array of command strings; no blank commands |

Unknown fields are rejected at every configuration level. `out`, `state_dir`, and `debounce` are not configuration fields. An external command's own `--out` argument is valid if that tool supports it.

## Paths and events

- Relative `path` and `state` values resolve against **the YAML file's parent directory**, not the launching shell's working directory.
- `~` and `~/...` expand using HOME. Shell variables such as `$HOME` inside YAML paths are not expanded.
- Paths resolve to their actual filesystem locations. Roots must not overlap or contain one another. State must not equal a root or lie inside one.
- Roots are watched recursively. Only regular files are eligible; directories themselves and input paths containing symlinks are not processed.
- A key is a root-relative path using `/`, without a leading `/`. With root `/data/work`, `/data/work/reports/Weekly.txt` has key `reports/Weekly.txt`.
- `created` covers both new files and updates to existing files. Existing inputs are also scanned once at startup. `updated`, `deleted`, and other event names are unsupported.
- Size must remain stable for `settle` before the file identity is finalized. A size change restarts the wait; disappearance discards pending work. Deletion is not itself a processing event.
- `settle` does not guarantee the writer has finished. Normal operation uses native OS notifications, not repeated full-directory polling.

## Regular expressions and captures

`match` is a Rust `regex` expression, not a glob. It must match the entire key. Lookahead, lookbehind, and backreferences are unsupported. Use single-quoted YAML strings to preserve `\`.

| Regex | Matches |
| --- | --- |
| `'^(?P<name>[^/.]+)\.txt$'` | Root-level `.txt` inputs with no dot in the basename |
| `'^reports/(?P<name>[^/.]+)\.txt$'` | The same naming convention directly inside `reports/` |
| `'^(?P<name>[^/.]+)\.upper\.txt$'` | Root-level `.upper.txt` inputs for the next stage |

For `(?P<name>...)`, uppercase the capture **name** and prefix it with `MATCH_`. The captured value is unchanged: `Weekly Report.txt` produces `MATCH_NAME=Weekly Report`. An unmatched optional group produces an empty string.

Capture names must be accepted by the regex engine and contain only ASCII letters, digits, and `_`. Names that collide after uppercasing, such as `name` and `NAME`, are rejected within the same rule. No `{{ name }}` or other direct command-template substitution exists.

The sample's exclusion of dots and subdirectories is a naming convention for avoiding output loops, not a universal filename restriction. If you broaden a pattern, explicitly exclude or segregate generated outputs.

## Environment and shell execution

| Environment variable | Value |
| --- | --- |
| `ROOT` | Absolute watched-root path |
| `KEY` | Root-relative input path |
| `FILE` | Absolute input-file path |
| `MATCH_<NAME>` | This rule's named capture value |

Quote expansions as `"$FILE"` and `"${MATCH_NAME}"`. The working directory is the root. Standard input is closed, so commands cannot request interactive input. Standard output and standard error are inherited from fev.

Inherited variables beginning with `MATCH_` are removed before setting this rule's captures. Legacy `OUT` and `OUT_TMP` are also removed. Other inherited environment variables remain available; provide the PATH, credentials, and other settings required by external tools.

A string `run` executes once through `sh -c`. An array executes each item sequentially through a **separate** `sh -c`. A previous item's `cd`, shell variables, and `export` do not carry over; filesystem changes and external effects do. Do not assume Bash-specific syntax works under `sh`.

Exit code `0` advances to the next item. A launch failure or nonzero exit stops the remaining items. Within a multiline item, fev sees only the shell's final exit status, not each line's status. Use suitable `set -eu` or `&&` handling when intermediate failures must stop execution, and account separately for failures inside pipelines.

## Sequential, parallel, and chained work

- **Every matching rule runs**, not just the first match. Separate rules have no guaranteed start or completion order.
- Global `concurrency` limits simultaneous rule jobs across all roots. An entire `run` array occupies one slot; its items are not parallelized.
- At most one command sequence runs for a given root, key, and rule. Different rules matching the same key may execute in parallel.
- If an input changes during execution, the rule can process the latest changed identity after its current run finishes. Processing every intermediate update individually is not guaranteed.
- Use one `run` array for commands that depend on one another. `concurrency: 1` does not establish rule ordering.
- To chain through files, make each stage's output match only the intended next stage. Outputs pass through normal OS notifications and `settle`; completing a command does not directly start the next rule.

## Outputs and failures

A rule succeeds when all its commands exit with code `0`. Producing no output file is valid. Commands choose output names, destinations, directory creation, and file counts. fev does not automatically move or delete inputs.

Commands are trusted and **not sandboxed**. Configuration and commands must prevent unsafe output paths, competing writes, and self-processing loops.

Direct writes can expose incomplete files, including files created before a command fails, to downstream rules. To publish only completed output, write a temporary file **outside all watched roots on the same filesystem as the destination**, then rename it into the target root after successful completion. Handle temporary-file cleanup on failure in the command. A cross-filesystem `mv` may copy instead of atomically rename and does not provide the same guarantee.

Failure does not roll back files or external effects, and does not interrupt other matching rules. The same failed identity is not immediately retried within the same process. Changing the input's size or modification time, or restarting fev, makes a failed rule eligible again, beginning at the first array item.

## Ledger, replay, and migration

Successes are stored in `state/ledger.sqlite3`. Deduplication uses **root id, key, rule id, and file identity**. Identity is file size plus modification time in nanoseconds, not a content hash.

- A recorded successful combination is skipped, including after restart.
- A size or modification-time change makes matching rules eligible again.
- Changing only `run` or `match` does not invalidate success records. To intentionally replay successful input, update the input or change its rule id.
- External effects and success recording are not one transaction. A crash after a command finishes but before recording can cause re-execution. Make operations idempotent where necessary; do not depend on exactly-once effects.
- Configuration is loaded at startup, not hot-reloaded. Restart after editing YAML. SIGINT / SIGTERM stop scheduling new work and drain active command sequences before exit.

For legacy configurations, remove `out` and specify outputs directly in `run`. Replace commands depending on `OUT` or `OUT_TMP`. An old ledger without rule ids is archived as `legacy_successes`; those records do not suppress new execution. The first run after migration reprocesses existing matching input. Review side effects first, and never run an old binary against the same state concurrently.

## Running and validation

With fev installed on PATH, start it using `fev --config config.yaml`; `-c` is equivalent to `--config`. It runs in the foreground. Use `fev --help` for available arguments and `fev --version` for the version. Runtime logs use standard error; command output also appears in the foreground process.

There is **no `--check` or dry-run option**. Startup can execute commands against existing inputs. A generic YAML parser checks syntax but cannot establish valid roots, supported regex, or fev-specific constraints. Validate with trusted commands, isolated inputs/state, and test environments for external services.
