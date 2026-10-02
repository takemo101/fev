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
| `rules[].events` | Required | Nonempty array of `startup`, `created`, `updated`, `deleted`, or `renamed`; duplicates do not multiply execution |
| `rules[].match` | Required | Regex string beginning with `^` and ending with `$` |
| `rules[].run` | Required | Command string or nonempty array of command strings; no blank commands |

Unknown fields are rejected at every configuration level. `out`, `state_dir`, and `debounce` are not configuration fields. An external command's own `--out` argument is valid if that tool supports it.

## Paths and events

- Relative `path` and `state` values resolve against **the YAML file's parent directory**, not the launching shell's working directory.
- `~` and `~/...` expand using HOME. Shell variables such as `$HOME` inside YAML paths are not expanded.
- Paths resolve to their actual filesystem locations. Roots must not overlap or contain one another. State must not equal a root or lie inside one.
- Roots are watched recursively. Events describe regular files, not directory entries; symlink paths are excluded. Directory creation, deletion, and rename expand to regular-file descendants.
- A key is a root-relative path using `/`, without a leading `/`. With root `/data/work`, `/data/work/reports/Weekly.txt` has key `reports/Weekly.txt`.
- `startup` means an existing regular file found in the one-time initial root scan, delivered only to rules explicitly accepting it. `created` means a newly appearing file after that scan, never initial inventory. `updated` means a change at an existing key.
- The initial scan always builds the internal inventory for update/deletion/rename classification, even without startup rules. Once ready, updates to existing files remain eligible, including while initial settling is pending. Without startup selected, initial inputs execute no commands even with a fresh or missing ledger.
- Startup/created/updated content events coalesce. Size must remain stable for `settle` before the material identity is finalized; a size change restarts the wait. Disappearance invalidates pending content work, not genuine queued transitions. If an initial file updates during initial settle, startup-only and updated-only rules remain independently eligible; a rule accepting both coalesces the latest material to one `updated` run.
- Only the initial root scan generates `startup`. Scans of introduced or moved-in directories generate `created`/`updated`; correlated same-root renames remain `renamed`.
- `deleted` describes a vanished key. It is queued after a rename-correlation grace of `max(settle, 100ms)`.
- `renamed` describes a correlated old/new pair within one root. It queues immediately when correlated, without waiting for `settle`; matching and captures use the **new** key.
- Startup has no offline deletion/rename history. Native notifications are not a complete operation log: macOS FSEvents supplies single-path rename notifications, so fev correlates its inventory using device/inode identity. Missing or ambiguous observations can prevent rename correlation.
- Native rename trackers pair captured source snapshots with their destinations; known transitions can survive later disappearance or reuse of those destinations. Directory moves correlate inventoried source descendants; files introduced afterward retain their creation/update events. Explicit removals remain deletions even if a replacement already occupies the same key.
- Uncorrelated moves are handled as deletion and creation/update. Moves between watched roots are deletion in the source root and creation/update in the destination, never same-root rename.
- `settle` does not guarantee the writer has finished. Normal operation uses native OS notifications, not repeated full-root polling; introduced or renamed directory subtrees are scanned.

### Choose initial processing per rule

| Events | Behavior |
| --- | --- |
| `[startup]` | Initial inventory only; no live creation/update subscription |
| `[created, updated]` | Live-only content processing; no initial commands, even without state |
| `[startup, created, updated]` | Initial inventory plus live content processing |

The basic template stays live-only. There is no default event-list change or compatibility alias for the old initial-scan-created behavior; explicitly add the exact type `startup` to each rule that should retain initial processing.

### Transition example

Add these rules to an existing root's `rules` array to observe path transitions without reading historical file contents:

```yaml
- id: report-renames
  events: [renamed]
  match: '^reports/(?P<name>[^/]+)\.txt$'
  run: 'printf "%s: %s -> %s (new name: %s)\n" "$EVENT" "$OLD_KEY" "$KEY" "$MATCH_NAME"'
- id: report-deletions
  events: [deleted]
  match: '^reports/(?P<name>[^/]+)\.txt$'
  run: 'printf "%s: %s\n" "$EVENT" "$KEY"'
```

A move from `drafts/Weekly.txt` to `reports/Weekly.txt` matches the rename rule with `MATCH_NAME=Weekly`; moving out of `reports/` does not match it. Directory renames apply the same new-key matching to each regular descendant.

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
| `EVENT` | `startup`, `created`, `updated`, `deleted`, or `renamed` |
| `KEY` | Current relative path for content events, vanished path for deletion, new path for rename |
| `FILE` | Absolute path corresponding to `KEY` |
| `OLD_KEY` | Previous relative path for rename; explicitly empty otherwise |
| `OLD_FILE` | Previous absolute path for rename; explicitly empty otherwise |
| `MATCH_<NAME>` | This rule's named capture value |

Quote expansions as `"$FILE"` and `"${MATCH_NAME}"`. The working directory is the root. Standard input is closed, so commands cannot request interactive input. Standard output and standard error are inherited from fev.

For deleted/renamed events, `FILE` and `OLD_FILE` are historical path references, not snapshots; they may be absent or reused when commands start. fev requires a valid root-relative key and a real root directory, rejects symlinks and non-directory existing parents, and permits missing descendant components. Rename validates both paths. Startup/created/updated still require a regular input file. These guards do not sandbox command outputs or guarantee historical contents.

Inherited variables beginning with `MATCH_` are removed before setting this rule's captures. `EVENT`, `OLD_KEY`, and `OLD_FILE` are always explicitly set so inherited values cannot leak. Legacy `OUT` and `OUT_TMP` are removed. Other inherited environment variables remain available; provide the PATH, credentials, and other settings required by external tools.

A string `run` executes once through `sh -c`. An array executes each item sequentially through a **separate** `sh -c`. A previous item's `cd`, shell variables, and `export` do not carry over; filesystem changes and external effects do. Do not assume Bash-specific syntax works under `sh`.

Exit code `0` advances to the next item. A launch failure or nonzero exit stops the remaining items. Within a multiline item, fev sees only the shell's final exit status, not each line's status. Use suitable `set -eu` or `&&` handling when intermediate failures must stop execution, and account separately for failures inside pipelines.

## Sequential, parallel, and chained work

- **Every matching rule runs**, not just the first match. Separate rules have no guaranteed start or completion order.
- Global `concurrency` limits simultaneous rule jobs across all roots. An entire `run` array occupies one slot; its items are not parallelized.
- At most one command sequence runs for a given root, key, and rule across **all event types**. Different matching rules may execute in parallel.
- Startup/created/updated events can process the latest changed identity after a current run finishes; every intermediate update is not guaranteed. Distinct queued deletion/rename occurrences are preserved in their queued order for each root/key/rule, even when later changes invalidate pending content jobs.
- Use one `run` array for commands that depend on one another. `concurrency: 1` does not establish rule ordering.
- To chain through files, make each stage's output match only the intended next stage. Outputs pass through normal OS notifications and `settle`; completing a command does not directly start the next rule.

## Outputs and failures

A rule succeeds when all its commands exit with code `0`. Producing no output file is valid. Commands choose output names, destinations, directory creation, and file counts. fev does not automatically move or delete inputs.

Commands are trusted and **not sandboxed**. Configuration and commands must prevent unsafe output paths, competing writes, and self-processing loops.

Direct writes can expose incomplete files, including files created before a command fails, to downstream rules. To publish only completed output, write a temporary file **outside all watched roots on the same filesystem as the destination**, then rename it into the target root after successful completion. Handle temporary-file cleanup on failure in the command. A cross-filesystem `mv` may copy instead of atomically rename and does not provide the same guarantee.

Failure does not roll back files or external effects, and does not interrupt other matching rules. The same failed content identity is not immediately retried within the same process. A size or modification-time change can make a matching live content rule eligible again, beginning at the first array item. Restart alone retries failed initial work only when that rule subscribes to `startup`; live-only rules do not retry unchanged initial files merely because fev restarted. Failed historical transitions have no offline replay on restart.

## Ledger, replay, and migration

Successes are stored in `state/ledger.sqlite3`. Startup/created/updated share deduplication by **root id, key, rule id, and material identity**, with `event_id = 0`. Identity is file size plus modification time in nanoseconds, not a content hash. Event kind is recorded but does not distinguish these three types for content deduplication. Each genuine deleted/renamed transition receives a unique durable positive occurrence id, so repeated transitions with the same prior size/mtime remain distinct.

- A recorded successful content combination is skipped, including after restart and including successes recorded before startup was split from created. Enabling startup does not invalidate those successes; the split requires no ledger schema change or migration.
- Retaining the ledger already prevented replay of the same successful identity. Omitting startup additionally avoids all initial commands with fresh or missing state. With startup enabled, deleting/changing the ledger, changing root/rule ids, or a crash before recording success can otherwise permit initial re-execution.
- A size or modification-time change makes matching content rules eligible again.
- Changing only `run` or `match` does not invalidate success records. To intentionally replay successful input, update the input or change its rule id.
- External effects and success recording are not one transaction. A crash after a command finishes but before recording can cause re-execution. Make operations idempotent where necessary; do not depend on exactly-once effects.
- Configuration is loaded at startup, not hot-reloaded. Restart after editing YAML. SIGINT / SIGTERM stop scheduling new work and drain active command sequences before exit.

Existing per-rule ledgers without event columns are migrated atomically in a transaction, retaining successes as content records (`event_id = 0`, historical kind `created`); this older migration is separate from the startup split, which needs no migration. For older configurations, remove `out` and specify outputs directly in `run`; replace commands depending on `OUT` or `OUT_TMP`. A ledger without rule ids is atomically archived as `legacy_successes`; those records cannot suppress new execution, so initial inputs may be reprocessed by matching startup rules. Review side effects first, and never run an old binary against the same state concurrently.

## Running and validation

With fev installed on PATH, start it using `fev --config config.yaml`; `-c` is equivalent to `--config`. It runs in the foreground. Use `fev --help` for available arguments and `fev --version` for the version. Runtime logs use standard error; command output also appears in the foreground process.

There is **no `--check` or dry-run option**. Only matching startup rules can execute commands for existing inputs from the initial scan; once running, native events can still trigger live rules. A generic YAML parser checks syntax but cannot establish valid roots, supported regex, or fev-specific constraints. Validate with trusted commands, isolated inputs/state, and test environments for external services.
