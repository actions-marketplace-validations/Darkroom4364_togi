#!/usr/bin/env bash
# Demo: togi on a polyglot change — one run, one report, one score gate.
# Requires: go, cargo, python3. TOGI_BIN may supply a trusted Togi build.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TOGI="${TOGI_BIN:-$ROOT/target/debug/togi}"
FIXTURE="$ROOT/tests/fixtures/polyglot"

if [[ ! -x "$TOGI" ]]; then
  [[ -z "${TOGI_BIN:-}" ]] || { echo "Togi binary is not executable: $TOGI" >&2; exit 1; }
  echo "Building togi..."
  cargo build --locked --manifest-path "$ROOT/Cargo.toml" --target-dir "$ROOT/target"
fi
# Resolve a caller-supplied relative path before entering the temporary project.
TOGI="$(cd "$(dirname "$TOGI")" && pwd)/$(basename "$TOGI")"

echo "=== togi demo: one mutation run across Go + Rust + Python ==="
echo ""
echo "The fixture is one PR-sized change touching three languages, each"
echo "with deliberately weak tests. togi.toml carries per-language test"
echo "commands, so a single 'togi check' runs each mutant against its own"
echo "language's suite and reports one unified result with one score gate."
echo ""

# Work in a temp copy so we don't pollute the fixture
DEMO_DIR=$(mktemp -d "${TMPDIR:-/tmp}/togi-polyglot.XXXXXX")
trap 'rm -rf -- "$DEMO_DIR"' EXIT
WORK="$DEMO_DIR/project"
REPORT="$DEMO_DIR/togi-report.json"
mkdir "$WORK"
cp -r "$FIXTURE"/* "$WORK/"
cd "$WORK"

git init -q
git config user.email "togi-demo@example.invalid"
git config user.name "togi demo"
git commit --allow-empty -q -m "empty"
git add -A
git commit -q -m "add calc helpers in go, rust, and python"

echo "Running: togi check --base HEAD~1 --jobs 1"
echo ""

# GOWORK=off avoids Go complaining about workspace in temp dirs.
# Exit 1 can mean survivors OR an unhealthy run. Check the same run's report.
STATUS=0
GOWORK=off "$TOGI" check --base HEAD~1 --timeout 60 --jobs 1 --json-report "$REPORT" || STATUS=$?
if [[ "$STATUS" != 0 && "$STATUS" != 1 ]]; then
  exit "$STATUS"
fi
python3 - "$REPORT" "$STATUS" <<'PY'
import json
import sys

try:
    with open(sys.argv[1], encoding="utf-8") as source:
        report = json.load(source)
    mutations = report["mutations"]
    valid = (
        report["schema_version"] == 1 and report["kind"] == "mutation_report"
        and report["partial"] is False
        and report["timeout"] == report["build_errors"] == 0
        and report["total"] == report["planned_total"] == report["tested"]
        == report["killed"] + report["survived"] == len(mutations) > 0
        and {mutation["language"] for mutation in mutations} == {"go", "rust", "python"}
        and all(mutation["result"] in ("killed", "survived")
                and mutation["execution"]["state"] == "executed" for mutation in mutations)
        and int(sys.argv[2]) == int(report["survived"] > 0)
    )
except (OSError, ValueError, KeyError, TypeError):
    valid = False
if not valid:
    sys.exit("Expected a complete Go/Rust/Python report with no timeouts or build errors.")
PY

echo ""
echo "=== one run, one report, one gate — no per-language glue required ==="
