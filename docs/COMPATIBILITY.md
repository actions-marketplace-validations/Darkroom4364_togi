# Compatibility Contract

This document is the authoritative compatibility contract for togi. It defines
supported languages and test runners, operating system and architecture coverage,
the Minimum Supported Rust Version (MSRV), and the stability guarantees for
versioned JSON reports and CLI flags.

Every supported tier maps to an actual CI leg that exercises it. Nothing here
is aspirational or forward-looking — if it is not verified in CI today, it is
not claimed as supported.

**This contract is versioned.** When behavior changes (new language support,
OS/arch additions, MSRV bumps, stability-policy updates), this document must be
updated in the same pull request that changes the behavior.

---

## Guarantee Tiers

| Tier | Meaning |
|------|---------|
| **Tier 1 — CI-verified** | End-to-end mutation testing exercises this configuration in CI. Regressions block the PR. |
| **Tier 2 — Build-verified** | The binary compiles and passes its unit test suite in CI, but end-to-end language mutation tests do not run on this platform. |
| **Not supported** | No CI coverage. May work incidentally but is not guaranteed. |

---

## Language and Test-Runner Support

Nine languages are supported. All nine have end-to-end integration tests that
run the full mutation→test-command→outcome pipeline on **Linux x86_64** (Tier 1).

Auto-detected test commands are listed below. Any test command can be overridden
via `togi.toml` or `--test-cmd`; the auto-detected commands are defaults only.

| Language | Extensions | Auto-detected test commands | Schemata | CI leg |
|----------|------------|-----------------------------|----------|--------|
| Go | `.go` | `go test ./...` | Yes | Integration Tests |
| Rust | `.rs` | `cargo test` | Yes | Integration Tests, Dogfood |
| Python | `.py` | `pytest` | No | Integration Tests |
| TypeScript | `.ts`, `.tsx` | `npm test`, `pnpm test`, `yarn test`, `bun test` | No | Integration Tests |
| Java | `.java` | `mvn test`, `./gradlew test` | Yes | Integration Tests |
| C | `.c`, `.h` | `ctest` | Yes | Integration Tests |
| C++ | `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx` | `ctest` | Yes | Integration Tests |
| Ruby | `.rb` | `bundle exec rspec` | No | Integration Tests |
| C# | `.cs` | `dotnet test` | No | Integration Tests |

**Schemata** (the `TOGI_MUTANT` batch-execution mode) are supported for Go,
Rust, Java, C, and C++. For unsupported languages or operators, togi
automatically falls back to one-mutant-at-a-time execution. Use `--no-schemata`
to force the regular runner for all mutations.

### CI evidence

- **Integration Tests** ([`ci.yml`](../.github/workflows/ci.yml) `integration`
  job, `ubuntu-24.04` with a fail-closed native target/arch assertion): runs
  `cargo test --locked -- --ignored`, which
  executes the end-to-end fixture for every language listed above. The job
  provisions Go (stable), Node.js 24.14.1, and .NET 8.0.x; Python, Ruby, Java,
  C, and C++ toolchains are provided by the standard Ubuntu runner image.
- **Dogfood** ([`ci.yml`](../.github/workflows/ci.yml) `dogfood` job,
  `ubuntu-24.04` with a fail-closed native target/arch assertion): runs togi
  on togi itself, exercising Rust end-to-end on
  every push to `main` and every PR.
- **PR-loop Benchmark Evidence** ([`ci.yml`](../.github/workflows/ci.yml)
  `pr-loop-benchmarks` job, Linux x86_64 only on `ubuntu-24.04` with the same
  fail-closed native target/arch assertion): builds the release binary and
  uploads the complete PR-loop harness output, including raw files and
  `pr-loop-benchmark-result.json`. Go is pinned to 1.26.5 and every harness
  invocation binds one job-private `GOCACHE` (created empty, warmed before
  measurement) so cache provenance is explicit. This job is telemetry only:
  it compares nothing and gates nothing.
- **PR-loop Scale Evidence**
  ([`pr-loop-scale-evidence.yml`](../.github/workflows/pr-loop-scale-evidence.yml),
  Linux x86_64 only on the `ubuntu-24.04` runner class with the same
  fail-closed native target/arch assertion): Go 1.26.5 uses a job-private cache
  created empty, warmed once, then acquires three primed schema-v3 scale-harness
  samples and uploads their complete output plus `scale-summary.json` as an
  artifact. This evidence compares nothing and gates nothing.
- **PR-loop Regression Gate**
  ([`pr-loop-regression-gate.yml`](../.github/workflows/pr-loop-regression-gate.yml),
  Linux x86_64 only on `ubuntu-24.04` with the same fail-closed native
  target/arch assertion): on every PR and every push to `main`, measures
  exactly three primed harness samples against the durable baseline
  `benchmarks/pr-loop/baseline.json` under the fixed
  `tolerance_policy` v1 (median of three; hard failure when any
  workload/metric median exceeds 1.5x the baseline median plus the absolute
  floor of 250 ms wall / 100 ms reported). On pull requests the baseline is
  read from the validated trusted base commit, not the PR head; the pinned
  Go toolchain must match the baseline's. A missing, stale, or incomparable
  baseline fails the workflow run closed. Merge blocking applies while the
  `PR-loop Regression Gate` context is required by the active `main`
  ruleset; the corpus, both PR-loop workflows, and CODEOWNERS itself are
  owned by `@Darkroom4364` in
  [`.github/CODEOWNERS`](../.github/CODEOWNERS), so gate inputs cannot be
  rewritten by an ordinary PR without code-owner review.

### Auto-detection fallback

When no language-specific marker file is found, togi falls back to `make test`.
This is a best-effort default; projects without a `Makefile` that defines a
`test` target should configure an explicit test command via `togi init` or
`togi.toml`.

---

## Operating System and Architecture Matrix

| OS | Arch | Tier | CI leg | Notes |
|----|------|------|--------|-------|
| Linux | x86_64 | Tier 1 | Build & Test, MSRV, Integration Tests, Dogfood, PR-loop Benchmark Evidence, PR-loop Regression Gate | Primary development target. All features including replay are supported. |
| macOS | arm64 | Tier 2 | Build & Test | Binary compiles and passes unit tests. End-to-end language mutation tests are not run on macOS in CI. |
| Windows | x86_64 | Tier 2 | Build & Test | Binary compiles and passes unit tests. **Replay is file-only on Windows when its temp root is a normal path on the Windows system volume; directory overlays stay fail-closed** — see below. |

macOS runs on **arm64** only: macOS x86_64 (Intel) is not supported and has
no release asset ([#485](https://github.com/Darkroom4364/togi/issues/485)).
Every CI and release leg for the matrix above runs on an explicit,
arch-pinned runner (`ubuntu-24.04`, `macos-15`, `windows-2022`) and asserts
the Rust host target and runner architecture at runtime, so a runner-image
change fails the leg instead of silently proving the wrong target. **ARM64
(aarch64) on Linux and Windows is not covered by CI and is not claimed as
supported.** It may work incidentally but is not guaranteed.

### Windows replay: file-only, directory overlays fail-closed

`togi replay` supports **file-only replay** on Windows. Disposable-workspace
population, overlay application, guarded mutation, and restoration run through
capability-bounded, no-follow filesystem operations beneath pinned parent
directory handles. Source validation is path-based; Git clone/checkout and user
commands use the pinned workspace path, whose system-volume, temp-root, outer,
and clone components remain held. File and symlink overlay removals are
supported; junction leaves and mid-path components are removed as reparse points
and never traversed, so outside-target contents stay intact.

Replay accepts a temp root only when it is a normal absolute `X:\...` path on
the Windows system volume. UNC, verbatim, mapped, and `SUBST` drive roots fail
closed before temporary-workspace or Git setup. This assumes a normal
non-administrator process: the shared Windows system-volume boot-drive mapping
is the OS-owned anchor. Administrator, LocalSystem, and mount-manager control
are outside this replay path's threat boundary.

Any overlay that would require removing a directory — a directory leaf
removal, a directory↔file type change, or a removal whose source ancestor is
missing or non-directory — remains **fail-closed before any mutation or test
command spawns**. Race-free directory removal is unavailable under
`cap-primitives` 4.0.2 on Windows (it drops the directory handle, then
path-deletes), so replay aborts workspace setup with a diagnostic naming the
requirement:

```text
replay cannot remove directory <name> on Windows: safe disposable workspace
setup requires race-free directory removal; only file and symlink overlay
removals are supported
```

Implemented in [#449](https://github.com/Darkroom4364/togi/issues/449).
Beyond replay, the Windows guarantee is the Tier 2 scope in the matrix above:
the binary compiles and the non-ignored unit test suite passes in the
Build & Test `windows-2022` leg; the end-to-end language mutation fixtures
(Integration Tests leg) are not exercised on Windows in CI.

### CI evidence

- **Build & Test** ([`ci.yml`](../.github/workflows/ci.yml) `check` job,
  matrix: `ubuntu-24.04` (x86_64, Tier 1), `macos-15` (arm64, Tier 2),
  `windows-2022` (x86_64, Tier 2)): runs `cargo build --locked` and
  `cargo test --locked` natively for each supported target on every push to
  `main` and every PR.
- **Release build** ([`release.yml`](../.github/workflows/release.yml) `build`
  job, same target/runner matrix): on every release tag, builds and packages
  exactly three archives — `togi-linux-x86_64.tar.gz`,
  `togi-macos-arm64.tar.gz`, and `togi-windows-x86_64.zip` — plus
  `checksums.txt`.
- **Verify Published Release** ([`release.yml`](../.github/workflows/release.yml)
  `verify-release` job, matrix: `ubuntu-24.04`, `macos-15`,
  `windows-2022`): on every release tag, after the GitHub Release is
  published, downloads the public release archives and verifies each archive's
  checksum, install, and `--version` against the tag. The Linux x86_64 (Tier 1)
  archive additionally passes the real Go mutation smoke and exact-survivor
  replay/test-repair verification; the macOS arm64 and Windows x86_64 (Tier 2)
  archives get checksum/install/version smoke only.
  The `verify-release-identity` job re-resolves the triggering tag to its
  peeled commit, requires it to match the successful release-workflow head, and
  verifies the public release association for that exact tag, per the
  [publishing policy](PUBLISHING.md).

---

## Minimum Supported Rust Version (MSRV)

togi's MSRV is **Rust 1.87**, as declared in [`Cargo.toml`](../Cargo.toml)
(`rust-version = "1.87"`) and tested in CI.

### Policy

- The MSRV is the minimum Rust version that togi is guaranteed to compile and
  pass its test suite with.
- The MSRV is tested in the dedicated **MSRV** CI job
  ([`ci.yml`](../.github/workflows/ci.yml) `msrv` job, `ubuntu-24.04` with a
  fail-closed native target/arch assertion, toolchain `1.87`), which runs
  `cargo test --locked`.
- MSRV bumps are a breaking-change decision and must be documented in the
  release notes and in this contract.

---

## JSON Report Schema Stability

### Versioned envelope

togi's normal mutation-report JSON output (`--format json`) carries a
versioned envelope whose `schema_version` field gates `togi replay`
compatibility:

- **Current schema version**: `1` (`REPORT_SCHEMA_VERSION` in
  [`src/replay.rs`](../src/replay.rs))
- **Current report kind**: `"mutation_report"` (`REPORT_KIND`)
- **Generator**: `"togi/<cargo-pkg-version>"` (e.g. `"togi/<version>"`)

A replayable Git-based versioned report envelope looks like:

```json
{
  "kind": "mutation_report",
  "schema_version": 1,
  "generator": "togi/<version>",
  "source_revision": "<full git sha>",
  "mutations": [...]
}
```

Git-based schema-1 reports include `source_revision`; a mutation with a direct
replay recipe can be replayed. A non-Git `togi check --all --format json`
report is also a valid schema-1 mutation report, but omits `source_revision`
and is non-replayable. `togi replay` reports that it was generated without a
Git source revision; rerun `togi check` from a Git worktree to create a
replayable report.

### Compatibility guarantees

- `togi replay` rejects reports whose `schema_version` does not match the
  current `REPORT_SCHEMA_VERSION`. There is no silent best-effort replay across
  schema versions.
- `togi replay` rejects a schema-1 mutation report without `source_revision`
  before invoking any command because it was generated outside a Git worktree.
- The cache subsystem uses its own internal schema versions
  (`CACHE_SCHEMA_VERSION = "3"`, `HISTORY_SCHEMA_VERSION = 2`) to invalidate
  stale cache and history entries on upgrade. These are internal
  implementation details and are not part of the public contract.
- Non-normal JSON documents emitted instead of a mutation report (such as
  `dry_run` and `suite_failure` output, which carry their own `kind` tag)
  omit the versioned envelope entirely.
- `schema_version` applies to the report envelope, not to individual
  mutations: a versioned mutation report can contain individual mutations
  whose replay recipe is unavailable (`replay: {"kind": "unavailable", ...}`,
  e.g. capture failure, schemata execution, or a mutation that never ran).

### SARIF and human-readable output

- SARIF output stays at SARIF **2.1.0**. Extensions are additive properties
  only; no existing SARIF field is removed, renamed, or retyped in v1.
- Terminal, HTML, and GitHub output layouts are **not** stability guarantees
  and may change in any release.

### Stability policy

- The `schema_version` integer is bumped when the JSON report structure changes
  in a way that would break `togi replay` or downstream consumers.
- Within a schema version, fields may be added but existing fields will not be
  removed, renamed, or have their types changed.
- Consumers that parse JSON reports SHOULD tolerate unknown fields (additive
  compatibility).
- Schema-version bumps are documented in the release notes and in this
  contract.

---

## CLI Stability

### Subcommands

togi exposes these subcommands:

| Subcommand | Stability |
|------------|-----------|
| `togi check` | Stable — the primary subcommand for mutation testing. |
| `togi init` | Stable — generates `togi.toml`. |
| `togi clean` | Stable — removes leftover mutant files. |
| `togi explain` | Stable — explains a mutation from a JSON report. |
| `togi replay` | Stable — replays a mutation from a versioned JSON report. |
| `togi list-operators` | Stable — lists available mutation operators. |
| `togi test-map` | Stable — generates a source-line to test-name map. |

### Flag stability

- Flags documented in the `--help` output and in the README are stable.
  Undocumented flags or environment variables are internal and may change
  without notice.
- Flags that accept values (e.g., `--timeout`, `--jobs`, `--fail-under`) will
  not change the type or range of accepted values within a minor version.
- New flags may be added in any release.

### Deprecation policy

From v1, a configuration key, CLI flag or subcommand, or documented JSON
report field may be deprecated only together with migration instructions and
a notice in the release notes and in this contract. Deprecated surface
remains usable until v2.

**Accepted exception:** `mutations.confirm_survivors` was removed before v1
([#488](https://github.com/Darkroom4364/togi/issues/488)) and is not covered
by this policy. A configuration that still sets it fails with a migration
diagnostic: narrowed survivors are now always re-run through their full test
route before they are reported, and the setting must be deleted.

### Exit codes

| Code | Meaning |
|------|---------|
| 0 | Without `--fail-under`: no unaccepted survivors or fresh timeout/build-error outcomes. With `--fail-under`: score meets the threshold. No baseline regression. |
| 1 | Without `--fail-under`: unaccepted survivors or fresh timeout/build-error outcomes; with `--fail-under`: threshold not met; or baseline regression detected. |
| 2 | Error (configuration, git, parse failure). |
| 130 | Interrupted by signal (SIGINT). |

Exit codes are stable and will not change within a major version.

---

## Configuration Stability

- `togi.toml` parsing is strict: unknown keys are rejected, and that behavior
  is retained for v1.
- Documented configuration keys, value types, and defaults are stable for v1.
  New configuration is optional and additive only.
- With the single `mutations.confirm_survivors` exception above, the
  documented final-0.5 configuration surface parses under v1 (frozen fixture:
  [`tests/fixtures/togi-v0.5.toml`](../tests/fixtures/togi-v0.5.toml)).
  Configurations that set the removed key do not load unchanged; they fail
  with the migration diagnostic.

---

## Security Support

Security fixes are applied on a best-effort basis to the current `main`
branch and the latest tagged v1 release only. Older v1 releases and
maintenance branches are not supported. Support for the final 0.5 release
ends when v1 is released. togi provides no sandboxing or response-time
guarantee; see [SECURITY.md](../SECURITY.md) for the security model and
vulnerability reporting.

---

## Version History

| Date | Change |
|------|--------|
| 2026-07-28 | Initial compatibility contract. |
| 2026-08-04 | Removed the macOS x86_64 (Intel) release target; pinned explicit per-target runners with runtime target/arch assertions; named native build/unit legs by tier and target; pinned the Tier-1 Integration Tests and Dogfood evidence jobs to `ubuntu-24.04` with the same assertion ([#485](https://github.com/Darkroom4364/togi/issues/485)). |
| 2026-08-04 | Stated the v1 stability and deprecation policy: strict configuration parsing with stable documented keys (except the pre-v1 `confirm_survivors` removal), additive-only JSON schema 1 and SARIF 2.1.0 evolution, deprecation with migration instructions until v2, and v1 security-support scope ([#484](https://github.com/Darkroom4364/togi/issues/484)). |
| 2026-08-10 | Fresh timeout and build-error mutation outcomes now fail the default no-`--fail-under` exit gate; exact-cache and incremental-history reuse do not invoke that fresh-outcome gate ([#503](https://github.com/Darkroom4364/togi/issues/503)). |
| 2026-09-11 | Extended the Linux x86_64 published-archive smoke to replay a genuine survivor, reject unchanged weak tests, and verify the exact kill after adding a test. Tier 2 archive checks are unchanged. |
