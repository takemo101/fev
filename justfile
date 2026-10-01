set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

bin := "fev"
install_dir := env_var_or_default("INSTALL_DIR", env_var("HOME") / ".local" / "bin")

# Show available recipes
_default:
    @just --list

# Build the release binary
build-release:
    if ! command -v cargo >/dev/null 2>&1 && [ -f "${CARGO_HOME:-$HOME/.cargo}/env" ]; then \
        . "${CARGO_HOME:-$HOME/.cargo}/env"; \
    fi; \
    cargo build --release

# Install fev to INSTALL_DIR (default: ~/.local/bin)
install: build-release
    mkdir -p "{{install_dir}}"
    install -m 0755 "target/release/{{bin}}" "{{install_dir}}/{{bin}}"
    @echo "Installed {{bin}} to {{install_dir}}/{{bin}}"
    @echo "Ensure {{install_dir}} is on your PATH."

# Remove fev from INSTALL_DIR (default: ~/.local/bin)
uninstall:
    rm -f "{{install_dir}}/{{bin}}"
    @echo "Removed {{install_dir}}/{{bin}}"

# Watch the two-stage sample (Ctrl-C to stop)
demo: build-release
    mkdir -p examples/demo/work
    "target/release/{{bin}}" --config examples/demo.yaml

# Create/update sample input while `just demo` is running
demo-input:
    mkdir -p examples/demo/work
    printf 'hello fev\n' > examples/demo/work/hello.txt
    @echo "Input: examples/demo/work/hello.txt"
    @echo "Result: examples/demo/work/hello.done.txt"

# Check startup chaining and live updates in an isolated temporary directory
demo-test: build-release
    bash scripts/demo-test.sh "target/release/{{bin}}"
