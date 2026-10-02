# fev

Run shell commands when files change. Configure watched directories, filename patterns, and commands in one YAML file.

Use it to convert files, call external tools, or chain processing steps through their output files. fev uses native OS notifications, not repeated directory polling.

## Install

For macOS and Linux (arm64 or x86_64):

```sh
curl -fsSL https://raw.githubusercontent.com/takemo101/fev/main/install.sh | sh
export PATH="$HOME/.local/bin:$PATH"
fev --version
```

The installer downloads the latest [release](https://github.com/takemo101/fev/releases) and verifies its SHA-256 checksum. No Rust installation is needed.

Optional: set `INSTALL_DIR="$HOME/bin"` for a different location, or `VERSION=v0.1.1` for an exact release tag. Add your installation directory to PATH in your shell configuration.

## Try it

This example turns `.txt` files into uppercase copies.

### 1. Create a directory and configuration

```sh
mkdir -p "$HOME/fev-example/work"
```

Save this as `$HOME/fev-example/config.yaml`:

```yaml
settle: 200ms

roots:
  - id: example
    path: ./work
    rules:
      - id: uppercase
        events: [created, updated]
        match: '^(?P<name>[^/.]+)\.txt$'
        run: |
          tr '[:lower:]' '[:upper:]' < "$FILE" > "${MATCH_NAME}.upper.txt"
          printf 'Processed: %s\n' "$KEY"
```

- `path`: directory to watch. Relative paths start at the YAML file's directory.
- `events`: changes that trigger the rule.
- `match`: regular expression for the entire root-relative file path.
- `run`: shell command to execute inside the watched directory.

The named capture `name` becomes `$MATCH_NAME`; `$FILE` is the absolute input path. The pattern excludes names containing dots, so the generated `.upper.txt` file does not trigger a processing loop.

### 2. Start watching

```sh
fev --config "$HOME/fev-example/config.yaml"
```

Keep this terminal open. Once it prints `fev: watching`, create an input in a second terminal:

```sh
printf 'hello fev\n' > "$HOME/fev-example/work/hello.txt"
```

After the first terminal prints `Processed: hello.txt`, inspect the result:

```sh
cat "$HOME/fev-example/work/hello.upper.txt"
# HELLO FEV
```

Editing `hello.txt` updates the uppercase copy. Stop with `Ctrl-C`; fev waits for running commands to finish.

## Choose events

| Event | Runs when… |
| --- | --- |
| `startup` | An existing file is found during the initial scan |
| `created` | A new file appears after the initial scan |
| `updated` | A file at an existing path changes |
| `deleted` | A file disappears |
| `renamed` | A move is correlated within the same watched root |

**Existing files do not run at startup unless you explicitly include `startup`.**

```yaml
# New files and live updates only:
events: [created, updated]

# Also process existing files when fev starts:
events: [startup, created, updated]
```

Only regular files are processed; symlink paths are excluded. Rename rules match the **new** path. Moves between watched roots are deletion plus creation/update, not rename.

## Good to know

- Every matching rule runs. Use a `run` array for commands that must execute in order; separate rules have no guaranteed order.
- Output files can trigger other rules. Commands may also finish without producing files.
- Successes are saved in `./state` beside the YAML by default. Keep this directory to skip unchanged successful inputs, including when `startup` is enabled. Changing only a command does not force reprocessing.
- Commands are not sandboxed, and external effects are not exactly-once. Use trusted commands and avoid output patterns that trigger loops.
- `settle` waits for stable file size; it does not guarantee a writer has finished. Native notifications may be incomplete, so rename correlation is not guaranteed.
- Restart fev after changing the YAML. `fev -c config.yaml` is equivalent to `--config`; use `fev --help` for CLI options.

For command variables, multiple roots, concurrency, state, and failure behavior, see the [configuration reference](skills/creating-fev-yaml/references/configuration.md).

## Examples and agent skill

- [Starter workflow](skills/creating-fev-yaml/templates/basic.yaml): two-stage file processing plus an outputless observer.
- [Repository demo](examples/demo.yaml): try it with `just demo` and `just demo-input` in separate terminals, or run the isolated smoke check with `just demo-test`.
- [YAML authoring skill](skills/creating-fev-yaml/SKILL.md): help an agent create and verify configurations. Copy the entire `skills/creating-fev-yaml/` directory, including its references and templates; it works independently of this repository.
- [Japanese specification](SPEC.md): the detailed project specification.

## Build from source

Requires Rust/Cargo and a C compiler. SQLite is bundled.

```sh
git clone https://github.com/takemo101/fev.git
cd fev
cargo build --release
./target/release/fev --help
```

With [just](https://just.systems/), use `just install` to install into `~/.local/bin`, or `just --list` to see available recipes.

For development:

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
just demo-test
python3 scripts/test-install.py
```

The installer checks use the native release binary built by `just demo-test` and temporary directories, without accessing GitHub.

## Uninstall

```sh
rm -f "$HOME/.local/bin/fev"
```

Use your chosen directory for a custom installation. Configurations, watched files, outputs, and state are not removed.

## License

MIT.
