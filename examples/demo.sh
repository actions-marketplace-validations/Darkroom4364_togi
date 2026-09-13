#!/usr/bin/env bash
# Demo: find, replay, and repair one genuine Go test gap.
# Requires: Bash, Git, Go, jq, and Rust (unless TOGI_BIN supplies a trusted build).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
TOGI="${TOGI_BIN:-$ROOT/target/debug/togi}"
FIXTURE="$ROOT/tests/fixtures/go"

for tool in git go jq; do
  command -v "$tool" >/dev/null || { echo "Missing demo prerequisite: $tool" >&2; exit 1; }
done

if [[ -z "${TOGI_BIN:-}" ]]; then
  echo "Building togi..."
  cargo build --locked --manifest-path "$ROOT/Cargo.toml" --target-dir "$ROOT/target"
fi
[[ -x "$TOGI" ]] || { echo "Togi binary is not executable: $TOGI" >&2; exit 1; }
# Resolve a caller-supplied relative path before entering the temporary project.
TOGI="$(cd "$(dirname "$TOGI")" && pwd)/$(basename "$TOGI")"
if ! "$TOGI" replay --help | grep -F -- '--verify-killed' >/dev/null; then
  echo "This demo requires a Togi build with replay --verify-killed (not v0.5.2)." >&2
  exit 1
fi

echo "=== togi demo: find -> replay -> add a test -> verify ==="
echo "TestIsPositive checks 1, but never checks zero."

# Keep the source fixture untouched and the report outside the project snapshot.
DEMO_DIR=$(mktemp -d "${TMPDIR:-/tmp}/togi-demo.XXXXXX")
trap 'rm -rf -- "$DEMO_DIR"' EXIT
WORK="$DEMO_DIR/project"
REPORT="$DEMO_DIR/togi-report.json"
mkdir "$WORK"
cp "$FIXTURE"/*.go "$FIXTURE/go.mod" "$WORK/"
cd "$WORK"
export GOWORK=off
export GOTOOLCHAIN=local

# Stage a one-line PR: fix the negative-input check, leaving zero untested.
# Only that changed line is selected; unrelated/equivalent Max mutants stay out.
sed 's/if n > 0 {/if n != 0 {/' "$FIXTURE/calc.go" > calc.go
git init -q
git config --local user.name "Togi Demo"
git config --local user.email "demo@example.com"
git config --local commit.gpgsign false
git config --local core.hooksPath /dev/null
git config --local core.autocrlf false
git add -A
git commit -q -m "add calc module"
cp "$FIXTURE/calc.go" calc.go
git add calc.go
git commit -q -m "fix IsPositive for negative inputs"

echo "1. Check the original suite passes."
go test -count=1 ./...

echo "2. Find the changed-line boundary mutation (gt_to_gte)."
CHECK_STATUS=0
"$TOGI" check --base HEAD~1 --operators gt_to_gte --max-per-run 1 \
  --test-cmd "go test -count=1 ./..." --timeout 60 --jobs 1 --format json \
  > "$REPORT" || CHECK_STATUS=$?
if [[ "$CHECK_STATUS" != 1 ]]; then
  echo "Expected a surviving mutation (exit 1), got exit $CHECK_STATUS." >&2
  exit 1
fi
# Exit 1 alone could also mean an abnormal result. Require a complete, fresh,
# replayable survivor report before calling it a demonstrated gap.
if ! MUTANT_ID=$(jq -er '
  select(.schema_version == 1 and .kind == "mutation_report" and .partial == false
    and .total == 1 and .planned_total == 1 and .killed == 0 and .survived == 1
    and .timeout == 0 and .build_errors == 0 and (.mutations | length) == 1)
  | .mutations[0]
  | select(.source_path == "calc.go" and .operator == "gt_to_gte"
    and .original == ">" and .replacement == ">=" and .result == "survived"
    and .execution.state == "executed" and .replay.kind == "regular_direct")
  | .id | select(type == "number" and . > 0 and floor == .)
' "$REPORT"); then
  echo "Expected one complete, fresh, replayable boundary survivor." >&2
  exit 1
fi
echo "SURVIVED: IsPositive(0) changes from false to true; mutation #$MUTANT_ID."

echo "3. Replay the same mutation with the existing tests."
"$TOGI" replay "$MUTANT_ID" --report "$REPORT"

VERIFY_STATUS=0
"$TOGI" replay "$MUTANT_ID" --report "$REPORT" --verify-killed \
  > "$DEMO_DIR/before.stdout" 2> "$DEMO_DIR/before.stderr" || VERIFY_STATUS=$?
if [[ "$VERIFY_STATUS" != 2 ]] || ! grep -Fq \
  'repair not verified: expected killed, fresh execution returned survived' "$DEMO_DIR/before.stderr"; then
  cat "$DEMO_DIR/before.stderr" >&2
  echo "Expected the unchanged tests to leave the mutation alive; exit $VERIFY_STATUS." >&2
  exit 1
fi
echo "Unchanged weak tests: repair correctly rejected."

echo "4. Add the missing assertion (tests only; calc.go stays unchanged)."
printf '\n%s\n' 'func TestIsPositiveAtZero(t *testing.T) {
    if IsPositive(0) {
        t.Fatal("zero must not be positive")
    }
}' | tee -a calc_test.go

echo "5. Verify the original suite passes and the exact recorded mutant is killed."
"$TOGI" replay "$MUTANT_ID" --report "$REPORT" --verify-killed
echo "Demo complete: the added test kills the recorded boundary mutation."
