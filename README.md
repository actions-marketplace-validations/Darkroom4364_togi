# togi

[![CI](https://github.com/Darkroom4364/togi/actions/workflows/ci.yml/badge.svg)](https://github.com/Darkroom4364/togi/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-1.87%2B-orange.svg)](Cargo.toml)
[![mutation score (dogfood: src/report/json.rs)](https://img.shields.io/endpoint?url=https://raw.githubusercontent.com/Darkroom4364/togi/badges/mutation-score.json)](https://github.com/Darkroom4364/togi/actions/workflows/dogfood-badge.yml)
The mutation score (dogfood: src/report/json.rs) badge presents both percentage and tested mutant count for its bounded `src/report/json.rs` selector; it is not a repository-wide score.

togi (鍛 — Japanese for "sharpening") is a fast, diff-targeted mutation testing engine for pull requests.

It finds the test gaps hidden behind green builds: togi mutates the code you changed, runs the relevant tests, and shows the exact mutants your suite failed to kill.

## External released-binary evidence

A released [Togi v0.4.1](docs/external-dogfood/mitigrid-v0.4.1-f5f3f57/) binary completed one permitted, bounded run on Mitigrid revision [`f5f3f57c92fdb3405b92eca7c9b6a6d3d704c1e8`](https://github.com/Darkroom4364/Mitigrid/commit/f5f3f57c92fdb3405b92eca7c9b6a6d3d704c1e8); the [workflow artifact](https://github.com/Darkroom4364/togi/actions/runs/30696356930) recorded 2/2 tested and killed, with 0 survivors, timeouts, or build errors, a complete non-partial report, reported duration 80,289ms, and outer wall time 120,264ms. Its artifact offline verifier passed. This is one bounded reproducibility result, not adoption, general compatibility, or performance evidence.

## What it does

Mutation testing is usually too slow to run on every PR. togi is built for the PR loop:

- **Diff-targeted by default**: mutates changed lines instead of the whole repository
- **Multi-language**: Go, Rust, Python, TypeScript, Java, C, C++, Ruby, and C#
- **Single binary**: no service, database, or language-specific plugin stack
- **Zero config start**: auto-detects common test commands, with `togi init` for explicit config
- **CI-ready reports**: terminal, JSON, GitHub annotations, SARIF, HTML, and PR comment markdown
- **Performance controls**: caching, sharding, fail-fast commands, LCOV filtering, and source-line test selection
- **Guardrails**: build pre-checks, baselines, operator filters, noisy-file skips, and path-safe mutation execution

If a mutation survives, your tests still pass after the edit. Check whether it changes observable behavior: a behavior-changing survivor is a test gap; an equivalent mutation is not.

```
$ togi check --base HEAD~1

  ✓ KILLED  src/auth.rs:47  - lt_to_lte: changed < to <=
  ✗ SURVIVED  src/handler.rs:15  - eq_to_neq: changed == to !=
  ✓ KILLED  src/handler.rs:31  - remove_if_body: removed if body

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Results: 2/3 mutations killed (1 survived)
Duration: 0.84s
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

## Try a complete test repair

From a Togi source checkout, with Bash, Git, Go, and `jq` installed:

```bash
bash examples/demo.sh
```

The script builds this checkout's debug binary (requires Rust), or you can
supply a trusted build with `TOGI_BIN=/path/to/togi bash examples/demo.sh`.
This walkthrough requires `replay --verify-killed`, included in
[v0.6.0](https://github.com/Darkroom4364/togi/releases/tag/v0.6.0).
After installing the [released binary](#install), use it without a Rust build:

```bash
TOGI_BIN="$(command -v togi)" bash examples/demo.sh
```

The demo stages a one-line change to `IsPositive` in a temporary Go project
and focuses on one boundary mutation: `n > 0` becomes `n >= 0`. The original
tests pass, but never check zero. It saves the survivor's JSON report, replays
that exact mutation, and confirms the unchanged tests cannot verify a repair.
It then adds an assertion that `IsPositive(0)` is false and runs
`togi replay <id> --report <report.json> --verify-killed` successfully.

Success ends with `Demo complete: the added test kills the recorded boundary
mutation.` The source fixture is unchanged, and the temporary project/report
are removed. This demonstrates one repaired gap, not complete test adequacy;
see the [verification contract](#example-finding-real-test-gaps) before using
the same loop in your own project.

## Why

Good mutation testing tools exist, but each makes trade-offs:

| Tool | Languages | Diff-targeted | Zero config | Single binary |
|------|-----------|---------------|-------------|---------------|
| Stryker | JS/TS/.NET | Incremental mode | No | No |
| cargo-mutants | Rust | `--in-diff` flag | Partial | Yes |
| mewt | Multi (tree-sitter) | No | No | Yes |
| mutahunter | Multi (LLM) | Yes | No | No |
| **togi** | **9 languages** | **By default** | **Yes** | **Yes** |

togi's differentiator: multi-language + diff-targeted by default + single binary + zero config. It mutates only changed lines, keeps reports tied to reviewable code, and has the CI mechanics needed for real repositories: cache identity, baselines, coverage filtering, sharding, GitHub annotations, and machine-readable output.

## Polyglot monorepos

One PR can touch several languages without splitting the mutation run. Set a per-language test command in `togi.toml`, and a single `togi check` runs each mutant against its own language's suite, then reports one unified result with one score gate:

```toml
[test.languages.go]
command = ["go", "test", "./..."]

[test.languages.rust]
command = ["cargo", "test"]

[test.languages.python]
command = ["python3", "-m", "unittest", "discover"]
```

Try it: `examples/polyglot-demo.sh` runs a PR-sized Go + Rust + Python change through one `togi check` — mutants from all three languages appear in the same report, the same mutation score, and the same `--fail-under` gate. No per-language glue, no stitched-together CI jobs.

## Install

### Linux x86_64 (Tier 1) first success

GitHub Releases are the supported binary-install path. This tested release
archive path is for Linux x86_64 only. See the
[compatibility contract](docs/COMPATIBILITY.md) for other platforms and support
tiers.

You need Bash, `curl`, `tar`, `sha256sum`, and `git`, plus a project with a
passing test command that togi can auto-detect or that you configure as
described in [Configuration](#configuration). Run the check from a Git checkout
with a parent commit and at least one changed supported source line.

```bash
(
  set -euo pipefail
  TOGI_VERSION=v0.6.0
  TOGI_ARCHIVE=togi-linux-x86_64.tar.gz
  RELEASE_BASE="https://github.com/Darkroom4364/togi/releases/download/${TOGI_VERSION}"
  TEMP_DIR="$(mktemp -d)"
  trap 'rm -rf "$TEMP_DIR"' EXIT
  cd "$TEMP_DIR"

  curl -fsSLo "$TOGI_ARCHIVE" "${RELEASE_BASE}/${TOGI_ARCHIVE}"
  curl -fsSLo checksums.txt "${RELEASE_BASE}/checksums.txt"
  EXPECTED_SHA=$(awk -v file="$TOGI_ARCHIVE" '$2 == file || $2 == "./" file { print $1 }' checksums.txt)
  [ -n "$EXPECTED_SHA" ] || { echo "No checksum found for ${TOGI_ARCHIVE}" >&2; exit 1; }
  ACTUAL_SHA=$(sha256sum "$TOGI_ARCHIVE" | awk '{print $1}')
  [ "$ACTUAL_SHA" = "$EXPECTED_SHA" ] || { echo "Checksum mismatch for ${TOGI_ARCHIVE}" >&2; exit 1; }

  tar xzf "$TOGI_ARCHIVE"
  mkdir -p "$HOME/.local/bin"
  install -m 0755 ./togi "$HOME/.local/bin/togi"
)
export PATH="$HOME/.local/bin:$PATH"
togi --version
```

In the checkout you want to check:

```bash
git rev-parse --verify HEAD~1
togi check --base HEAD~1
```

Without `--fail-under`, exit `0` means the default mutation gate passed: no
unaccepted survivors and no fresh timeouts or build errors; exit `1` means an
unaccepted survivor or fresh timeout/build error. With an explicit `--fail-under`,
its score threshold determines that gate instead; a baseline regression still exits
`1`. Exit `2` means an execution error (for example configuration, Git, or parsing)
that you should fix before relying on the result.

`togi` is not currently published on crates.io. If you intentionally want a
source-based install tied to a reviewed revision:
```bash
cargo install --git https://github.com/Darkroom4364/togi --rev <commit-sha> --locked
```

Or build from source:

```bash
git clone https://github.com/Darkroom4364/togi
cd togi
cargo build --release
```

### Before first run

**Zero config** only means togi auto-detects a test command from a supported
project marker. It does not provision your project's dependencies, runtimes, or
test runner, or make an unsupported platform or language supported.

Before running `togi check`, use a trusted Git checkout with a resolvable base:
the default is `origin/main`, or choose one with `--base`. Install the project's
normal dependencies and ensure the selected test command passes.

**First run in a clean clone:** The selected diff is empty; that is normal for a
clean clone, not an error. Preview all supported files without tests (bounded):

```bash
togi check --all --dry-run --max-per-run 10
```

After a supported source change, run `togi check`, or `togi check --base <ref>`
for another intended base. `--all` and `--base` are alternatives; do not combine
them. If command detection is ambiguous, run `togi init` from the repository
root.

The [compatibility contract](docs/COMPATIBILITY.md) contains the supported
marker defaults. When no supported marker is present, togi's best-effort fallback
is `make test`, which requires a `Makefile` with a `test` target. If detection is
missing, ambiguous, or unsuitable, run `togi init` and review or edit the
generated `togi.toml`, configure `togi.toml` directly, or use one-shot
`--test-cmd`.

The compatibility contract remains the authoritative support matrix: Tier 1 is
end-to-end CI-verified, Tier 2 is build- and unit-test-verified, and not
supported has no CI guarantee. On every release tag, the published archives are
verified after publication: the Tier 1 Linux x86_64 archive passes checksum,
install, version, and a real Go mutation smoke, while the Tier 2 macOS arm64
and Windows x86_64 archives pass checksum, install, and version smoke. Linux
and Windows ARM64 (aarch64) are not supported. macOS x86_64 (Intel) is not
supported and has no release asset: the only macOS target is arm64.

## Usage

```bash
# Run mutation testing on your current changes vs main
togi check

# Diff against a specific branch or commit
togi check --base HEAD~1
togi check --base origin/develop

# See what mutations would be generated without running tests
togi check --dry-run

# JSON output for CI
togi check --format json

# Explain a mutation from a JSON report
togi check --format json > togi-report.json
togi explain 1 --report togi-report.json

# Force a fresh replay from a trusted versioned JSON report
togi replay 1 --report togi-report.json

# After improving tests, verify that the same survivor is now killed
togi replay 1 --report togi-report.json --verify-killed

# GitHub annotations or HTML report
togi check --format github
togi check --format html

# SARIF report for GitHub code scanning
togi check --format sarif > togi-report.sarif

# Mutate all supported files (not just the diff)
togi check --all

# Adjust parallelism and timeout
togi check --jobs 2 --timeout 60

# Derive timeout from one unmutated baseline test run
togi check --calibrate-timeout --timeout-multiplier 4 --timeout-slack 2

# Keep laptop CPU/temperature lower
togi check --profile cool

# Cap mutation count for a bounded exploratory run
togi check --max-per-run 50  # --max-per-run 0 = unlimited

# Stop once a PR gate has enough signal
togi check --first-survivor
togi check --max-survivors 3

# Disable schemata if you need one-mutant-at-a-time execution
togi check --no-schemata

# Scope to a directory
togi check --all --path src/rules/

# Exclude noisy operators
togi check --operators=-string_to_empty

# Include only specific categories
togi check --operators=binary,removal

# Stop on first test failure per mutation
togi check --fail-fast

# Run an explicitly configured build check before tests
togi check --build-cmd "cargo check"

# Only mutate lines covered by an LCOV file
togi check --coverage-file coverage/lcov.info

# Ask togi to collect coverage itself when the project is supported
togi check --coverage auto

# Run your own coverage command, then consume the generated LCOV
togi check --coverage-cmd ./scripts/collect-coverage.sh --coverage-file coverage/lcov.info

# Gate on line and diff coverage thresholds
togi check --coverage-file coverage/lcov.info --min-line-coverage 80 --min-diff-coverage 90
togi check --coverage-file coverage/lcov.info --fail-on-uncovered-diff

# Generate and use a source-line to test-name map (Go helper shown)
togi test-map --path . --output coverage/test-selection.json
togi check --test-selection-file coverage/test-selection.json

# Fail below a mutation score threshold
togi check --fail-under 80  # stops early once 80% is impossible

# Split mutation work across parallel CI jobs
togi check --shard 1/4

# Save or compare against a baseline
togi check --save-baseline
togi check --check-baseline

# Write a PR comment body to a markdown file
togi check --pr-comment togi-pr-comment.md

# Show each mutation as it runs
togi check --verbose

# List all operators and categories
togi list-operators

# Clear mutation cache
togi clean

# Re-run even if cache/history has a matching result
togi check --force-rerun

# Disable structured incremental history for one run
togi check --no-incremental-history

# Skip mutants that share a recorded killer test with an earlier mutant
# (learned subsumption clusters; opt-in, requires incremental history).
# Killer tests are attributed automatically from test output on kills —
# no test map needed.
togi check --learned-selection

# Generate a config file
togi init
```

Mutation cache entries include the Togi package version and an internal cache
schema version, so upgrades and operator behavior changes automatically stop
matching older `.togi-cache` entries. Structured incremental history is stored
under `.togi-cache/history.json`; it can reuse killed/survived results when the
mutant source, command context, and relevant covering tests are unchanged.

The displayed mutation score, `tested` count, and report formats stay
fresh-only. `--fail-under` uses a gate score that incorporates final
exact-cache killed, survived, and timed-out verdicts; incremental history
remains excluded from that gate-only calculation.

## PR-loop performance evidence

The PR-loop benchmark harness measures togi on versioned PR-shaped corpus
scenarios. Reproduce it locally:

```bash
cargo build --locked --release && bash benchmarks/pr-loop/run-pr-loop-benchmarks.sh
```

Prerequisites: bash, git, go, jq, sed, sha256sum or shasum, and python3 (for
the monotonic wall clock). The harness copies `tests/fixtures/go` into one
disposable temp project per scenario, applies the scenario's fixed patch, and
runs each workload through `togi check --base HEAD` (never `--all`). Semantic
and provenance invariants fail the run; wall time is observational only.
Local runs are `unclassified`: they make no Go build cache requirement and
their timings are evidence for the reader, not comparable measurements.

To exercise the calibration cache protocol locally (not to produce a
comparable baseline), use exactly Go 1.26.5 and a fresh absolute private cache:

```bash
cargo build --locked --release
test "$(go version | awk '{print $3}')" = go1.26.5
GOCACHE="$(mktemp -d "${TMPDIR:-/tmp}/.togi-pr-loop-gocache.XXXXXX")"
OUTPUT="$(mktemp -d "${TMPDIR:-/tmp}/.togi-pr-loop-calibration.XXXXXX")"
trap 'rm -rf "$GOCACHE" "$OUTPUT"' EXIT
BENCH_GO_BUILD_CACHE_STATE=warmup GOCACHE="$GOCACHE" \
  bash benchmarks/pr-loop/run-pr-loop-benchmarks.sh --output "$OUTPUT/warmup"
for sample in 1 2 3 4 5; do
  BENCH_GO_BUILD_CACHE_STATE=primed GOCACHE="$GOCACHE" \
    bash benchmarks/pr-loop/run-pr-loop-benchmarks.sh --output "$OUTPUT/sample-$sample"
done
```

This verifies the same empty-private-cache, warmup, and primed-acquisition
protocol, but remains **not GHA baseline-comparable**: a local runner has a
different runner class.

The corpus (`benchmarks/pr-loop/manifest.json`, schema v2) declares two
scenarios:

- `single-file` — `fixture-change.patch` appends `Clamp` to `calc.go`
  (4 mutations). Workloads: `cold-regular` (per-mutant, fresh cache),
  `warm-exact-cache` (exact-cache reuse of `cold-regular`), `cold-schemata`
  (fast-path + fallback), and `pr-diff-default` (out-of-the-box defaults).
- `multi-file` — `fixture-change-multi.patch` rewrites `Add` in `calc.go`
  through `Sum` and changes the `Sum` body in `numbers.go` (9 mutations
  across both files). Workloads: `multi-file-regular` and
  `multi-file-default`.

Each scenario gets its own disposable project and `.togi-cache`; cache reuse
never crosses scenarios, and mutation identity is compared within a scenario,
never between them. Every workload records its scenario, runner mode
(regular/schemata/default), resolved test command, cache policy, wall time,
and machine/runner provenance.

The metric that supports the PR-loop speed claim is the warm-exact-cache
versus cold-regular wall-clock median, computed from a reviewed, activated
baseline. The comparison policy below is fixed by the repository; the durable
baseline it runs against is generated data added by a separate activation PR.
The current baseline records, on the `github-actions-ubuntu-24.04-linux-x86_64`
runner class (4 logical CPUs, Go 1.26.5, togi 0.5.0 at `af02876`), a
warm-exact-cache wall median of 228 ms against a cold-regular wall median of
933 ms; these are the promoted calibration measurements, not a universal
performance guarantee on other hardware.

## PR-loop calibration acquisition

Maintainers may manually dispatch **PR-loop Calibration** from `main`. The
job pins Go to 1.26.5 and creates a job-private Go build cache under
`runner.temp`, proven empty before use. An unmeasured warmup primes that
cache, then five independent, observational-only Linux x86_64 harness samples
run against the identical `GOCACHE` with fresh Togi `.togi-cache` and a
primed Go build cache; the harness refuses warmup/primed measurements whose
`GOCACHE` is not absolute and identical to `go env GOCACHE`. The 14-day
artifact contains warmup and measured raw outputs plus a candidate
calibration JSON. It does not create a baseline, compare results, or gate CI.

Baseline promotion is a deliberately reviewed flow: a maintainer downloads
the calibration artifact ZIP plus its positive GitHub artifact ID and
normalized SHA-256 digest to
`python3 benchmarks/pr-loop/promote-baseline.py`, which fail-closed re-verifies
the archive and extracted artifact (contained regular files, sample digests,
manifest/fixture/patch digests, five distinct primed v2 samples, cache-policy
identity, complete sample data, and no wall sample above 3x its per-workload
median) before writing a deterministic baseline document. The volatile cache
path is kept only as calibration evidence, never as cross-run identity. The
promoter requires the reviewed activation metadata (positive PR number,
non-empty actor, RFC 3339 UTC) and always pins the fixed tolerance policy
described below; hand-authored thresholds have no CLI surface. Do not copy or
invent a candidate baseline by hand.

## PR-loop regression gate policy

The comparison policy is fixed in `benchmarks/pr-loop/compare-baseline.py`
and pinned into every durable baseline as `tolerance_policy` v1. The gate
measures exactly three primed harness samples and, for every workload and
both metrics (`wall_ms` and `reported_duration_ms`), takes the median M of
the three. A workload/metric is over tolerance iff `2*M > 3*B + 2*floor`,
where B is the baseline's stored median (verified against its five raw
values) and the floor is 250 ms for wall time and 100 ms for reported
duration. Any workload/metric median over the cap is a hard regression and
fails the comparison. A single high raw sample whose median stays under the
cap (one spike in three) is recorded and warned about observationally; it
never fails the gate. A missing, malformed, stale, or incomparable baseline
or sample fails closed: the **PR-loop Regression Gate** workflow run itself
fails, with no skip or bypass path inside the workflow. On `pull_request` the
gate never trusts the PR head's copies: it checks out full history, validates
the event's base SHA as a 40-hex commit that is locally available, and
compares against `benchmarks/pr-loop/baseline.json` read from that trusted
base commit. The single exception is the one-time bootstrap: when the base
genuinely carries no baseline yet, the gate says so explicitly and uses the
PR-head baseline; an invalid or unavailable base SHA is never a fallback and
fails the run. On `push` to `main` the checked-out head baseline is used.
The Go toolchain is a pinned comparable dimension: a result measured under a
different Go version than the baseline recorded is incomparable (exit 2),
while truly volatile provenance (Git, kernel, image, togi versions) only
warns. Merge blocking is a
separate enforcement layer: it applies only while the exact check context
`PR-loop Regression Gate` is required by the active `main` ruleset and the
protected paths are code-owner reviewed (see the activation runbook below).
`.github/CODEOWNERS` assigns the entire PR-loop corpus, the gate and
calibration workflows, and CODEOWNERS itself to `@Darkroom4364`, so an
ordinary PR cannot self-authorize by rewriting its own baseline, comparator,
or gate definition. Warm/cold wall-ratio drift beyond 25%,
schemata-versus-cold wall-delta sign changes, and volatile
execution-provenance drift print observational warnings but never fail the
gate.

Activation runbook: a maintainer dispatches **PR-loop Calibration** from
`main`, downloads the fresh calibration artifact ZIP with its positive GitHub
artifact ID and normalized SHA-256, and runs the promoter with the activation
metadata. The promoter writes the durable `benchmarks/pr-loop/baseline.json`
deterministically; the separately reviewed activation PR adds exactly that
generated file together with the gate workflow, atomically. That bootstrap
PR is a reviewed one-time exception: it merges before ruleset enforcement is
enabled, so its gate run necessarily uses the bootstrap path (the base has no
baseline yet). Once the bootstrap PR's own **PR-loop Regression Gate** check
is green and it merges, a repository admin performs the final activation
step: enable required code-owner review and add the exact context
`PR-loop Regression Gate` as a required status check to ruleset 15308939
(the active `main` ruleset), then verify both are enforced before
considering the gate live. From that point, ordinary PRs compare against the
trusted base baseline and cannot modify gate inputs without code-owner
review. The current durable baseline was promoted from calibration run
30928964359 (GitHub artifact 8900358130, measured 2026-08-04T16:26:18Z on
`af02876`) with activation recorded as PR #497 by Darkroom4364 at
2026-08-04T16:43:25Z.

### Corpus changes and recalibration

There is no automatic escape hatch, and none should be invented. Any
intentional corpus identity change (manifest, scenario patches, fixture tree,
workload definitions, or mutation surface) makes the checked-in baseline
incomparable: the gate fails closed with exit 2 on every subsequent run until
a refreshed baseline lands. The authorized procedure is:

1. A maintainer lands the corpus change using a reviewed temporary ruleset
   exception or an approved bypass. Expect an interim gate-red window: every
   PR and push runs a failing **PR-loop Regression Gate** from the moment the
   corpus change merges until the refreshed baseline lands.
2. Immediately dispatch **PR-loop Calibration** on the merged `main`.
3. Promote the fresh artifact with current activation metadata and land the
   refreshed `benchmarks/pr-loop/baseline.json` as an immediate fast-follow
   PR (the gate is green again from that merge).
4. Restore the ruleset to its normal state (remove the temporary exception)
   and verify the `PR-loop Regression Gate` context is again required and
   green on `main`.

Minimize the gate-red window; never leave the required-check enforcement
disabled after the fast-follow lands.

## PR-loop scaling evidence (observational)

Beside the gated corpus, `benchmarks/pr-loop-scale/` holds an observational
scaling corpus: `tests/fixtures/go-scale` plus a sha256-pinned patch that
produces a 98-mutation PR diff, measured by six workloads
(`scale-regular-jobs1`, `scale-warm-exact-cache`, `scale-regular-jobs4`,
`scale-schemata`, `scale-schemata-jobs4`, `scale-default`). The harness
emits schema-3 results, which the schema-2 gate comparator rejects by
construction: this corpus has no baseline, no tolerance, and no gate, and
its numbers are runner-class evidence only, never a general performance
claim. `scale-default` is the zero-flag, config-present path: the fixture's
`togi.toml` omits the schemata key (whose config-file default is off; a
zero-config run would enable it), so it measures a regular run with default
parallelism and `schemata: null`.

Reproduce locally with a primed private Go build cache and summarize three
samples:

```bash
cargo build --locked --release
GOCACHE="$(mktemp -d "${TMPDIR:-/tmp}/.togi-pr-loop-scale-gocache.XXXXXX")"
OUTPUT="$(mktemp -d "${TMPDIR:-/tmp}/.togi-pr-loop-scale.XXXXXX")"
trap 'rm -rf "$GOCACHE" "$OUTPUT"' EXIT
BENCH_GO_BUILD_CACHE_STATE=warmup GOCACHE="$GOCACHE" \
  bash benchmarks/pr-loop-scale/run-pr-loop-scale-benchmarks.sh --output "$OUTPUT/warmup"
for sample in 1 2 3; do
  BENCH_GO_BUILD_CACHE_STATE=primed GOCACHE="$GOCACHE" \
    bash benchmarks/pr-loop-scale/run-pr-loop-scale-benchmarks.sh --output "$OUTPUT/sample-$sample"
done
python3 benchmarks/pr-loop-scale/summarize-scale.py \
  --output "$OUTPUT/scale-summary.json" \
  "$OUTPUT"/sample-{1,2,3}/pr-loop-benchmark-result.json
```

The summarizer is stdlib-only and parse-only: it requires exactly three
successful primed v3 results from one runner class and fails closed (exit 2)
on anything else. It reports per-workload medians, the signed
wall-minus-reported diagnostic (a diagnostic only — it is not an
engine-versus-test-time attribution), and the four wall-ms ratio families
(jobs4/jobs1 regular, schemata/jobs1 regular, schemata-jobs4/schemata-jobs1,
warm/cold), each as the median of the three paired per-sample ratios.

## Configuration

Optional. togi auto-detects a test command where supported; see [Before first run](#before-first-run) for its exact scope, prerequisites, and support boundaries.

Run `togi init` to auto-detect your language, test runner (including pnpm/yarn/bun), and generate a `togi.toml`. It uses a locally resolvable `[diff].base` when one is available, otherwise retains the `origin/main` fallback; review that setting for CI or your PR base. For polyglot repos with distinct root markers it creates per-language sections automatically; if two markers select different commands for one language, configure an explicit `[test]` command or `[projects.*.test]` routes.

Create a `togi.toml` for customization:

```toml
[test]
# Optional: cool, balanced, or ci
profile = "balanced"
command = ["go", "test", "./..."]
timeout = 30
calibrate_timeout = false
timeout_multiplier = 4.0
timeout_slack = 2
jobs = 2
# Optional: opt in to a build check before each mutation's tests.
# Use "NUL" instead of "/dev/null" on Windows.
build_command = ["go", "test", "-c", "-vet=off", "-o", "/dev/null", "./..."]
sandbox_command = ["bwrap", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--"]

[test.languages.python]
command = ["pytest"]
timeout = 45

[projects.api]
path = "services/api"

[projects.api.test]
command = ["go", "test", "./services/api/..."]
timeout = 60

[diff]
base = "origin/main"

[mutations]
max_per_run = 20
max_per_file = 20
schemata = true
# coverage = "auto"
# coverage_command = ["./scripts/collect-coverage.sh"]
coverage_file = "coverage/lcov.info"
min_line_coverage = 80.0
min_diff_coverage = 90.0
fail_on_uncovered_diff = false
test_selection_file = "coverage/test-selection.json"
incremental_history = true
operators = ["-string_to_empty"]
exclude_paths = ["vendor/**"]
skip_noisy_files = true
respect_workspace_ignores = true
```

Test command precedence is: CLI `--test-cmd` / `--timeout`, matching
`[projects.*.test]` by longest path prefix, matching `[test.languages.*]`,
then the global `[test]` command.
When `--test-cmd` is set, `--fail-fast` does not modify the custom command;
include runner-specific fail-fast flags in `--test-cmd` instead.
Resource profiles provide defaults only: explicit CLI flags and `togi.toml`
settings such as `jobs` keep taking precedence.

A nonempty `[test] build_command` or `--build-cmd` opts in to a build check
before each mutant's test command. A failed configured build is reported as a
build error rather than a test kill; a fresh build error fails the default
no-`--fail-under` gate.

`togi init` suggests a safe `build_command` when it detects exactly one root
build system, but detection alone never runs a build before tests. Uncomment
or copy that suggestion to opt in.

For Go, the suggested compile-only command is `go test -c -vet=off -o <host
null device> ./...`: `/dev/null` on Unix and `NUL` on Windows. It compiles
test code without running package initialization, `TestMain`, or `go vet`.

`[test] sandbox_command` is an optional wrapper that runs every build and test
command inside your own sandbox tool. togi appends the selected command after
the wrapper argv, so tools that expect `--` as a separator can use it directly.

`--calibrate-timeout` (or `[test] calibrate_timeout = true`) runs the
unmutated build/test command once in a disposable workspace, then sets the
default mutation timeout to `max(build_time, test_time) * timeout_multiplier +
timeout_slack`. When no build check is available, only `test_time` is used.
Explicit `--timeout` wins. Use `--skip-baseline-timing` in CI jobs that already
provide a tuned timeout or should avoid repeating the baseline run across
shards.

## Resource tuning

The default worker count is conservative for local machines: `1` on 1-2 CPU
systems, otherwise `2`. For named presets, use `--profile cool`, `--profile
balanced`, or `--profile ci`.

`cool` uses one togi worker, enables fail-fast when togi owns the test command,
and sets safe caps for known nested runners when those environment variables are
not already set (`CARGO_BUILD_JOBS`, `RUST_TEST_THREADS`, `GOMAXPROCS`, and
`PYTEST_XDIST_AUTO_NUM_WORKERS`). `balanced` matches the current conservative
local defaults. `ci` uses the available CPU count for togi jobs. Explicit
`--jobs`, `[test] jobs`, `--test-cmd`, and existing environment variables still
win.

Each togi worker runs the configured test command, and that test command may
parallelize too. For cooler local runs, cap both layers:

```bash
# Togi only runs one mutation test process at a time
togi check --profile cool

# Rust: cap Cargo build jobs and libtest threads
togi check --jobs 1 --test-cmd "cargo test -j 2 -- --test-threads=2"

# Go: cap package and test-level parallelism
togi check --jobs 1 --test-cmd "go test -p 2 -parallel 2 ./..."

# Jest: avoid worker fan-out
togi check --jobs 1 --test-cmd "npm test -- --runInBand"

# pytest: avoid xdist workers unless you explicitly want them
togi check --jobs 1 --test-cmd "pytest"
```

## CI workflows

Use togi as a PR gate, a non-blocking annotation job, or a scheduled deeper scan.

### Fast PR gate

```bash
togi check --base origin/main --fail-under 80 --first-survivor
```

`--first-survivor` is shorthand for `--max-survivors 1`: it stops scheduling
new mutants after the first survived result. `--max-survivors N` keeps collecting
up to `N` survivors for triage. `--fail-under` also stops early once even killing
every remaining scheduled mutation could not reach the threshold. Early-stop
runs report how many scheduled mutations completed, so use full runs for
baselines and detailed trend reports.

### GitHub annotations

```bash
togi check --base origin/main --format github
```

### PR comment body

```bash
togi check --base origin/main --pr-comment togi-pr-comment.md
```

Survivors can carry a **likely equivalent (advisory)** reason when a narrow Rust syntax rule
matches: identical boolean literals or a stricter primitive-integer `&&` bound that already
excludes the changed endpoint. They remain `survived` and continue to count toward scores and
gates; JSON uses `likely_equivalent`, and every report format displays the same advisory reason.

### Parallel CI shards

```bash
togi check --base origin/main --shard 1/4
togi check --base origin/main --shard 2/4
togi check --base origin/main --shard 3/4
togi check --base origin/main --shard 4/4
```

### Regression baseline

```bash
togi check --save-baseline
togi check --check-baseline
```

Baselines let existing weak spots stay visible without blocking every PR. New regressions still fail the run.

### Mutant schemata

Schemata are enabled by default. They batch compatible mutations into one build and switch mutants at runtime with `TOGI_MUTANT`. The runner currently supports expression-safe mutations in runtime contexts for Go, Rust, Java, C, and C++; unsupported languages, unsupported operators, and compile-time contexts automatically fall back to the regular one-mutant-at-a-time runner. Use `--no-schemata` or `schemata = false` to force regular execution.

Before reporting a schemata survivor, Togi confirms the concrete edit with a fresh, regular run of its full test route. This also applies to cached schemata survivors: confirmation consumes one tested-mutant slot and records a direct replay recipe in a source-validated JSON report. Killed schemata mutants keep the fast path; they do not gain a direct replay recipe.

## Coverage and test selection

For large repos, togi can avoid work before the runner starts and can also
fail the run when LCOV coverage is below a threshold:

- `--coverage auto` asks togi to collect coverage through a built-in adapter
  when the current project is supported. Today that built-in path supports Go.
- `--coverage-file coverage/lcov.info` runs only mutations on covered lines.
  Mutants on lines the coverage data reports as never executed are not run —
  they are guaranteed survivors — and are reported as `uncovered` instead of
  `survived`: visible in the terminal and JSON reports, but excluded from the
  survivor count, the mutation-score denominator, and `--fail-under` gating.
  Files or lines missing from the coverage data are filtered out as before.
- `--coverage-cmd ./scripts/collect-coverage.sh --coverage-file coverage/lcov.info`
  runs a user-provided command before mutation generation, then reads the
  resulting LCOV file. `TOGI_COVERAGE_FILE` is exported for the command.
- `--min-line-coverage 80` fails when overall LCOV line coverage is below 80%
- `--min-diff-coverage 90` fails when changed-line coverage is below 90%
- `--fail-on-uncovered-diff` fails when any changed line is uncovered
- `togi test-map` generates a Go line-to-test map from per-test coverage
- `--test-selection-file coverage/test-selection.json` narrows each mutant to tests that cover that line when the configured runner supports test selection; narrowed survivors are always re-run through their original full route, so a full-suite kill is reported as `killed`, not as an actionable survivor.

```bash
togi check --coverage auto
togi check --coverage-cmd ./scripts/collect-coverage.sh --coverage-file coverage/lcov.info
togi test-map --path . --output coverage/test-selection.json
togi check --coverage-file coverage/lcov.info --test-selection-file coverage/test-selection.json
togi check --coverage-file coverage/lcov.info --min-line-coverage 80 --min-diff-coverage 90
```

`uncovered` classification and the coverage gates answer different questions.
The gates (`--min-line-coverage`, `--min-diff-coverage`,
`--fail-on-uncovered-diff`) run before mutation generation and fail the whole
run when the diff's coverage is too weak. The `uncovered` classification keeps
individual zero-coverage mutants from inflating the survivor count when the
gates let the run proceed — use the gates to block under-covered diffs, and
the classification to keep surviving-mutant reports free of zero-coverage
noise.

The selection file is a JSON map of source file to line number to test names.
Entries can be strings or objects with `name` and optional `duration_ms`; timed
entries run shortest first. togi currently narrows Go `go test`, pytest node IDs
or simple `-k` names, Jest/Vitest `-t` names, Maven `-Dtest`, Gradle `--tests`,
and single-test Cargo filters. Unsupported commands safely run the full test
command.

## Example: finding real test gaps

The repo includes a Go fixture (`tests/fixtures/go/`) with deliberately weak tests.
Running togi against it produces:

```
  ✓ KILLED  calc.go:5   — plus_to_minus: Replace + with -
  ✓ KILLED  calc.go:10  — zero_to_one: Replace 0 with 1
  ✓ KILLED  calc.go:11  — true_to_false: Replace true with false
  ✗ SURVIVED  calc.go:13  — false_to_true: Replace false with true
              Your tests don't catch this mutation.
  ✗ SURVIVED  calc.go:18  — gt_to_gte: Replace > with >=
              Your tests don't catch this mutation.
  ✗ SURVIVED  calc.go:19  — return_empty: Replace return value with default
              Your tests don't catch this mutation.
  ✓ KILLED  calc.go:21  — return_empty: Replace return value with default
  ✗ SURVIVED  calc.go:26  — zero_to_one: Replace 0 with 1
              Your tests don't catch this mutation.
  ✗ SURVIVED  calc.go:29  — return_empty: Replace return value with default
              Your tests don't catch this mutation.

━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Results: 4/9 mutations killed (5 survived)
Duration: 1.59s
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

Most survivors here expose test gaps, but one is equivalent:

- **`false_to_true` at line 13** — `TestIsPositive` never checks `IsPositive(0)` or negative inputs
- **`gt_to_gte` at line 18** — equivalent: when `a == b`, either branch returns the same integer. No test can distinguish this edit.
- **`return_empty` at line 19** — `Max` return value never verified for first-arg-wins; add an assertion such as `Max(5,3) == 5`
- **`zero_to_one` at line 26** — `TestAbs` is entirely missing
- **`return_empty` at line 29** — `Abs` return value never tested

For a runnable find-and-repair walkthrough, see [Try a complete test repair](#try-a-complete-test-repair).

To close a genuine gap, save a JSON report, replay its survivor, then add a test and run `togi replay <id> --report togi-report.json --verify-killed`. Verification succeeds only if the unmutated build/test route passes and a fresh direct run kills that exact mutant, using two isolated copies of the same input snapshot. Ordinary replay still checks the historical outcome and Git HEAD. Verification allows committed or uncommitted test changes at a different HEAD, but the entire target source file must remain unchanged (including any inline tests). It proves the snapshotted suite rejects the mutant, not that only tests changed or that the suite is non-flaky. Both modes use the report's stored commands and leave the report and Togi cache/history unchanged.

## Supported languages

| Language | Extensions |
|----------|------------|
| Go | `.go` |
| Rust | `.rs` |
| Python | `.py` |
| TypeScript | `.ts`, `.tsx` |
| Java | `.java` |
| C | `.c`, `.h` |
| C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx` |
| Ruby | `.rb` |
| C# | `.cs` |

See [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md) for the authoritative compatibility
contract: guarantee tiers, OS/architecture support, auto-detected test runners, MSRV
policy, JSON report schema stability, and CLI stability guarantees — every tier
mapped to an actual CI leg.

Building on the **Rust 1.87** MSRV, togi compiles and passes its non-ignored
unit test suite in CI on Linux (x86_64), macOS (arm64), and Windows (x86_64);
end-to-end mutation testing is exercised on Linux only. `togi replay` supports
file-only replay on Windows when its temp root is a normal path on the Windows
system volume; mapped, `SUBST`, UNC, and verbatim roots fail closed before any
test command runs ([#449](https://github.com/Darkroom4364/togi/issues/449)).
See the compatibility contract linked above for the exact per-platform
guarantee tiers.

Adding a language is ~5-10 lines via the `define_language!` macro.

## Mutation operators

togi applies 26 targeted mutation operators:

| Category (--operators name) | Mutations |
|-----------------------------|-----------|
| `binary` | `<` to `<=`, `>` to `>=`, `==` to `!=`, `&&` to `\|\|`, `\|\|` to `&&`, `*` to `/`, `/` to `*`, `%` to `*` |
| `literal` | `true` to `false`, `false` to `true`, `0` to `1`, string to `""`, increment numeric, decrement numeric |
| `boundary` | `+` to `-`, `-` to `+` |
| `removal` | Remove if body, remove else branch, remove call statement, remove assignment |
| `unary` | Remove `!`, remove unary `-` |
| `loop` | Remove `break`, remove `continue` |
| `return` | Replace return value with default |
| `negate` | Negate condition expression |

Run `togi list-operators` to see the exact operator IDs accepted by `--operators`.

## Explaining mutations

Use JSON output as the handoff format for `togi explain`:

```bash
togi check --format json > togi-report.json
togi explain 1 --report togi-report.json
```

The explanation includes the mutation location, operator, result, before/after values, diff when available, and the recorded test/build command context.

## VS Code integration

A lightweight VS Code extension lives in `editors/vscode`. It reads
`togi-report.json`, shows survived mutants as inline warnings, and adds a quick
fix to open mutation details or the diff:

```bash
togi check --format json > togi-report.json
```

## Baselines

Save a passing mutation baseline and fail later runs only when the score regresses:

```bash
togi check --save-baseline
togi check --check-baseline
```

Baselines are stored in `.togi-baseline`.

New baselines persist source-validated per-mutant evidence. An eligible `--check-baseline` labels fresh surviving mutants `historic`, `new`, or `non_comparable` in reports, while existing aggregate and per-file score gates remain unchanged.

## Workspace copies

togi runs mutations in temporary workspace copies so parallel jobs do not observe
each other's edits. These copies respect project ignore rules from `.ignore` and
`.gitignore`, while always excluding VCS metadata, togi internals, and common
dependency/build directories such as `node_modules`, `.venv`, `dist`, `build`,
and `target`. Set `[mutations] respect_workspace_ignores = false` only when a
test command genuinely needs ignored files copied into the mutation workspace.

## Security model

togi executes repository-defined build and test commands with the permissions of
the current user or CI runner.

`togi replay` intentionally executes the report's stored argv and Togi-owned
environment overrides to reproduce its recorded command context. Replay only
trusted reports: report commands are not sandboxed by togi. Its no-residue
guarantee applies only to Togi-owned workspace, cache, history, and lock
operations; report commands remain responsible for their own behavior.

On Windows, replay is file-only: any overlay that would require removing a
directory fails closed before any mutation or test command spawns, because
race-free directory removal is unavailable there
([#449](https://github.com/Darkroom4364/togi/issues/449)).

Workspace copies, timeouts, descendant-process cleanup, and the optional
`[test] sandbox_command` wrapper improve correctness and reduce exposure, but
they are not a complete security sandbox. togi does not itself block network
access or confine filesystem access beyond what the host OS, container, or CI
environment already enforces.

Run togi only against repositories you trust, or place it inside a separate
container or VM when evaluating less-trusted code. Running less-trusted
repositories directly on the host is out of scope for the current security
model. On Linux, a wrapper such as `bwrap` or `firejail` is a practical opt-in
strategy; on macOS, use a platform sandbox or container boundary; on Windows,
use a container, VM, or equivalent host-managed isolation. See
[SECURITY.md](SECURITY.md) for the supported-version policy and vulnerability
reporting instructions.

## How it works

1. Parse `git diff` to find changed lines
2. Parse changed files with [tree-sitter](https://tree-sitter.github.io/)
3. Map changed lines to AST nodes
4. Apply targeted mutations to those nodes only
5. Run your test suite against each mutant in parallel
6. Report which mutations survived

## Exit codes

| Code | Meaning |
|------|---------|
| 0 | Without `--fail-under`: no unaccepted survivors or fresh timeout/build-error outcomes. With `--fail-under`: score meets the threshold. No baseline regression. |
| 1 | Without `--fail-under`: unaccepted survivors or fresh timeout/build-error outcomes; with `--fail-under`: threshold not met; or baseline regression. |
| 2 | Error (config, git, parse failure) |
| 130 | Interrupted (SIGINT) |

## CI Integration

Add togi to your pull request workflow:

```yaml
# .github/workflows/togi.yml
name: Mutation Testing
on: [pull_request]
jobs:
  togi:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8
        with:
          toolchain: stable
      - uses: actions/cache@v4
        with:
          path: |
            ~/.cargo/registry
            ~/.cargo/git
            target
          key: ${{ runner.os }}-${{ runner.arch }}-cargo-${{ hashFiles('**/Cargo.lock') }}
          restore-keys: ${{ runner.os }}-${{ runner.arch }}-cargo-
      - run: |
          TOGI_VERSION=vX.Y.Z
          curl -fsSLo togi.tar.gz \
            "https://github.com/Darkroom4364/togi/releases/download/${TOGI_VERSION}/togi-linux-x86_64.tar.gz"
          curl -fsSLo checksums.txt \
            "https://github.com/Darkroom4364/togi/releases/download/${TOGI_VERSION}/checksums.txt"
          sha256sum --ignore-missing -c checksums.txt
          tar xzf togi.tar.gz
          mkdir -p "$HOME/.local/bin"
          install -m 0755 ./togi "$HOME/.local/bin/togi"
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - run: togi check --base origin/main --format github --fail-under 80
```

### GitHub Action

The composite Action installs a released Togi binary, runs it, and uploads a
replayable JSON report. This Node/TypeScript example is a blocking PR gate:

```yaml
# .github/workflows/togi.yml
name: Mutation testing

on:
  pull_request:

permissions:
  contents: read

jobs:
  togi:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
        with:
          fetch-depth: 0
          persist-credentials: false
      - uses: actions/setup-node@820762786026740c76f36085b0efc47a31fe5020 # v7.0.0
        with:
          node-version: 24 # Choose the version required by this repository.
      - name: Install project test dependencies
        run: npm ci
      - id: togi
        uses: Darkroom4364/togi@10970abd97c0d13d8e0ee4b0c568552ebcf84720 # v0.6.0
        with:
          version: v0.6.0
          base: origin/${{ github.base_ref }}
          test-cmd: npm test
          format: json
          report-artifact-name: togi-report
          report-retention-days: '14'
      - name: Record Togi report
        if: ${{ always() }}
        env:
          TOGI_REPORT_PATH: ${{ steps.togi.outputs.report-path }}
          TOGI_MUTATION_SCORE: ${{ steps.togi.outputs.mutation-score }}
          TOGI_SURVIVOR_COUNT: ${{ steps.togi.outputs.survivor-count }}
        run: |
          printf 'report path: %s\n' "$TOGI_REPORT_PATH"
          printf 'mutation score: %s\n' "$TOGI_MUTATION_SCORE"
          printf 'survivor count: %s\n' "$TOGI_SURVIVOR_COUNT"
```

Choose the Node version required by your project rather than relying on the
Ubuntu runner's preinstalled Node. Replace `npm ci` and `npm test` with your
project's dependency-install and test commands. The example uses the Tier 1
Linux x86_64 target on `ubuntu-latest`; the Action selects the release asset
from the runner. See the [compatibility contract](docs/COMPATIBILITY.md) before
using another runner or architecture.

`fetch-depth: 0` makes the PR base available for
`origin/${{ github.base_ref }}`. The checkout therefore needs a Git history
that contains the base branch and a project with changed supported source
lines. The Action source is pinned to the v0.6.0 release commit, and
`version: v0.6.0` selects its checksum-verified release archive. The source SHA
does not independently pin archive bytes. When upgrading, update the source
commit and release version together.

The Action passes `--base`, `--timeout`, `--format`, and `--test-cmd` only for
non-empty inputs. Those inputs override `togi.toml`; remove `test-cmd` to use
your configured `[test] command`, and remove `base` when your `[diff].base`
should select it instead. Output format is CLI-only, so `togi.toml` has no
format setting. For a polyglot repo, commit the `togi init`-generated
`togi.toml` and omit the Action `test-cmd` input so language-specific routes
apply; the example's explicit `test-cmd` is a single-language override. The
following TOML is a single-language example:

```toml
[diff]
base = "origin/main"

[test]
command = ["npm", "test"]
```

`format: json` prints the campaign's JSON report. For GitHub annotations, set
`format: github`. In either case, the Action runs one mutation campaign and
writes `togi-report.json` using `--json-report`; the review output and saved
report describe the same execution.

For a normal mutation report, the Action uploads `togi-report.json` as the
`togi-report` artifact. This example explicitly retains it for 14 days. Set a
unique `report-artifact-name` in a matrix or when invoking the Action more than
once, and use that same name when downloading. Set `upload-report: 'false'`
only when you intentionally do not need the artifact. The `report-path`,
`mutation-score`, and `survivor-count` outputs are runner-local evidence; the
post-Action `if: always()` step records them after an intentional mutation-gate
failure without changing the failed Action result.

Without `--fail-under`, exit `1` means an unaccepted survivor or fresh timeout/build
error. With `--fail-under`, it means the score threshold was missed. A saved-baseline
regression also exits `1`, so the example remains a PR gate.
A failed baseline test or build is a fatal exit `2`, produces no normal mutation
report, and leaves no valid Action outputs or artifact; `always()` does not make
that error successful.

Use `pull_request` for untrusted forks, keep the read-only permissions and
disabled persisted credentials shown above, and do not expose secrets to this
job. Never use `pull_request_target` to run PR code. Togi executes
repository-defined test commands with the runner's permissions.

Download and replay a report only in a checkout at the report's recorded source
revision and only when the artifact is trusted:

```yaml
- uses: actions/download-artifact@v8
  if: ${{ always() }}
  with:
    name: togi-report
    path: togi-artifact
- name: Replay a recorded mutant
  if: ${{ always() }}
  run: togi replay <mutant-id> --report togi-artifact/togi-report.json
```

Choose an ID whose report has `replay.kind` set to `regular_direct`; records
explicitly marked unavailable by Togi's replay contract cannot be replayed.
Replay executes recorded commands, so retain artifacts only as long as their
source and command metadata remain appropriate for your repository.

## Code scanning (SARIF)

`togi check --format sarif` emits a [SARIF 2.1.0](https://docs.github.com/en/code-security/code-scanning/integrating-with-code-scanning/sarif-support-for-code-scanning) report: one result per surviving mutant with its file and line, plus the mutation score in the run's invocation properties. Upload it to GitHub code scanning to see surviving mutants as annotations on the PR diff:

```yaml
# .github/workflows/togi-sarif.yml
name: Mutation Testing (SARIF)
on: [pull_request]
permissions:
  security-events: write  # needed to upload SARIF
jobs:
  togi:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      # Install togi as shown in "CI Integration" above
      - run: togi check --base origin/main --format sarif > togi-report.sarif
      - uses: github/codeql-action/upload-sarif@v3
        if: always()  # upload even when togi exits 1 on surviving mutants
        with:
          sarif_file: togi-report.sarif
```

The togi step still exits 1 when mutants survive, so the job keeps gating the PR while the annotations land on the changed lines.

## Dependabot auto-merge

togi can auto-merge routine Dependabot PRs after green CI through
`.github/workflows/dependabot-auto-merge.yml`.

The workflow is intentionally narrow:

- it only runs for `dependabot[bot]` pull requests into `main`
- it only acts when all changed files stay inside an allowlist of dependency
  manifests, lockfiles, or workflow files
- if the PR branch is behind, it asks GitHub to update the branch first
- it then enables GitHub's native auto-merge instead of merging directly

That keeps the behavior conservative while still removing routine manual merges
for dependency and pinned-action updates.

## License

MIT OR Apache-2.0
