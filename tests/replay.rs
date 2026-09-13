use assert_cmd::Command;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

fn togi() -> Command {
    Command::cargo_bin("togi").unwrap()
}

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_status(root: &Path) -> Vec<u8> {
    std::process::Command::new("git")
        .args(["status", "--porcelain=v1", "-z"])
        .current_dir(root)
        .output()
        .unwrap()
        .stdout
}

fn snapshot_tree(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(root: &Path, current: &Path, entries: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(read_dir) = fs::read_dir(current) else {
            return;
        };
        let mut paths = read_dir
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                visit(root, &path, entries);
            } else if path.is_file() {
                entries.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(&path).unwrap(),
                ));
            }
        }
    }

    if !root.exists() {
        return Vec::new();
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries);
    entries
}

struct ReplayFixture {
    repo: TempDir,
    report_dir: TempDir,
    report_path: PathBuf,
    log_path: PathBuf,
    source_path: PathBuf,
    report: Value,
}

fn setup_replay_fixture() -> ReplayFixture {
    let repo = TempDir::new().unwrap();
    let root = repo.path();
    git(root, &["init"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Togi Test"]);

    let source_path = root.join("main.go");
    fs::write(
        &source_path,
        "package main\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n",
    )
    .unwrap();
    fs::write(
        root.join("go.mod"),
        "module example.com/replay\n\ngo 1.21\n",
    )
    .unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "initial"]);

    // Keep the target dirty: matching exact target bytes, rather than a clean
    // worktree, are the replay source identity contract.
    fs::write(
        &source_path,
        "package main\n\nfunc add(a, b int) int {\n\tif a > b {\n\t\treturn a\n\t}\n\treturn a + b\n}\n",
    )
    .unwrap();
    #[cfg(windows)]
    fs::write(
        root.join("test.cmd"),
        "@echo off\r\ngit rev-parse --is-inside-work-tree >nul || exit /b 1\r\n>>\"%TOGI_REPLAY_LOG%\" echo x\r\nexit /b 0\r\n",
    )
    .unwrap();
    #[cfg(not(windows))]
    fs::write(
        root.join("test.sh"),
        "#!/bin/sh\ntest \"$(git rev-parse --is-inside-work-tree)\" = true || exit 1\nprintf x >> \"$TOGI_REPLAY_LOG\"\nexit 0\n",
    )
    .unwrap();

    // Both report and command-log paths intentionally live outside the repo.
    let report_dir = TempDir::new().unwrap();
    let log_path = report_dir.path().join("invocations.log");
    let output = togi()
        .args([
            "check",
            "--all",
            "--format",
            "json",
            "--test-cmd",
            fixture_test_cmd(),
            "--no-schemata",
            "--max-per-run",
            "1",
        ])
        .current_dir(root)
        .env("TOGI_REPLAY_LOG", &log_path)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(1),
        "surviving fixture should leave a JSON report\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "check did not emit one JSON report: {error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    assert_eq!(report["kind"], "mutation_report");
    assert_eq!(report["schema_version"], 1);
    assert!(report["source_revision"].as_str().is_some());
    assert_eq!(report["mutations"][0]["source_path"], "main.go");
    assert!(
        report["mutations"][0]["source_fingerprint"]
            .as_str()
            .is_some_and(|fingerprint| fingerprint.starts_with("sha256:"))
    );
    assert_eq!(report["mutations"][0]["replay"]["kind"], "regular_direct");

    let report_path = report_dir.path().join("report.json");
    fs::write(&report_path, &output.stdout).unwrap();
    ReplayFixture {
        repo,
        report_dir,
        report_path,
        log_path,
        source_path,
        report,
    }
}

#[cfg(windows)]
fn fixture_test_cmd() -> &'static str {
    "cmd /C test.cmd"
}

#[cfg(not(windows))]
fn fixture_test_cmd() -> &'static str {
    "sh test.sh"
}

#[cfg(windows)]
fn fixture_effective_command() -> &'static str {
    "Effective command: [\"cmd\",\"/C\",\"test.cmd\"]"
}

#[cfg(not(windows))]
fn fixture_effective_command() -> &'static str {
    "Effective command: [\"sh\",\"test.sh\"]"
}

fn write_json(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn fake_go_path(root: &Path) -> OsString {
    let bin = root.join("fake-go-bin");
    fs::create_dir(&bin).unwrap();
    #[cfg(windows)]
    {
        let source = bin.join("go.rs");
        let go = bin.join("go.exe");
        fs::write(
            &source,
            r#"use std::{fs::OpenOptions, io::Write};

fn main() {
    let mut log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::var_os("TOGI_REPLAY_LOG").unwrap())
        .unwrap();
    log.write_all(b"g").unwrap();
}"#,
        )
        .unwrap();
        let status = std::process::Command::new("rustc")
            .arg(source)
            .arg("-o")
            .arg(go)
            .status()
            .unwrap();
        assert!(status.success(), "failed to build fake go.exe");
    }
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;

        let go = bin.join("go");
        fs::write(&go, "#!/bin/sh\nprintf g >> \"$TOGI_REPLAY_LOG\"\nexit 0\n").unwrap();
        let mut permissions = fs::metadata(&go).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&go, permissions).unwrap();
    }
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).unwrap()
}

fn assert_rejected_without_invocation(fixture: &ReplayFixture, contents: &[u8], id: &str) {
    fs::write(&fixture.report_path, contents).unwrap();
    fs::write(&fixture.log_path, []).unwrap();
    let output = togi()
        .args([
            "replay",
            id,
            "--report",
            fixture.report_path.to_str().unwrap(),
        ])
        .current_dir(fixture.repo.path())
        .env("TOGI_REPLAY_LOG", &fixture.log_path)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "malformed/non-replayable report unexpectedly succeeded\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        fs::read(&fixture.log_path).unwrap().is_empty(),
        "replay spawned the test command before rejecting the report"
    );
}

#[cfg(windows)]
fn non_git_fixture_test_cmd() -> &'static str {
    "cmd /C test.cmd"
}

#[cfg(not(windows))]
fn non_git_fixture_test_cmd() -> &'static str {
    "sh test.sh"
}

#[test]
fn non_git_all_schema_v1_report_is_valid_but_not_replayable() {
    let project = TempDir::new().unwrap();
    let root = project.path();
    let report_dir = TempDir::new().unwrap();
    let report_path = report_dir.path().join("report.json");
    let log_path = report_dir.path().join("invocations.log");

    fs::write(
        root.join("main.go"),
        "package main\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n",
    )
    .unwrap();
    #[cfg(windows)]
    fs::write(
        root.join("test.cmd"),
        "@echo off\r\n>>\"%TOGI_REPLAY_LOG%\" echo x\r\nexit /b 0\r\n",
    )
    .unwrap();
    #[cfg(not(windows))]
    fs::write(
        root.join("test.sh"),
        "#!/bin/sh\nprintf x >> \"$TOGI_REPLAY_LOG\"\nexit 0\n",
    )
    .unwrap();

    let check = togi()
        .args([
            "check",
            "--all",
            "--format",
            "json",
            "--test-cmd",
            non_git_fixture_test_cmd(),
            "--no-schemata",
            "--max-per-run",
            "1",
        ])
        .current_dir(root)
        .env("TOGI_REPLAY_LOG", &log_path)
        .output()
        .unwrap();
    assert_eq!(
        check.status.code(),
        Some(1),
        "surviving fixture should emit a JSON report\nstderr:\n{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let report: Value = serde_json::from_slice(&check.stdout).unwrap_or_else(|error| {
        panic!(
            "check did not emit one JSON report: {error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&check.stdout),
            String::from_utf8_lossy(&check.stderr)
        )
    });
    assert_eq!(report["kind"], "mutation_report");
    assert_eq!(report["schema_version"], 1);
    assert!(
        report["generator"]
            .as_str()
            .is_some_and(|generator| !generator.is_empty())
    );
    assert!(report.get("source_revision").is_none());
    let id = report["mutations"][0]["id"].as_u64().unwrap().to_string();
    fs::write(&report_path, &check.stdout).unwrap();
    fs::write(&log_path, []).unwrap();

    let replay = togi()
        .args(["replay", &id, "--report", report_path.to_str().unwrap()])
        .current_dir(root)
        .env("TOGI_REPLAY_LOG", &log_path)
        .output()
        .unwrap();
    assert!(!replay.status.success());
    let stderr = String::from_utf8_lossy(&replay.stderr);
    assert!(
        stderr.contains("generated without a Git source revision"),
        "{stderr}"
    );
    assert!(
        stderr.contains("rerun `togi check` from a Git worktree"),
        "{stderr}"
    );
    assert!(
        fs::read(&log_path).unwrap().is_empty(),
        "replay spawned the test command for a non-replayable report"
    );
}

#[test]
fn replay_forces_a_real_direct_execution_without_source_or_cache_residue() {
    let fixture = setup_replay_fixture();
    let id = fixture.report["mutations"][0]["id"]
        .as_u64()
        .unwrap()
        .to_string();
    let source_before = fs::read(&fixture.source_path).unwrap();
    let status_before = git_status(fixture.repo.path());
    let git_worktrees_before = snapshot_tree(&fixture.repo.path().join(".git/worktrees"));
    let cache_before = snapshot_tree(&fixture.repo.path().join(".togi-cache"));
    let lock_before = fs::read(fixture.repo.path().join(".togi.lock")).unwrap_or_default();
    let log_before = fs::read(&fixture.log_path).unwrap();

    let output = togi()
        .args([
            "replay",
            &id,
            "--report",
            fixture.report_path.to_str().unwrap(),
        ])
        .current_dir(fixture.repo.path())
        .env("TOGI_REPLAY_LOG", &fixture.log_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "replay failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Expected historical result: survived"));
    assert!(stdout.contains("Fresh result: survived"));
    assert!(stdout.contains("forced fresh direct execution"));
    assert!(stdout.contains(fixture_effective_command()));
    assert_ne!(fs::read(&fixture.log_path).unwrap(), log_before);
    assert_eq!(fs::read(&fixture.source_path).unwrap(), source_before);
    assert_eq!(git_status(fixture.repo.path()), status_before);
    assert_eq!(
        snapshot_tree(&fixture.repo.path().join(".git/worktrees")),
        git_worktrees_before
    );
    assert_eq!(
        snapshot_tree(&fixture.repo.path().join(".togi-cache")),
        cache_before
    );
    assert_eq!(
        fs::read(fixture.repo.path().join(".togi.lock")).unwrap_or_default(),
        lock_before
    );
    assert!(fixture.report_dir.path().exists());
}

#[test]
fn verify_killed_fails_closed_before_or_after_execution() {
    for (case, expected_error, invocations) in [
        ("survived", "repair not verified: expected killed", 2),
        ("baseline", "current unmutated suite must pass first", 1),
        (
            "baseline-side-effect",
            "repair not verified: expected killed",
            2,
        ),
        ("source", "source fingerprint does not match", 0),
        (
            "live-test-change",
            "repair not verified: expected killed",
            2,
        ),
        ("git-origin", "repair not verified: expected killed", 2),
        ("not-survivor", "requires a recorded survivor", 0),
    ] {
        let fixture = setup_replay_fixture();
        match case {
            "baseline" => {
                #[cfg(windows)]
                fs::write(
                    fixture.repo.path().join("test.cmd"),
                    "@echo off\r\n>>\"%TOGI_REPLAY_LOG%\" echo x\r\nexit /b 1\r\n",
                )
                .unwrap();
                #[cfg(not(windows))]
                fs::write(
                    fixture.repo.path().join("test.sh"),
                    "printf x >> \"$TOGI_REPLAY_LOG\"\nexit 1\n",
                )
                .unwrap();
            }
            "source" => fs::write(&fixture.source_path, "package main\n// changed\n").unwrap(),
            "baseline-side-effect" => {
                #[cfg(windows)]
                fs::write(fixture.repo.path().join("test.cmd"), "@echo off\r\n>>\"%TOGI_REPLAY_LOG%\" echo x\r\nif exist baseline-side-effect exit /b 1\r\ntype nul >baseline-side-effect\r\nexit /b 0\r\n").unwrap();
                #[cfg(not(windows))]
                fs::write(fixture.repo.path().join("test.sh"), "printf x >> \"$TOGI_REPLAY_LOG\"\ntest ! -f baseline-side-effect || exit 1\ntouch baseline-side-effect\n").unwrap();
            }
            "live-test-change" => {
                // Deterministically change the live test file while the
                // baseline runs. The mutant must still use the frozen passing
                // test, not count the newly broken live suite as a kill.
                #[cfg(windows)]
                let (script, content) = (
                    "test.cmd",
                    "@echo off\r\n>>\"%TOGI_REPLAY_LOG%\" echo x\r\n>\"%TOGI_LIVE_TEST%\" echo @exit /b 1\r\nexit /b 0\r\n",
                );
                #[cfg(not(windows))]
                let (script, content) = (
                    "test.sh",
                    "printf x >> \"$TOGI_REPLAY_LOG\"\nprintf 'exit 1\\n' > \"$TOGI_LIVE_TEST\"\nexit 0\n",
                );
                let script_path = fixture.repo.path().join(script);
                fs::write(&script_path, content).unwrap();
                let mut report = fixture.report.clone();
                report["mutations"][0]["replay"]["env"]["TOGI_LIVE_TEST"] = json!(script_path);
                write_json(&fixture.report_path, &report);
            }
            "git-origin" => {
                // The frozen mutant must not point origin at a discarded
                // baseline directory: an origin lookup is not a mutation kill.
                #[cfg(windows)]
                fs::write(fixture.repo.path().join("test.cmd"), "@echo off\r\n>>\"%TOGI_REPLAY_LOG%\" echo x\r\ngit ls-remote origin HEAD >nul\r\nexit /b %errorlevel%\r\n").unwrap();
                #[cfg(not(windows))]
                fs::write(
                    fixture.repo.path().join("test.sh"),
                    "printf x >> \"$TOGI_REPLAY_LOG\"\ngit ls-remote origin HEAD >/dev/null\n",
                )
                .unwrap();
            }
            "not-survivor" => {
                let mut report = fixture.report.clone();
                report["mutations"][0]["result"] = json!("killed");
                write_json(&fixture.report_path, &report);
            }
            _ => {}
        }
        let log_before = fs::read(&fixture.log_path).unwrap();
        let output = verify_fixture(&fixture);
        assert_eq!(output.status.code(), Some(2), "{case}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "{case}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Verified:"));
        let log_after = fs::read(&fixture.log_path).unwrap();
        assert_eq!(
            log_after.iter().filter(|&&b| b == b'x').count()
                - log_before.iter().filter(|&&b| b == b'x').count(),
            invocations,
            "{case}"
        );
    }
}

fn verify_fixture(fixture: &ReplayFixture) -> std::process::Output {
    togi()
        .args([
            "replay",
            &fixture.report["mutations"][0]["id"].to_string(),
            "--report",
            fixture.report_path.to_str().unwrap(),
            "--verify-killed",
        ])
        .current_dir(fixture.repo.path())
        .env("TOGI_REPLAY_LOG", &fixture.log_path)
        .output()
        .unwrap()
}

#[test]
fn verify_killed_accepts_committed_and_uncommitted_tests_without_residue() {
    let fixture = setup_replay_fixture();
    let root = fixture.repo.path();
    let source = fs::read(&fixture.source_path).unwrap();
    fs::write(root.join("original.txt"), &source).unwrap();
    // The baseline writes a sentinel. Verification must use a clean workspace
    // for the mutant so an unrelated baseline side effect cannot count as a kill.
    #[cfg(windows)]
    fs::write(root.join("test.cmd"), "@echo off\r\n>>\"%TOGI_REPLAY_LOG%\" echo x\r\nif exist baseline-side-effect exit /b 1\r\ntype nul >baseline-side-effect\r\nfc /B main.go original.txt >nul\r\nexit /b %errorlevel%\r\n").unwrap();
    #[cfg(not(windows))]
    fs::write(root.join("test.sh"), "printf x >> \"$TOGI_REPLAY_LOG\"\ntest ! -f baseline-side-effect || exit 1\ntouch baseline-side-effect\ncmp -s main.go original.txt\n").unwrap();
    for committed in [false, true] {
        if committed {
            git(root, &["add", "main.go", "original.txt"]);
            #[cfg(windows)]
            git(root, &["add", "test.cmd"]);
            #[cfg(not(windows))]
            git(root, &["add", "test.sh"]);
            git(root, &["commit", "-m", "test repair"]);
        }
        let status_before = git_status(root);
        let cache_before = snapshot_tree(&root.join(".togi-cache"));
        let report_before = fs::read(&fixture.report_path).unwrap();
        let output = verify_fixture(&fixture);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("current unmutated suite passed"));
        assert!(stdout.contains("Fresh result: killed"));
        assert!(stdout.contains("Verified: the current suite kills mutation"));
        assert_eq!(fs::read(&fixture.source_path).unwrap(), source);
        assert_eq!(git_status(root), status_before);
        assert_eq!(snapshot_tree(&root.join(".togi-cache")), cache_before);
        assert_eq!(fs::read(&fixture.report_path).unwrap(), report_before);
        assert!(!root.join("baseline-side-effect").exists());

        let historical = togi()
            .args([
                "replay",
                &fixture.report["mutations"][0]["id"].to_string(),
                "--report",
                fixture.report_path.to_str().unwrap(),
            ])
            .current_dir(root)
            .env("TOGI_REPLAY_LOG", &fixture.log_path)
            .output()
            .unwrap();
        assert_eq!(historical.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&historical.stderr).contains(if committed {
                "does not match current Git HEAD"
            } else {
                "replay divergence"
            })
        );
    }
}

#[cfg(unix)]
#[test]
fn verify_killed_does_not_accept_timeout_or_build_error() {
    for result in ["timeout", "build_error"] {
        let fixture = setup_replay_fixture();
        fs::write(
            fixture.repo.path().join("original.txt"),
            fs::read(&fixture.source_path).unwrap(),
        )
        .unwrap();
        let mut report = fixture.report.clone();
        let recipe = &mut report["mutations"][0]["replay"];
        if result == "timeout" {
            recipe["timeout_ms"] = json!(500);
            recipe["test_command"] = json!(["sh", "-c", "cmp -s main.go original.txt || sleep 2"]);
        } else {
            recipe["build_command"] = json!(["sh", "-c", "cmp -s main.go original.txt"]);
            recipe["build_command_origin"] = json!("configured");
        }
        write_json(&fixture.report_path, &report);
        let output = verify_fixture(&fixture);
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("fresh execution returned {result}")),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("Verified:"));
    }
}

#[test]
#[ignore = "requires Go"]
fn default_go_survivor_can_be_replayed_and_verified_after_a_boundary_test() {
    let repo = TempDir::new().unwrap();
    let state = TempDir::new().unwrap();
    let root = repo.path();
    git(root, &["init"]);
    git(root, &["config", "user.email", "test@example.com"]);
    git(root, &["config", "user.name", "Togi Test"]);
    git(root, &["config", "core.autocrlf", "false"]);
    let source = "package boundary\n\nfunc Above(n int) bool { return n > 10 }\n";
    let weak_tests = "package boundary\nimport \"testing\"\nfunc TestAbove(t *testing.T) {\n if Above(9) || !Above(11) { t.Fatal(\"wrong result\") }\n}\n";
    fs::write(root.join("calc.go"), source).unwrap();
    fs::write(root.join("calc_test.go"), weak_tests).unwrap();
    fs::write(
        root.join("go.mod"),
        "module example.com/boundary\n\ngo 1.21\n",
    )
    .unwrap();
    fs::write(root.join(".gitignore"), "/.togi-cache/\n/.togi.lock\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "weak tests"]);
    let go_cache = state.path().join("go-cache");
    let campaign = togi()
        .args([
            "check",
            "--all",
            "--operators",
            "gt_to_gte",
            "--max-per-run",
            "1",
            "--timeout",
            "60",
            "--format",
            "json",
        ])
        .current_dir(root)
        .env("GOCACHE", &go_cache)
        .env("GOTOOLCHAIN", "local")
        .output()
        .unwrap();
    assert_eq!(
        campaign.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&campaign.stderr)
    );
    let report: Value = serde_json::from_slice(&campaign.stdout).unwrap();
    let mutation = &report["mutations"][0];
    assert_eq!(report["mutations"].as_array().unwrap().len(), 1);
    assert_eq!(report["schemata"]["fast_path"], 1);
    assert_eq!(mutation["result"], "survived");
    assert_eq!(mutation["execution"]["state"], "executed");
    assert_eq!(mutation["replay"]["kind"], "regular_direct");
    assert!(
        mutation["replay"]["test_command"]
            .as_array()
            .unwrap()
            .contains(&json!("-count=1"))
    );
    assert!(mutation["replay"]["env"].get("TOGI_MUTANT").is_none());
    let report_path = state.path().join("report.json");
    fs::write(&report_path, &campaign.stdout).unwrap();
    let replay = |verify: bool| {
        let mut command = togi();
        command.args([
            "replay",
            &mutation["id"].to_string(),
            "--report",
            report_path.to_str().unwrap(),
        ]);
        if verify {
            command.arg("--verify-killed");
        }
        command
            .current_dir(root)
            .env("GOCACHE", &go_cache)
            .env("GOTOOLCHAIN", "local")
            .output()
            .unwrap()
    };
    let historical = replay(false);
    assert!(
        historical.status.success(),
        "{}",
        String::from_utf8_lossy(&historical.stderr)
    );
    assert!(String::from_utf8_lossy(&historical.stdout).contains("Fresh result: survived"));
    assert_eq!(
        replay(true).status.code(),
        Some(2),
        "unchanged weak tests must not verify"
    );
    let strong_tests = format!(
        "{weak_tests}\nfunc TestBoundary(t *testing.T) {{ if Above(10) {{ t.Fatal(\"10 is not above 10\") }} }}\n"
    );
    fs::write(root.join("calc_test.go"), &strong_tests).unwrap();
    for committed in [false, true] {
        if committed {
            git(root, &["add", "calc_test.go"]);
            git(root, &["commit", "-m", "cover the boundary"]);
        }
        let cache_before = snapshot_tree(&root.join(".togi-cache"));
        let status_before = git_status(root);
        let verified = replay(true);
        assert!(
            verified.status.success(),
            "{}",
            String::from_utf8_lossy(&verified.stderr)
        );
        assert!(
            String::from_utf8_lossy(&verified.stdout)
                .contains("Verified: the current suite kills mutation")
        );
        assert_eq!(fs::read_to_string(root.join("calc.go")).unwrap(), source);
        assert_eq!(
            fs::read_to_string(root.join("calc_test.go")).unwrap(),
            strong_tests
        );
        assert_eq!(snapshot_tree(&root.join(".togi-cache")), cache_before);
        assert_eq!(git_status(root), status_before);
        assert_eq!(fs::read(&report_path).unwrap(), campaign.stdout);
    }
}

#[cfg(unix)]
mod go_demo {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn fixture() -> TempDir {
        let sandbox = TempDir::new().unwrap();
        let root = sandbox.path();
        let repo = root.join("demo repo");
        fs::create_dir_all(repo.join("examples")).unwrap();
        fs::create_dir_all(repo.join("tests/fixtures/go")).unwrap();
        let original = Path::new(env!("CARGO_MANIFEST_DIR"));
        fs::copy(
            original.join("examples/demo.sh"),
            repo.join("examples/demo.sh"),
        )
        .unwrap();
        for name in ["calc.go", "calc_test.go", "numbers.go", "go.mod"] {
            fs::copy(
                original.join("tests/fixtures/go").join(name),
                repo.join("tests/fixtures/go").join(name),
            )
            .unwrap();
        }
        fs::create_dir(root.join("bin")).unwrap();
        fs::create_dir(root.join("scratch with spaces")).unwrap();
        // No configured identity, and commits would require signing unless the
        // demo explicitly configures its disposable repository.
        fs::write(root.join("gitconfig"), "[commit]\n\tgpgsign = true\n").unwrap();
        sandbox
    }

    fn command(root: &Path) -> Command {
        let mut cmd = Command::new("bash");
        cmd.arg(root.join("demo repo/examples/demo.sh"))
            .current_dir(root)
            .env("TOGI_BIN", "bin/togi with spaces")
            .env("GIT_CONFIG_GLOBAL", root.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GOCACHE", root.join("go-cache"))
            .env("GOTOOLCHAIN", "local")
            .env("TMPDIR", root.join("scratch with spaces"))
            .timeout(std::time::Duration::from_secs(180));
        cmd
    }

    #[test]
    #[ignore = "requires Bash, Go, and jq"]
    fn script_finds_replays_and_verifies_a_real_gap() {
        let sandbox = fixture();
        let root = sandbox.path();
        symlink(togi().get_program(), root.join("bin/togi with spaces")).unwrap();
        let before = snapshot_tree(&root.join("demo repo"));
        let output = command(root).output().unwrap();
        assert!(
            output.status.success(),
            "demo failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        for expected in [
            "SURVIVED: IsPositive(0)",
            "Fresh result: survived",
            "Unchanged weak tests: repair correctly rejected.",
            "func TestIsPositiveAtZero",
            "Fresh result: killed",
            "Verified: the current suite kills mutation",
            "Demo complete: the added test kills the recorded boundary mutation.",
        ] {
            assert!(stdout.contains(expected), "missing {expected}\n{stdout}");
        }
        assert_eq!(snapshot_tree(&root.join("demo repo")), before);
        assert_eq!(
            fs::read_dir(root.join("scratch with spaces"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    #[ignore = "requires Bash and jq"]
    fn script_rejects_campaign_and_repair_failures() {
        let sandbox = fixture();
        let root = sandbox.path();
        let bin = root.join("bin");
        fs::write(bin.join("go"), "#!/bin/sh\nexit \"$DEMO_GO_STATUS\"\n").unwrap();
        fs::set_permissions(bin.join("go"), fs::Permissions::from_mode(0o755)).unwrap();
        let stub = bin.join("togi with spaces");
        fs::write(
            &stub,
            r#"#!/usr/bin/env bash
set -eu
if [[ ${2:-} == --help ]]; then
  echo '  --verify-killed'
  exit 0
fi
echo "$1" >> "$DEMO_CALLS"
if [[ $1 == check ]]; then
  cat "$DEMO_REPORT"
  exit "$DEMO_CHECK_STATUS"
fi
if [[ ${5:-} != --verify-killed ]]; then
  [[ $DEMO_CASE != replay_error ]] || exit 2
  echo 'Fresh result: survived'
elif ! grep -q 'func TestIsPositiveAtZero' calc_test.go; then
  [[ $DEMO_CASE != weak_success ]] || exit 0
  [[ $DEMO_CASE != weak_unrelated_error ]] || exit 2
  echo 'repair not verified: expected killed, fresh execution returned survived' >&2
  exit 2
else
  [[ $DEMO_CASE != repair_error ]] || exit 2
  echo 'Verified: the current suite kills mutation #1'
fi
"#,
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths(
            std::iter::once(bin).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        let before = snapshot_tree(&root.join("demo repo"));
        for case in [
            "baseline_error",
            "check_success",
            "check_error",
            "invalid_json",
            "partial",
            "timeout",
            "build_error",
            "cached",
            "unreplayable",
            "replay_error",
            "weak_success",
            "weak_unrelated_error",
            "repair_error",
        ] {
            let mut report = json!({
                "schema_version": 1, "kind": "mutation_report", "partial": false,
                "total": 1, "planned_total": 1, "killed": 0, "survived": 1,
                "timeout": 0, "build_errors": 0,
                "mutations": [{"id": 1, "source_path": "calc.go", "operator": "gt_to_gte",
                    "original": ">", "replacement": ">=", "result": "survived",
                    "execution": {"state": "executed"}, "replay": {"kind": "regular_direct"}}]
            });
            match case {
                "partial" => report["partial"] = json!(true),
                "timeout" => report["timeout"] = json!(1),
                "build_error" => report["build_errors"] = json!(1),
                "cached" => report["mutations"][0]["execution"]["state"] = json!("exact_cache"),
                "unreplayable" => report["mutations"][0]["replay"]["kind"] = json!("unavailable"),
                _ => {}
            }
            let report_path = root.join("stub-report.json");
            if case == "invalid_json" {
                fs::write(&report_path, "not a report").unwrap();
            } else {
                write_json(&report_path, &report);
            }
            let calls = root.join("calls");
            fs::write(&calls, "").unwrap();
            let output = command(root)
                .env("PATH", &path)
                .env("DEMO_CASE", case)
                .env("DEMO_REPORT", &report_path)
                .env("DEMO_CALLS", &calls)
                .env(
                    "DEMO_GO_STATUS",
                    if case == "baseline_error" { "1" } else { "0" },
                )
                .env(
                    "DEMO_CHECK_STATUS",
                    match case {
                        "check_success" => "0",
                        "check_error" => "2",
                        _ => "1",
                    },
                )
                .output()
                .unwrap();
            assert!(!output.status.success(), "{case} unexpectedly succeeded");
            let expected_calls = match case {
                "baseline_error" => "",
                "replay_error" => "check\nreplay\n",
                "weak_success" | "weak_unrelated_error" => "check\nreplay\nreplay\n",
                "repair_error" => "check\nreplay\nreplay\nreplay\n",
                _ => "check\n",
            };
            assert_eq!(
                fs::read_to_string(&calls).unwrap(),
                expected_calls,
                "{case} did not reach its intended failure stage: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!String::from_utf8_lossy(&output.stdout).contains("Demo complete:"));
            assert_eq!(snapshot_tree(&root.join("demo repo")), before);
            assert_eq!(
                fs::read_dir(root.join("scratch with spaces"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }
}

#[test]
fn legacy_report_without_build_origin_replays_configured_go_lookalike_unchanged() {
    let fixture = setup_replay_fixture();
    let id = fixture.report["mutations"][0]["id"]
        .as_u64()
        .unwrap()
        .to_string();

    let mut legacy = fixture.report.clone();
    let replay = legacy["mutations"][0]["replay"].as_object_mut().unwrap();
    replay.insert(
        "build_command".into(),
        json!(["go", "test", "-c", "-vet=off", "-o", "NUL", "./..."]),
    );
    replay.remove("build_command_origin");
    write_json(&fixture.report_path, &legacy);
    fs::write(&fixture.log_path, []).unwrap();

    let output = togi()
        .args([
            "replay",
            &id,
            "--report",
            fixture.report_path.to_str().unwrap(),
        ])
        .current_dir(fixture.repo.path())
        .env("TOGI_REPLAY_LOG", &fixture.log_path)
        .env("PATH", fake_go_path(fixture.report_dir.path()))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "legacy replay failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(
        "Effective build command: [\"go\",\"test\",\"-c\",\"-vet=off\",\"-o\",\"NUL\",\"./...\"]"
    ));
    let log = String::from_utf8(fs::read(&fixture.log_path).unwrap()).unwrap();
    let ordered: String = log
        .chars()
        .filter(|character| !matches!(character, '\r' | '\n'))
        .collect();
    assert_eq!(
        ordered, "gx",
        "build command must run before the test command"
    );
}

#[test]
fn replay_rejects_invalid_or_mismatched_reports_before_running_tests() {
    let fixture = setup_replay_fixture();
    let id = fixture.report["mutations"][0]["id"]
        .as_u64()
        .unwrap()
        .to_string();

    assert_rejected_without_invocation(&fixture, b"{not json", &id);
    assert_rejected_without_invocation(&fixture, br#"{"mutations":[]}"#, &id);

    let mut unsupported_version = fixture.report.clone();
    unsupported_version["schema_version"] = json!(2);
    write_json(&fixture.report_path, &unsupported_version);
    assert_rejected_without_invocation(&fixture, &fs::read(&fixture.report_path).unwrap(), &id);

    let mut wrong_kind = fixture.report.clone();
    wrong_kind["kind"] = json!("dry_run");
    write_json(&fixture.report_path, &wrong_kind);
    assert_rejected_without_invocation(&fixture, &fs::read(&fixture.report_path).unwrap(), &id);

    assert_rejected_without_invocation(
        &fixture,
        &serde_json::to_vec(&fixture.report).unwrap(),
        "999",
    );

    for replay in [
        json!({"kind": "unavailable", "reason": "schemata"}),
        json!({"kind": "unavailable", "reason": "not_executed"}),
    ] {
        let mut unavailable = fixture.report.clone();
        unavailable["mutations"][0]["replay"] = replay;
        write_json(&fixture.report_path, &unavailable);
        assert_rejected_without_invocation(&fixture, &fs::read(&fixture.report_path).unwrap(), &id);
    }

    let mut non_executed_result = fixture.report.clone();
    non_executed_result["mutations"][0]["result"] = json!("build_error");
    write_json(&fixture.report_path, &non_executed_result);
    assert_rejected_without_invocation(&fixture, &fs::read(&fixture.report_path).unwrap(), &id);

    let mut not_executed = fixture.report.clone();
    not_executed["mutations"][0]["execution"] =
        json!({"state": "not_executed", "reason": "build_error"});
    write_json(&fixture.report_path, &not_executed);
    assert_rejected_without_invocation(&fixture, &fs::read(&fixture.report_path).unwrap(), &id);

    let mut mismatched_origin = fixture.report.clone();
    mismatched_origin["mutations"][0]["replay"]["origin"] = json!("exact_cache");
    write_json(&fixture.report_path, &mismatched_origin);
    assert_rejected_without_invocation(&fixture, &fs::read(&fixture.report_path).unwrap(), &id);

    let mut missing_execution = fixture.report.clone();
    missing_execution["mutations"][0]
        .as_object_mut()
        .unwrap()
        .remove("execution");
    write_json(&fixture.report_path, &missing_execution);
    assert_rejected_without_invocation(&fixture, &fs::read(&fixture.report_path).unwrap(), &id);

    let mut control_path = fixture.report.clone();
    control_path["mutations"][0]["source_path"] = json!(".git/config");
    write_json(&fixture.report_path, &control_path);
    assert_rejected_without_invocation(&fixture, &fs::read(&fixture.report_path).unwrap(), &id);

    let mut uppercase_control_path = fixture.report.clone();
    uppercase_control_path["mutations"][0]["source_path"] = json!(".GIT/config");
    write_json(&fixture.report_path, &uppercase_control_path);
    assert_rejected_without_invocation(&fixture, &fs::read(&fixture.report_path).unwrap(), &id);

    let tampered_values = [
        ("source_path", json!("../test.sh")),
        ("byte_start", json!(999_999)),
        ("original", json!("tampered")),
        (
            "source_fingerprint",
            json!("sha256:0000000000000000000000000000000000000000000000000000000000000000"),
        ),
    ];
    for (field, value) in tampered_values {
        let mut tampered = fixture.report.clone();
        tampered["mutations"][0][field] = value;
        write_json(&fixture.report_path, &tampered);
        assert_rejected_without_invocation(&fixture, &fs::read(&fixture.report_path).unwrap(), &id);
    }

    let mut empty_command = fixture.report.clone();
    empty_command["mutations"][0]["replay"]["test_command"] = json!([]);
    write_json(&fixture.report_path, &empty_command);
    assert_rejected_without_invocation(&fixture, &fs::read(&fixture.report_path).unwrap(), &id);

    let source_before = fs::read(&fixture.source_path).unwrap();
    fs::write(&fixture.source_path, b"package main\n// target changed\n").unwrap();
    assert_rejected_without_invocation(
        &fixture,
        &serde_json::to_vec(&fixture.report).unwrap(),
        &id,
    );
    fs::write(&fixture.source_path, source_before).unwrap();

    git(fixture.repo.path(), &["add", "main.go"]);
    git(fixture.repo.path(), &["commit", "-m", "different head"]);
    assert_rejected_without_invocation(
        &fixture,
        &serde_json::to_vec(&fixture.report).unwrap(),
        &id,
    );
}

#[cfg(unix)]
#[test]
fn replay_rejects_symlink_alias_to_control_path_before_spawning() {
    let fixture = setup_replay_fixture();
    let id = fixture.report["mutations"][0]["id"]
        .as_u64()
        .unwrap()
        .to_string();
    let cached_source = fixture.repo.path().join(".togi-cache/alias-target.go");
    let source_bytes = fs::read(&fixture.source_path).unwrap();
    fs::create_dir_all(cached_source.parent().unwrap()).unwrap();
    fs::write(&cached_source, &source_bytes).unwrap();
    std::os::unix::fs::symlink(&cached_source, fixture.repo.path().join("alias.go")).unwrap();

    let mut alias_report = fixture.report.clone();
    alias_report["mutations"][0]["source_path"] = json!("alias.go");
    write_json(&fixture.report_path, &alias_report);
    fs::write(&fixture.log_path, []).unwrap();
    let output = togi()
        .args([
            "replay",
            &id,
            "--report",
            fixture.report_path.to_str().unwrap(),
        ])
        .current_dir(fixture.repo.path())
        .env("TOGI_REPLAY_LOG", &fixture.log_path)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("resolved replay source path targets a Togi or Git control path")
    );
    assert!(fs::read(&fixture.log_path).unwrap().is_empty());
    assert_eq!(fs::read(&cached_source).unwrap(), source_bytes);
}

#[cfg(windows)]
#[test]
fn replay_rejects_junction_alias_to_control_path_before_spawning() {
    let fixture = setup_replay_fixture();
    let id = fixture.report["mutations"][0]["id"]
        .as_u64()
        .unwrap()
        .to_string();
    let cache_dir = fixture.repo.path().join(".togi-cache");
    let cached_source = cache_dir.join("alias-target.go");
    let source_bytes = fs::read(&fixture.source_path).unwrap();
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(&cached_source, &source_bytes).unwrap();
    // Junctions need no special privilege and must be treated like symlinks:
    // the alias resolves into Togi control state and is rejected pre-spawn.
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(fixture.repo.path().join("alias.d"))
        .arg(&cache_dir)
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J failed");

    let mut alias_report = fixture.report.clone();
    alias_report["mutations"][0]["source_path"] = json!("alias.d/alias-target.go");
    write_json(&fixture.report_path, &alias_report);
    fs::write(&fixture.log_path, []).unwrap();
    let output = togi()
        .args([
            "replay",
            &id,
            "--report",
            fixture.report_path.to_str().unwrap(),
        ])
        .current_dir(fixture.repo.path())
        .env("TOGI_REPLAY_LOG", &fixture.log_path)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("resolved replay source path targets a Togi or Git control path")
    );
    assert!(fs::read(&fixture.log_path).unwrap().is_empty());
    assert_eq!(fs::read(&cached_source).unwrap(), source_bytes);
}
