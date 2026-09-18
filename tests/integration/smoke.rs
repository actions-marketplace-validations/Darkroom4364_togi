//! End-to-end smoke fixtures for runtimes available on the standard Ubuntu CI
//! image. These prove more than mutation generation by running the full
//! mutation -> test-command -> outcome pipeline across several languages.
//!

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use togi::{ChangedFile, LineRange};

struct FixtureCase {
    name: &'static str,
    dir: &'static str,
    file: &'static str,
}

fn fixture_path(dir: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
}

fn source_line_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .expect("fixture source should be readable")
        .lines()
        .count()
        .max(1)
}

fn run_fixture(case: FixtureCase) -> togi::MutationReport {
    let root = fixture_path(case.dir);
    togi::cache::clear(&root).expect("failed to clear togi cache");

    let changed = vec![ChangedFile {
        path: PathBuf::from(case.file),
        hunks: vec![LineRange {
            start: 1,
            end: source_line_count(&root.join(case.file)),
        }],
    }];

    let mutations = togi::mutator::generate_mutations(&changed, &root, 200, 0, &[])
        .expect("failed to generate mutations");
    assert!(
        !mutations.is_empty(),
        "{} fixture should generate at least one mutation",
        case.name
    );

    let baseline = Command::new("bash")
        .arg("run-tests.sh")
        .current_dir(&root)
        .output()
        .expect("failed to run baseline test command");
    assert!(
        baseline.status.success(),
        "{} baseline failed\nstdout:\n{}\nstderr:\n{}",
        case.name,
        String::from_utf8_lossy(&baseline.stdout),
        String::from_utf8_lossy(&baseline.stderr)
    );

    let runner = togi::runner::TestRunner {
        commands: togi::runner::CommandConfig {
            command: vec!["bash".into(), "run-tests.sh".into()],
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec![],
            sandbox_command: vec![],
            build_command_origin: togi::config::BuildCommandOrigin::None,
            timeout: Duration::from_secs(30),
            language_timeouts: HashMap::new(),
            test_selection: None,
        },
        parallelism: 1,
        project_root: root,
        verbose: false,
        show_output: false,
        max_tested: None,
        early_stop: Default::default(),
        respect_workspace_ignores: true,
        env: HashMap::new(),
        incremental_history: false,
        force_rerun: true,
        learned_selection: false,
        cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };

    let report = runner.run(mutations).report;
    assert!(
        report.total > 0,
        "{} fixture should execute mutations",
        case.name
    );
    assert_eq!(
        report.timeout, 0,
        "{} fixture should not time out",
        case.name
    );
    assert_eq!(
        report.build_errors, 0,
        "{} fixture should not produce build errors",
        case.name
    );
    assert_eq!(
        report.total,
        report.killed + report.survived,
        "{} fixture should classify every mutation as killed or survived",
        case.name
    );

    println!(
        "{} fixture: {} total, {} killed, {} survived",
        case.name, report.total, report.killed, report.survived
    );
    report
}

/// Requires `bash` plus the language toolchains bundled on the Ubuntu CI image.
#[test]
#[ignore]
fn rust_fixture_runs_end_to_end() {
    run_fixture(FixtureCase {
        name: "rust",
        dir: "rust",
        file: "src/lib.rs",
    });
}

/// Requires `bash` plus the language toolchains bundled on the Ubuntu CI image.
#[test]
#[ignore]
fn python_fixture_runs_end_to_end() {
    run_fixture(FixtureCase {
        name: "python",
        dir: "python",
        file: "calc.py",
    });
}

/// Requires `bash` plus the language toolchains bundled on the Ubuntu CI image.
#[test]
#[ignore]
fn java_fixture_runs_end_to_end() {
    run_fixture(FixtureCase {
        name: "java",
        dir: "java",
        file: "Calc.java",
    });
}

/// Requires `bash` plus the language toolchains bundled on the Ubuntu CI image.
#[test]
#[ignore]
fn c_fixture_runs_end_to_end() {
    run_fixture(FixtureCase {
        name: "c",
        dir: "c",
        file: "calc.c",
    });
}

/// Requires `bash` plus the language toolchains bundled on the Ubuntu CI image.
#[test]
#[ignore]
fn cpp_fixture_runs_end_to_end() {
    run_fixture(FixtureCase {
        name: "cpp",
        dir: "cpp",
        file: "calc.cpp",
    });
}

/// Requires `bash` plus the language toolchains bundled on the Ubuntu CI image.
#[test]
#[ignore]
fn ruby_fixture_runs_end_to_end() {
    run_fixture(FixtureCase {
        name: "ruby",
        dir: "ruby",
        file: "calc.rb",
    });
}

/// Requires bash plus Node.js 24.14.1 for native TypeScript type stripping.
#[test]
#[ignore]
fn typescript_fixture_runs_end_to_end() {
    let report = run_fixture(FixtureCase {
        name: "typescript",
        dir: "typescript",
        file: "calc.ts",
    });
    assert!(
        report.killed > 0,
        "typescript fixture should kill at least one mutation"
    );
}

/// Requires bash plus the .NET 8 SDK.
#[test]
#[ignore]
fn csharp_fixture_runs_end_to_end() {
    let report = run_fixture(FixtureCase {
        name: "csharp",
        dir: "csharp",
        file: "Calc.cs",
    });
    assert!(
        report.killed > 0,
        "csharp fixture should kill at least one mutation"
    );
}

#[cfg(unix)]
fn demo_executable(path: &Path, source: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, source).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn polyglot_demo_rejects_fatal_and_incomplete_results() {
    use serde_json::json;
    use std::fs;

    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path();
    for dir in [
        "examples",
        "tests/fixtures/polyglot",
        "target/debug",
        "scratch",
    ] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    fs::copy(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/polyglot-demo.sh"),
        root.join("examples/polyglot-demo.sh"),
    )
    .unwrap();
    fs::write(root.join("tests/fixtures/polyglot/fixture.txt"), "fixture").unwrap();
    demo_executable(
        &root.join("target/debug/togi"),
        r#"#!/bin/sh
printf '%s\n' "$1" >> "$DEMO_CALLS"
while [ "$#" -gt 0 ]; do
  if [ "$1" = --json-report ] && [ -f "$DEMO_REPORT" ]; then
    cp "$DEMO_REPORT" "$2"
  fi
  shift
done
exit "$DEMO_STATUS"
"#,
    );
    let valid = json!({
        "schema_version": 1, "kind": "mutation_report", "partial": false,
        "total": 3, "planned_total": 3, "tested": 3, "killed": 0, "survived": 3,
        "timeout": 0, "build_errors": 0,
        "mutations": (["go", "rust", "python"].map(|language| json!({
            "language": language, "result": "survived", "execution": {"state": "executed"}
        })))
    });
    let mut killed = valid.clone();
    killed["killed"] = json!(3);
    killed["survived"] = json!(0);
    for mutation in killed["mutations"].as_array_mut().unwrap() {
        mutation["result"] = json!("killed");
    }
    let mut cases = vec![
        ("survivors", 1, Some(valid.to_string()), 0),
        ("all killed", 0, Some(killed.to_string()), 0),
        ("fatal error", 2, Some(valid.to_string()), 2),
        ("interrupted", 130, None, 130),
        ("missing report", 1, None, 1),
        ("malformed report", 1, Some("{".into()), 1),
        ("survivors with exit zero", 0, Some(valid.to_string()), 1),
    ];
    for (field, value) in [
        ("timeout", json!(1)),
        ("build_errors", json!(1)),
        ("partial", json!(true)),
        ("total", json!(0)),
        ("planned_total", json!(4)),
        ("kind", json!("coverage_gate_report")),
    ] {
        let mut report = valid.clone();
        report[field] = value;
        cases.push((field, 1, Some(report.to_string()), 1));
    }
    for (name, status, report, expected) in cases {
        let report_path = root.join("report.json");
        if let Some(report) = report {
            fs::write(&report_path, report).unwrap();
        } else if report_path.exists() {
            fs::remove_file(&report_path).unwrap();
        }
        fs::write(root.join("calls"), "").unwrap();
        let output = Command::new("bash")
            .arg(root.join("examples/polyglot-demo.sh"))
            .env_remove("TOGI_BIN")
            .env("DEMO_REPORT", &report_path)
            .env("DEMO_STATUS", status.to_string())
            .env("DEMO_CALLS", root.join("calls"))
            .env("TMPDIR", root.join("scratch"))
            .env("GIT_CONFIG_GLOBAL", root.join("no-git-config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{name}\n{stdout}\n{stderr}"
        );
        assert_eq!(
            stdout.contains("=== one run, one report, one gate"),
            expected == 0,
            "{name}\n{stdout}\n{stderr}"
        );
        assert_eq!(fs::read_to_string(root.join("calls")).unwrap(), "check\n");
        assert!(fs::read_dir(root.join("scratch")).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("togi-polyglot.")
        }));
    }
}

/// Proves the documented polyglot demo routes mutations to each language's
/// configured test suite, rather than applying the Rust default everywhere.
#[test]
#[ignore]
#[cfg(unix)]
fn polyglot_demo_routes_mutations_to_each_language_test_suite() {
    // The demo creates commits, so it must not rely on a CI runner's identity.
    let sandbox = tempfile::tempdir().unwrap();
    let scratch = sandbox.path();
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let wrapper = scratch.join("togi with spaces");
    demo_executable(
        &wrapper,
        r#"#!/bin/sh
printf '%s\n' "$1" >> "$DEMO_CALLS"
if [ "$DEMO_FATAL" = 1 ]; then
  "$DEMO_REAL_TOGI" "$@" --config missing-demo-config.toml
else
  "$DEMO_REAL_TOGI" "$@"
fi
status=$?
printf '%s\n' "$status" >> "$DEMO_STATUSES"
while [ "$#" -gt 0 ]; do
  if [ "$1" = --json-report ] && [ -f "$2" ]; then
    cp "$2" "$DEMO_CAPTURE"
  fi
  shift
done
exit "$status"
"#,
    );
    let run = |fatal: bool| {
        assert_cmd::Command::new("bash")
            .arg(root.join("examples/polyglot-demo.sh"))
            .current_dir(scratch)
            .env("TOGI_BIN", "./togi with spaces")
            .env("DEMO_REAL_TOGI", env!("CARGO_BIN_EXE_togi"))
            .env("DEMO_FATAL", if fatal { "1" } else { "0" })
            .env("DEMO_CALLS", scratch.join("calls"))
            .env("DEMO_STATUSES", scratch.join("statuses"))
            .env("DEMO_CAPTURE", scratch.join("report.json"))
            .env("GIT_CONFIG_GLOBAL", scratch.join("no-git-config"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GOTOOLCHAIN", "local")
            .env_remove("CARGO_TARGET_DIR")
            .timeout(Duration::from_secs(180))
            .output()
            .expect("failed to run polyglot demo")
    };
    let output = run(false);
    assert!(
        output.status.success(),
        "polyglot demo failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "✓ KILLED      calc.go:5 — zero_to_one",
        "✓ KILLED      calc.py:2 — increment_numeric",
        "✓ KILLED      src/lib.rs:2 — negate_condition",
    ] {
        assert!(
            stdout.contains(expected),
            "polyglot demo did not route {expected} to its language test suite\nstdout:\n{stdout}"
        );
    }
    let results_line = stdout
        .lines()
        .find(|line| line.starts_with("Results: "))
        .expect("polyglot demo should print a result summary");
    let has_clean_results = ["0 timeout", "0 build errors"]
        .into_iter()
        .all(|expected| results_line.split(", ").any(|field| field == expected));
    assert!(
        has_clean_results,
        "polyglot demo reported a timeout or build error\nstdout:\n{stdout}"
    );
    assert_eq!(stdout.matches("Results: ").count(), 1);
    assert_eq!(
        std::fs::read_to_string(scratch.join("calls")).unwrap(),
        "check\n"
    );
    assert_eq!(
        std::fs::read_to_string(scratch.join("statuses")).unwrap(),
        "1\n"
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(scratch.join("report.json")).unwrap()).unwrap();
    assert_eq!(report["kind"], "mutation_report");
    assert_eq!(report["partial"], false);
    assert_eq!(report["total"], report["planned_total"]);
    assert_eq!(report["timeout"], 0);
    assert_eq!(report["build_errors"], 0);
    assert!(report["survived"].as_u64().unwrap() > 0);
    for language in ["go", "rust", "python"] {
        assert!(
            report["mutations"]
                .as_array()
                .unwrap()
                .iter()
                .any(|mutation| {
                    mutation["language"] == language && mutation["execution"]["state"] == "executed"
                })
        );
    }

    // A genuine engine error must survive the script, without its success banner.
    let failure = run(true);
    assert_eq!(failure.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&failure.stderr)
            .contains("could not read missing-demo-config.toml")
    );
    assert!(
        !String::from_utf8_lossy(&failure.stdout).contains("=== one run, one report, one gate")
    );
    assert_eq!(
        std::fs::read_to_string(scratch.join("calls")).unwrap(),
        "check\ncheck\n"
    );
    assert_eq!(
        std::fs::read_to_string(scratch.join("statuses")).unwrap(),
        "1\n2\n"
    );
}
