#!/usr/bin/env bash
set -euo pipefail

binary=${1:?Usage: bash scripts/demo-test.sh PATH_TO_FEV}
workdir=$(mktemp -d "${TMPDIR:-/tmp}/fev-demo.XXXXXX")
runner_pid=""

cleanup() {
    status=$?
    trap - EXIT
    if [[ -n "$runner_pid" ]]; then
        kill "$runner_pid" 2>/dev/null || true
        wait "$runner_pid" 2>/dev/null || true
    fi
    if (( status != 0 )) && [[ -f "$workdir/runner.log" ]]; then
        cat "$workdir/runner.log" >&2
    fi
    rm -rf -- "$workdir"
    exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# Copy the actual sample so its relative paths stay inside this temporary tree.
cp examples/demo.yaml "$workdir/demo.yaml"
mkdir -p "$workdir/demo/work"
input="$workdir/demo/work/hello.txt"
output="$workdir/demo/work/hello.done.txt"
expected="$workdir/expected.txt"

wait_for_result() {
    for ((attempt = 0; attempt < 100; attempt++)); do
        if [[ -f "$output" ]] && cmp -s "$expected" "$output"; then
            return
        fi
        if ! kill -0 "$runner_pid" 2>/dev/null; then
            echo "Demo runner exited before producing the expected result." >&2
            return 1
        fi
        sleep 0.1
    done
    echo "Timed out waiting for the two-stage demo result." >&2
    return 1
}

printf 'hello fev\n' > "$input"
printf 'Processed: hello.upper.txt\nHELLO FEV\n' > "$expected"
"$binary" --config "$workdir/demo.yaml" > "$workdir/runner.log" 2>&1 &
runner_pid=$!

wait_for_result
echo "PASS: startup scan -> uppercase -> finish"
cat "$output"

# Exercise a live OS notification, not just the startup scan.
printf 'updated input\n' > "$input"
printf 'Processed: hello.upper.txt\nUPDATED INPUT\n' > "$expected"
wait_for_result
echo "PASS: live input update -> republish -> finish"
cat "$output"

kill "$runner_pid"
wait "$runner_pid"
runner_pid=""
echo "PASS: graceful shutdown"
