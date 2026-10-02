# fev

`fev` is a CLI tool that runs shell commands on native filesystem events, using a single YAML configuration file.

Watch local directories, match file paths with regular expressions, and run one or more commands. Commands can create files for the next stage, call external tools, or finish without producing any files.

## Manual

- [Configuration reference](skills/creating-fev-yaml/references/configuration.md) — fields, matching, execution, state, and failure behavior.
- [YAML authoring skill](skills/creating-fev-yaml/SKILL.md) — step-by-step configuration creation and verification.
- [Starter configuration](skills/creating-fev-yaml/templates/basic.yaml) — a runnable example using standard shell tools.

The authoring skill includes its own specification and template, so it can be used outside this repository without other project documents.

## Purpose

- React to file creation, updates, deletion, and same-root renames using native OS notifications rather than repeated directory polling.
- Run every matching rule independently, with a shared concurrency limit.
- Combine sequential commands and file-triggered stages without requiring an output file from every job.
- Keep a durable, per-rule success ledger so unchanged successful inputs are skipped after restart.

## CLI

### Install on macOS / Linux

Install a prebuilt binary from [GitHub Releases](https://github.com/takemo101/fev/releases) to `~/.local/bin/fev`. No Rust toolchain or repository clone is required.

```sh
curl -fsSL https://raw.githubusercontent.com/takemo101/fev/main/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
fev --help
```

The default installation path is `~/.local/bin/fev`. Add that directory to PATH in your shell configuration to make it available in future terminals.

Choose a different installation directory:

```sh
curl -fsSL https://raw.githubusercontent.com/takemo101/fev/main/install.sh | INSTALL_DIR="$HOME/bin" sh
```

Install an exact release tag instead of the latest release:

```sh
curl -fsSL https://raw.githubusercontent.com/takemo101/fev/main/install.sh | VERSION=v0.1.0 sh
```

The installer requires `curl`, `tar`, standard POSIX utilities, and either `sha256sum` or `shasum`. It selects the release archive using the host OS and CPU:

| OS | Architecture | Release asset |
| --- | --- | --- |
| macOS | Apple Silicon / arm64 | `fev-aarch64-apple-darwin.tar.gz` |
| macOS | Intel / x86_64 | `fev-x86_64-apple-darwin.tar.gz` |
| Linux | x86_64 / amd64 | `fev-x86_64-unknown-linux-musl.tar.gz` |
| Linux | arm64 / aarch64 | `fev-aarch64-unknown-linux-musl.tar.gz` |

Linux archives use musl targets. Windows and other OS/architecture combinations are unsupported. Each archive contains `fev`; `checksums.txt` covers all four archives. Installation fails if the checksum is missing or invalid, before replacing the installed executable. An existing directory or directory symlink at the destination `fev` path is rejected without writing inside it. A published release containing these assets is required.

Add a custom installation directory to PATH as needed. `FEV_REPO=owner/repo` can select a different repository publishing the same asset format.

### Uninstall

If installed with `install.sh`, remove the executable:

```sh
rm -f "$HOME/.local/bin/fev"
# Use the directory you selected for a custom installation:
rm -f "$HOME/bin/fev"
```

If installed from a source checkout using `just install`, run from that checkout:

```sh
just uninstall
# Use the same directory as your source installation:
INSTALL_DIR="$HOME/bin" just uninstall
```

Uninstalling removes only the executable. YAML configurations, watched files, command outputs, and success ledgers remain untouched.

### Build and install from source

Requires Git, Rust/Cargo, and a C compiler. SQLite is bundled; a separate SQLite installation is not required.

```sh
git clone https://github.com/takemo101/fev.git
cd fev
```

Build without installing:

```sh
cargo build --release
./target/release/fev --help
```

With [`just`](https://just.systems/) installed, run these commands from the repository root:

```sh
just install
# Or build without installing:
just build-release
# Optional custom destination:
INSTALL_DIR="$HOME/bin" just install
```

If Cargo is not on PATH, the recipe tries `${CARGO_HOME:-$HOME/.cargo}/env`; Rust must already be installed. Source installation defaults to `~/.local/bin/fev`, just like the download installer.

You can also run locally through Cargo:

```sh
cargo run -- --help
cargo run -- --config examples/demo.yaml
```

Create `examples/demo/work` before running the second command, or use `just demo`, which creates it for you.

### Run

```sh
fev --config config.yaml
# Equivalent short option:
fev -c config.yaml

fev --help
fev --version
```

`fev` runs in the foreground. Stop with `Ctrl-C` or SIGTERM; it stops starting new work and waits for active command sequences to finish. Runtime logs go to stderr, and command stdout/stderr are inherited.

There is **no `--check` or dry-run option**. Starting the runner scans existing inputs, but only matching rules subscribed to `startup` may execute for that initial scan after settling. Once running, native events can trigger the normal live rules. Use trusted commands and isolated test directories when trying a new configuration.

## Quick start

After installation, no repository clone is needed for this example:

```sh
mkdir -p "$HOME/fev-example/work"
curl -fsSL https://raw.githubusercontent.com/takemo101/fev/main/skills/creating-fev-yaml/templates/basic.yaml -o "$HOME/fev-example/config.yaml"
fev --config "$HOME/fev-example/config.yaml"
```

Keep that terminal running. In a second terminal, create an input:

```sh
printf 'hello fev\n' > "$HOME/fev-example/work/Weekly Report.txt"
```

The starter configuration performs these jobs:

```text
Weekly Report.txt
  uppercase -> Weekly Report.upper.txt
  observe   -> log the original input; no output file
Weekly Report.upper.txt
  finish    -> Weekly Report.done.txt
Weekly Report.done.txt
  no matching rule; processing stops
```

After processing completes, inspect the result:

```sh
cat "$HOME/fev-example/work/Weekly Report.done.txt"
```

Expected content:

```text
HELLO FEV
```

The original and intermediate files remain in `work`. Update the input to process it again:

```sh
printf 'updated input\n' > "$HOME/fev-example/work/Weekly Report.txt"
```

After processing, the final file contains `UPDATED INPUT`. If inspection is too early, wait and repeat it. The starter uses `[created, updated]`: existing files do not run on initial scan, even with fresh or missing state. The state directory is `$HOME/fev-example/state`; keeping it also suppresses unchanged successful material identities. Stop the runner with `Ctrl-C` before editing and restarting the configuration.

### Repository demo

The repository also includes a two-stage demo with a final report header:

```sh
# Terminal 1: watch; Ctrl-C to stop.
just demo
```

```sh
# Terminal 2: create or update input, then inspect after processing.
just demo-input
cat examples/demo/work/hello.done.txt
```

Expected content:

```text
Processed: hello.upper.txt
HELLO FEV
```

The demo explicitly uses `[startup, created, updated]` to process existing inputs too. For an automated, isolated demo that checks this opt-in startup processing, live updates, and shutdown:

```sh
just demo-test
```

Demo files and state live under `examples/demo/` and are git-ignored.

## Configuration

This is the starter workflow. Save it as `config.yaml`, and create `work` beside that file before starting fev:

```yaml
settle: 200ms
concurrency: 2
state: ./state

roots:
  - id: example
    path: ./work
    rules:
      - id: uppercase
        events: [created, updated]
        match: '^(?P<name>[^/.]+)\.txt$'
        run:
          - |
            tr '[:lower:]' '[:upper:]' < "$FILE" > "${MATCH_NAME}.upper.txt"
          - 'printf "Uppercased: %s\n" "$KEY"'

      - id: finish
        events: [created, updated]
        match: '^(?P<name>[^/.]+)\.upper\.txt$'
        run: 'cat "$FILE" > "${MATCH_NAME}.done.txt"'

      - id: observe
        events: [created, updated]
        match: '^(?P<name>[^/.]+)\.txt$'
        run: 'printf "Observed: %s\n" "$KEY"'
```

### Fields

| Field | Meaning |
| --- | --- |
| `settle` | File-size stability wait; defaults to `2s` |
| `concurrency` | Maximum simultaneous rule jobs across all roots; defaults to `2`, minimum `1` |
| `state` | Success-ledger directory outside every root; defaults to `./state` beside the YAML |
| `roots[].id` | Nonempty root identifier, unique in the configuration |
| `roots[].path` | Existing directory to watch recursively |
| `roots[].rules` | Rules applied only to that root |
| `rules[].id` | Nonempty rule identifier, unique within its root |
| `rules[].events` | Nonempty list of `startup`, `created`, `updated`, `deleted`, or `renamed`; repeated names do not repeat execution |
| `rules[].match` | Full-key regex beginning with `^` and ending with `$` |
| `rules[].run` | Command string or nonempty array of nonblank command strings |

Only `settle`, `concurrency`, and `state` are optional. Relative root/state paths resolve against **the YAML file's directory**, not the launching terminal's working directory. `~` expands to HOME; `$HOME` inside a YAML path does not expand. Roots cannot overlap, and state cannot be inside any root. State is created automatically if missing.

`startup` means an existing regular file found by the initial root scan, and is opt-in per rule. `created` means a newly appearing file after that scan; `updated` means a change at an existing key. Use `[created, updated]` for live-only processing, `[startup]` for initial processing only, or `[startup, created, updated]` to include both. There is no default event-list change or compatibility alias: add the exact name `startup` explicitly to retain initial processing.

The initial scan always builds the internal inventory for update, deletion, and rename classification, even without any startup subscription. Existing-file updates work once ready, including while initial settling is pending. Startup/created/updated content events settle and coalesce; `settle` is not a writer-completion guarantee. If an initial file updates during settling, startup-only and updated-only rules remain independently eligible; a rule accepting both coalesces the latest material into one `updated` run. Only the initial root scan generates `startup`; introduced or moved-in directory scans produce `created`/`updated`, while correlated same-root moves remain `renamed`. Startup does not replay deletions or renames that happened while fev was stopped.

`deleted` means a disappearance, queued after a rename-correlation grace of at least `100ms` or `settle`, whichever is longer. `renamed` means a correlated move within one root, queued immediately once the old/new pair is known, without settling. Directory operations expand into events for regular-file descendants, not directory entries. Symlink paths are excluded.

Native notifications are not a complete operation history. On macOS, single-path FSEvents rename notifications are correlated using the root's inventory and device/inode identity; a missing or ambiguous pair cannot guarantee `renamed`. Uncorrelated moves become deletion and creation/update events. Moves across watched roots are `deleted` in the source and `created`/`updated` in the destination, not `renamed`. Newly introduced or renamed directory subtrees are scanned without periodic full-root polling.

### Matching and environment

A key is the root-relative file path, such as `reports/Weekly.txt`. A rule must accept the event type and match the entire key. For `renamed`, matching and captures use the **new** key, never the old one. `match` uses Rust regular expressions, not shell globs; lookaround and backreferences are unsupported.

The example's `[^/.]+` accepts spaces and mixed case, but excludes dots and subdirectories. This is an example naming convention that keeps `.upper.txt` and `.done.txt` out of the original-input rule, not a restriction on all fev inputs.

| Variable | Value |
| --- | --- |
| `ROOT` | Absolute watched-root path |
| `EVENT` | `startup`, `created`, `updated`, `deleted`, or `renamed` |
| `KEY` | Current key for content events; vanished key for deletion; new key for rename |
| `FILE` | Absolute path corresponding to `KEY` |
| `OLD_KEY` / `OLD_FILE` | Previous relative/absolute path for rename; explicitly empty for every other event |
| `MATCH_<NAME>` | Named regex capture value |

`(?P<name>...)` becomes `MATCH_NAME`. Only the variable name is uppercased; the value stays unchanged. An unmatched optional capture is empty. Capture names must use ASCII letters, digits, or `_`, be accepted by the regex engine, and not collide after uppercasing.

Quote `"$FILE"` and `"${MATCH_NAME}"`. Commands run with the root as their working directory and with standard input closed. There is no `{{ name }}` template substitution.

For deletion and rename, paths are historical references, not file snapshots: `FILE` and `OLD_FILE` may be absent or reused when commands start. fev validates safe root-relative paths and rejects symlinks in existing components, but does not require historical paths to exist. Do not read them as guaranteed old/new contents.

### Sequential and parallel execution

- Every matching rule runs; the first match does not suppress later rules.
- Separate rules have no guaranteed execution order, even with `concurrency: 1`.
- A `run` array executes sequentially and occupies one concurrency slot for its entire duration.
- Each array item uses a separate `sh -c`: `cd`, variables, and `export` do not carry over. Use one multiline item when shell state must be shared.
- A nonzero exit or launch failure stops the remaining array items, without interrupting other matching rules.
- The same root/key/rule cannot have overlapping command sequences, across all event types. Content updates coalesce; distinct queued deletion/rename occurrences are preserved in their queued order for that root/key/rule, even if later changes invalidate pending content work.

For dependent steps, use one array or separate file patterns for each stage. Output-triggered stages run through OS notifications and `settle`, not directly when the producer exits.

## State and safety

Successes are stored in `state/ledger.sqlite3`. Content events share material-identity deduplication by root id, key, rule id, file size, and nanosecond modification time (`event_id = 0`) across `startup`, `created`, and `updated`. Unchanged successful inputs are skipped across restarts, including successes recorded before startup was split from created; enabling startup does not invalidate them. This split requires no ledger schema change or migration. Deleted/renamed transitions each receive a durable positive occurrence id, so repeated transitions are not suppressed by identical size/mtime. Event kind is recorded for visibility, not as a content-deduplication discriminator. Changing only `run` or `match` does not force content replay; update the input or intentionally change the rule id.

With retained state, the same successful identity was already suppressed on restart. Opting out of startup additionally prevents initial commands even if the ledger is fresh or missing. With startup enabled, deleting/changing the ledger, changing root/rule ids, or a crash before success recording can otherwise cause initial re-execution.

- Commands own output paths and file creation. Outputless jobs are valid; fev does not move or delete inputs automatically.
- Commands are not sandboxed. Prevent unsafe paths, competing writes, and processing loops in your configuration and commands.
- `settle` is not a writer-completion guarantee. Direct writes can expose partial outputs to later stages. For completed-file publication, write outside all watched roots on the destination filesystem, then rename into the target root after success.
- Failure does not roll back files or external effects. Failed content identities are not immediately retried in the same process; a matching live identity change makes them eligible again. Restart alone retries failed initial work only for rules subscribed to `startup`. Historical deletion/rename events are not replayed on restart.
- External effects and ledger recording are not atomic. Operations may run again after a crash; do not rely on exactly-once execution.
- YAML changes require a restart. Shutdown has no command timeout, so a command that never finishes can keep shutdown waiting.

Legacy `out` configurations are rejected, and `OUT` / `OUT_TMP` are not provided. Existing per-rule ledgers are migrated atomically, preserving successes as content records with `event_id = 0`. Older success records without rule ids are atomically archived as `legacy_successes` and do not suppress new jobs; that migration can reprocess existing input. Do not run old and new binaries against the same state simultaneously.

See the [configuration reference](skills/creating-fev-yaml/references/configuration.md) for the full contract.

## Agent skill

The reusable skill lives at [`skills/creating-fev-yaml/`](skills/creating-fev-yaml/SKILL.md). Keep the entire directory together, including `references/` and `templates/`, when copying it into a skill-capable agent's skills location.

Example request:

> Use the creating-fev-yaml skill to create a configuration that watches my input directory, runs my processing command, and logs completion. Include setup, verification, and shutdown instructions.

The skill documents fev's configuration contract without depending on other repository documents. Installing it into an agent is separate from installing the fev executable.

## Releases

The [release workflow](.github/workflows/release.yml) builds all four target archives when a `v*` tag is pushed. Keep the package version in `Cargo.toml` aligned with the release tag.

- macOS builds run on Apple Silicon and Intel runners.
- Linux builds use `cross` with musl targets and the C cross-compilers needed by bundled SQLite.
- After every target builds successfully, one publication job uploads the four archives and a complete `checksums.txt` to GitHub Releases.
- Manual `workflow_dispatch` runs build downloadable workflow artifacts without publishing a release, even when run against a tag.

The installer uses the latest published release by default, or an exact tag supplied through `VERSION`. It never compiles from source or skips checksum verification.

## Development

Run from the repository root:

```sh
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
just demo-test
python3 scripts/test-install.py
```

The installer regression suite requires Python 3 and the native `target/release/fev` built by `just demo-test` or `cargo build --release`. It uses real archives, a loopback HTTP server, and temporary installation directories; no GitHub access or user installation is needed.

Use `just --list` to see the available build, install, uninstall, and demo recipes. For always-on operation, manage the foreground process with a service manager such as macOS `launchd` or Linux `systemd`.

## License

MIT.
