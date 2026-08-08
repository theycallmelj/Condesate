#!/usr/bin/env bash
# Sets up everything the real eval integrations need (a Python venv with
# harness-evals/httpx/agentevals/strands-agents-evals, a Node.js check for
# the iris-eval MCP server) and then runs every eval path in this crate:
#
#   1. the native suite               (cargo run -p evals)
#   2. real harness-evals over HTTP   (cargo run -p evals --bin harness_evals_run)
#   3. real agentevals trajectories   (cargo run -p evals --bin agentevals_run)
#   4. real strands-agents-evals      (cargo run -p evals --bin strands_evals_run)
#   5. real iris-eval MCP server      (cargo run -p evals --bin iris_eval_run)
#
# See crates/evals/README.md for what each one actually does. Safe to
# re-run — the venv is created once and reused.
#
# NOTE: steps 2 and 4 hit a real HTTP target that uses a REAL, BILLED model
# (Anthropic/OpenAI) if PROVIDER + an API key are set — including via a
# `.env` file at the workspace root. Unset PROVIDER or remove the key from
# `.env` first if you want this run to stay fully offline/free.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
VENV_DIR="$SCRIPT_DIR/.venv"

echo "== condesate evals: setup =="

if ! command -v python3 >/dev/null 2>&1; then
    echo "error: python3 not found — required for the harness-evals/agentevals/strands-agents-evals integrations" >&2
    exit 1
fi

if [ ! -d "$VENV_DIR" ]; then
    echo "-- creating venv at $VENV_DIR"
    python3 -m venv "$VENV_DIR"
fi
# shellcheck disable=SC1091
source "$VENV_DIR/bin/activate"

echo "-- installing harness-evals, httpx, agentevals, strands-agents-evals into the venv"
pip install --quiet --upgrade pip
pip install --quiet harness-evals httpx agentevals strands-agents-evals

HAVE_NODE=1
if ! command -v npx >/dev/null 2>&1; then
    HAVE_NODE=0
    echo "-- warning: npx not found; will skip the iris-eval mcp-server integration (needs Node.js 20+)"
fi

echo "-- building the evals crate"
(cd "$WORKSPACE_ROOT" && cargo build -p evals)

echo
echo "== condesate evals: run everything =="

NAMES=()
RESULTS=()

run_one() {
    local name="$1"
    shift
    echo
    echo "############################################################"
    echo "## $name"
    echo "############################################################"
    if (cd "$WORKSPACE_ROOT" && "$@"); then
        RESULTS+=("PASS")
    else
        RESULTS+=("FAIL")
    fi
    NAMES+=("$name")
}

run_one "native suite" cargo run -p evals
run_one "harness-evals (real CLI over HTTP)" cargo run -p evals --bin harness_evals_run
run_one "agentevals (real trajectory match)" cargo run -p evals --bin agentevals_run
run_one "strands-agents-evals (real Experiment)" cargo run -p evals --bin strands_evals_run

if [ "$HAVE_NODE" -eq 1 ]; then
    run_one "iris-eval (real MCP server)" cargo run -p evals --bin iris_eval_run
else
    NAMES+=("iris-eval (real MCP server)")
    RESULTS+=("SKIPPED")
fi

echo
echo "== summary =="
overall=0
for i in "${!NAMES[@]}"; do
    printf "  [%s] %s\n" "${RESULTS[$i]}" "${NAMES[$i]}"
    if [ "${RESULTS[$i]}" = "FAIL" ]; then
        overall=1
    fi
done

exit "$overall"
