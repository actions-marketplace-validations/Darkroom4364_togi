use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
#[cfg(unix)]
use std::os::unix::{
    fs::{PermissionsExt, symlink},
    process::CommandExt,
};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn togi() -> Command {
    Command::cargo_bin("togi").unwrap()
}

fn bash_available() -> bool {
    std::process::Command::new("bash")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Set up a minimal git repo with a Go file and a diff to test against.
fn setup_git_repo() -> TempDir {
    let dir = TempDir::new().unwrap();
    let path = dir.path();

    // Init git repo
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(path)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(path)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(path)
        .output()
        .unwrap();

    // Create initial commit with a Go file
    let go_file = path.join("main.go");
    fs::write(
        &go_file,
        "package main\n\nfunc add(a, b int) int {\n\treturn a + b\n}\n",
    )
    .unwrap();
    fs::write(path.join("go.mod"), "module example.com/test\n\ngo 1.21\n").unwrap();

    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(path)
        .output()
        .unwrap();

    // Make a change (add a comparison)
    fs::write(
        &go_file,
        "package main\n\nfunc add(a, b int) int {\n\tif a > b {\n\t\treturn a\n\t}\n\treturn a + b\n}\n",
    )
    .unwrap();

    dir
}

#[cfg(unix)]
fn assert_json_report_destination_preflight(
    dir: &TempDir,
    json_report_path: &Path,
    configure: impl FnOnce(&mut Command),
) {
    let campaign_marker = dir.path().join("campaign-ran");
    let _ = fs::remove_file(&campaign_marker);
    fs::write(
        dir.path().join("record-campaign.sh"),
        "#!/bin/sh\ntouch campaign-ran\n",
    )
    .unwrap();

    let mut command = togi();
    command
        .args(["check", "--all", "--path", "main.go", "--json-report"])
        .arg(json_report_path)
        .args(["--test-cmd", "sh record-campaign.sh"])
        .current_dir(dir.path());
    configure(&mut command);
    let output = command.output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty(), "stdout: {:?}", output.stdout);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("conflicts with"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !campaign_marker.exists(),
        "collision must stop before a campaign"
    );
    assert!(
        !dir.path().join(".togi-cache").exists(),
        "collision must stop before mutation side effects"
    );
}

#[cfg(unix)]
fn assert_json_report_destination_collision(
    dir: &TempDir,
    json_report_path: &Path,
    destination: &Path,
    configure: impl FnOnce(&mut Command),
) {
    let existing = fs::read(destination).unwrap();
    assert_json_report_destination_preflight(dir, json_report_path, configure);
    assert_eq!(fs::read(destination).unwrap(), existing);
}

#[cfg(unix)]
fn assert_missing_intermediate_destination_rejected(
    dir: &TempDir,
    json_report_path: &Path,
    protected_output: &Path,
    configure: impl FnOnce(&mut Command),
) {
    let missing_parent = dir.path().join("foo");
    let campaign_marker = dir.path().join("campaign-ran");
    assert!(!missing_parent.exists());
    fs::write(
        dir.path().join("create-foo.sh"),
        "#!/bin/sh\nmkdir foo\ntouch campaign-ran\n",
    )
    .unwrap();
    let existing = fs::read(protected_output).unwrap();

    let mut command = togi();
    command
        .args(["check", "--all", "--path", "main.go", "--json-report"])
        .arg(json_report_path)
        .args(["--test-cmd", "sh create-foo.sh"])
        .current_dir(dir.path());
    configure(&mut command);
    let output = command.output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("missing intermediate component"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !missing_parent.exists(),
        "the test command must not create foo"
    );
    assert!(
        !campaign_marker.exists(),
        "the test command must not create a campaign marker"
    );
    assert!(
        !dir.path().join(".togi-cache").exists(),
        "missing intermediate paths must stop before a campaign"
    );
    assert_eq!(fs::read(protected_output).unwrap(), existing);
}

#[test]
fn init_creates_config() {
    let dir = TempDir::new().unwrap();

    togi()
        .arg("init")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Created togi.toml"));

    let config = fs::read_to_string(dir.path().join("togi.toml")).unwrap();
    assert!(config.contains("base = \"origin/main\""));
}

#[test]
fn init_uses_head_parent_in_no_remote_repo() {
    let dir = setup_git_repo();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "second"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    togi()
        .arg("init")
        .current_dir(dir.path())
        .assert()
        .success();
    assert!(
        fs::read_to_string(dir.path().join("togi.toml"))
            .unwrap()
            .contains("base = \"HEAD~1\"")
    );

    togi()
        .args(["check", "--dry-run", "--test-cmd", "true"])
        .current_dir(dir.path())
        .assert()
        .success();
}

#[test]
fn init_uses_head_for_root_commit_in_no_remote_repo() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for args in [
        &["init"][..],
        &["config", "user.email", "test@test.com"][..],
        &["config", "user.name", "Test"][..],
    ] {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
    }
    fs::write(root.join("go.mod"), "module example.com/test\n\ngo 1.21\n").unwrap();
    fs::write(root.join("main.go"), "package main\nfunc main() {}\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(root)
        .output()
        .unwrap();

    togi().arg("init").current_dir(root).assert().success();
    let config = fs::read_to_string(root.join("togi.toml")).unwrap();
    assert!(config.contains("base = \"HEAD\""));

    togi()
        .args(["check", "--dry-run", "--test-cmd", "true"])
        .current_dir(root)
        .assert()
        .success();
}

#[cfg(not(windows))] // Git cannot create quote-containing refs on Windows.
#[test]
fn init_escapes_quoted_origin_head_target_in_relative_config() {
    let dir = setup_git_repo();
    let root = dir.path();
    let update_ref = std::process::Command::new("git")
        .args(["update-ref", "refs/remotes/origin/trunk\"quoted", "HEAD"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(update_ref.status.success());
    let origin_head = std::process::Command::new("git")
        .args([
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk\"quoted",
        ])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(origin_head.status.success());

    togi().arg("init").current_dir(root).assert().success();
    let config: toml::Value =
        toml::from_str(&fs::read_to_string(root.join("togi.toml")).unwrap()).unwrap();
    assert_eq!(
        config["diff"]["base"].as_str(),
        Some("origin/trunk\"quoted")
    );
}
#[test]
fn init_generates_all_supported_root_routes() {
    let dir = TempDir::new().unwrap();
    for file in [
        "Cargo.toml",
        "go.mod",
        "pyproject.toml",
        "package.json",
        "pom.xml",
        "Gemfile",
        "CMakeLists.txt",
        "example.csproj",
    ] {
        fs::write(dir.path().join(file), "").unwrap();
    }

    togi()
        .arg("init")
        .current_dir(dir.path())
        .assert()
        .success()
        .stderr(predicate::str::contains("multiple build systems detected").not());
    let config = fs::read_to_string(dir.path().join("togi.toml")).unwrap();
    for language in [
        "go",
        "python",
        "typescript",
        "java",
        "ruby",
        "c",
        "cpp",
        "c_sharp",
    ] {
        assert!(
            config.contains(&format!("[test.languages.{language}]")),
            "missing route for {language}"
        );
    }
}

#[test]
fn init_rejects_conflicting_java_routes_without_writing_config() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("pom.xml"), "").unwrap();
    fs::write(dir.path().join("build.gradle"), "").unwrap();

    togi()
        .arg("init")
        .current_dir(dir.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("pom.xml"))
        .stderr(predicate::str::contains("build.gradle"))
        .stderr(predicate::str::contains("[projects.*.test]"));
    assert!(!dir.path().join("togi.toml").exists());
}

#[test]
fn init_rejects_ambiguous_dotnet_routes_without_writing_fallback() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("App.sln"), "").unwrap();
    fs::write(dir.path().join("App.csproj"), "").unwrap();

    togi()
        .arg("init")
        .current_dir(dir.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "multiple test runtimes detected (.sln/.csproj)",
        ))
        .stderr(predicate::str::contains("set [test] command in togi.toml"))
        .stderr(predicate::str::contains("make test").not());
    assert!(!dir.path().join("togi.toml").exists());
}

#[cfg(unix)]
#[test]
fn init_routes_non_primary_languages_over_cargo() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init"]);
    git(&["config", "user.email", "test@test.com"]);
    git(&["config", "user.name", "Test"]);
    for (file, content) in [
        (
            "Cargo.toml",
            "[package]\nname = \"example\"\nversion = \"0.1.0\"\n",
        ),
        ("Gemfile", ""),
        ("pom.xml", ""),
        ("CMakeLists.txt", ""),
        ("example.csproj", ""),
        ("calc.rb", "def is_big(x)\n  x + 1 > 3\nend\n"),
        (
            "Calc.java",
            "public final class Calc {\n    public static boolean isBig(int x) {\n        return x + 1 > 3;\n    }\n}\n",
        ),
        ("calc.c", "int is_big(int x) {\n    return x + 1 > 3;\n}\n"),
        (
            "calc.cpp",
            "bool is_big(int x) {\n    return x + 1 > 3;\n}\n",
        ),
        (
            "Calc.cs",
            "public static class Calc\n{\n    public static bool IsBig(int x)\n    {\n        return x + 1 > 3;\n    }\n}\n",
        ),
    ] {
        fs::write(root.join(file), content).unwrap();
    }
    git(&["add", "."]);
    git(&["commit", "-m", "initial"]);

    togi().arg("init").current_dir(root).assert().success();
    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    for command in ["bundle", "mvn", "ctest", "dotnet"] {
        let executable = bin.join(command);
        fs::write(
            &executable,
            "#!/bin/sh\nbasename \"$0\" >> \"$TOGI_TEST_LOG\"\n",
        )
        .unwrap();
        let mut permissions = fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&executable, permissions).unwrap();
    }
    let log = root.join("route.log");
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    for (source, expected_command) in [
        ("calc.rb", "bundle"),
        ("Calc.java", "mvn"),
        ("calc.c", "ctest"),
        ("calc.cpp", "ctest"),
        ("Calc.cs", "dotnet"),
    ] {
        let changed = fs::read_to_string(root.join(source)).unwrap();
        fs::write(root.join(source), changed.replace("> 3", "> 4")).unwrap();
        git(&["add", source]);
        git(&["commit", "-m", source]);

        let _ = fs::remove_file(&log);
        let output = togi()
            .args([
                "check",
                "--base",
                "HEAD~1",
                "--max-per-run",
                "1",
                "--no-schemata",
            ])
            .current_dir(root)
            .env("PATH", &path)
            .env("TOGI_TEST_LOG", &log)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let log_content = fs::read_to_string(&log).unwrap_or_default();
        let commands: Vec<_> = log_content.lines().collect();
        assert!(!commands.is_empty(), "{source} ran no test command");
        assert!(
            commands.iter().all(|command| *command == expected_command),
            "{source} did not use {expected_command}: {commands:?}"
        );
    }
}

#[test]
fn init_fails_if_config_exists() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("togi.toml"), "").unwrap();

    togi()
        .arg("init")
        .current_dir(dir.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn check_empty_diff_prints_activation_path_and_exits_zero() {
    let dir = setup_git_repo();

    // Commit the working change so there's no unstaged diff against HEAD
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "second"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    // Diff HEAD against HEAD = no changes
    let output = togi()
        .args(["check", "--base", "HEAD"])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("selected diff is empty; that is normal for a clean clone, not an error."),
        "stdout: {stdout}"
    );
    assert!(
        stdout.contains("togi check --all --dry-run --max-per-run 10"),
        "stdout: {stdout}"
    );
    for expected in [
        "After a supported source change, run `togi check`, or `togi check --base <ref>` for another intended base.",
        "`--all` and `--base` are alternatives; do not combine them.",
        "If command detection is ambiguous, run `togi init` from the repository root.",
    ] {
        assert!(stdout.contains(expected), "stdout: {stdout}");
    }
    assert!(
        !dir.path().join(".togi-cache").exists(),
        "an empty diff must not start a mutation run"
    );
}

#[test]
fn check_dry_run_lists_mutations() {
    let dir = setup_git_repo();

    togi()
        .args(["check", "--base", "HEAD", "--dry-run", "--test-cmd", "true"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("mutations would be generated"));
}

fn setup_ambiguous_runtime_repo() -> TempDir {
    let dir = setup_git_repo();
    fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"example\"\n",
    )
    .unwrap();
    dir
}

#[test]
fn check_rejects_ambiguous_automatic_test_command() {
    let dir = setup_ambiguous_runtime_repo();

    togi()
        .args(["check", "--base", "not-a-valid-revision"])
        .current_dir(dir.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "multiple test runtimes detected (Cargo.toml, go.mod)",
        ))
        .stderr(predicate::str::contains("[test] command"))
        .stderr(predicate::str::contains("--test-cmd"));

    assert!(!dir.path().join(".togi.lock").exists());
}

#[test]
fn check_rejects_empty_language_route_before_baseline_or_cache() {
    let dir = setup_ambiguous_runtime_repo();
    fs::write(
        dir.path().join("togi.toml"),
        r#"
[test.languages.go]
command = []
"#,
    )
    .unwrap();

    togi()
        .args(["check", "--base", "not-a-valid-revision"])
        .current_dir(dir.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "multiple test runtimes detected (Cargo.toml, go.mod)",
        ))
        .stderr(predicate::str::contains("[test] command"));

    assert!(!dir.path().join(".togi.lock").exists());
    assert!(!dir.path().join(".togi-cache").exists());
}

#[test]
fn check_rejects_unrouted_path_when_ambiguity_is_deferred() {
    let dir = setup_ambiguous_runtime_repo();
    fs::write(
        dir.path().join("togi.toml"),
        r#"
[test.languages.rust]
command = ["true"]
"#,
    )
    .unwrap();

    togi()
        .args(["check", "--base", "HEAD"])
        .current_dir(dir.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "multiple test runtimes detected (Cargo.toml, go.mod)",
        ))
        .stderr(predicate::str::contains(
            "main.go has no explicit project or language test command",
        ));

    assert!(!dir.path().join(".togi-cache").exists());
}

#[cfg(unix)]
#[test]
fn check_rejects_empty_project_route_before_baseline_or_cache() {
    let dir = setup_ambiguous_runtime_repo();
    let log = dir.path().join("language-route.log");
    fs::write(
        dir.path().join("togi.toml"),
        r#"
[test.languages.go]
command = ["sh", "-c", 'echo language-route >> "$TOGI_TEST_LOG"']

[projects.main]
path = "main.go"

[projects.main.test]
command = []
"#,
    )
    .unwrap();

    togi()
        .args(["check", "--base", "HEAD"])
        .env("TOGI_TEST_LOG", &log)
        .current_dir(dir.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains(
            "multiple test runtimes detected (Cargo.toml, go.mod)",
        ))
        .stderr(predicate::str::contains(
            "main.go has no explicit project or language test command",
        ));

    assert!(!log.exists());
    assert!(!dir.path().join(".togi-cache").exists());
}

#[test]
fn check_cli_test_command_overrides_ambiguous_automatic_detection() {
    let dir = setup_ambiguous_runtime_repo();

    togi()
        .args(["check", "--base", "HEAD", "--dry-run", "--test-cmd", "true"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("mutations would be generated"));
}

#[test]
fn check_config_test_command_overrides_ambiguous_automatic_detection() {
    let dir = setup_ambiguous_runtime_repo();
    fs::write(
        dir.path().join("togi.toml"),
        r#"
[test]
command = ["true"]
"#,
    )
    .unwrap();

    togi()
        .args(["check", "--base", "HEAD", "--dry-run"])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("mutations would be generated"));
}

#[cfg(unix)]
#[test]
fn check_uses_explicit_routes_in_ambiguous_mixed_runtime_repo() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    std::process::Command::new("git")
        .arg("init")
        .current_dir(root)
        .output()
        .unwrap();
    fs::create_dir_all(root.join("rust/src")).unwrap();
    fs::create_dir_all(root.join("go")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"mixed\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(root.join("go.mod"), "module example.com/mixed\n\ngo 1.21\n").unwrap();
    fs::write(
        root.join("rust/src/lib.rs"),
        "pub fn compare(a: i32, b: i32) -> bool { a > b }\n",
    )
    .unwrap();
    fs::write(
        root.join("go/main.go"),
        "package sample\n\nfunc compare(a, b int) bool { return a > b }\n",
    )
    .unwrap();
    fs::write(
        root.join("togi.toml"),
        r#"
[test.languages.rust]
command = ["sh", "-c", 'echo rust-route >> "$TOGI_TEST_LOG"']

[projects.go]
path = "go"

[projects.go.test]
command = ["sh", "-c", 'echo project-route >> "$TOGI_TEST_LOG"']
"#,
    )
    .unwrap();
    let log = root.join("routes.log");

    let output = togi()
        .args([
            "check",
            "--all",
            "--max-per-run",
            "2",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
        ])
        .env("TOGI_TEST_LOG", &log)
        .current_dir(root)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let routes = fs::read_to_string(log).unwrap();
    assert!(routes.lines().any(|route| route == "rust-route"));
    assert!(routes.lines().any(|route| route == "project-route"));
}

#[cfg(unix)]
#[test]
fn check_uses_absolute_nested_project_route_in_ambiguous_mixed_runtime_repo() {
    let dir = setup_ambiguous_runtime_repo();
    let root = dir.path();
    let services = root.join("services");
    let api = services.join("api");
    fs::create_dir_all(&api).unwrap();
    fs::write(
        api.join("main.go"),
        "package api\n\nfunc compare(a, b int) bool { return a > b }\n",
    )
    .unwrap();
    fs::write(
        root.join("togi.toml"),
        format!(
            r#"
[test.languages.go]
command = ["sh", "-c", 'echo language-route >> "$TOGI_TEST_LOG"']

[projects.services]
path = "{}"

[projects.services.test]
command = ["sh", "-c", 'echo services-route >> "$TOGI_TEST_LOG"']

[projects.api]
path = "{}"

[projects.api.test]
command = ["sh", "-c", 'echo api-route >> "$TOGI_TEST_LOG"']
"#,
            services.display(),
            api.display()
        ),
    )
    .unwrap();
    let log = root.join("routes.log");

    let output = togi()
        .args([
            "check",
            "--all",
            "--max-per-run",
            "2",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
        ])
        .env("TOGI_TEST_LOG", &log)
        .current_dir(root)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let routes = fs::read_to_string(log).unwrap();
    assert!(routes.lines().any(|route| route == "language-route"));
    assert!(routes.lines().any(|route| route == "api-route"));
    assert!(!routes.lines().any(|route| route == "services-route"));
}

/// Extend the fixture so mutants land on two lines: line 4 (the added `if`)
/// and line 7 (`return a - b`).
fn setup_two_line_mutation_repo() -> TempDir {
    let dir = setup_git_repo();
    fs::write(
        dir.path().join("main.go"),
        "package main\n\nfunc add(a, b int) int {\n\tif a > b {\n\t\treturn a\n\t}\n\treturn a - b\n}\n",
    )
    .unwrap();
    dir
}

#[cfg(unix)]
#[test]
fn check_classifies_zero_coverage_mutants_as_uncovered() {
    let dir = setup_two_line_mutation_repo();
    // Line 4 covered; line 7 tracked but never executed.
    fs::write(
        dir.path().join("lcov.info"),
        "SF:main.go\nDA:4,1\nDA:7,0\nend_of_record\n",
    )
    .unwrap();
    let expected = tempfile::NamedTempFile::new().unwrap();
    fs::write(
        expected.path(),
        fs::read(dir.path().join("main.go")).unwrap(),
    )
    .unwrap();
    let test_cmd = format!(
        "sh -c {}",
        shell_quote_text(&format!("cmp -s main.go {}", shell_quote(expected.path())))
    );

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--test-cmd",
            &test_cmd,
            "--coverage-file",
            "lcov.info",
            "--fail-under",
            "100",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    // The command passes for the unmutated file and kills each executed
    // mutant. The zero-coverage mutant must not count as a survivor, so the
    // run passes even with --fail-under 100.
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("reported as uncovered"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON output: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(value["killed"], 3);
    assert_eq!(value["survived"], 0);
    assert_eq!(value["uncovered"], 1);
    assert_eq!(value["mutation_score"], 100.0);

    let mutations = value["mutations"].as_array().unwrap();
    let uncovered: Vec<_> = mutations
        .iter()
        .filter(|m| m["result"] == "uncovered")
        .collect();
    assert_eq!(uncovered.len(), 1);
    assert_eq!(uncovered[0]["line"], 7);
    assert_eq!(uncovered[0]["file"], "main.go");
}

#[test]
fn check_all_mutants_uncovered_skips_execution() {
    let dir = setup_two_line_mutation_repo();
    // Both mutated lines have zero coverage: nothing should execute.
    fs::write(
        dir.path().join("lcov.info"),
        "SF:main.go\nDA:4,0\nDA:7,0\nend_of_record\n",
    )
    .unwrap();

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--test-cmd",
            "true",
            "--coverage-file",
            "lcov.info",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("Running"),
        "no mutations should execute: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON output: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(value["total"], 4);
    assert_eq!(value["killed"], 0);
    assert_eq!(value["survived"], 0);
    assert_eq!(value["uncovered"], 4);
    assert_eq!(value["mutation_score"], 100.0);
    assert!(
        value["mutations"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["result"] == "uncovered")
    );
}

#[test]
fn check_all_mutants_uncovered_aborts_when_baseline_test_fails() {
    let dir = setup_two_line_mutation_repo();
    fs::write(
        dir.path().join("lcov.info"),
        "SF:main.go\nDA:4,0\nDA:7,0\nend_of_record\n",
    )
    .unwrap();

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--test-cmd",
            "false",
            "--coverage-file",
            "lcov.info",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Test suite failure before mutation execution."));
    assert!(stderr.contains("Baseline phase: test"));
    assert!(stderr.contains("Outcome: failed"));
    assert!(stderr.contains("baseline test command failed (`false`)"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("\"killed\""));
    assert!(!stdout.contains("mutation_score"));
    assert!(!dir.path().join(".togi-cache").exists());
}

#[test]
fn check_all_mutants_uncovered_aborts_when_baseline_build_fails() {
    let dir = setup_two_line_mutation_repo();
    fs::write(
        dir.path().join("lcov.info"),
        "SF:main.go\nDA:4,0\nDA:7,0\nend_of_record\n",
    )
    .unwrap();
    let report_path = dir.path().join("baseline-failure-report.json");

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--json-report",
            report_path.to_str().unwrap(),
            "--test-cmd",
            "true",
            "--build-cmd",
            "false",
            "--coverage-file",
            "lcov.info",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("baseline build command failed (`false`)"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let failure: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("invalid suite-failure JSON: {error}\nstdout:\n{stdout}"));
    assert_eq!(failure["kind"], "suite_failure");
    assert_eq!(failure["phase"], "build");
    assert_eq!(failure["command"], serde_json::json!(["false"]));
    assert_eq!(failure["outcome"], "failed");
    assert!(failure.get("mutations").is_none());
    assert!(failure.get("mutation_score").is_none());
    assert!(!dir.path().join(".togi-cache").exists());
    assert!(!report_path.exists());
}

#[test]
fn check_warns_when_fail_fast_is_ignored_for_custom_test_cmd() {
    let dir = setup_git_repo();

    togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--dry-run",
            "--test-cmd",
            "true",
            "--fail-fast",
        ])
        .current_dir(dir.path())
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "--fail-fast is ignored when --test-cmd is set",
        ));
}

#[test]
fn check_format_json_outputs_standalone_json_for_explain() {
    let dir = setup_git_repo();

    let output = togi()
        .args(["check", "--all", "--format", "json", "--test-cmd", "true"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Scanning all 1 supported files..."),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON output: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert!(value.get("total").is_some());
    assert!(value.get("mutations").is_some());

    let report_path = dir.path().join("togi-report.json");
    fs::write(&report_path, &output.stdout).unwrap();
    let mutant_id = value["mutations"][0]["id"]
        .as_u64()
        .expect("report must contain a mutation")
        .to_string();

    togi()
        .args([
            "explain",
            &mutant_id,
            "--report",
            report_path.to_str().unwrap(),
        ])
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains(format!("Mutation #{mutant_id}")));
}

#[test]
fn check_format_json_outputs_valid_json() {
    let dir = setup_git_repo();

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--test-cmd",
            "true",
            "--fail-under",
            "0",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON output: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert!(value.get("total").is_some());
    assert!(value.get("mutations").is_some());
}

#[test]
fn check_json_report_sidecar_preserves_non_json_output_and_replays() {
    let dir = setup_git_repo();
    let report_path = dir.path().join("report.json");

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "github",
            "--json-report",
            report_path.to_str().unwrap(),
            "--no-schemata",
            "--test-cmd",
            "true",
            "--fail-under",
            "0",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("::warning"));

    let report: serde_json::Value = serde_json::from_slice(&fs::read(&report_path).unwrap())
        .expect("sidecar must be valid JSON");
    assert_eq!(report["kind"], "mutation_report");
    assert_eq!(report["schema_version"], 1);
    let mutation = report["mutations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|mutation| mutation["replay"]["kind"] == "regular_direct")
        .expect("sidecar must contain a directly replayable mutation");
    let mutant_id = mutation["id"].as_u64().unwrap().to_string();

    togi()
        .args([
            "replay",
            &mutant_id,
            "--report",
            report_path.to_str().unwrap(),
        ])
        .current_dir(dir.path())
        .assert()
        .success();
}

#[test]
fn check_json_report_sidecar_reuses_json_stdout_payload() {
    let dir = setup_git_repo();
    let report_path = dir.path().join("report.json");
    fs::write(&report_path, "stale report\n").unwrap();

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--json-report",
            report_path.to_str().unwrap(),
            "--no-schemata",
            "--test-cmd",
            "true",
            "--fail-under",
            "0",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout, fs::read(&report_path).unwrap());
}

#[cfg(unix)]
#[test]
fn check_json_report_staging_failure_preserves_existing_report() {
    if std::process::Command::new("id")
        .arg("-u")
        .output()
        .is_ok_and(|output| output.status.success() && output.stdout == b"0\n")
    {
        eprintln!("skipping staging-failure test because the test runs as root");
        return;
    }
    let dir = setup_git_repo();
    let report_dir = dir.path().join("locked-report-directory");
    fs::create_dir(&report_dir).unwrap();
    let report_path = report_dir.join("report.json");
    let existing = "{\"kind\":\"mutation_report\",\"old\":true}\n";
    fs::write(&report_path, existing).unwrap();
    let original_permissions = fs::metadata(&report_dir).unwrap().permissions();
    let mut locked_permissions = original_permissions.clone();
    locked_permissions.set_mode(0o500);
    fs::set_permissions(&report_dir, locked_permissions).unwrap();

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "github",
            "--json-report",
            report_path.to_str().unwrap(),
            "--no-schemata",
            "--test-cmd",
            "true",
            "--fail-under",
            "0",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();
    fs::set_permissions(&report_dir, original_permissions).unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(fs::read_to_string(&report_path).unwrap(), existing);
}

#[cfg(unix)]
#[test]
fn check_json_report_rejects_later_output_destination_collisions_before_campaign() {
    let dir = setup_git_repo();

    let html_parent = dir.path().join("html-parent");
    fs::create_dir(&html_parent).unwrap();
    let html_report = dir.path().join("togi-report.html");
    fs::write(&html_report, "existing HTML report\n").unwrap();
    let html_sidecar = html_parent.join("..").join("togi-report.html");
    assert_json_report_destination_collision(&dir, &html_sidecar, &html_report, |command| {
        command.args(["--format", "html"]);
    });

    let baseline_parent = dir.path().join("baseline-parent");
    fs::create_dir(&baseline_parent).unwrap();
    let baseline = dir.path().join(".togi-baseline");
    fs::write(&baseline, "existing baseline\n").unwrap();
    let baseline_sidecar = baseline_parent.join("..").join(".togi-baseline");
    assert_json_report_destination_collision(&dir, &baseline_sidecar, &baseline, |command| {
        command.arg("--save-baseline");
    });

    let comment_parent = dir.path().join("comment-parent");
    fs::create_dir(&comment_parent).unwrap();
    let comment = dir.path().join("togi-comment.md");
    fs::write(&comment, "existing PR comment\n").unwrap();
    let comment_sidecar = comment_parent.join("..").join("togi-comment.md");
    assert_json_report_destination_collision(&dir, &comment_sidecar, &comment, |command| {
        command.arg("--pr-comment").arg(&comment);
    });
}

#[cfg(unix)]
#[test]
fn check_json_report_rejects_symlink_output_aliases_before_campaign() {
    let dangling_html_repo = setup_git_repo();
    let dangling_html = dangling_html_repo.path().join("togi-report.html");
    let dangling_html_sidecar = dangling_html_repo.path().join("report.json");
    symlink("report.json", &dangling_html).unwrap();
    assert_json_report_destination_preflight(
        &dangling_html_repo,
        &dangling_html_sidecar,
        |command| {
            command.args(["--format", "html"]);
        },
    );
    assert!(!dangling_html_sidecar.exists());
    assert!(
        fs::symlink_metadata(&dangling_html)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let comment_repo = setup_git_repo();
    let comment_target = comment_repo.path().join("comment-target");
    fs::create_dir_all(comment_target.join("nested")).unwrap();
    symlink(
        "comment-target/nested",
        comment_repo.path().join("comment-link"),
    )
    .unwrap();
    let comment_sidecar = comment_target.join("togi-comment.md");
    fs::write(&comment_sidecar, "JSON sidecar must survive\n").unwrap();
    let comment_alias = PathBuf::from("comment-link")
        .join("..")
        .join("togi-comment.md");
    assert_json_report_destination_collision(
        &comment_repo,
        &comment_sidecar,
        &comment_sidecar,
        |command| {
            command.arg("--pr-comment").arg(&comment_alias);
        },
    );

    let baseline_repo = setup_git_repo();
    let dangling_baseline = baseline_repo.path().join(".togi-baseline");
    let dangling_baseline_sidecar = baseline_repo.path().join("baseline-sidecar.json");
    symlink("baseline-sidecar.json", &dangling_baseline).unwrap();
    assert_json_report_destination_preflight(
        &baseline_repo,
        &dangling_baseline_sidecar,
        |command| {
            command.arg("--save-baseline");
        },
    );
    assert!(!dangling_baseline_sidecar.exists());
    assert!(
        fs::symlink_metadata(&dangling_baseline)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(unix)]
#[test]
fn check_json_report_rejects_missing_intermediate_output_aliases_before_campaign() {
    let html_repo = setup_git_repo();
    let html_output = html_repo.path().join("togi-report.html");
    fs::write(&html_output, "HTML output must survive\n").unwrap();
    assert_missing_intermediate_destination_rejected(
        &html_repo,
        Path::new("foo/../togi-report.html"),
        &html_output,
        |command| {
            command.args(["--format", "html"]);
        },
    );

    let comment_repo = setup_git_repo();
    let comment_output = comment_repo.path().join("togi-comment.md");
    fs::write(&comment_output, "PR comment must survive\n").unwrap();
    assert_missing_intermediate_destination_rejected(
        &comment_repo,
        Path::new("foo/../togi-comment.md"),
        &comment_output,
        |command| {
            command.arg("--pr-comment").arg("togi-comment.md");
        },
    );
}

#[test]
fn check_json_report_allows_distinct_nested_pr_comment_output() {
    let dir = setup_git_repo();
    let report_path = dir.path().join("report.json");
    let comment_path = dir.path().join("reports/comment.md");
    assert!(!comment_path.parent().unwrap().exists());

    let output = togi()
        .args([
            "check",
            "--all",
            "--path",
            "main.go",
            "--json-report",
            report_path.to_str().unwrap(),
            "--pr-comment",
            comment_path.to_str().unwrap(),
            "--no-schemata",
            "--test-cmd",
            "true",
            "--fail-under",
            "0",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&report_path).unwrap()).expect("sidecar must be JSON");
    assert_eq!(report["kind"], "mutation_report");
    assert_eq!(report["schema_version"], 1);
    assert!(
        report["total"].as_u64().unwrap() > 0,
        "the nested output path must not skip the mutation campaign"
    );
    let comment = fs::read_to_string(&comment_path).expect("nested PR comment must be written");
    assert!(comment.contains("<!-- togi-mutation-report -->"));
    assert_ne!(fs::read(&report_path).unwrap(), comment.as_bytes());
}

#[cfg(unix)]
#[test]
fn check_json_report_revalidates_nested_pr_comment_after_campaign() {
    let dir = setup_git_repo();
    let report_path = dir.path().join("report.json");
    let comment_path = Path::new("reports/report.json");
    let campaign_marker = dir.path().join("campaign-ran");
    let sentinel = "JSON sidecar must survive\n";
    fs::write(&report_path, sentinel).unwrap();
    assert!(!dir.path().join("reports").exists());
    let root = dir.path().to_str().unwrap();
    fs::write(
        dir.path().join("create-reports-alias.sh"),
        format!(
            "#!/bin/sh\nif [ ! -L \"{root}/reports\" ]; then\n  rm -rf \"{root}/reports\"\n  ln -s . \"{root}/reports\"\nfi\ntouch \"{root}/campaign-ran\"\n"
        ),
    )
    .unwrap();

    let output = togi()
        .args([
            "check",
            "--all",
            "--path",
            "main.go",
            "--json-report",
            report_path.to_str().unwrap(),
            "--pr-comment",
            comment_path.to_str().unwrap(),
            "--no-schemata",
            "--jobs",
            "1",
            "--test-cmd",
            "sh create-reports-alias.sh",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("conflicts with"), "stderr: {stderr}");
    assert!(
        !stdout.contains("Results:"),
        "the report renderer must not publish after the collision: {stdout}"
    );
    assert!(
        !stderr.contains("PR comment written"),
        "the PR comment writer must not publish after the collision: {stderr}"
    );
    assert!(
        campaign_marker.exists(),
        "the post-campaign check must run after test commands"
    );
    assert_eq!(fs::read_to_string(&report_path).unwrap(), sentinel);
    assert!(
        fs::symlink_metadata(dir.path().join("reports"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the test command must leave reports aliased to the project root"
    );
}

#[test]
fn check_json_report_rejects_case_only_html_alias_before_campaign() {
    let dir = setup_git_repo();
    let sidecar = dir.path().join("TOGI-REPORT.HTML");
    let existing = "JSON sidecar must survive\n";
    fs::write(&sidecar, existing).unwrap();

    let output = togi()
        .args([
            "check",
            "--all",
            "--path",
            "main.go",
            "--format",
            "html",
            "--json-report",
            sidecar.to_str().unwrap(),
            "--test-cmd",
            "command-that-must-not-run",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("conflicts with"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !dir.path().join(".togi-cache").exists(),
        "case-only collision must stop before a campaign"
    );
    assert_eq!(fs::read_to_string(&sidecar).unwrap(), existing);
}

#[test]
fn check_format_json_dry_run_outputs_a_single_preview_document() {
    let dir = setup_git_repo();

    let output = togi()
        .args([
            "check",
            "--all",
            "--format",
            "json",
            "--dry-run",
            "--test-cmd",
            "true",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid dry-run JSON output: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(value["kind"], "dry_run");
    assert_eq!(value["dry_run"], true);
    assert_eq!(
        value["planned_total"],
        value["mutations"].as_array().unwrap().len()
    );
    assert!(value.get("mutation_score").is_none());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Scanning all 1 supported files..."),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!dir.path().join(".togi-cache").exists());
}

#[test]
fn check_all_dry_run_max_per_run_preview_is_bounded() {
    let dir = setup_git_repo();
    // Eleven comparison expressions provide more candidate mutations than the cap.
    fs::write(
        dir.path().join("main.go"),
        r#"package main

func compare0(a, b int) bool { return a > b }
func compare1(a, b int) bool { return a > b }
func compare2(a, b int) bool { return a > b }
func compare3(a, b int) bool { return a > b }
func compare4(a, b int) bool { return a > b }
func compare5(a, b int) bool { return a > b }
func compare6(a, b int) bool { return a > b }
func compare7(a, b int) bool { return a > b }
func compare8(a, b int) bool { return a > b }
func compare9(a, b int) bool { return a > b }
func compare10(a, b int) bool { return a > b }
"#,
    )
    .unwrap();

    let output = togi()
        .args([
            "check",
            "--all",
            "--dry-run",
            "--max-per-run",
            "10",
            "--format",
            "json",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid capped dry-run JSON output: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(value["kind"], "dry_run");
    assert_eq!(value["dry_run"], true);
    assert_eq!(value["planned_total"], 10);
    assert_eq!(value["mutations"].as_array().unwrap().len(), 10);
    assert!(value.get("mutation_score").is_none());
    assert!(
        !dir.path().join(".togi-cache").exists(),
        "a dry run must not start a mutation run"
    );
}

#[test]
fn check_dry_run_rejects_json_report_and_preserves_existing_report() {
    for has_mutations in [false, true] {
        let dir = setup_git_repo();
        if !has_mutations {
            assert_command_success(
                std::process::Command::new("git")
                    .args(["add", "."])
                    .current_dir(dir.path())
                    .output()
                    .unwrap(),
                "commit no-diff setup",
            );
            assert_command_success(
                std::process::Command::new("git")
                    .args(["commit", "-m", "second"])
                    .current_dir(dir.path())
                    .output()
                    .unwrap(),
                "commit no-diff setup",
            );
        }
        let report_path = dir.path().join("dry-run-report.json");
        let existing = "{\"kind\":\"mutation_report\",\"schema_version\":1}\n";
        fs::write(&report_path, existing).unwrap();

        let output = togi()
            .args([
                "check",
                "--base",
                "HEAD",
                "--format",
                "json",
                "--dry-run",
                "--json-report",
                report_path.to_str().unwrap(),
                "--test-cmd",
                "true",
            ])
            .current_dir(dir.path())
            .output()
            .unwrap();

        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cannot be used with"),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(fs::read_to_string(&report_path).unwrap(), existing);
    }
}

#[test]
fn check_json_dry_run_ignores_non_utf8_diff_payload() {
    let dir = setup_git_repo();
    let payload = dir.path().join("payload.txt");
    fs::write(&payload, b"before\n").unwrap();
    assert_command_success(
        std::process::Command::new("git")
            .args(["add", "payload.txt"])
            .current_dir(dir.path())
            .output()
            .unwrap(),
        "stage payload",
    );
    assert_command_success(
        std::process::Command::new("git")
            .args(["commit", "-m", "add payload"])
            .current_dir(dir.path())
            .output()
            .unwrap(),
        "commit payload",
    );
    fs::write(payload, b"\xff\n").unwrap();

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--dry-run",
            "--test-cmd",
            "true",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let mutations = value["mutations"].as_array().unwrap();
    assert!(!mutations.is_empty());
    assert!(
        mutations
            .iter()
            .all(|mutation| mutation["file"] == "main.go")
    );
}

#[test]
fn check_format_json_no_mutations_outputs_an_empty_report() {
    let dir = setup_git_repo();
    assert_command_success(
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap(),
        "commit no-diff setup",
    );
    assert_command_success(
        std::process::Command::new("git")
            .args(["commit", "-m", "second"])
            .current_dir(dir.path())
            .output()
            .unwrap(),
        "commit no-diff setup",
    );
    let report_path = dir.path().join("empty-report.json");

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--json-report",
            report_path.to_str().unwrap(),
            "--test-cmd",
            "true",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid empty-report JSON output: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(value["total"], 0);
    assert_eq!(value["planned_total"], 0);
    assert_eq!(value["mutation_score"], 100.0);
    assert_eq!(value["mutations"], serde_json::json!([]));
    assert_eq!(value["kind"], "mutation_report");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(output.stdout, fs::read(&report_path).unwrap());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("No changes found"), "stderr: {stderr}");
    assert!(
        stderr.contains("selected diff is empty; that is normal for a clean clone, not an error."),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("togi check --all --dry-run --max-per-run 10"),
        "stderr: {stderr}"
    );
    for expected in [
        "After a supported source change, run `togi check`, or `togi check --base <ref>` for another intended base.",
        "`--all` and `--base` are alternatives; do not combine them.",
        "If command detection is ambiguous, run `togi init` from the repository root.",
    ] {
        assert!(stderr.contains(expected), "stderr: {stderr}");
    }
    assert!(
        !dir.path().join(".togi-cache").exists(),
        "an empty diff must not start a mutation run"
    );
}

#[test]
fn check_format_json_dry_run_empty_diff_keeps_stdout_machine_readable() {
    let dir = setup_git_repo();
    assert_command_success(
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap(),
        "commit no-diff setup",
    );
    assert_command_success(
        std::process::Command::new("git")
            .args(["commit", "-m", "second"])
            .current_dir(dir.path())
            .output()
            .unwrap(),
        "commit no-diff setup",
    );

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--dry-run",
            "--test-cmd",
            "true",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid empty dry-run JSON output: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(
        value,
        serde_json::json!({
            "kind": "dry_run",
            "dry_run": true,
            "planned_total": 0,
            "mutations": [],
        })
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    for expected in [
        "No changes found",
        "selected diff is empty; that is normal for a clean clone, not an error.",
        "togi check --all --dry-run --max-per-run 10",
        "After a supported source change, run `togi check`, or `togi check --base <ref>` for another intended base.",
        "`--all` and `--base` are alternatives; do not combine them.",
        "If command detection is ambiguous, run `togi init` from the repository root.",
    ] {
        assert!(stderr.contains(expected), "stderr: {stderr}");
    }
    assert!(
        !dir.path().join(".togi-cache").exists(),
        "an empty diff must not start a mutation run"
    );
}

#[test]
fn check_format_json_post_generation_no_mutations_outputs_empty_report() {
    let dir = setup_git_repo();
    let main_go = dir.path().join("main.go");
    fs::write(
        &main_go,
        "package main\n\nconst name = \"before\"\n\nfunc main() {}\n",
    )
    .unwrap();
    assert_command_success(
        std::process::Command::new("git")
            .args(["add", "main.go"])
            .current_dir(dir.path())
            .output()
            .unwrap(),
        "stage no-candidate baseline",
    );
    assert_command_success(
        std::process::Command::new("git")
            .args(["commit", "-m", "no-candidate baseline"])
            .current_dir(dir.path())
            .output()
            .unwrap(),
        "commit no-candidate baseline",
    );
    fs::write(
        &main_go,
        "package main\n\nconst name = \"after\"\n\nfunc main() {}\n",
    )
    .unwrap();
    let report_path = dir.path().join("post-generation-empty-report.json");

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--json-report",
            report_path.to_str().unwrap(),
            "--operators",
            "string_to_empty",
            "--test-cmd",
            "true",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid post-generation empty-report JSON output: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(value["total"], 0);
    assert_eq!(value["planned_total"], 0);
    assert_eq!(value["mutations"], serde_json::json!([]));
    assert_eq!(value["kind"], "mutation_report");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(output.stdout, fs::read(&report_path).unwrap());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No mutations generated"),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("No changes found"), "stderr: {stderr}");
}

#[test]
fn check_all_no_supported_files_json_outputs_an_empty_report() {
    let dir = setup_git_repo();

    let output = togi()
        .args([
            "check",
            "--all",
            "--path",
            "go.mod",
            "--format",
            "json",
            "--test-cmd",
            "true",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(output.status.success());
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid no-supported-files JSON output: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(value["total"], 0);
    assert_eq!(value["mutations"], serde_json::json!([]));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("No supported source files found"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
#[test]
fn check_aborts_before_mutations_when_baseline_test_fails() {
    let dir = setup_git_repo();

    let output = togi()
        .args([
            "check",
            "--all",
            "--path",
            "main.go",
            "--max-per-run",
            "1",
            "--no-schemata",
            "--test-cmd",
            "false",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
            "--format",
            "json",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let failure: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|error| panic!("invalid suite-failure JSON: {error}\nstdout:\n{stdout}"));
    assert_eq!(failure["kind"], "suite_failure");
    assert_eq!(failure["phase"], "test");
    assert_eq!(failure["command"], serde_json::json!(["false"]));
    assert_eq!(failure["outcome"], "failed");
    assert!(stderr.contains("baseline test command failed (`false`)"));
    assert!(!stderr.contains("Running 1 mutations"));
    assert!(failure.get("mutations").is_none());
    assert!(failure.get("mutation_score").is_none());
    assert!(
        !dir.path().join(".togi-cache").exists(),
        "baseline failure must not create mutation cache entries"
    );
}

#[cfg(unix)]
#[test]
fn check_schemata_cannot_bypass_a_source_sensitive_baseline_failure() {
    let dir = setup_git_repo();
    let test_cmd = format!(
        "sh -c {}",
        shell_quote_text("if grep -Fq 'if a > b' main.go; then exit 1; fi")
    );

    let output = togi()
        .args([
            "check",
            "--all",
            "--path",
            "main.go",
            "--max-per-run",
            "1",
            "--operators",
            "gt_to_gte",
            "--test-cmd",
            &test_cmd,
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
            "--format",
            "json",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("baseline test command failed"));
    assert!(!stderr.contains("Running 1 mutations"));
    assert!(!stdout.contains("\"killed\""));
    assert!(!stdout.contains("mutation_score"));
    assert!(!dir.path().join(".togi-cache").exists());
}

#[cfg(unix)]
#[test]
fn check_failing_baseline_does_not_accept_a_preseeded_exact_cache_verdict() {
    let dir = setup_git_repo();
    let log = dir.path().join("cache-baseline.log");
    let test_cmd = format!(
        "sh -c {}",
        shell_quote_text(
            "if [ \"$TOGI_BASELINE_MODE\" = fail ] && grep -Fq 'if a > b' main.go; then echo baseline-failed >> \"$TOGI_TEST_LOG\"; exit 1; fi; if grep -Fq 'if a > b' main.go; then echo baseline-pass >> \"$TOGI_TEST_LOG\"; else echo mutant >> \"$TOGI_TEST_LOG\"; fi"
        )
    );

    let seeded = togi()
        .args([
            "check",
            "--all",
            "--path",
            "main.go",
            "--max-per-run",
            "1",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--test-cmd",
            &test_cmd,
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
            "--format",
            "json",
        ])
        .env("TOGI_BASELINE_MODE", "pass")
        .env("TOGI_TEST_LOG", &log)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        seeded.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&seeded.stdout),
        String::from_utf8_lossy(&seeded.stderr)
    );
    assert!(
        dir.path().join(".togi-cache").exists(),
        "the first run must seed an exact cache verdict"
    );

    let output = togi()
        .args([
            "check",
            "--all",
            "--path",
            "main.go",
            "--max-per-run",
            "1",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--test-cmd",
            &test_cmd,
            "--no-incremental-history",
            "--fail-under",
            "0",
            "--format",
            "json",
        ])
        .env("TOGI_BASELINE_MODE", "fail")
        .env("TOGI_TEST_LOG", &log)
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("baseline test command failed"));
    assert!(!stderr.contains("Running 1 mutations"));
    assert!(!stdout.contains("\"killed\""));
    assert!(!stdout.contains("mutation_score"));
    assert_eq!(
        fs::read_to_string(log).unwrap().lines().collect::<Vec<_>>(),
        vec!["baseline-pass", "mutant", "baseline-failed"]
    );
}

#[cfg(unix)]
#[test]
fn check_configured_build_classifies_mutant_build_failure_before_test() {
    let dir = setup_git_repo();
    let fake_bin = dir.path().join("fake-go-bin");
    let build_marker = dir.path().join("build_ran.marker");
    let test_marker = dir.path().join("test_ran.marker");
    let path = fake_go_path_env(&fake_bin);
    let fake_go = fake_bin.join("go");
    fs::write(
        &fake_go,
        format!(
            "#!/bin/sh\nif grep -q '>=' main.go; then\n    touch {}\n    exit 1\nfi\n",
            shell_quote(&build_marker)
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&fake_go).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_go, permissions).unwrap();
    let test_cmd = format!(
        "sh -c {}",
        shell_quote_text(&format!(
            "if grep -q '>=' main.go; then touch {}; fi",
            shell_quote(&test_marker)
        ))
    );

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--test-cmd",
            &test_cmd,
            "--build-cmd",
            "go test -c -vet=off -o /dev/null ./...",
            "--operators",
            "gt_to_gte",
            "--max-per-run",
            "1",
            "--no-schemata",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
            "--format",
            "json",
        ])
        .env("PATH", path)
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["build_errors"], 1);
    assert_eq!(report["killed"], 0);
    assert_eq!(
        report["build_command"],
        serde_json::json!(["go", "test", "-c", "-vet=off", "-o", "/dev/null", "./..."])
    );
    assert!(build_marker.exists(), "configured build check did not run");
    assert!(
        !test_marker.exists(),
        "test command ran after configured build failure"
    );
}

#[cfg(unix)]
#[test]
fn check_detected_build_suggestion_does_not_pre_filter_mutants() {
    let dir = setup_git_repo();
    let fake_bin = dir.path().join("fake-go-bin");
    let build_marker = dir.path().join("build_ran.marker");
    let test_marker = dir.path().join("test_ran.marker");
    let path = fake_go_path_env(&fake_bin);
    let fake_go = fake_bin.join("go");
    fs::write(
        &fake_go,
        format!(
            "#!/bin/sh\nif grep -q '>=' main.go; then\n    touch {}\n    exit 1\nfi\n",
            shell_quote(&build_marker)
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&fake_go).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_go, permissions).unwrap();
    let test_cmd = format!(
        "sh -c {}",
        shell_quote_text(&format!(
            "if grep -q '>=' main.go; then touch {}; exit 1; fi",
            shell_quote(&test_marker)
        ))
    );

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--test-cmd",
            &test_cmd,
            "--operators",
            "gt_to_gte",
            "--max-per-run",
            "1",
            "--no-schemata",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
            "--format",
            "json",
        ])
        .env("PATH", path)
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["build_errors"], 0);
    assert_eq!(report["killed"], 1);
    assert_eq!(report["build_command"], serde_json::json!([]));
    assert!(
        !build_marker.exists(),
        "detected build suggestion must not execute"
    );
    assert!(
        test_marker.exists(),
        "mutant should run the configured test command"
    );
}

#[cfg(unix)]
#[test]
fn check_schemata_baselines_go_with_the_no_cache_argv() {
    let dir = setup_git_repo();
    let fake_bin = dir.path().join("fake-go-bin");
    let log = dir.path().join("fake-go.log");
    fs::write(
        dir.path().join("togi.toml"),
        r#"
[test]
command = ["go", "test", "./..."]
build_command = ["true"]
timeout = 5

[mutations]
schemata = true
"#,
    )
    .unwrap();

    let output = togi()
        .args([
            "check",
            "--all",
            "--path",
            "main.go",
            "--max-per-run",
            "1",
            "--operators",
            "gt_to_gte",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
            "--format",
            "json",
        ])
        .env("PATH", fake_go_path_env(&fake_bin))
        .env("TOGI_GO_LOG", &log)
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("baseline test command failed"));
    assert!(stderr.contains("-count=1"));
    assert_eq!(fs::read_to_string(log).unwrap(), "no-cache");
    assert!(!stderr.contains("Running 1 mutations"));
    assert!(!stdout.contains("\"killed\""));
    assert!(!stdout.contains("mutation_score"));
    assert!(!dir.path().join(".togi-cache").exists());
}

#[cfg(unix)]
#[test]
fn check_no_schemata_keeps_the_raw_go_test_argv() {
    let dir = setup_git_repo();
    let fake_bin = dir.path().join("fake-go-bin");
    let log = dir.path().join("fake-go.log");
    fs::write(
        dir.path().join("togi.toml"),
        r#"
[test]
command = ["go", "test", "./..."]
build_command = ["true"]
timeout = 5
"#,
    )
    .unwrap();

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--max-per-run",
            "1",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
            "--format",
            "json",
        ])
        .env("PATH", fake_go_path_env(&fake_bin))
        .env("TOGI_GO_LOG", &log)
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["survived"], 1);
    assert_eq!(fs::read_to_string(log).unwrap(), "rawraw");
}

#[test]
fn check_runs_mutations_after_a_passing_baseline() {
    let dir = setup_git_repo();

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--max-per-run",
            "1",
            "--no-schemata",
            "--test-cmd",
            "true",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
            "--format",
            "json",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Checking baseline test suites..."));
    assert!(stderr.contains("Running 1 mutations..."));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["total"], 1);
    assert_eq!(value["survived"], 1);
    assert!(dir.path().join(".togi-cache").exists());
}

#[cfg(unix)]
#[test]
fn check_calibration_reuses_the_passing_baseline_run() {
    let dir = setup_git_repo();
    let log = dir.path().join("baseline-runs.log");
    let test_cmd = format!(
        "sh -c {}",
        shell_quote_text(
            "if grep -q 'if a > b' main.go; then echo baseline >> \"$TOGI_TEST_LOG\"; else echo mutant >> \"$TOGI_TEST_LOG\"; fi"
        )
    );

    let output = togi()
        .args([
            "check",
            "--all",
            "--path",
            "main.go",
            "--max-per-run",
            "1",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--test-cmd",
            &test_cmd,
            "--calibrate-timeout",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
        ])
        .env("TOGI_TEST_LOG", &log)
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(log).unwrap().lines().collect::<Vec<_>>(),
        vec!["baseline", "mutant"]
    );
}

#[cfg(unix)]
#[test]
fn check_calibration_extends_a_slow_global_default_route() {
    let dir = setup_git_repo();
    fs::write(
        dir.path().join("togi.toml"),
        r#"
[test]
command = ["sh", "-c", "sleep 2"]
timeout = 1
calibrate_timeout = true
timeout_multiplier = 1.0
timeout_slack = 1
"#,
    )
    .unwrap();

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--max-per-run",
            "1",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["timeout"], 0);
    assert!(
        value["baseline_timing"]["calibrated_timeout_ms"]
            .as_u64()
            .is_some_and(|timeout| timeout >= 3_000)
    );
}

#[cfg(unix)]
#[test]
fn check_calibration_uses_the_slowest_default_route() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for args in [
        &["init"][..],
        &["config", "user.email", "test@example.com"],
        &["config", "user.name", "Test"],
    ] {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
    }
    fs::write(
        root.join("a.rs"),
        "pub fn compare(a: i32, b: i32) -> bool { a >= b }\n",
    )
    .unwrap();
    fs::write(
        root.join("z.go"),
        "package sample\n\nfunc compare(a, b int) bool { return a >= b }\n",
    )
    .unwrap();
    fs::write(
        root.join("togi.toml"),
        r#"
[test]
command = ["true"]
timeout = 1
calibrate_timeout = true
timeout_multiplier = 1.0
timeout_slack = 1

[test.languages.go]
command = ["sh", "-c", "sleep 2"]
"#,
    )
    .unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(root)
        .output()
        .unwrap();
    fs::write(
        root.join("a.rs"),
        "pub fn compare(a: i32, b: i32) -> bool { a > b }\n",
    )
    .unwrap();
    fs::write(
        root.join("z.go"),
        "package sample\n\nfunc compare(a, b int) bool { return a > b }\n",
    )
    .unwrap();

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--max-per-run",
            "2",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
        ])
        .current_dir(root)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["timeout"], 0);
    assert!(
        value["baseline_timing"]["calibrated_timeout_ms"]
            .as_u64()
            .is_some_and(|timeout| timeout >= 3_000)
    );
}

#[cfg(unix)]
#[test]
fn check_calibration_uses_a_slow_project_route_without_an_explicit_timeout() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    for args in [
        &["init"][..],
        &["config", "user.email", "test@example.com"],
        &["config", "user.name", "Test"],
    ] {
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
    }
    fs::create_dir_all(root.join("zproject")).unwrap();
    fs::write(
        root.join("a.rs"),
        "pub fn compare(a: i32, b: i32) -> bool { a >= b }\n",
    )
    .unwrap();
    fs::write(
        root.join("zproject/main.go"),
        "package sample\n\nfunc compare(a, b int) bool { return a >= b }\n",
    )
    .unwrap();
    fs::write(
        root.join("togi.toml"),
        r#"
[test]
command = ["true"]
timeout = 1
calibrate_timeout = true
timeout_multiplier = 1.0
timeout_slack = 1

[projects.api]
path = "zproject"

[projects.api.test]
command = ["sh", "-c", "sleep 2"]
"#,
    )
    .unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(root)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(root)
        .output()
        .unwrap();
    fs::write(
        root.join("a.rs"),
        "pub fn compare(a: i32, b: i32) -> bool { return a > b }\n",
    )
    .unwrap();
    fs::write(
        root.join("zproject/main.go"),
        "package sample\n\nfunc compare(a, b int) bool { return a > b }\n",
    )
    .unwrap();

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--max-per-run",
            "2",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
        ])
        .current_dir(root)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["timeout"], 0);
    assert!(
        value["baseline_timing"]["calibrated_timeout_ms"]
            .as_u64()
            .is_some_and(|timeout| timeout >= 3_000)
    );
}

#[cfg(unix)]
#[test]
fn check_baselines_global_language_and_project_routes() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    std::process::Command::new("git")
        .arg("init")
        .current_dir(root)
        .output()
        .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("services/api")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn compare(a: i32, b: i32) -> bool { a > b }\n",
    )
    .unwrap();
    fs::write(
        root.join("worker.go"),
        "package worker\n\nfunc compare(a, b int) bool { return a > b }\n",
    )
    .unwrap();
    fs::write(
        root.join("services/api/main.go"),
        "package api\n\nfunc compare(a, b int) bool { return a > b }\n",
    )
    .unwrap();
    fs::write(
        root.join("togi.toml"),
        r#"
[test]
command = ["sh", "-c", 'if grep -Fq "a > b" src/lib.rs; then echo global >> "$TOGI_TEST_LOG"; fi; exit 0']

[test.languages.go]
command = ["sh", "-c", 'if grep -Fq "a > b" worker.go; then echo language >> "$TOGI_TEST_LOG"; fi; exit 0']

[projects.api]
path = "services/api"

[projects.api.test]
command = ["sh", "-c", 'if grep -Fq "a > b" services/api/main.go; then echo project >> "$TOGI_TEST_LOG"; fi; exit 0']
"#,
    )
    .unwrap();
    let log = root.join("baseline-routes.log");

    let output = togi()
        .args([
            "check",
            "--all",
            "--max-per-run",
            "3",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
        ])
        .env("TOGI_TEST_LOG", &log)
        .current_dir(root)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let mut baseline_routes = fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    baseline_routes.sort();
    assert_eq!(baseline_routes, vec!["global", "language", "project"]);
}

#[cfg(unix)]
#[test]
fn check_aborts_before_mutation_work_when_a_later_project_baseline_fails() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    std::process::Command::new("git")
        .arg("init")
        .current_dir(root)
        .output()
        .unwrap();
    fs::create_dir_all(root.join("zproject")).unwrap();
    fs::write(
        root.join("a.rs"),
        "pub fn compare(a: i32, b: i32) -> bool { a > b }\n",
    )
    .unwrap();
    fs::write(
        root.join("m.go"),
        "package worker\n\nfunc compare(a, b int) bool { return a > b }\n",
    )
    .unwrap();
    fs::write(
        root.join("zproject/main.go"),
        "package api\n\nfunc compare(a, b int) bool { return a > b }\n",
    )
    .unwrap();
    fs::write(
        root.join("togi.toml"),
        r#"
[test]
command = ["sh", "-c", 'if grep -Fq "a > b" a.rs; then echo global >> "$TOGI_TEST_LOG"; fi; exit 0']

[test.languages.go]
command = ["sh", "-c", 'if grep -Fq "a > b" m.go; then echo language >> "$TOGI_TEST_LOG"; fi; exit 0']

[projects.api]
path = "zproject"

[projects.api.test]
command = ["sh", "-c", 'if grep -Fq "a > b" zproject/main.go; then echo project-failed >> "$TOGI_TEST_LOG"; exit 1; fi; exit 0']
"#,
    )
    .unwrap();
    let log = root.join("baseline-routes.log");

    let output = togi()
        .args([
            "check",
            "--all",
            "--max-per-run",
            "3",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
            "--format",
            "json",
        ])
        .env("TOGI_TEST_LOG", &log)
        .current_dir(root)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("baseline test command failed"), "{stderr}");
    assert!(!stderr.contains("Running 3 mutations"));
    assert!(!stdout.contains("\"killed\""));
    assert!(!stdout.contains("mutation_score"));
    assert!(!root.join(".togi-cache").exists());
    assert_eq!(
        fs::read_to_string(log).unwrap().lines().collect::<Vec<_>>(),
        vec!["global", "language", "project-failed"]
    );
}

#[test]
fn check_format_sarif_outputs_valid_sarif() {
    let dir = setup_git_repo();

    // `true` lets every mutant survive so the SARIF report has results.
    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "sarif",
            "--test-cmd",
            "true",
            "--no-schemata",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid SARIF output: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(value["version"], "2.1.0");
    let run = &value["runs"][0];
    assert_eq!(run["tool"]["driver"]["name"], "togi");
    assert!(run["invocations"][0]["properties"]["mutation_score"].is_number());

    let results = run["results"].as_array().unwrap();
    assert!(!results.is_empty(), "expected surviving mutant results");
    assert_eq!(results[0]["level"], "warning");
    let location = &results[0]["locations"][0]["physicalLocation"];
    assert_eq!(location["artifactLocation"]["uri"], "main.go");
    assert!(location["region"]["startLine"].as_u64().unwrap() >= 1);
}

#[cfg(unix)]
#[test]
fn check_aborts_before_report_when_baseline_build_fails() {
    let dir = setup_git_repo();

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--test-cmd",
            "true",
            "--build-cmd",
            "false",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--max-per-run",
            "1",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(2),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("baseline build command failed (`false`)"));
    assert!(!stderr.contains("Running 1 mutations"));
    assert!(!stdout.contains("\"killed\""));
    assert!(!stdout.contains("mutation_score"));
    assert!(!dir.path().join(".togi-cache").exists());
}

#[cfg(unix)]
#[test]
fn check_fresh_build_error_fails_default_gate_with_diagnostics() {
    let dir = setup_git_repo();
    let expected = tempfile::NamedTempFile::new().unwrap();
    fs::write(
        expected.path(),
        fs::read(dir.path().join("main.go")).unwrap(),
    )
    .unwrap();
    let failing_command = format!(
        "{} not-a-togi-command",
        shell_quote(&assert_cmd::cargo::cargo_bin("togi"))
    );
    let build_cmd = format!(
        "sh -c {}",
        shell_quote_text(&format!(
            "if cmp -s main.go {}; then exit 0; else exec {failing_command}; fi",
            shell_quote(expected.path())
        ))
    );

    togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--test-cmd",
            "true",
            "--build-cmd",
            &build_cmd,
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--max-per-run",
            "1",
            "--jobs",
            "1",
            "--force-rerun",
            "--no-incremental-history",
        ])
        .current_dir(dir.path())
        .assert()
        .code(1)
        .stdout(predicate::str::contains(
            "Results: 0 killed, 0 survived, 0 timeout, 1 build errors",
        ))
        .stdout(predicate::str::contains("Build error diagnostics:"))
        .stdout(predicate::str::contains("build_command"))
        .stdout(predicate::str::contains("not-a-togi-command"));
}

#[cfg(unix)]
#[test]
fn check_fresh_timeout_fails_default_gate_with_or_without_baseline() {
    for with_baseline in [false, true] {
        let dir = setup_git_repo();
        if with_baseline {
            write_baseline(dir.path(), 0, 1);
        }
        let expected = tempfile::NamedTempFile::new().unwrap();
        fs::write(
            expected.path(),
            fs::read(dir.path().join("main.go")).unwrap(),
        )
        .unwrap();
        let test_cmd = format!(
            "sh -c {}",
            shell_quote_text(&format!(
                "if cmp -s main.go {}; then exit 0; else sleep 2; fi",
                shell_quote(expected.path())
            ))
        );

        let mut command = togi();
        command
            .args([
                "check",
                "--base",
                "HEAD",
                "--format",
                "json",
                "--test-cmd",
                &test_cmd,
                "--timeout",
                "1",
                "--no-schemata",
                "--operators",
                "gt_to_gte",
                "--max-per-run",
                "1",
                "--jobs",
                "1",
                "--force-rerun",
                "--no-incremental-history",
            ])
            .current_dir(dir.path());
        if with_baseline {
            command.arg("--check-baseline");
        }
        let output = command.output().unwrap();

        assert_eq!(
            output.status.code(),
            Some(1),
            "with_baseline={with_baseline}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if with_baseline {
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                !stderr.contains("warning: no baseline found"),
                "baseline should load: {stderr}"
            );
            assert!(
                !stderr.contains("Mutation score regression detected!"),
                "fresh timeout should fail independently of baseline regression: {stderr}"
            );
        }
        let report: serde_json::Value =
            serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
                panic!(
                    "invalid JSON report: {error}\nstdout:\n{}",
                    String::from_utf8_lossy(&output.stdout)
                )
            });
        assert_eq!(report["total"], 1);
        assert_eq!(report["survived"], 0);
        assert_eq!(report["timeout"], 1);
        assert_eq!(report["build_errors"], 0);
        assert_eq!(report["mutation_score"], 0.0);
        assert_eq!(report["mutations"][0]["result"], "timeout");
        assert_eq!(report["mutations"][0]["execution"]["state"], "executed");
    }
}

fn write_baseline(dir: &Path, killed: usize, total: usize) {
    fs::write(
        dir.join(".togi-baseline"),
        format!(
            r#"{{
  "files": {{
    "main.go": {{
      "killed": {killed},
      "total": {total}
    }}
  }},
  "killed": {killed},
  "total": {total}
}}"#
        ),
    )
    .unwrap();
}

#[test]
fn check_baseline_allows_existing_survivors() {
    let dir = setup_git_repo();
    write_baseline(dir.path(), 0, 1);

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--test-cmd",
            "true",
            "--check-baseline",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "baseline check failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid JSON output: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert!(value["survived"].as_u64().unwrap() > 0);
    let survivors = value["mutations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|mutation| mutation["result"] == "survived")
        .collect::<Vec<_>>();
    assert!(!survivors.is_empty());
    assert!(
        survivors
            .iter()
            .all(|mutation| mutation["baseline_status"] == "non_comparable")
    );
}

#[test]
fn check_baseline_annotates_historic_survivors_in_a_single_json_document() {
    let dir = setup_git_repo();
    let common = [
        "check",
        "--base",
        "HEAD",
        "--format",
        "json",
        "--test-cmd",
        "true",
        "--no-schemata",
        "--operators",
        "gt_to_gte",
        "--max-per-run",
        "1",
        "--jobs",
        "1",
        "--force-rerun",
        "--no-incremental-history",
        "--fail-under",
        "0",
    ];

    let saved = togi()
        .args(common)
        .arg("--save-baseline")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        saved.status.success(),
        "baseline save failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&saved.stdout),
        String::from_utf8_lossy(&saved.stderr)
    );
    let baseline: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.path().join(".togi-baseline")).unwrap()).unwrap();
    assert_eq!(baseline["mutant_snapshot"]["version"], 1);

    let checked = togi()
        .args(common)
        .arg("--check-baseline")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "baseline check failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&checked.stdout).unwrap_or_else(|e| {
        panic!(
            "redirected check-baseline JSON was not one document: {e}\nstdout: {}",
            String::from_utf8_lossy(&checked.stdout)
        )
    });
    let survivors = report["mutations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|mutation| mutation["result"] == "survived")
        .collect::<Vec<_>>();
    assert_eq!(survivors.len(), 1);
    assert_eq!(survivors[0]["baseline_status"], "historic");
}

#[test]
fn check_baseline_annotates_historic_survivors_in_terminal() {
    let dir = setup_git_repo();
    let common = [
        "check",
        "--base",
        "HEAD",
        "--test-cmd",
        "true",
        "--no-schemata",
        "--operators",
        "gt_to_gte",
        "--max-per-run",
        "1",
        "--jobs",
        "1",
        "--force-rerun",
        "--no-incremental-history",
        "--fail-under",
        "0",
    ];

    let saved = togi()
        .args(common)
        .arg("--save-baseline")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        saved.status.success(),
        "baseline save failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&saved.stdout),
        String::from_utf8_lossy(&saved.stderr)
    );

    let checked = togi()
        .args(common)
        .arg("--check-baseline")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        checked.status.success(),
        "baseline check failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&checked.stdout),
        String::from_utf8_lossy(&checked.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&checked.stdout)
            .matches("Baseline: historic")
            .count(),
        1
    );
}

#[test]
fn check_baseline_annotations_reach_github_html_sarif_and_pr_comment() {
    let dir = setup_git_repo();
    let common = [
        "check",
        "--base",
        "HEAD",
        "--test-cmd",
        "true",
        "--no-schemata",
        "--operators",
        "gt_to_gte",
        "--max-per-run",
        "1",
        "--jobs",
        "1",
        "--force-rerun",
        "--no-incremental-history",
        "--fail-under",
        "0",
    ];

    let saved = togi()
        .args(common)
        .arg("--save-baseline")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        saved.status.success(),
        "baseline save failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&saved.stdout),
        String::from_utf8_lossy(&saved.stderr)
    );

    let github = togi()
        .args(common)
        .args(["--format", "github", "--check-baseline"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        github.status.success(),
        "GitHub report failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&github.stdout),
        String::from_utf8_lossy(&github.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&github.stdout)
            .matches("[baseline: historic]")
            .count(),
        1
    );

    let sarif = togi()
        .args(common)
        .args(["--format", "sarif", "--check-baseline"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        sarif.status.success(),
        "SARIF report failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&sarif.stdout),
        String::from_utf8_lossy(&sarif.stderr)
    );
    let sarif: serde_json::Value = serde_json::from_slice(&sarif.stdout).unwrap();
    assert_eq!(
        sarif["runs"][0]["results"][0]["properties"]["baseline_status"],
        "historic"
    );

    let html = togi()
        .args(common)
        .args(["--format", "html", "--check-baseline"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        html.status.success(),
        "HTML report failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&html.stdout),
        String::from_utf8_lossy(&html.stderr)
    );
    let html = fs::read_to_string(dir.path().join("togi-report.html")).unwrap();
    assert!(html.contains("<th>Baseline</th>"));
    assert!(html.contains("<td>historic</td>"));

    let pr_comment = dir.path().join("togi-pr-comment.md");
    let pr_comment_run = togi()
        .args(common)
        .arg("--check-baseline")
        .arg("--pr-comment")
        .arg(&pr_comment)
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(
        pr_comment_run.status.success(),
        "PR comment report failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&pr_comment_run.stdout),
        String::from_utf8_lossy(&pr_comment_run.stderr)
    );
    let pr_comment = fs::read_to_string(pr_comment).unwrap();
    assert!(pr_comment.contains("| File | Line | Operator | Description | Baseline |"));
    assert!(pr_comment.contains(" | historic |"));
}

#[test]
fn check_baseline_keeps_json_report_when_baseline_is_invalid() {
    let dir = setup_git_repo();
    fs::write(dir.path().join(".togi-baseline"), "not json").unwrap();

    let output = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--test-cmd",
            "true",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--max-per-run",
            "1",
            "--jobs",
            "1",
            "--force-rerun",
            "--no-incremental-history",
            "--check-baseline",
        ])
        .current_dir(dir.path())
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("invalid baseline"),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "invalid baseline must still leave one report document on stdout: {e}\nstdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    let survivors = report["mutations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|mutation| mutation["result"] == "survived")
        .collect::<Vec<_>>();
    assert_eq!(survivors.len(), 1);
    assert!(survivors[0].get("baseline_status").is_none());
}

#[test]
fn check_baseline_fails_on_regression() {
    let dir = setup_git_repo();
    write_baseline(dir.path(), 1, 1);

    togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--test-cmd",
            "true",
            "--check-baseline",
        ])
        .current_dir(dir.path())
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "Mutation score regression detected",
        ));
}

#[test]
fn check_baseline_still_honors_fail_under() {
    let dir = setup_git_repo();
    write_baseline(dir.path(), 0, 1);

    togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--test-cmd",
            "true",
            "--check-baseline",
            "--fail-under",
            "1",
        ])
        .current_dir(dir.path())
        .assert()
        .code(1)
        .stderr(predicate::str::contains("below --fail-under threshold"))
        .stderr(predicate::str::contains("Fail-under gate score"))
        .stderr(predicate::str::contains("fresh-only"));
}

#[cfg(unix)]
#[test]
fn check_fail_under_exact_cache_matches_cold_gate_with_fresh_only_json_score() {
    let dir = setup_git_repo();
    let test_cmd = format!(
        "sh -c {}",
        shell_quote_text("if grep -Fq 'a >= b' main.go; then exit 1; fi")
    );
    let args = [
        "check",
        "--base",
        "HEAD",
        "--format",
        "json",
        "--test-cmd",
        test_cmd.as_str(),
        "--no-schemata",
        "--operators",
        "gt_to_gte",
        "--max-per-run",
        "1",
        "--jobs",
        "1",
        "--no-incremental-history",
        "--fail-under",
        "100",
    ];

    let cold = togi().args(args).current_dir(dir.path()).output().unwrap();
    assert!(
        cold.status.success(),
        "cold gate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&cold.stdout),
        String::from_utf8_lossy(&cold.stderr)
    );
    let cold_report: serde_json::Value = serde_json::from_slice(&cold.stdout).unwrap();
    assert_eq!(cold_report["tested"], 1);
    assert_eq!(cold_report["mutation_score"], 100.0);

    let warm = togi().args(args).current_dir(dir.path()).output().unwrap();
    assert!(
        warm.status.success(),
        "warm exact-cache gate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&warm.stdout),
        String::from_utf8_lossy(&warm.stderr)
    );
    let warm_report: serde_json::Value = serde_json::from_slice(&warm.stdout).unwrap();
    assert_eq!(warm_report["tested"], 0);
    assert_eq!(warm_report["mutation_score"], 0.0);
    assert_eq!(
        warm_report["mutations"][0]["execution"]["state"],
        "exact_cache"
    );
}

#[cfg(unix)]
#[test]
fn check_fail_under_fails_cached_survivors_and_excludes_incremental_history() {
    let survivor_dir = setup_git_repo();
    let survivor_seed = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--test-cmd",
            "true",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--max-per-run",
            "1",
            "--jobs",
            "1",
            "--no-incremental-history",
            "--fail-under",
            "0",
        ])
        .current_dir(survivor_dir.path())
        .output()
        .unwrap();
    assert!(
        survivor_seed.status.success(),
        "survivor seed failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&survivor_seed.stdout),
        String::from_utf8_lossy(&survivor_seed.stderr)
    );
    let survivor_warm = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--test-cmd",
            "true",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--max-per-run",
            "1",
            "--jobs",
            "1",
            "--no-incremental-history",
            "--fail-under",
            "1",
        ])
        .current_dir(survivor_dir.path())
        .output()
        .unwrap();
    assert_eq!(survivor_warm.status.code(), Some(1));
    let survivor_report: serde_json::Value = serde_json::from_slice(&survivor_warm.stdout).unwrap();
    assert_eq!(survivor_report["tested"], 0);
    assert_eq!(survivor_report["mutation_score"], 0.0);
    assert_eq!(survivor_report["mutations"][0]["result"], "survived");
    assert_eq!(
        survivor_report["mutations"][0]["execution"]["state"],
        "exact_cache"
    );
    assert!(String::from_utf8_lossy(&survivor_warm.stderr).contains("Fail-under gate score"));

    let history_dir = setup_git_repo();
    let killed_test_cmd = format!(
        "sh -c {}",
        shell_quote_text("if grep -Fq 'a >= b' main.go; then exit 1; fi")
    );
    let history_seed = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--test-cmd",
            killed_test_cmd.as_str(),
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--max-per-run",
            "1",
            "--jobs",
            "1",
            "--fail-under",
            "0",
        ])
        .current_dir(history_dir.path())
        .output()
        .unwrap();
    assert!(
        history_seed.status.success(),
        "history seed failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&history_seed.stdout),
        String::from_utf8_lossy(&history_seed.stderr)
    );
    let cache_dir = history_dir.path().join(".togi-cache");
    for entry in fs::read_dir(&cache_dir).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file()
            && entry.file_name().to_string_lossy() != "history.json"
        {
            fs::remove_file(entry.path()).unwrap();
        }
    }

    let history_warm = togi()
        .args([
            "check",
            "--base",
            "HEAD",
            "--format",
            "json",
            "--test-cmd",
            killed_test_cmd.as_str(),
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--max-per-run",
            "1",
            "--jobs",
            "1",
            "--fail-under",
            "100",
        ])
        .current_dir(history_dir.path())
        .output()
        .unwrap();
    assert_eq!(history_warm.status.code(), Some(1));
    let history_report: serde_json::Value = serde_json::from_slice(&history_warm.stdout).unwrap();
    assert_eq!(history_report["tested"], 0);
    assert_eq!(history_report["mutation_score"], 0.0);
    assert_eq!(history_report["mutations"][0]["result"], "killed");
    assert_eq!(
        history_report["mutations"][0]["execution"]["state"],
        "incremental_history"
    );
    assert!(String::from_utf8_lossy(&history_warm.stderr).contains("Fail-under gate score"));
}

#[test]
fn check_skips_baseline_actions_without_fresh_cache_or_history_evidence() {
    for (reuse_source, expected_state) in [
        ("exact cache", "exact_cache"),
        ("incremental history", "incremental_history"),
    ] {
        let dir = setup_git_repo();
        let baseline_path = dir.path().join(".togi-baseline");

        let fresh = togi()
            .args([
                "check",
                "--base",
                "HEAD",
                "--format",
                "json",
                "--test-cmd",
                "true",
                "--no-schemata",
                "--operators",
                "gt_to_gte",
                "--max-per-run",
                "1",
                "--jobs",
                "1",
                "--fail-under",
                "0",
                "--save-baseline",
            ])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            fresh.status.success(),
            "fresh baseline run failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&fresh.stdout),
            String::from_utf8_lossy(&fresh.stderr)
        );
        let baseline_before = fs::read(&baseline_path).unwrap();
        let saved: serde_json::Value = serde_json::from_slice(&baseline_before).unwrap();
        assert_eq!(saved["total"], 1, "{reuse_source} fixture needs a baseline");

        for action in ["--save-baseline", "--check-baseline"] {
            if reuse_source == "incremental history" {
                let cache_dir = dir.path().join(".togi-cache");
                for entry in fs::read_dir(&cache_dir).unwrap() {
                    let entry = entry.unwrap();
                    if entry.file_type().unwrap().is_file()
                        && entry.file_name().to_string_lossy() != "history.json"
                    {
                        fs::remove_file(entry.path()).unwrap();
                    }
                }
                assert!(cache_dir.join("history.json").exists());
            }
            let reused = togi()
                .args([
                    "check",
                    "--base",
                    "HEAD",
                    "--format",
                    "json",
                    "--test-cmd",
                    "true",
                    "--no-schemata",
                    "--operators",
                    "gt_to_gte",
                    "--max-per-run",
                    "1",
                    "--jobs",
                    "1",
                ])
                .arg(action)
                .current_dir(dir.path())
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&reused.stderr);
            assert_eq!(
                reused.status.code(),
                Some(1),
                "{reuse_source} {action} verdict must retain normal no-baseline survivor behavior\nstdout:\n{}\nstderr:\n{stderr}",
                String::from_utf8_lossy(&reused.stdout),
            );
            assert_eq!(
                stderr
                    .matches("Report has no complete fresh execution evidence; skipping baseline save/check.")
                    .count(),
                1,
                "{reuse_source} {action}: {stderr}"
            );
            assert!(
                !stderr.contains("Baseline saved to .togi-baseline"),
                "{stderr}"
            );
            assert!(
                !stderr.contains("Mutation score regression detected!"),
                "{stderr}"
            );
            assert_eq!(fs::read(&baseline_path).unwrap(), baseline_before);

            let report: serde_json::Value = serde_json::from_slice(&reused.stdout).unwrap();
            assert_eq!(
                report["mutations"][0]["execution"]["state"], expected_state,
                "{reuse_source} {action}: {report}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn check_skips_baseline_actions_for_mixed_fresh_and_reused_results() {
    for action in ["--save-baseline", "--check-baseline"] {
        let dir = setup_git_repo();
        fs::write(
            dir.path().join("history.go"),
            "package main\n\nfunc history(a, b int) bool {\n\treturn a > b\n}\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("fresh.go"),
            "package main\n\nfunc fresh(a, b int) bool {\n\treturn a > b\n}\n",
        )
        .unwrap();
        let state = tempfile::NamedTempFile::new().unwrap();
        fs::write(state.path(), "kill").unwrap();
        let test_cmd = format!(
            "sh -c {}",
            shell_quote_text(&format!(
                "if grep -Fq '>=' main.go history.go fresh.go; then test \"$(cat {})\" = survive; else exit 0; fi",
                shell_quote(state.path())
            ))
        );
        let baseline_path = dir.path().join(".togi-baseline");

        let initial = togi()
            .args([
                "check",
                "--all",
                "--format",
                "json",
                "--test-cmd",
                &test_cmd,
                "--no-schemata",
                "--operators",
                "gt_to_gte",
                "--max-per-run",
                "3",
                "--jobs",
                "1",
                "--fail-under",
                "0",
                "--save-baseline",
            ])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            initial.status.success(),
            "initial baseline run failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&initial.stdout),
            String::from_utf8_lossy(&initial.stderr)
        );
        assert!(
            String::from_utf8_lossy(&initial.stderr).contains("Baseline saved to .togi-baseline")
        );
        let initial_report: serde_json::Value = serde_json::from_slice(&initial.stdout).unwrap();
        assert_eq!(initial_report["mutation_score"].as_f64(), Some(100.0));
        let baseline_before = fs::read(&baseline_path).unwrap();
        let baseline: serde_json::Value = serde_json::from_slice(&baseline_before).unwrap();
        assert_eq!(baseline["killed"], 3);
        assert_eq!(baseline["total"], 3);
        fs::remove_dir_all(dir.path().join(".togi-cache")).unwrap();

        fs::write(state.path(), "survive").unwrap();
        for path in ["history.go", "main.go"] {
            let seeded = togi()
                .args([
                    "check",
                    "--all",
                    "--path",
                    path,
                    "--format",
                    "json",
                    "--test-cmd",
                    &test_cmd,
                    "--no-schemata",
                    "--operators",
                    "gt_to_gte",
                    "--max-per-run",
                    "1",
                    "--jobs",
                    "1",
                    "--fail-under",
                    "0",
                ])
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(
                seeded.status.success(),
                "seed {path} failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&seeded.stdout),
                String::from_utf8_lossy(&seeded.stderr)
            );

            if path == "history.go" {
                let cache_dir = dir.path().join(".togi-cache");
                for entry in fs::read_dir(&cache_dir).unwrap() {
                    let entry = entry.unwrap();
                    if entry.file_type().unwrap().is_file()
                        && entry.file_name().to_string_lossy() != "history.json"
                    {
                        fs::remove_file(entry.path()).unwrap();
                    }
                }
                assert!(cache_dir.join("history.json").exists());
            }
        }

        let mixed = togi()
            .args([
                "check",
                "--all",
                "--format",
                "json",
                "--test-cmd",
                &test_cmd,
                "--no-schemata",
                "--operators",
                "gt_to_gte",
                "--max-per-run",
                "3",
                "--jobs",
                "1",
            ])
            .arg(action)
            .current_dir(dir.path())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&mixed.stderr);
        assert_eq!(
            mixed.status.code(),
            Some(1),
            "mixed {action} must retain normal no-baseline survivor behavior\nstdout:\n{}\nstderr:\n{stderr}",
            String::from_utf8_lossy(&mixed.stdout)
        );
        assert_eq!(
            stderr
                .matches(
                    "Report has no complete fresh execution evidence; skipping baseline save/check."
                )
                .count(),
            1,
            "{action}: {stderr}"
        );
        assert!(
            !stderr.contains("Baseline saved to .togi-baseline"),
            "{stderr}"
        );
        assert!(
            !stderr.contains("Mutation score regression detected!"),
            "{stderr}"
        );
        assert_eq!(fs::read(&baseline_path).unwrap(), baseline_before);

        let report: serde_json::Value = serde_json::from_slice(&mixed.stdout).unwrap();
        assert_eq!(report["tested"].as_u64(), Some(1));
        assert_eq!(report["exact_cache_reused"].as_u64(), Some(1));
        assert_eq!(report["incremental_history_reused"].as_u64(), Some(1));
        assert_eq!(report["killed"].as_u64(), Some(0));
        assert_eq!(report["survived"].as_u64(), Some(3));
        assert_eq!(report["mutation_score"].as_f64(), Some(0.0));
        for (file, expected_state) in [
            ("main.go", "exact_cache"),
            ("history.go", "incremental_history"),
            ("fresh.go", "executed"),
        ] {
            let mutation = report["mutations"]
                .as_array()
                .unwrap()
                .iter()
                .find(|mutation| mutation["file"].as_str() == Some(file))
                .unwrap_or_else(|| panic!("missing {file} mutation: {report}"));
            assert_eq!(
                mutation["execution"]["state"], expected_state,
                "{action}: {report}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn interrupted_mutation_exits_130_without_a_report_or_side_effects() {
    let dir = setup_git_repo();
    let baseline_marker = dir.path().join("baseline-marker");
    let mutation_marker = dir.path().join("mutation-marker");
    let release = dir.path().join("mutation-release");
    let stdout_path = dir.path().join("togi-stdout.log");
    let stderr_path = dir.path().join("togi-stderr.log");
    let pr_comment_path = dir.path().join("togi-pr-comment.md");
    let baseline_path = dir.path().join(".togi-baseline");
    let cache_path = dir.path().join(".togi-cache");
    let test_script = dir.path().join("block-mutant.sh");
    fs::write(
        &test_script,
        format!(
            "#!/bin/sh\n\
             baseline={}\n\
             mutation={}\n\
             release={}\n\
             if grep -Fq 'if a > b' main.go; then\n\
             \tprintf baseline > \"$baseline\"\n\
             \texit 0\n\
             fi\n\
             printf mutation > \"$mutation\"\n\
             while [ ! -f \"$release\" ]; do\n\
             \tsleep 0.05\n\
             done\n",
            shell_quote(&baseline_marker),
            shell_quote(&mutation_marker),
            shell_quote(&release),
        ),
    )
    .unwrap();

    let stdout = fs::File::create(&stdout_path).unwrap();
    let stderr = fs::File::create(&stderr_path).unwrap();
    let test_command = format!("sh {}", shell_quote(&test_script));
    // foxguard: ignore[rs/no-command-injection]
    // The test launches the just-built `togi` binary from Cargo metadata.
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("togi"))
        .args([
            "check",
            "--all",
            "--path",
            "main.go",
            "--max-per-run",
            "1",
            "--no-schemata",
            "--operators",
            "gt_to_gte",
            "--test-cmd",
            &test_command,
            "--timeout",
            "5",
            "--force-rerun",
            "--no-incremental-history",
            "--fail-under",
            "0",
            "--format",
            "json",
            "--save-baseline",
            "--pr-comment",
            pr_comment_path
                .to_str()
                .expect("PR comment path should be utf-8"),
        ])
        .current_dir(dir.path())
        .process_group(0)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .unwrap();

    if !wait_for_path(&baseline_marker, Duration::from_secs(10))
        || !wait_for_path(&mutation_marker, Duration::from_secs(10))
    {
        send_signal_to_process_group(child.id(), libc::SIGKILL);
        let _ = child.wait();
        panic!(
            "baseline or mutation command did not start\nstdout:\n{}\nstderr:\n{}",
            read_log(&stdout_path),
            read_log(&stderr_path)
        );
    }

    send_signal_to_process_group(child.id(), libc::SIGINT);
    fs::write(&release, "").unwrap();
    let status = wait_for_child(&mut child, Duration::from_secs(10)).unwrap_or_else(|message| {
        send_signal_to_process_group(child.id(), libc::SIGKILL);
        let _ = child.wait();
        panic!(
            "{message}\nstdout:\n{}\nstderr:\n{}",
            read_log(&stdout_path),
            read_log(&stderr_path)
        );
    });

    assert_eq!(
        status.code(),
        Some(130),
        "interrupted mutation should exit 130\nstatus: {status:?}\nstdout:\n{}\nstderr:\n{}",
        read_log(&stdout_path),
        read_log(&stderr_path)
    );
    let stdout = read_log(&stdout_path);
    assert!(!stdout.contains("mutation_score"));
    assert!(!stdout.contains("\"total\""));
    assert!(!stdout.contains("\"killed\""));
    assert!(!cache_path.exists(), "interrupted mutation wrote a cache");
    assert!(
        !baseline_path.exists(),
        "interrupted mutation wrote a baseline"
    );
    assert!(
        !pr_comment_path.exists(),
        "interrupted mutation wrote a PR comment"
    );
}

#[cfg(unix)]
#[test]
fn interrupted_check_exits_130_without_writing_side_effects() {
    let dir = setup_git_repo();
    let marker = dir.path().join("test-command-started");
    let release = dir.path().join("test-command-release");
    let stdout_path = dir.path().join("togi-stdout.log");
    let stderr_path = dir.path().join("togi-stderr.log");
    let pr_comment_path = dir.path().join("togi-pr-comment.md");
    let baseline_path = dir.path().join(".togi-baseline");
    let cache_path = dir.path().join(".togi-cache");
    let slow_test = dir.path().join("slow-test.sh");

    fs::write(
        &slow_test,
        format!(
            "#!/bin/sh\n\
             marker={}\n\
             release={}\n\
             printf started > \"$marker\"\n\
             while [ ! -f \"$release\" ]; do\n\
             \tsleep 0.05\n\
             done\n",
            shell_quote(&marker),
            shell_quote(&release)
        ),
    )
    .unwrap();

    let stdout = fs::File::create(&stdout_path).unwrap();
    let stderr = fs::File::create(&stderr_path).unwrap();
    // foxguard: ignore[rs/no-command-injection]
    // The test launches the just-built `togi` binary from Cargo metadata.
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("togi"))
        .args([
            "check",
            "--base",
            "HEAD",
            "--test-cmd",
            &format!("sh {}", shell_quote(&slow_test)),
            "--timeout",
            "5",
            "--jobs",
            "1",
            "--save-baseline",
            "--pr-comment",
            pr_comment_path
                .to_str()
                .expect("PR comment path should be utf-8"),
        ])
        .current_dir(dir.path())
        .process_group(0)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .unwrap();

    if !wait_for_path(&marker, Duration::from_secs(10)) {
        send_signal_to_process_group(child.id(), libc::SIGKILL);
        let _ = child.wait();
        panic!(
            "test command did not start\nstdout:\n{}\nstderr:\n{}",
            read_log(&stdout_path),
            read_log(&stderr_path)
        );
    }

    send_signal_to_process_group(child.id(), libc::SIGINT);
    let _ = wait_for_file_contains(&stderr_path, "Interrupted", Duration::from_secs(1));
    fs::write(&release, "").unwrap();

    let status = wait_for_child(&mut child, Duration::from_secs(10)).unwrap_or_else(|message| {
        send_signal_to_process_group(child.id(), libc::SIGKILL);
        let _ = child.wait();
        panic!(
            "{message}\nstdout:\n{}\nstderr:\n{}",
            read_log(&stdout_path),
            read_log(&stderr_path)
        );
    });

    assert_eq!(
        status.code(),
        Some(130),
        "interrupted check should exit 130\nstatus: {status:?}\nstdout:\n{}\nstderr:\n{}",
        read_log(&stdout_path),
        read_log(&stderr_path)
    );
    assert!(
        !baseline_path.exists(),
        "interrupted check wrote {}",
        baseline_path.display()
    );
    assert!(
        !pr_comment_path.exists(),
        "interrupted check wrote {}",
        pr_comment_path.display()
    );
    assert!(
        !cache_path.exists(),
        "interrupted check wrote {}",
        cache_path.display()
    );
    let stdout = read_log(&stdout_path);
    assert!(!stdout.contains("mutation_score"));
    assert!(!stdout.contains("\"killed\""));
}

#[cfg(unix)]
fn shell_quote(path: &Path) -> String {
    let value = path.to_str().expect("test path should be utf-8");
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn shell_quote_text(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(unix)]
fn fake_go_path_env(fake_bin: &Path) -> std::ffi::OsString {
    fs::create_dir_all(fake_bin).unwrap();
    let fake_go = fake_bin.join("go");
    fs::write(
        &fake_go,
        r#"#!/bin/sh
if [ "$1" = "test" ] && [ "$2" = "-count=1" ]; then
    printf no-cache >> "$TOGI_GO_LOG"
    exit 1
fi
printf raw >> "$TOGI_GO_LOG"
"#,
    )
    .unwrap();
    let mut permissions = fs::metadata(&fake_go).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_go, permissions).unwrap();

    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![fake_bin.to_path_buf()];
    paths.extend(std::env::split_paths(&inherited));
    std::env::join_paths(paths).unwrap()
}

#[cfg(unix)]
fn wait_for_path(path: &Path, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[cfg(unix)]
fn wait_for_file_contains(path: &Path, needle: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if fs::read_to_string(path).is_ok_and(|content| content.contains(needle)) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    false
}

#[cfg(unix)]
fn wait_for_child(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<std::process::ExitStatus, String> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return Ok(status);
        }
        if start.elapsed() >= timeout {
            return Err(format!("togi did not exit within {timeout:?}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn send_signal_to_process_group(pid: u32, signal: libc::c_int) {
    unsafe {
        let _ = libc::killpg(pid as libc::pid_t, signal);
    }
}

#[cfg(unix)]
fn read_log(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn check_help_lists_test_selection_flags() {
    togi()
        .args(["check", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--test-selection-file"));
}

fn jq_available() -> bool {
    std::process::Command::new("jq")
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

#[cfg(unix)]
fn action_install_run_script(action_path: &Path) -> String {
    let action_yml =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("action.yml")).unwrap();
    let action: serde_yaml::Value = serde_yaml::from_str(&action_yml).unwrap();
    let run = action
        .get("runs")
        .and_then(|runs| runs.get("steps"))
        .and_then(serde_yaml::Value::as_sequence)
        .and_then(|steps| {
            steps.iter().find(|step| {
                step.get("name")
                    .and_then(serde_yaml::Value::as_str)
                    .is_some_and(|name| name == "Install togi")
            })
        })
        .and_then(|step| step.get("run"))
        .and_then(serde_yaml::Value::as_str)
        .expect("Action is missing the Install togi shell script");
    run.replace(
        "${{ github.action_path }}",
        action_path
            .to_str()
            .expect("temporary action path must be valid UTF-8"),
    )
}

struct ActionHelperFixture {
    dir: TempDir,
    fake_togi: PathBuf,
    invocation_log: PathBuf,
    github_output: PathBuf,
    child_environment_log: PathBuf,
    report_path: PathBuf,
}

fn action_helper_fixture() -> ActionHelperFixture {
    let dir = TempDir::new().unwrap();
    let fake_togi = dir.path().join("fake-togi.sh");
    let invocation_log = dir.path().join("invocations.log");
    let github_output = dir.path().join("github-output");
    let report_path = dir.path().join("togi-report.json");
    let child_environment_log = dir.path().join("child-environment.log");
    fs::write(
        &fake_togi,
        r#"#!/usr/bin/env bash
set -u
if [[ -n "${FAKE_TOGI_ENV_LOG:-}" ]]; then
  printf 'TOGI_BASE=%s\n' "${TOGI_BASE-__unset__}" >> "$FAKE_TOGI_ENV_LOG"
  printf 'TOGI_TIMEOUT=%s\n' "${TOGI_TIMEOUT-__unset__}" >> "$FAKE_TOGI_ENV_LOG"
  printf 'TOGI_FORMAT=%s\n' "${TOGI_FORMAT-__unset__}" >> "$FAKE_TOGI_ENV_LOG"
  printf 'TOGI_TEST_CMD=%s\n' "${TOGI_TEST_CMD-__unset__}" >> "$FAKE_TOGI_ENV_LOG"
  printf 'TOGI_REPORT_PATH=%s\n' "${TOGI_REPORT_PATH-__unset__}" >> "$FAKE_TOGI_ENV_LOG"
  printf 'TOGI_BIN=%s\n' "${TOGI_BIN-__unset__}" >> "$FAKE_TOGI_ENV_LOG"
  printf 'TOGI_EXPECTED_VERSION=%s\n' "${TOGI_EXPECTED_VERSION-__unset__}" >> "$FAKE_TOGI_ENV_LOG"
  printf '%s\n' '--' >> "$FAKE_TOGI_ENV_LOG"
fi

args=("$@")
format=terminal
report_path=
for ((index = 0; index < ${#args[@]}; index++)); do
  case "${args[index]}" in
    --format)
      format="${args[$((index + 1))]}"
      ;;
    --json-report)
      report_path="${args[$((index + 1))]}"
      ;;
  esac
done
for arg in "${args[@]}"; do
  printf '<%s>\n' "$arg" >> "$FAKE_TOGI_LOG"
done
printf '%s\n' '--' >> "$FAKE_TOGI_LOG"

if [[ "${args[0]:-}" == "--version" ]]; then
  if [[ -n "${FAKE_TOGI_VERSION_STDERR:-}" ]]; then
    printf '%s\n' "$FAKE_TOGI_VERSION_STDERR" >&2
  fi
  printf 'togi %s\n' "${FAKE_TOGI_VERSION:-0.5.2}"
  exit "${FAKE_TOGI_VERSION_STATUS:-0}"
fi
if [[ "${args[0]:-}" == "help" && "${args[1]:-}" == "check" ]]; then
  if [[ "${FAKE_TOGI_SUPPORTS_JSON_REPORT:-1}" == "1" ]]; then
    printf '%s\n' '--json-report <PATH>'
  else
    printf '%s\n' 'check help without JSON sidecar support'
  fi
  exit 0
fi

status="${FAKE_TOGI_STATUS:-0}"
if [[ "$status" == "0" || "$status" == "1" ]] && [[ -n "$report_path" ]]; then
  printf '%s\n' "${FAKE_TOGI_JSON:?}" > "$report_path"
fi
if [[ "$format" == "json" ]]; then
  printf '%s\n' "${FAKE_TOGI_JSON:?}"
else
  printf 'review-format=%s\n' "$format"
fi
exit "$status"
"#,
    )
    .unwrap();
    let output = std::process::Command::new("bash")
        .args(["-c", "chmod +x \"$1\"", "--"])
        .arg(&fake_togi)
        .output()
        .unwrap();
    assert!(output.status.success());

    ActionHelperFixture {
        dir,
        fake_togi,
        invocation_log,
        github_output,
        child_environment_log,
        report_path,
    }
}

struct ActionHelperRun<'a> {
    base: Option<&'a str>,
    timeout: Option<&'a str>,
    format: Option<&'a str>,
    test_cmd: Option<&'a str>,
    status: i32,
    json: &'a str,
}

fn action_helper_command(
    fixture: &ActionHelperFixture,
    run: ActionHelperRun<'_>,
) -> std::process::Command {
    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/scripts/run-togi.sh");
    let mut command = std::process::Command::new("bash");
    command
        .arg(helper)
        .current_dir(fixture.dir.path())
        .env("TOGI_BIN", &fixture.fake_togi)
        .env("TOGI_EXPECTED_VERSION", "v0.5.2")
        .env("RUNNER_TEMP", fixture.dir.path())
        .env("GITHUB_OUTPUT", &fixture.github_output)
        .env("TOGI_REPORT_PATH", &fixture.report_path)
        .env("FAKE_TOGI_LOG", &fixture.invocation_log)
        .env("FAKE_TOGI_ENV_LOG", &fixture.child_environment_log)
        .env("FAKE_TOGI_STATUS", run.status.to_string())
        .env("FAKE_TOGI_JSON", run.json);
    for (name, value) in [
        ("TOGI_BASE", run.base),
        ("TOGI_TIMEOUT", run.timeout),
        ("TOGI_FORMAT", run.format),
        ("TOGI_TEST_CMD", run.test_cmd),
    ] {
        if let Some(value) = value {
            command.env(name, value);
        } else {
            command.env_remove(name);
        }
    }
    command
}

fn run_action_helper(
    fixture: &ActionHelperFixture,
    run: ActionHelperRun<'_>,
) -> std::process::Output {
    action_helper_command(fixture, run).output().unwrap()
}

fn action_helper_invocations(fixture: &ActionHelperFixture) -> Vec<Vec<String>> {
    let mut invocations = Vec::new();
    let mut invocation = Vec::new();
    for line in fs::read_to_string(&fixture.invocation_log).unwrap().lines() {
        if line == "--" {
            invocations.push(std::mem::take(&mut invocation));
        } else {
            assert!(
                line.starts_with('<') && line.ends_with('>'),
                "unexpected fake invocation log line: {line}"
            );
            invocation.push(line[1..line.len() - 1].to_string());
        }
    }
    assert!(invocation.is_empty(), "unterminated fake invocation");
    invocations
}

fn action_helper_check_invocations(fixture: &ActionHelperFixture) -> Vec<Vec<String>> {
    action_helper_invocations(fixture)
        .into_iter()
        .filter(|invocation| invocation.first().is_some_and(|arg| arg == "check"))
        .collect()
}

fn action_args(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_string()).collect()
}

fn action_args_with_report(fixture: &ActionHelperFixture, args: &[&str]) -> Vec<String> {
    let mut args = action_args(args);
    args.extend([
        "--json-report".to_string(),
        fixture.report_path.display().to_string(),
    ]);
    args
}

fn assert_action_outputs(fixture: &ActionHelperFixture) {
    assert_eq!(
        fs::read_to_string(&fixture.github_output).unwrap(),
        format!(
            "report-path={}\nmutation-score=75.5\nsurvivor-count=1\n",
            fixture.report_path.display()
        )
    );
}

const NORMAL_ACTION_REPORT: &str =
    r#"{"kind":"mutation_report","schema_version":1,"mutation_score":75.5,"survived":1}"#;

#[test]
fn github_action_run_helper_preserves_selected_format_and_exit_one() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping action helper test because bash or jq is unavailable");
        return;
    }

    let fixture = action_helper_fixture();
    let output = run_action_helper(
        &fixture,
        ActionHelperRun {
            base: Some("HEAD~1"),
            timeout: Some("45"),
            format: Some("github"),
            test_cmd: Some("cargo test --workspace --all-features"),
            status: 1,
            json: NORMAL_ACTION_REPORT,
        },
    );

    assert_eq!(
        output.status.code(),
        Some(1),
        "helper failed with stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "review-format=github\n"
    );
    assert_eq!(
        action_helper_invocations(&fixture),
        vec![
            action_args(&["--version"]),
            action_args(&["help", "check"]),
            action_args_with_report(
                &fixture,
                &[
                    "check",
                    "--base",
                    "HEAD~1",
                    "--timeout",
                    "45",
                    "--format",
                    "github",
                    "--test-cmd",
                    "cargo test --workspace --all-features",
                ],
            ),
        ]
    );
    assert_eq!(
        fs::read_to_string(&fixture.report_path).unwrap(),
        format!("{NORMAL_ACTION_REPORT}\n")
    );
    assert_action_outputs(&fixture);
}

#[test]
fn github_action_run_helper_runs_each_selected_format_once() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping action helper test because bash or jq is unavailable");
        return;
    }

    for format in [
        None,
        Some("terminal"),
        Some("github"),
        Some("html"),
        Some("sarif"),
        Some("json"),
    ] {
        let fixture = action_helper_fixture();
        let output = run_action_helper(
            &fixture,
            ActionHelperRun {
                base: None,
                timeout: None,
                format,
                test_cmd: None,
                status: 1,
                json: NORMAL_ACTION_REPORT,
            },
        );

        assert_eq!(
            output.status.code(),
            Some(1),
            "{format:?} helper stderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected_stdout = if format == Some("json") {
            format!("{NORMAL_ACTION_REPORT}\n")
        } else {
            format!("review-format={}\n", format.unwrap_or("terminal"))
        };
        assert_eq!(String::from_utf8_lossy(&output.stdout), expected_stdout);
        let mut expected_args = action_args(&["check"]);
        if let Some(format) = format {
            expected_args.extend(["--format".to_string(), format.to_string()]);
        }
        expected_args.extend([
            "--json-report".to_string(),
            fixture.report_path.display().to_string(),
        ]);
        assert_eq!(
            action_helper_check_invocations(&fixture),
            vec![expected_args],
            "{format:?} must run exactly one check campaign"
        );
        assert_eq!(
            fs::read_to_string(&fixture.report_path).unwrap(),
            format!("{NORMAL_ACTION_REPORT}\n")
        );
        assert_action_outputs(&fixture);
    }
}

#[test]
fn github_action_run_helper_rejects_version_mismatches_before_check() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping action helper test because bash or jq is unavailable");
        return;
    }

    for version in ["0.5.0", "0.5.1"] {
        let fixture = action_helper_fixture();
        let existing = "{\"kind\":\"mutation_report\",\"schema_version\":1}\n";
        fs::write(&fixture.report_path, existing).unwrap();
        let mut command = action_helper_command(
            &fixture,
            ActionHelperRun {
                base: Some("HEAD~1"),
                timeout: None,
                format: Some("github"),
                test_cmd: None,
                status: 1,
                json: NORMAL_ACTION_REPORT,
            },
        );
        command.env("FAKE_TOGI_VERSION", version);
        let output = command.output().unwrap();

        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("does not match expected"),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            action_helper_invocations(&fixture),
            vec![action_args(&["--version"])]
        );
        assert!(action_helper_check_invocations(&fixture).is_empty());
        assert_eq!(fs::read_to_string(&fixture.report_path).unwrap(), existing);
        assert!(
            !fixture.github_output.exists()
                || fs::read_to_string(&fixture.github_output)
                    .unwrap()
                    .is_empty()
        );
    }
}

#[test]
fn github_action_run_helper_accepts_matching_version_with_stderr() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping action helper test because bash or jq is unavailable");
        return;
    }

    let fixture = action_helper_fixture();
    let mut command = action_helper_command(
        &fixture,
        ActionHelperRun {
            base: Some("HEAD~1"),
            timeout: None,
            format: Some("github"),
            test_cmd: None,
            status: 0,
            json: NORMAL_ACTION_REPORT,
        },
    );
    command.env("FAKE_TOGI_VERSION_STDERR", "binary loader warning");
    let output = command.output().unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("binary loader warning"));
    assert_eq!(action_helper_check_invocations(&fixture).len(), 1);
    assert_action_outputs(&fixture);
}

#[test]
fn github_action_run_helper_rejects_binary_without_json_report_before_check() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping action helper test because bash or jq is unavailable");
        return;
    }

    let fixture = action_helper_fixture();
    let existing = "{\"kind\":\"mutation_report\",\"schema_version\":1}\n";
    fs::write(&fixture.report_path, existing).unwrap();
    let mut command = action_helper_command(
        &fixture,
        ActionHelperRun {
            base: Some("HEAD~1"),
            timeout: None,
            format: Some("github"),
            test_cmd: None,
            status: 1,
            json: NORMAL_ACTION_REPORT,
        },
    );
    command.env("FAKE_TOGI_SUPPORTS_JSON_REPORT", "0");
    let output = command.output().unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("does not support --json-report"));
    assert_eq!(
        action_helper_invocations(&fixture),
        vec![action_args(&["--version"]), action_args(&["help", "check"])]
    );
    assert!(action_helper_check_invocations(&fixture).is_empty());
    assert_eq!(fs::read_to_string(&fixture.report_path).unwrap(), existing);
    assert!(
        !fixture.github_output.exists()
            || fs::read_to_string(&fixture.github_output)
                .unwrap()
                .is_empty()
    );
}

#[test]
fn github_action_run_helper_isolates_private_environment_from_child_togi() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping action helper test because bash or jq is unavailable");
        return;
    }

    let fixture = action_helper_fixture();
    let output = run_action_helper(
        &fixture,
        ActionHelperRun {
            base: Some("origin/main"),
            timeout: Some("120"),
            format: Some("github"),
            test_cmd: Some("cargo test --locked"),
            status: 0,
            json: NORMAL_ACTION_REPORT,
        },
    );

    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(&fixture.report_path).unwrap(),
        format!("{NORMAL_ACTION_REPORT}\n")
    );
    assert_action_outputs(&fixture);
    let isolated_child_environment = concat!(
        "TOGI_BASE=__unset__\n",
        "TOGI_TIMEOUT=__unset__\n",
        "TOGI_FORMAT=__unset__\n",
        "TOGI_TEST_CMD=__unset__\n",
        "TOGI_REPORT_PATH=__unset__\n",
        "TOGI_BIN=__unset__\n",
        "TOGI_EXPECTED_VERSION=__unset__\n",
    );
    assert_eq!(
        fs::read_to_string(&fixture.child_environment_log).unwrap(),
        format!(
            "{isolated_child_environment}--\n{isolated_child_environment}--\n{isolated_child_environment}--\n"
        )
    );
}

#[cfg(not(windows))]
#[test]
fn github_action_run_helper_keeps_native_report_output_path() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping action helper test because bash or jq is unavailable");
        return;
    }

    let fixture = action_helper_fixture();
    let cygpath_dir = fixture.dir.path().join("fake-bin");
    fs::create_dir(&cygpath_dir).unwrap();
    let cygpath = cygpath_dir.join("cygpath");
    fs::write(
        &cygpath,
        "#!/usr/bin/env bash\n[[ \"$1\" == \"-u\" ]] || exit 2\nprintf '%s\\n' \"$FAKE_CYGPATH_RESULT\"\n",
    )
    .unwrap();
    let output = std::process::Command::new("bash")
        .args(["-c", "chmod +x \"$1\"", "--"])
        .arg(&cygpath)
        .output()
        .unwrap();
    assert!(output.status.success());
    let inherited_path = std::env::var("PATH").expect("PATH must be set");
    let path = format!("{}:{inherited_path}", cygpath_dir.display());
    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/scripts/run-togi.sh");
    let output = std::process::Command::new("bash")
        .arg(helper)
        .current_dir(fixture.dir.path())
        .env("PATH", path)
        .env("TOGI_BIN", &fixture.fake_togi)
        .env("TOGI_EXPECTED_VERSION", "v0.5.2")
        .env("TOGI_REPORT_PATH", r"C:\runner\temp\togi-report.json")
        .env("RUNNER_TEMP", fixture.dir.path())
        .env("GITHUB_OUTPUT", &fixture.github_output)
        .env("FAKE_TOGI_LOG", &fixture.invocation_log)
        .env("FAKE_TOGI_STATUS", "0")
        .env("FAKE_TOGI_JSON", NORMAL_ACTION_REPORT)
        .env("FAKE_CYGPATH_RESULT", &fixture.report_path)
        .env_remove("TOGI_BASE")
        .env_remove("TOGI_TIMEOUT")
        .env_remove("TOGI_TEST_CMD")
        .env("TOGI_FORMAT", "github")
        .output()
        .unwrap();

    assert!(output.status.success());
    assert!(fixture.report_path.exists());
    assert_eq!(
        action_helper_check_invocations(&fixture),
        vec![action_args_with_report(
            &fixture,
            &["check", "--format", "github"],
        )]
    );
    assert_eq!(
        fs::read_to_string(&fixture.github_output).unwrap(),
        "report-path=C:\\runner\\temp\\togi-report.json\nmutation-score=75.5\nsurvivor-count=1\n"
    );
}

#[test]
fn github_action_run_helper_omits_unset_review_flags() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping action helper test because bash or jq is unavailable");
        return;
    }

    let fixture = action_helper_fixture();
    let output = run_action_helper(
        &fixture,
        ActionHelperRun {
            base: None,
            timeout: None,
            format: None,
            test_cmd: None,
            status: 0,
            json: NORMAL_ACTION_REPORT,
        },
    );

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "review-format=terminal\n"
    );
    assert_eq!(
        action_helper_check_invocations(&fixture),
        vec![action_args_with_report(&fixture, &["check"])]
    );
    assert_action_outputs(&fixture);
}

#[test]
fn github_action_run_helper_removes_invalid_or_fatal_sidecar_reports() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping action helper test because bash or jq is unavailable");
        return;
    }

    for (status, json) in [(1, "not json"), (2, NORMAL_ACTION_REPORT)] {
        let fixture = action_helper_fixture();
        let output = run_action_helper(
            &fixture,
            ActionHelperRun {
                base: Some("HEAD~1"),
                timeout: None,
                format: Some("github"),
                test_cmd: None,
                status,
                json,
            },
        );

        assert_eq!(
            output.status.code(),
            Some(2),
            "unexpected status for JSON sidecar status {status}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.report_path.exists());
        assert!(
            !fixture.github_output.exists()
                || fs::read_to_string(&fixture.github_output)
                    .unwrap()
                    .is_empty()
        );
    }
}

#[test]
fn github_action_run_helper_propagates_fatal_review_and_cleans_stale_report() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping action helper test because bash or jq is unavailable");
        return;
    }

    let fixture = action_helper_fixture();
    fs::write(&fixture.report_path, "stale report").unwrap();
    let output = run_action_helper(
        &fixture,
        ActionHelperRun {
            base: Some("HEAD~1"),
            timeout: None,
            format: Some("github"),
            test_cmd: None,
            status: 2,
            json: NORMAL_ACTION_REPORT,
        },
    );

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        action_helper_check_invocations(&fixture),
        vec![action_args_with_report(
            &fixture,
            &["check", "--base", "HEAD~1", "--format", "github"],
        )]
    );
    assert!(!fixture.report_path.exists());
    assert!(
        !fixture.github_output.exists()
            || fs::read_to_string(&fixture.github_output)
                .unwrap()
                .is_empty()
    );
}

#[test]
fn github_action_report_replays_a_direct_mutation() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping action replay test because bash or jq is unavailable");
        return;
    }

    let repo = setup_git_repo();
    fs::write(
        repo.path().join("togi.toml"),
        "[mutations]\nschemata = false\nmax_per_run = 1\n",
    )
    .unwrap();
    fs::write(repo.path().join("test.sh"), "#!/bin/sh\nexit 0\n").unwrap();

    let report_dir = TempDir::new().unwrap();
    let report_path = report_dir.path().join("togi-report.json");
    let github_output = report_dir.path().join("github-output");
    let helper = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/scripts/run-togi.sh");
    let output = std::process::Command::new("bash")
        .arg(helper)
        .current_dir(repo.path())
        .env("TOGI_BIN", assert_cmd::cargo::cargo_bin("togi"))
        .env(
            "TOGI_EXPECTED_VERSION",
            concat!("v", env!("CARGO_PKG_VERSION")),
        )
        .env("RUNNER_TEMP", report_dir.path())
        .env("GITHUB_OUTPUT", &github_output)
        .env("TOGI_BASE", "HEAD")
        .env("TOGI_FORMAT", "github")
        .env("TOGI_TEST_CMD", "sh test.sh")
        .output()
        .unwrap();

    assert_eq!(
        output.status.code(),
        Some(1),
        "Action helper did not preserve the surviving review failure\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("::warning"));

    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(&report_path).unwrap()).unwrap();
    assert_eq!(report["kind"], "mutation_report");
    assert!(report["schema_version"].as_u64().is_some());
    let mutation = report["mutations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|mutation| mutation["replay"]["kind"] == "regular_direct")
        .expect("report should contain a directly replayable mutation");
    let mutation_id = mutation["id"].as_u64().unwrap().to_string();
    let output_values = fs::read_to_string(&github_output).unwrap();
    assert!(output_values.contains(&format!("report-path={}", report_path.display())));
    assert!(output_values.contains("mutation-score="));
    assert!(output_values.contains("survivor-count="));

    togi()
        .args([
            "replay",
            &mutation_id,
            "--report",
            report_path.to_str().unwrap(),
        ])
        .current_dir(repo.path())
        .assert()
        .success();
}

#[cfg(unix)]
#[test]
fn github_action_install_rejects_failed_fetch_without_ambient_archive_fallback() {
    if !bash_available() {
        eprintln!("skipping Action install regression test because bash is unavailable");
        return;
    }

    let dir = TempDir::new().unwrap();
    let action_path = dir.path().join("fake-action");
    let scripts = action_path.join(".github/scripts");
    fs::create_dir_all(&scripts).unwrap();
    let resolver_log = dir.path().join("resolver-environment");
    fs::write(
        scripts.join("resolve-togi-asset.sh"),
        "#!/usr/bin/env bash\nprintf '%s/%s\\n' \"${TOGI_OS-__unset__}\" \"${TOGI_ARCH-__unset__}\" > \"$RESOLVER_LOG\"\nprintf 'TOGI_ARCHIVE=%q\\n' 'verified.tar.gz'\nprintf 'TOGI_BINARY=%q\\n' 'togi'\n",
    )
    .unwrap();
    let fetch_log = dir.path().join("fetch-version");
    fs::write(
        scripts.join("fetch-togi-release-asset.sh"),
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$TOGI_VERSION\" > \"$FETCH_LOG\"\nexit 1\n",
    )
    .unwrap();
    let install_marker = dir.path().join("installer-ran");
    fs::write(
        scripts.join("install-togi-archive.sh"),
        "#!/usr/bin/env bash\ntouch \"$INSTALL_MARKER\"\n",
    )
    .unwrap();
    let github_output = dir.path().join("github-output");

    let runner_temp = dir.path().join("runner-temp");
    let github_env = dir.path().join("github-env");
    let ambient_archive = dir.path().join("ambient-unverified.tar.gz");
    fs::write(&ambient_archive, "unverified archive").unwrap();
    let output = std::process::Command::new("bash")
        .args(["-c", &action_install_run_script(&action_path)])
        .current_dir(dir.path())
        .env("GITHUB_OUTPUT", &github_output)
        .env("TOGI_VERSION_INPUT", "v0.5.2")
        .env("RUNNER_TEMP", &runner_temp)
        .env("GITHUB_ENV", &github_env)
        .env("TOGI_ARCHIVE", "ambient-archive.tar.gz")
        .env("TOGI_BINARY", "ambient-togi")
        .env("TOGI_ARCHIVE_PATH", &ambient_archive)
        .env("TOGI_OS", "forced-os")
        .env("TOGI_ARCH", "forced-arch")
        .env("RESOLVER_LOG", &resolver_log)
        .env("FETCH_LOG", &fetch_log)
        .env("INSTALL_MARKER", &install_marker)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Could not fetch and verify"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(fs::read_to_string(&fetch_log).unwrap(), "v0.5.2\n");
    assert_eq!(
        fs::read_to_string(&resolver_log).unwrap(),
        "__unset__/__unset__\n"
    );
    assert!(
        !install_marker.exists(),
        "failed fetch must not execute the installer with an ambient archive"
    );
    assert!(
        !runner_temp.join("togi-bin").exists(),
        "failed fetch must not publish a binary"
    );
    assert!(
        !github_env.exists(),
        "failed fetch must not publish binary or version environment"
    );
    assert!(
        !runner_temp.join("togi-report.json").exists() && !github_output.exists(),
        "failed fetch must not publish a report or Action outputs"
    );
}
#[cfg(unix)]
#[test]
fn github_action_install_uses_the_resolved_temp_root() {
    if !bash_available() {
        eprintln!("skipping Action install path test because bash is unavailable");
        return;
    }

    let dir = TempDir::new().unwrap();
    let action_path = dir.path().join("fake-action");
    let scripts = action_path.join(".github/scripts");
    fs::create_dir_all(&scripts).unwrap();
    fs::write(
        scripts.join("resolve-togi-asset.sh"),
        "#!/usr/bin/env bash\nprintf 'TOGI_ARCHIVE=%q\\n' 'verified.tar.gz'\nprintf 'TOGI_BINARY=%q\\n' 'togi'\n",
    )
    .unwrap();
    fs::write(
        scripts.join("fetch-togi-release-asset.sh"),
        "#!/usr/bin/env bash\nprintf 'TOGI_ARCHIVE_PATH=%q\\n' \"$FAKE_ARCHIVE_PATH\"\n",
    )
    .unwrap();
    let install_root_log = dir.path().join("install-root");
    fs::write(
        scripts.join("install-togi-archive.sh"),
        "#!/usr/bin/env bash\nprintf '%s\\n' \"$RUNNER_TEMP\" > \"$INSTALL_ROOT_LOG\"\n",
    )
    .unwrap();

    let fake_bin = dir.path().join("fake-bin");
    fs::create_dir(&fake_bin).unwrap();
    let cygpath = fake_bin.join("cygpath");
    fs::write(
        &cygpath,
        "#!/usr/bin/env bash\n[[ \"$1\" == \"-u\" ]] || exit 2\nprintf '%s\\n' \"$FAKE_TEMP_ROOT\"\n",
    )
    .unwrap();
    let mut permissions = fs::metadata(&cygpath).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&cygpath, permissions).unwrap();

    let raw_runner_temp = dir.path().join("raw-runner-temp");
    let resolved_temp_root = dir.path().join("resolved-temp-root");
    let archive_path = dir.path().join("verified.tar.gz");
    fs::write(&archive_path, "verified archive").unwrap();
    let github_env = dir.path().join("github-env");
    let github_path = dir.path().join("github-path");
    let mut paths = vec![fake_bin];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").expect("PATH must be set"),
    ));
    let install_script = action_install_run_script(&action_path);
    let output = std::process::Command::new("bash")
        .args(["-c", install_script.as_str()])
        .current_dir(dir.path())
        .env("TOGI_VERSION_INPUT", "v0.5.2")
        .env("RUNNER_TEMP", &raw_runner_temp)
        .env("GITHUB_ENV", &github_env)
        .env("GITHUB_PATH", &github_path)
        .env("FAKE_TEMP_ROOT", &resolved_temp_root)
        .env("FAKE_ARCHIVE_PATH", &archive_path)
        .env("INSTALL_ROOT_LOG", &install_root_log)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&install_root_log).unwrap(),
        format!("{}\n", resolved_temp_root.display())
    );
    assert_eq!(
        fs::read_to_string(&github_env).unwrap(),
        format!(
            "TOGI_BIN={}/togi-bin/togi\nTOGI_EXPECTED_VERSION=v0.5.2\n",
            resolved_temp_root.display()
        )
    );
}

#[test]
fn github_action_inputs_have_no_baked_in_defaults() {
    let action_yml =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("action.yml")).unwrap();
    let action: serde_yaml::Value = serde_yaml::from_str(&action_yml).unwrap();

    // Split the inputs: mapping into per-input blocks keyed by name.
    let mut blocks: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut in_inputs = false;
    for line in action_yml.lines() {
        if line == "inputs:" {
            in_inputs = true;
            continue;
        }
        if in_inputs {
            if !line.starts_with("  ") {
                break;
            }
            if let Some(name) = line.strip_prefix("  ") {
                if !name.starts_with(' ') && name.ends_with(':') {
                    blocks.push((name.trim_end_matches(':'), Vec::new()));
                    continue;
                }
            }
            if let Some((_, body)) = blocks.last_mut() {
                body.push(line);
            }
        }
    }

    for name in ["base", "timeout", "format"] {
        let (_, body) = blocks
            .iter()
            .find(|(block, _)| *block == name)
            .unwrap_or_else(|| panic!("action.yml is missing the `{name}` input"));
        assert!(
            !body
                .iter()
                .any(|line| line.trim_start().starts_with("default:")),
            "action.yml input `{name}` must not bake in a default; unset inputs defer to togi.toml"
        );
    }

    for (name, default) in [
        ("version", "'v0.6.0'"),
        ("upload-report", "'true'"),
        ("report-retention-days", "'14'"),
        ("report-artifact-name", "'togi-report'"),
    ] {
        let (_, body) = blocks
            .iter()
            .find(|(block, _)| *block == name)
            .unwrap_or_else(|| panic!("action.yml is missing the `{name}` input"));
        assert!(
            body.iter()
                .any(|line| line.trim() == format!("default: {default}")),
            "action.yml input `{name}` must keep its {default} default"
        );
    }
    let run_blocks = action
        .get("runs")
        .and_then(|runs| runs.get("steps"))
        .and_then(serde_yaml::Value::as_sequence)
        .expect("action.yml must contain composite Action steps")
        .iter()
        .filter_map(|step| step.get("run").and_then(serde_yaml::Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !run_blocks.contains("latest"),
        "Action releases must not resolve a mutable version"
    );
    for expected in [
        "VERSION=\"${TOGI_VERSION_INPUT:-v0.6.0}\"",
        "^v[0-9]+[.][0-9]+[.][0-9]+$",
        "resolve-togi-asset.sh",
        "fetch-togi-release-asset.sh",
        "install-togi-archive.sh",
        "TOGI_BIN=\"${TEMP_ROOT}/togi-bin/${TOGI_BINARY}\"",
        "RUNNER_TEMP=\"$TEMP_ROOT\"",
        "printf 'TOGI_EXPECTED_VERSION=%s\\n' \"$VERSION\"",
    ] {
        assert!(
            action_yml.contains(expected),
            "action.yml must contain `{expected}`"
        );
    }
    assert!(
        !run_blocks.contains("curl "),
        "Action downloads must go through the verified fetch helper"
    );
}

#[test]
fn github_action_declares_replay_report_contract() {
    let action_yml = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("action.yml"))
        .unwrap()
        .replace("\r\n", "\n");

    for expected in [
        "outputs:\n  report-path:",
        "value: ${{ steps.run-togi.outputs.report-path }}",
        "value: ${{ steps.run-togi.outputs.mutation-score }}",
        "value: ${{ steps.run-togi.outputs.survivor-count }}",
        "id: run-togi",
        "TOGI_REPORT_PATH: ${{ runner.temp }}/togi-report.json",
        "report-artifact-name:",
        "if: ${{ always() && inputs.upload-report == 'true' && steps.run-togi.outputs.report-path != '' }}",
        "uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7",
        "name: ${{ inputs.report-artifact-name }}",
        "path: ${{ runner.temp }}/togi-report.json",
        "retention-days: ${{ inputs.report-retention-days }}",
        "if-no-files-found: warn",
    ] {
        assert!(
            action_yml.contains(expected),
            "action.yml is missing `{expected}`"
        );
    }
}

#[test]
fn production_upload_artifact_pins_are_immutable() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let expected = "uses: actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a # v7";

    for path in [
        "action.yml",
        ".github/workflows/ci.yml",
        ".github/workflows/pr-loop-calibration.yml",
        ".github/workflows/pr-loop-regression-gate.yml",
        ".github/workflows/pr-loop-scale-evidence.yml",
        ".github/workflows/release.yml",
    ] {
        let source = fs::read_to_string(root.join(path))
            .unwrap()
            .replace("\r\n", "\n");
        let mut pins = source
            .lines()
            .map(str::trim)
            .map(|line| line.strip_prefix("- ").unwrap_or(line))
            .filter(|line| line.starts_with("uses: actions/upload-artifact@"));

        assert_eq!(
            pins.next(),
            Some(expected),
            "{path} must use the immutable upload-artifact v7 pin"
        );
        assert_eq!(
            pins.next(),
            None,
            "{path} must declare exactly one upload-artifact use"
        );
    }
}

#[test]
fn github_action_guide_and_advisory_pin_released_contract() {
    let readme = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
        .unwrap()
        .replace("\r\n", "\n");
    let marker = "```yaml\n# .github/workflows/togi.yml\nname: Mutation testing\n";
    let workflow_start = readme
        .find(marker)
        .expect("README must contain the named Togi workflow");
    let workflow = &readme[workflow_start + "```yaml\n".len()..];
    let (workflow, _) = workflow
        .split_once("\n```\n")
        .expect("named Togi workflow must have a closing fence");

    for expected in [
        "on:\n  pull_request:",
        "permissions:\n  contents: read",
        "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1",
        "fetch-depth: 0",
        "persist-credentials: false",
        "actions/setup-node@820762786026740c76f36085b0efc47a31fe5020 # v7.0.0",
        "node-version: 24 # Choose the version required by this repository.",
        "name: Install project test dependencies\n        run: npm ci",
        "Darkroom4364/togi@10970abd97c0d13d8e0ee4b0c568552ebcf84720 # v0.6.0",
        "version: v0.6.0",
        "base: origin/${{ github.base_ref }}",
        "test-cmd: npm test",
        "format: json",
        "report-artifact-name: togi-report",
        "report-retention-days: '14'",
        "name: Record Togi report\n        if: ${{ always() }}",
        "TOGI_REPORT_PATH: ${{ steps.togi.outputs.report-path }}",
        "TOGI_MUTATION_SCORE: ${{ steps.togi.outputs.mutation-score }}",
        "TOGI_SURVIVOR_COUNT: ${{ steps.togi.outputs.survivor-count }}",
    ] {
        assert!(
            workflow.contains(expected),
            "documented Togi workflow is missing `{expected}`"
        );
    }
    assert!(
        !workflow.contains("pull_request_target"),
        "documented Togi workflow must not use pull_request_target"
    );
    assert!(
        !workflow.contains("continue-on-error"),
        "documented Togi workflow must preserve the blocking gate"
    );

    for (before, after) in [
        (
            "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1",
            "actions/setup-node@820762786026740c76f36085b0efc47a31fe5020",
        ),
        (
            "actions/setup-node@820762786026740c76f36085b0efc47a31fe5020",
            "run: npm ci",
        ),
        (
            "run: npm ci",
            "Darkroom4364/togi@10970abd97c0d13d8e0ee4b0c568552ebcf84720",
        ),
    ] {
        assert!(
            workflow.find(before).unwrap() < workflow.find(after).unwrap(),
            "documented Togi workflow must place `{before}` before `{after}`"
        );
    }

    for expected in [
        "Those inputs override `togi.toml`",
        "the Action runs one mutation campaign",
        "`togi-report.json` using `--json-report`",
        "A failed baseline test or build is a fatal exit `2`",
        "Never use `pull_request_target` to run PR code.",
        "does not independently pin archive bytes.",
    ] {
        assert!(
            readme.contains(expected),
            "README Action guidance is missing `{expected}`"
        );
    }
    assert!(
        !readme.contains("mutable `latest` default"),
        "README Action guidance must not describe a mutable latest default"
    );

    let advisory = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/togi-action-advisory.yml"),
    )
    .unwrap()
    .replace("\r\n", "\n");
    for expected in [
        "actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1",
        "name: Fetch fixture dependencies\n        run: cargo fetch --locked",
        "Darkroom4364/togi@a1503b2ebac4c63d377b015c4825b97cab25ec68 # v0.4.1 release action source",
        "version: v0.4.1",
        "test-cmd: cargo test --locked",
        "format: json",
        "name: Record Togi report (advisory)\n        if: ${{ always() }}",
        "TOGI_REPORT_PATH: ${{ steps.togi.outputs.report-path }}",
        "TOGI_MUTATION_SCORE: ${{ steps.togi.outputs.mutation-score }}",
        "TOGI_SURVIVOR_COUNT: ${{ steps.togi.outputs.survivor-count }}",
    ] {
        assert!(
            advisory.contains(expected),
            "advisory workflow is missing `{expected}`"
        );
    }
}

#[test]
fn github_release_install_guide_documents_released_contract() {
    let readme = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
        .unwrap()
        .replace("\r\n", "\n");
    let install_start = readme
        .find("## Install\n")
        .expect("README must contain an Install section");
    let install_end = readme
        .find("\n### Before first run\n")
        .expect("Install section must end before Before first run");
    let install = &readme[install_start..install_end];

    for expected in [
        "GitHub Releases are the supported binary-install path.",
        "TOGI_VERSION=v0.6.0",
        "TOGI_ARCHIVE=togi-linux-x86_64.tar.gz",
        "https://github.com/Darkroom4364/togi/releases/download/${TOGI_VERSION}",
        "curl -fsSLo \"$TOGI_ARCHIVE\"",
        "curl -fsSLo checksums.txt",
        "EXPECTED_SHA=$(awk",
        "ACTUAL_SHA=$(sha256sum",
        "Checksum mismatch",
        "`togi` is not currently published on crates.io.",
        "cargo install --git https://github.com/Darkroom4364/togi --rev <commit-sha> --locked",
    ] {
        assert!(
            install.contains(expected),
            "Install section is missing `{expected}`"
        );
    }
    assert!(
        !install.contains("TOGI_VERSION=v0.5.2"),
        "Install section must not point the release archive at v0.5.2"
    );
    assert!(
        !install.contains("cargo install togi"),
        "Install section must not present an unavailable crates.io install"
    );
}

#[test]
fn before_first_run_documents_zero_config_contract() {
    let readme = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
        .unwrap()
        .replace("\r\n", "\n");
    let section_start = readme
        .find("### Before first run\n")
        .expect("README must contain a Before first run section");
    let activation = "**First run in a clean clone:";
    let preview = "togi check --all --dry-run --max-per-run 10";
    let usage_start = readme
        .find("\n## Usage\n")
        .expect("README must contain a Usage section");
    let section = &readme[section_start..usage_start];
    let activation_start = section
        .find(activation)
        .map(|start| section_start + start)
        .expect("Before first run section must contain the activation guidance");
    let preview_start = section
        .find(preview)
        .map(|start| section_start + start)
        .expect("Before first run section must contain the bounded preview");
    let prerequisites = "normal dependencies and ensure the selected test command passes.\n\n";
    let prerequisites_end = section
        .find(prerequisites)
        .map(|start| section_start + start + prerequisites.len())
        .expect("README must end the prerequisites paragraph before the preview");
    let compatibility_start = section
        .find("The [compatibility contract](docs/COMPATIBILITY.md)")
        .map(|start| section_start + start)
        .expect("README must contain compatibility guidance");
    assert_eq!(
        activation_start, prerequisites_end,
        "activation guidance must immediately follow the prerequisites paragraph"
    );
    assert!(
        activation_start < preview_start && preview_start < compatibility_start,
        "bounded preview must follow the activation guidance and precede compatibility guidance"
    );
    assert!(
        section_start < preview_start && preview_start < usage_start,
        "bounded preview must appear in Before first run before Usage"
    );
    let section = section.split_whitespace().collect::<Vec<_>>().join(" ");

    for expected in [
        "**Zero config** only means togi auto-detects a test command from a supported project marker.",
        "It does not provision your project's dependencies, runtimes, or test runner, or make an unsupported platform or language supported.",
        "Before running `togi check`, use a trusted Git checkout with a resolvable base: the default is `origin/main`, or choose one with `--base`.",
        "Install the project's normal dependencies and ensure the selected test command passes.",
        "**First run in a clean clone:** The selected diff is empty; that is normal for a clean clone, not an error. Preview all supported files without tests (bounded):",
        "togi check --all --dry-run --max-per-run 10",
        "After a supported source change, run `togi check`, or `togi check --base <ref>` for another intended base. `--all` and `--base` are alternatives; do not combine them. If command detection is ambiguous, run `togi init` from the repository root.",
        "The [compatibility contract](docs/COMPATIBILITY.md) contains the supported marker defaults.",
        "When no supported marker is present, togi's best-effort fallback is `make test`, which requires a `Makefile` with a `test` target.",
        "run `togi init` and review or edit the generated `togi.toml`, configure `togi.toml` directly, or use one-shot `--test-cmd`.",
        "The compatibility contract remains the authoritative support matrix: Tier 1 is end-to-end CI-verified, Tier 2 is build- and unit-test-verified, and not supported has no CI guarantee.",
        "the Tier 1 Linux x86_64 archive passes checksum, install, version, and a real Go mutation smoke, while the Tier 2 macOS arm64 and Windows x86_64 archives pass checksum, install, and version smoke.",
        "Linux and Windows ARM64 (aarch64) are not supported.",
    ] {
        assert!(
            section.contains(expected),
            "Before first run section is missing `{expected}`"
        );
    }
}

#[test]
fn github_action_asset_resolver_matches_release_assets() {
    if !bash_available() {
        eprintln!("skipping action asset resolver test because bash is unavailable");
        return;
    }

    assert_eq!(
        resolve_action_asset("Linux", "x86_64"),
        action_asset("togi-linux-x86_64.tar.gz", "togi")
    );
    assert_eq!(
        resolve_action_asset("Darwin", "arm64"),
        action_asset("togi-macos-arm64.tar.gz", "togi")
    );
    assert_eq!(
        resolve_action_asset("MINGW64_NT-10.0", "AMD64"),
        action_asset("togi-windows-x86_64.zip", "togi.exe")
    );

    // macOS x86_64 (Intel) is no longer shipped: the resolver must reject it
    // explicitly instead of resolving a removed asset.
    let output = run_action_resolver("Darwin", "x86_64");
    assert!(
        !output.status.success(),
        "resolver must reject macOS x86_64\nstdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("unsupported release target: macos-x86_64"),
        "resolver rejection must name the unsupported target\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn github_action_install_steps_place_binary_and_update_github_path() {
    if !bash_available() {
        eprintln!("skipping action install test because bash is unavailable");
        return;
    }

    assert_action_installs_asset(&resolve_action_asset("Linux", "x86_64"), false);
    assert_action_installs_asset(
        &resolve_action_asset("MINGW64_NT-10.0", "AMD64"),
        !cfg!(windows),
    );
}

#[test]
fn release_verification_workflow_uses_triggering_tag() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let ci = fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();
    assert!(
        !ci.contains("TOGI_VERSION"),
        "ci.yml must not pin a released-binary smoke version"
    );

    let release = fs::read_to_string(root.join(".github/workflows/release.yml")).unwrap();
    for expected in [
        "TOGI_VERSION: ${{ github.ref_name }}",
        "TOGI_WORKFLOW_HEAD: ${{ github.sha }}",
        "needs: release",
        "run-released-binary-smoke.sh",
        "run-released-binary-install-smoke.sh",
        "verify-release-identity.sh",
        "TOGI_EXPECTED_ARCH: ${{ matrix.arch }}",
        "TOGI_EXPECTED_ARCHIVE: ${{ matrix.archive }}",
        "TOGI_EXPECTED_BINARY: ${{ matrix.binary }}",
    ] {
        assert!(
            release.contains(expected),
            "release.yml is missing `{expected}`"
        );
    }
    assert!(
        !release.contains("v0.4.1"),
        "release.yml must not pin a stale release version"
    );
}

#[test]
fn verify_release_matrix_binds_exact_runner_targets() {
    if !bash_available() {
        eprintln!("skipping verify-release matrix test because bash is unavailable");
        return;
    }

    let release = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/release.yml"),
    )
    .unwrap()
    .replace("\r\n", "\n");
    let section_start = release
        .find("\n  verify-release:\n")
        .expect("release.yml must contain a verify-release job");
    let section = &release[section_start..];

    // Each leg's expected archive/binary must equal what the asset resolver
    // selects for that platform, so a moving runner label cannot verify the
    // wrong archive under the right name.
    for (asset, os, arch, expected) in [
        (
            "linux-x86_64",
            "ubuntu-24.04",
            "x86_64",
            resolve_action_asset("Linux", "x86_64"),
        ),
        (
            "macos-arm64",
            "macos-15",
            "arm64",
            resolve_action_asset("Darwin", "arm64"),
        ),
        (
            "windows-x86_64",
            "windows-2022",
            "x86_64",
            resolve_action_asset("MINGW64_NT-10.0", "AMD64"),
        ),
    ] {
        for expected_line in [
            format!("- asset: {asset}"),
            format!("os: {os}"),
            format!("arch: {arch}"),
            format!("archive: {}", expected.archive),
            format!("binary: {}", expected.binary),
        ] {
            assert!(
                section.contains(&expected_line),
                "verify-release matrix is missing `{expected_line}`"
            );
        }
    }
    assert!(
        !section.contains("macos-latest"),
        "verify-release must pin an explicit arm64 macOS runner, not macos-latest"
    );
}

// Semantic support-contract validation (#485): the documented support table
// in docs/COMPATIBILITY.md, the CI/release workflow matrices, the per-tier
// evidence steps, and the asset resolver must all describe exactly the same
// target set. Structured YAML/table parsing (not prose substring matching)
// rejects an extra release target, a wrong runner arch, a Tier-1 row without
// its required evidence, or a Tier-2 row without its prescribed evidence.
#[derive(Debug, PartialEq, Eq, Clone)]
struct SupportRow {
    os: &'static str,
    arch: &'static str,
    tier: u32,
    target: &'static str,
    asset: &'static str,
    archive: &'static str,
    binary: &'static str,
    uname_s: &'static str,
    uname_m: &'static str,
}

// The canonical support matrix the v1 decision pins. The documented support
// table must describe exactly this set — nothing aspirational, nothing
// removed.
const SUPPORT_CONTRACT: &[SupportRow] = &[
    SupportRow {
        os: "Linux",
        arch: "x86_64",
        tier: 1,
        target: "x86_64-unknown-linux-gnu",
        asset: "linux-x86_64",
        archive: "togi-linux-x86_64.tar.gz",
        binary: "togi",
        uname_s: "Linux",
        uname_m: "x86_64",
    },
    SupportRow {
        os: "macOS",
        arch: "arm64",
        tier: 2,
        target: "aarch64-apple-darwin",
        asset: "macos-arm64",
        archive: "togi-macos-arm64.tar.gz",
        binary: "togi",
        uname_s: "Darwin",
        uname_m: "arm64",
    },
    SupportRow {
        os: "Windows",
        arch: "x86_64",
        tier: 2,
        target: "x86_64-pc-windows-msvc",
        asset: "windows-x86_64",
        archive: "togi-windows-x86_64.zip",
        binary: "togi.exe",
        uname_s: "MINGW64_NT-10.0",
        uname_m: "AMD64",
    },
];

// Runner-label -> required host architecture. A runner image that changes
// architecture must fail the contract here (and at runtime via the workflow's
// own assertion step), never silently prove the wrong target.
const RUNNER_ARCH: &[(&str, &str)] = &[
    ("ubuntu-24.04", "x86_64"),
    ("macos-15", "arm64"),
    ("windows-2022", "x86_64"),
];

fn runner_arch(label: &str) -> Option<&'static str> {
    RUNNER_ARCH
        .iter()
        .find(|(runner, _)| *runner == label)
        .map(|(_, arch)| *arch)
}

fn documented_support_rows() -> Vec<(String, String, u32)> {
    let doc =
        fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/COMPATIBILITY.md"))
            .unwrap()
            .replace("\r\n", "\n");
    let section_start = doc
        .find("## Operating System and Architecture Matrix\n")
        .expect("COMPATIBILITY must contain the OS/architecture matrix section");
    let section = &doc[section_start..];
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in section.lines().skip(1) {
        if in_table {
            if !line.starts_with('|') {
                break;
            }
            if line.contains("---") || line.contains("| OS |") {
                continue;
            }
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            // Leading/trailing '|' produce empty edge cells.
            let [_, os, arch, tier, ..] = cells.as_slice() else {
                panic!("malformed support table row: {line}");
            };
            let tier = tier
                .strip_prefix("Tier ")
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or_else(|| panic!("unparseable tier in row: {line}"));
            rows.push((os.to_string(), arch.to_string(), tier));
        } else if line.starts_with("| OS | Arch | Tier |") {
            in_table = true;
        }
    }
    assert!(!rows.is_empty(), "support table has no rows");
    rows
}

fn matrix_include(workflow: &serde_yaml::Value, job: &str) -> Vec<serde_yaml::Value> {
    workflow
        .get("jobs")
        .and_then(|jobs| jobs.get(job))
        .unwrap_or_else(|| panic!("workflow is missing job `{job}`"))
        .get("strategy")
        .and_then(|strategy| strategy.get("matrix"))
        .and_then(|matrix| matrix.get("include"))
        .and_then(|include| include.as_sequence())
        .unwrap_or_else(|| panic!("job `{job}` must use an explicit matrix include list"))
        .clone()
}

fn matrix_field<'a>(leg: &'a serde_yaml::Value, job: &str, field: &str) -> &'a str {
    leg.get(field)
        .and_then(|value| value.as_str())
        .unwrap_or_else(|| panic!("job `{job}` leg is missing `{field}`: {leg:?}"))
}

fn matrix_tier(leg: &serde_yaml::Value, job: &str) -> u32 {
    let value = leg
        .get("tier")
        .unwrap_or_else(|| panic!("job `{job}` leg is missing `tier`: {leg:?}"));
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|tier| tier.parse::<u64>().ok()))
        .unwrap_or_else(|| panic!("job `{job}` leg has a non-numeric tier: {leg:?}")) as u32
}

fn job_steps<'a>(workflow: &'a serde_yaml::Value, job: &str) -> &'a [serde_yaml::Value] {
    workflow
        .get("jobs")
        .and_then(|jobs| jobs.get(job))
        .and_then(|job| job.get("steps"))
        .and_then(|steps| steps.as_sequence())
        .unwrap_or_else(|| panic!("job `{job}` must have steps"))
}

fn step_invokes_exactly(step: &serde_yaml::Value, invocation: &str) -> bool {
    step.get("run")
        .and_then(|run| run.as_str())
        .is_some_and(|run| run.trim() == invocation)
}

fn step_gated_on_tier(step: &serde_yaml::Value, tier: u32) -> bool {
    step.get("if")
        .and_then(|condition| condition.as_str())
        .is_some_and(|condition| {
            condition == format!("${{{{ matrix.tier == {tier} }}}}")
                || condition == format!("matrix.tier == {tier}")
        })
}

// Requires an exact `assert-native-target.sh` invocation whose environment
// binds the required target/arch exactly — a lookalike inline `echo` or a
// substring match must not satisfy the contract.
fn assert_native_target_step(
    steps: &[serde_yaml::Value],
    job: &str,
    expected_target: &str,
    expected_arch: &str,
) {
    let step = steps
        .iter()
        .find(|step| step_invokes_exactly(step, "bash ./.github/scripts/assert-native-target.sh"))
        .unwrap_or_else(|| panic!("job `{job}` must invoke assert-native-target.sh exactly"));
    for (key, expected) in [
        ("TOGI_EXPECTED_TARGET", expected_target),
        ("TOGI_EXPECTED_ARCH", expected_arch),
    ] {
        let actual = step
            .get("env")
            .and_then(|env| env.get(key))
            .and_then(|value| value.as_str())
            .unwrap_or_else(|| panic!("job `{job}` assertion step is missing env `{key}`"));
        assert_eq!(
            actual, expected,
            "job `{job}` assertion step must bind {key} to `{expected}`"
        );
    }
}

#[test]
fn support_contract_binds_docs_workflows_and_resolver() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let ci: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap())
            .expect("ci.yml must parse as YAML");
    let release: serde_yaml::Value = serde_yaml::from_str(
        &fs::read_to_string(root.join(".github/workflows/release.yml")).unwrap(),
    )
    .expect("release.yml must parse as YAML");

    // 1. The documented support table must describe exactly the canonical
    //    target set — no removed target lingering, no aspirational row.
    let documented = documented_support_rows();
    let expected: Vec<(String, String, u32)> = SUPPORT_CONTRACT
        .iter()
        .map(|row| (row.os.to_string(), row.arch.to_string(), row.tier))
        .collect();
    assert_eq!(
        documented, expected,
        "documented support table must equal the canonical support set"
    );

    // 2. Native build/unit matrices (ci.yml and release.yml `check` jobs):
    //    exactly the canonical targets, on runners whose architecture matches
    //    the target, each leg named by tier/target and guarded by an exact
    //    fail-closed native target/arch assertion.
    for (workflow, workflow_name) in [(&ci, "ci.yml"), (&release, "release.yml")] {
        let check_legs = matrix_include(workflow, "check");
        assert_eq!(
            check_legs.len(),
            SUPPORT_CONTRACT.len(),
            "{workflow_name} check matrix must cover exactly the supported targets"
        );
        for row in SUPPORT_CONTRACT {
            let leg = check_legs
                .iter()
                .find(|leg| matrix_field(leg, "check", "target") == row.target)
                .unwrap_or_else(|| {
                    panic!(
                        "{workflow_name} check matrix is missing target {}",
                        row.target
                    )
                });
            let os = matrix_field(leg, "check", "os");
            let arch = matrix_field(leg, "check", "arch");
            assert_eq!(
                runner_arch(os),
                Some(row.arch),
                "{workflow_name} check runner `{os}` has the wrong architecture for {}",
                row.target
            );
            assert_eq!(arch, row.arch, "check leg {} arch mismatch", row.target);
            assert_eq!(
                matrix_tier(leg, "check"),
                row.tier,
                "check leg {} must be named by tier {}",
                row.target,
                row.tier
            );
        }
        assert_native_target_step(
            job_steps(workflow, "check"),
            "check",
            "${{ matrix.target }}",
            "${{ matrix.arch }}",
        );
    }

    // 3. Release build matrix: exactly the canonical targets/assets — an
    //    extra release target (e.g. a re-added macOS x86_64) fails here.
    let build_legs = matrix_include(&release, "build");
    assert_eq!(
        build_legs.len(),
        SUPPORT_CONTRACT.len(),
        "release build matrix must ship exactly the supported targets"
    );
    for row in SUPPORT_CONTRACT {
        let leg = build_legs
            .iter()
            .find(|leg| matrix_field(leg, "build", "target") == row.target)
            .unwrap_or_else(|| panic!("release build matrix is missing target {}", row.target));
        let os = matrix_field(leg, "build", "os");
        assert_eq!(
            runner_arch(os),
            Some(row.arch),
            "release build runner `{os}` has the wrong architecture for {}",
            row.target
        );
        assert_eq!(
            matrix_field(leg, "build", "arch"),
            row.arch,
            "release build leg {} arch mismatch",
            row.target
        );
        assert_eq!(
            matrix_field(leg, "build", "name"),
            format!("togi-{}", row.asset),
            "release build leg {} must package the documented asset",
            row.target
        );
    }
    assert_native_target_step(
        job_steps(&release, "build"),
        "build",
        "${{ matrix.target }}",
        "${{ matrix.arch }}",
    );

    // 4. Post-publication verification: every canonical asset has a
    //    target-bound verify leg, and each tier gets its prescribed evidence.
    let verify_legs = matrix_include(&release, "verify-release");
    assert_eq!(
        verify_legs.len(),
        SUPPORT_CONTRACT.len(),
        "verify-release matrix must verify exactly the shipped assets"
    );
    for row in SUPPORT_CONTRACT {
        let leg = verify_legs
            .iter()
            .find(|leg| matrix_field(leg, "verify-release", "asset") == row.asset)
            .unwrap_or_else(|| panic!("verify-release matrix is missing asset {}", row.asset));
        let os = matrix_field(leg, "verify-release", "os");
        assert_eq!(
            runner_arch(os),
            Some(row.arch),
            "verify-release runner `{os}` has the wrong architecture for {}",
            row.asset
        );
        assert_eq!(matrix_field(leg, "verify-release", "arch"), row.arch);
        assert_eq!(matrix_field(leg, "verify-release", "archive"), row.archive);
        assert_eq!(matrix_field(leg, "verify-release", "binary"), row.binary);
        assert_eq!(
            matrix_tier(leg, "verify-release"),
            row.tier,
            "verify-release leg {} must carry its documented tier",
            row.asset
        );
    }
    let verify_steps = job_steps(&release, "verify-release");
    for tier in [1u32, 2] {
        let (script, evidence) = if tier == 1 {
            (
                "run-released-binary-smoke.sh",
                "Tier 1 must run the released-binary mutation smoke",
            )
        } else {
            (
                "run-released-binary-install-smoke.sh",
                "Tier 2 must run the released-archive install/version smoke",
            )
        };
        assert!(
            verify_steps.iter().any(|step| {
                step_invokes_exactly(step, &format!("bash ./.github/scripts/{script}"))
                    && step_gated_on_tier(step, tier)
            }),
            "{evidence} (an exact `{script}` step gated on matrix.tier == {tier})"
        );
    }
    // Tier 1 keeps its build/toolchain/full-language evidence, bound to the
    // Linux x86_64 Tier-1 row: pinned runner, fail-closed native target/arch
    // assertion, and the exact ignored-test invocation.
    let tier1 = SUPPORT_CONTRACT
        .iter()
        .find(|row| row.tier == 1)
        .expect("support contract must contain a Tier 1 row");
    for (workflow, workflow_name) in [(&ci, "ci.yml"), (&release, "release.yml")] {
        for job in ["integration", "dogfood", "msrv"] {
            let runs_on = workflow
                .get("jobs")
                .and_then(|jobs| jobs.get(job))
                .and_then(|job| job.get("runs-on"))
                .and_then(|runs_on| runs_on.as_str())
                .unwrap_or_else(|| panic!("{workflow_name} job `{job}` must pin runs-on"));
            assert_eq!(
                runner_arch(runs_on),
                Some(tier1.arch),
                "{workflow_name} job `{job}` runner `{runs_on}` has the wrong architecture \
                 for the {} Tier 1 row",
                tier1.target
            );
            assert_ne!(
                runs_on, "ubuntu-latest",
                "{workflow_name} job `{job}` must not run Tier 1 evidence on a moving alias"
            );
            assert_native_target_step(job_steps(workflow, job), job, tier1.target, tier1.arch);
        }
    }
    let integration_steps = job_steps(&ci, "integration");
    assert!(
        integration_steps.iter().any(|step| {
            step.get("run")
                .and_then(|run| run.as_str())
                .is_some_and(|run| run.trim() == "cargo test --locked -- --ignored")
        }),
        "Tier 1 must retain the full ignored integration-test evidence"
    );

    // 5. The asset resolver must resolve each canonical target to its
    //    documented archive and reject macOS x86_64 explicitly.
    if bash_available() {
        for row in SUPPORT_CONTRACT {
            assert_eq!(
                resolve_action_asset(row.uname_s, row.uname_m),
                action_asset(row.archive, row.binary),
                "resolver must bind {} to {}",
                row.asset,
                row.archive
            );
        }
        let output = run_action_resolver("Darwin", "x86_64");
        assert!(
            !output.status.success(),
            "resolver must reject the removed macOS x86_64 target"
        );
    }

    // 6. No workflow may reference the removed macOS x86_64 target or fall
    //    back to moving OS aliases in the native matrices.
    let ci_text = fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();
    let release_text = fs::read_to_string(root.join(".github/workflows/release.yml")).unwrap();
    for stale in ["x86_64-apple-darwin", "macos-x86_64", "togi-macos-x86_64"] {
        assert!(
            !release_text.contains(stale) && !ci_text.contains(stale),
            "workflows must not reference removed target `{stale}`"
        );
    }
    for (workflow, job) in [
        (&ci, "check"),
        (&release, "check"),
        (&release, "build"),
        (&release, "verify-release"),
    ] {
        for leg in matrix_include(workflow, job) {
            let os = matrix_field(&leg, job, "os");
            assert!(
                !["ubuntu-latest", "macos-latest", "windows-latest"].contains(&os),
                "{job} must pin an explicit runner, not moving alias `{os}`"
            );
        }
    }
}

#[test]
fn pr_loop_benchmark_workflow_collects_observational_evidence() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let ci: serde_yaml::Value =
        serde_yaml::from_str(&fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap())
            .expect("ci.yml must parse as YAML");
    let job = ci
        .get("jobs")
        .and_then(|jobs| jobs.get("pr-loop-benchmarks"))
        .expect("ci.yml must define an isolated pr-loop-benchmarks job");
    assert_eq!(
        job.get("runs-on").and_then(|value| value.as_str()),
        Some("ubuntu-24.04")
    );
    assert_eq!(
        job.get("permissions")
            .and_then(|permissions| permissions.get("contents"))
            .and_then(|value| value.as_str()),
        Some("read")
    );
    assert!(
        job.get("continue-on-error").is_none(),
        "benchmark evidence must not mask job failure"
    );
    let steps = job_steps(&ci, "pr-loop-benchmarks");
    let checkout = steps
        .iter()
        .find(|step| {
            step.get("uses").and_then(|value| value.as_str()) == Some("actions/checkout@v7")
        })
        .expect("benchmark job must checkout");
    assert_eq!(
        checkout
            .get("with")
            .and_then(|with| with.get("persist-credentials"))
            .and_then(|value| value.as_bool()),
        Some(false),
        "benchmark checkout must not retain credentials"
    );
    assert_native_target_step(
        steps,
        "pr-loop-benchmarks",
        "x86_64-unknown-linux-gnu",
        "x86_64",
    );
    for (action, toolchain) in [
        (
            "dtolnay/rust-toolchain@29eef336d9b2848a0b548edc03f92a220660cdb8",
            Some("stable"),
        ),
        ("actions/setup-go@v7", Some("1.26.5")),
    ] {
        let step = steps
            .iter()
            .find(|step| step.get("uses").and_then(|value| value.as_str()) == Some(action))
            .unwrap_or_else(|| panic!("benchmark job must use `{action}`"));
        assert_eq!(
            step.get("with")
                .and_then(|with| with.get(if action.starts_with("dtolnay") {
                    "toolchain"
                } else {
                    "go-version"
                }))
                .and_then(|value| value.as_str()),
            toolchain
        );
    }
    let cache = steps
        .iter()
        .find(|step| {
            step.get("uses").and_then(|value| value.as_str())
                == Some("Swatinem/rust-cache@e18b497796c12c097a38f9edb9d0641fb99eee32")
        })
        .expect("benchmark job must use the project Rust cache");
    assert_eq!(
        cache
            .get("with")
            .and_then(|with| with.get("cache-bin"))
            .and_then(|value| value.as_bool()),
        Some(false)
    );

    let prerequisite = steps
        .iter()
        .find(|step| {
            step.get("name").and_then(|value| value.as_str())
                == Some("Check benchmark prerequisites")
        })
        .expect("benchmark job must check harness prerequisites");
    let prerequisite_run = prerequisite
        .get("run")
        .and_then(|value| value.as_str())
        .unwrap();
    for tool in ["bash", "git", "go", "jq", "sed", "sha256sum", "python3"] {
        assert!(
            prerequisite_run.contains(tool),
            "prerequisite step must check `{tool}`"
        );
    }
    assert!(prerequisite_run.contains("apt-get install --yes --no-install-recommends"));

    assert!(
        steps.iter().any(|step| {
            step.get("run").and_then(|value| value.as_str())
                == Some("cargo build --locked --release")
        }),
        "benchmark job must build the release binary"
    );
    let harness = steps
        .iter()
        .find(|step| {
            step.get("name").and_then(|value| value.as_str())
                == Some("Run PR-loop benchmark harness")
        })
        .expect("benchmark job must invoke the real harness");
    assert_eq!(
        harness
            .get("env")
            .and_then(|env| env.get("TOGI_BIN"))
            .and_then(|value| value.as_str()),
        Some("${{ github.workspace }}/target/release/togi")
    );
    assert_eq!(
        harness
            .get("env")
            .and_then(|env| env.get("BENCHMARK_OUTPUT"))
            .and_then(|value| value.as_str()),
        Some("${{ runner.temp }}/togi-pr-loop-benchmarks/measured")
    );
    assert_eq!(
        harness
            .get("env")
            .and_then(|env| env.get("BENCH_GO_BUILD_CACHE_STATE"))
            .and_then(|value| value.as_str()),
        Some("primed")
    );
    assert_eq!(
        harness
            .get("run")
            .and_then(|value| value.as_str())
            .map(str::trim),
        Some("bash benchmarks/pr-loop/run-pr-loop-benchmarks.sh --output \"$BENCHMARK_OUTPUT\"")
    );

    let upload = steps
        .iter()
        .find(|step| {
            step.get("uses").and_then(|value| value.as_str())
                == Some("actions/upload-artifact@043fb46d1a93c77aae656e7c1c64a875d1fc6a0a")
        })
        .expect("benchmark job must upload evidence with a pinned action");
    assert_eq!(
        upload.get("if").and_then(|value| value.as_str()),
        Some("${{ always() }}")
    );
    assert_eq!(
        upload
            .get("with")
            .and_then(|with| with.get("path"))
            .and_then(|value| value.as_str()),
        Some("${{ runner.temp }}/togi-pr-loop-benchmarks")
    );
    assert_eq!(
        upload
            .get("with")
            .and_then(|with| with.get("if-no-files-found"))
            .and_then(|value| value.as_str()),
        Some("warn")
    );
    assert_eq!(
        upload
            .get("with")
            .and_then(|with| with.get("name"))
            .and_then(|value| value.as_str()),
        Some("pr-loop-benchmarks-${{ github.run_id }}-${{ github.run_attempt }}")
    );

    let serialized = serde_yaml::to_string(job).unwrap();
    for forbidden in [
        "continue-on-error",
        "|| true",
        "baseline",
        "threshold",
        "score",
        "gate",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "observational benchmark job must not add `{forbidden}`"
        );
    }
    let compatibility = fs::read_to_string(root.join("docs/COMPATIBILITY.md"))
        .unwrap()
        .replace("\r\n", "\n");
    assert!(
        compatibility.contains("PR-loop Benchmark Evidence")
            && compatibility.contains("Linux x86_64 only")
            && compatibility.contains("telemetry only")
            && compatibility.contains("PR-loop Regression Gate"),
        "compatibility contract must distinguish telemetry-only benchmark evidence from the regression gate"
    );
}
fn write_fake_rustc(bin_dir: &Path, host: &str) {
    let fake_rustc = bin_dir.join("rustc");
    fs::write(
        &fake_rustc,
        format!(
            "#!/usr/bin/env bash\nprintf 'rustc 1.90.0 (fake 2026-01-01)\nbinary: rustc\nhost: {host}\nrelease: 1.90.0\nLLVM version: 20.1.0\n'\n"
        ),
    )
    .unwrap();
    chmod_executable(&fake_rustc);
}

fn run_assert_native_target(bin_dir: &Path, env: &[(&str, &str)]) -> std::process::Output {
    let mut command = std::process::Command::new("bash");
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/scripts/assert-native-target.sh"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin_dir.display()))
        .env_remove("TOGI_EXPECTED_TARGET")
        .env_remove("TOGI_EXPECTED_ARCH");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

// Behavioral coverage for the native target/arch assertion: mocked rustc and
// uname prove the script executes both probes, normalizes uname output, and
// fails closed with a clear diagnostic on each mismatch or missing binding.
#[test]
fn assert_native_target_matches_mismatches_and_requires_env() {
    if !bash_available() {
        eprintln!("skipping native target assertion test because bash is unavailable");
        return;
    }
    let dir = TempDir::new().unwrap();
    let bin_dir = dir.path().join("fake-bin");
    fs::create_dir_all(&bin_dir).unwrap();

    // Positive: a matching native Linux x86_64 runner passes.
    write_fake_rustc(&bin_dir, "x86_64-unknown-linux-gnu");
    write_fake_uname(&bin_dir, "Linux", "x86_64");
    let output = run_assert_native_target(
        &bin_dir,
        &[
            ("TOGI_EXPECTED_TARGET", "x86_64-unknown-linux-gnu"),
            ("TOGI_EXPECTED_ARCH", "x86_64"),
        ],
    );
    assert!(
        output.status.success(),
        "native target assertion failed on a matching runner\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Positive: Darwin arm64 uname output is accepted for the arm64 target.
    write_fake_rustc(&bin_dir, "aarch64-apple-darwin");
    write_fake_uname(&bin_dir, "Darwin", "arm64");
    let output = run_assert_native_target(
        &bin_dir,
        &[
            ("TOGI_EXPECTED_TARGET", "aarch64-apple-darwin"),
            ("TOGI_EXPECTED_ARCH", "arm64"),
        ],
    );
    assert!(
        output.status.success(),
        "native target assertion failed on a matching arm64 macOS runner\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Target mismatch: a different Rust host must fail closed.
    let output = run_assert_native_target(
        &bin_dir,
        &[
            ("TOGI_EXPECTED_TARGET", "x86_64-unknown-linux-gnu"),
            ("TOGI_EXPECTED_ARCH", "x86_64"),
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(
            "Rust host aarch64-apple-darwin does not match required target \
             x86_64-unknown-linux-gnu"
        ),
        "stderr must name the host/target mismatch\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Arch mismatch: the right host triple with the wrong uname architecture
    // (aarch64 normalized to arm64) must fail closed.
    write_fake_rustc(&bin_dir, "x86_64-unknown-linux-gnu");
    write_fake_uname(&bin_dir, "Linux", "aarch64");
    let output = run_assert_native_target(
        &bin_dir,
        &[
            ("TOGI_EXPECTED_TARGET", "x86_64-unknown-linux-gnu"),
            ("TOGI_EXPECTED_ARCH", "x86_64"),
        ],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("Runner architecture arm64 does not match expected x86_64"),
        "stderr must name the normalized arch mismatch\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Missing env: each required binding fails closed with a clear error.
    let output = run_assert_native_target(&bin_dir, &[("TOGI_EXPECTED_ARCH", "x86_64")]);
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TOGI_EXPECTED_TARGET is required"),
        "stderr must name the missing target binding\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let output = run_assert_native_target(
        &bin_dir,
        &[("TOGI_EXPECTED_TARGET", "x86_64-unknown-linux-gnu")],
    );
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("TOGI_EXPECTED_ARCH is required"),
        "stderr must name the missing arch binding\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn release_asset_fetch_verifies_checksum_manifest() {
    if !bash_available() {
        eprintln!("skipping release asset fetch test because bash is unavailable");
        return;
    }

    let archive = "togi-linux-x86_64.tar.gz";
    let archive_bytes = b"fake togi release archive payload";
    let sha = sha256_hex_bytes(archive_bytes);
    let dir = setup_fake_release(archive, archive_bytes, &format!("{sha}  ./{archive}\n"));

    let output = run_release_asset_fetch(&dir, "v9.9.9", archive);
    assert!(
        output.status.success(),
        "release asset fetch failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let path_line = stdout
        .lines()
        .find_map(|line| line.strip_prefix("TOGI_ARCHIVE_PATH="))
        .expect("fetch did not emit TOGI_ARCHIVE_PATH");
    let fetched = dir.path().join("out").join(archive);
    assert_eq!(Path::new(path_line), fetched.as_path());
    assert_eq!(fs::read(&fetched).unwrap(), archive_bytes);
}

#[test]
fn release_asset_fetch_rejects_missing_duplicate_and_mismatched_checksums() {
    if !bash_available() {
        eprintln!("skipping release asset fetch test because bash is unavailable");
        return;
    }

    let archive = "togi-linux-x86_64.tar.gz";
    let archive_bytes = b"fake togi release archive payload";
    let sha = sha256_hex_bytes(archive_bytes);

    for (checksums, expected_error) in [
        // Renamed asset: the manifest only names a different archive.
        (
            format!("{sha}  ./togi-linux-amd64.tar.gz\n"),
            "No checksum found",
        ),
        // Duplicate entries for the same archive.
        (
            format!("{sha}  {archive}\n{sha}  ./{archive}\n"),
            "Duplicate checksum entries",
        ),
        // Checksum does not match the downloaded archive.
        (
            format!("{}  ./{archive}\n", "0".repeat(64)),
            "Checksum mismatch",
        ),
        // Malformed checksum value.
        (format!("not-a-sha  ./{archive}\n"), "Malformed checksum"),
    ] {
        let dir = setup_fake_release(archive, archive_bytes, &checksums);
        let output = run_release_asset_fetch(&dir, "v9.9.9", archive);
        assert!(
            !output.status.success(),
            "release asset fetch unexpectedly succeeded for {expected_error}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "expected `{expected_error}` in stderr, got:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn released_archive_install_smoke_verifies_published_asset() {
    if !bash_available() {
        eprintln!("skipping released-archive install smoke test because bash is unavailable");
        return;
    }
    if command_succeeds("command -v togi") {
        eprintln!("skipping released-archive install smoke test because togi is already on PATH");
        return;
    }

    let asset = host_action_asset();
    let dir = TempDir::new().unwrap();

    let payload_dir = dir.path().join("payload");
    fs::create_dir_all(&payload_dir).unwrap();
    let payload_binary = payload_dir.join(&asset.binary);
    fs::write(
        &payload_binary,
        "#!/usr/bin/env bash\nprintf 'togi 9.9.9\\n'\n",
    )
    .unwrap();
    chmod_executable(&payload_binary);

    let release_dir = dir.path().join("release");
    fs::create_dir_all(&release_dir).unwrap();
    let archive_path = release_dir.join(&asset.archive);
    create_action_archive(&asset, &payload_dir, &archive_path);
    let sha = sha256_hex(&archive_path);
    fs::write(
        release_dir.join("checksums.txt"),
        format!("{sha}  ./{}\n", asset.archive),
    )
    .unwrap();

    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    write_fake_curl(&bin_dir);

    let runner_temp = dir.path().join("runner-temp");
    fs::create_dir_all(&runner_temp).unwrap();
    let github_path = dir.path().join("github_path");
    fs::write(&github_path, "").unwrap();

    let mut paths = vec![bin_dir];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let host_arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "arm64",
        other => panic!("unsupported test host architecture: {other}"),
    };
    let output = std::process::Command::new("bash")
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".github/scripts/run-released-binary-install-smoke.sh"),
        )
        .env("TOGI_VERSION", "v9.9.9")
        .env("TOGI_EXPECTED_ARCH", host_arch)
        .env("TOGI_EXPECTED_ARCHIVE", &asset.archive)
        .env("TOGI_EXPECTED_BINARY", &asset.binary)
        .env("RUNNER_TEMP", &runner_temp)
        .env("GITHUB_PATH", &github_path)
        .env("FAKE_RELEASE_DIR", &release_dir)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env_remove("TOGI_OS")
        .env_remove("TOGI_ARCH")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "released-archive install smoke failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("togi 9.9.9"),
        "smoke did not report the installed version\nstdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        runner_temp.join("togi-bin").join(&asset.binary).exists(),
        "missing installed binary for {}",
        asset.binary
    );
    let github_path_entry = fs::read_to_string(&github_path).unwrap();
    assert!(
        github_path_entry.contains("togi-bin"),
        "GITHUB_PATH did not gain the install dir: {github_path_entry}"
    );
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
#[ignore]
fn released_binary_smoke_stages_complete_go_fixture() {
    if !bash_available() {
        eprintln!("skipping released-binary smoke test because bash is unavailable");
        return;
    }

    let asset = action_asset("togi-linux-x86_64.tar.gz", "togi");
    let dir = TempDir::new().unwrap();
    let payload_dir = dir.path().join("payload");
    fs::create_dir_all(&payload_dir).unwrap();
    let payload_binary = payload_dir.join(&asset.binary);
    fs::copy(assert_cmd::cargo::cargo_bin("togi"), &payload_binary).unwrap();
    chmod_executable(&payload_binary);

    let release_dir = dir.path().join("release");
    fs::create_dir_all(&release_dir).unwrap();
    let archive_path = release_dir.join(&asset.archive);
    create_action_archive(&asset, &payload_dir, &archive_path);
    let sha = sha256_hex(&archive_path);
    fs::write(
        release_dir.join("checksums.txt"),
        format!("{sha}  ./{}\n", asset.archive),
    )
    .unwrap();

    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    write_fake_curl(&bin_dir);
    let cargo = bin_dir.join("cargo");
    fs::write(
        &cargo,
        "#!/usr/bin/env bash\necho 'release smoke must not rebuild togi' >&2\nexit 97\n",
    )
    .unwrap();
    chmod_executable(&cargo);
    let mut paths = vec![bin_dir];
    paths.extend(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
            .filter(|path| !path.join("togi").exists()),
    );

    let runner_temp = dir.path().join("runner-temp");
    fs::create_dir_all(&runner_temp).unwrap();
    let github_path = dir.path().join("github-path");
    fs::write(&github_path, "").unwrap();
    let version = format!("v{}", env!("CARGO_PKG_VERSION"));
    let output = std::process::Command::new("bash")
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".github/scripts/run-released-binary-smoke.sh"),
        )
        .env("GITHUB_WORKSPACE", env!("CARGO_MANIFEST_DIR"))
        .env("TOGI_VERSION", &version)
        .env("TOGI_EXPECTED_ARCH", "x86_64")
        .env("TOGI_EXPECTED_ARCHIVE", &asset.archive)
        .env("TOGI_EXPECTED_BINARY", &asset.binary)
        .env("RUNNER_TEMP", &runner_temp)
        .env("GITHUB_PATH", &github_path)
        .env("FAKE_RELEASE_DIR", &release_dir)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env_remove("TOGI_OS")
        .env_remove("TOGI_ARCH")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "released-binary smoke failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "SURVIVED: IsPositive(0) changes from false to true;",
        "Unchanged weak tests: repair correctly rejected.",
        "Demo complete: the added test kills the recorded boundary mutation.",
    ] {
        assert!(stdout.contains(expected), "missing {expected:?}: {stdout}");
    }
}

#[test]
fn release_target_binding_rejects_mismatched_host_and_asset() {
    if !bash_available() {
        eprintln!("skipping release target binding test because bash is unavailable");
        return;
    }

    // Success: host matches the expected arm64 macOS target exactly.
    let output = run_assert_release_target(
        "Darwin",
        "arm64",
        "arm64",
        "togi-macos-arm64.tar.gz",
        "togi",
    );
    assert!(
        output.status.success(),
        "target binding failed on a matching host\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("TOGI_ARCHIVE=togi-macos-arm64.tar.gz"));
    assert!(stdout.contains("TOGI_BINARY=togi"));

    for (sys, machine, arch, archive, binary, expected_error) in [
        // Intel host must not pass as an arm64 leg.
        (
            "Darwin",
            "x86_64",
            "arm64",
            "togi-macos-arm64.tar.gz",
            "togi",
            "Runner architecture x86_64 does not match expected arm64",
        ),
        // Intel macOS is not a shipped target: the resolver must reject the
        // host explicitly before any archive binding is emitted.
        (
            "Darwin",
            "x86_64",
            "x86_64",
            "togi-macos-arm64.tar.gz",
            "togi",
            "unsupported release target: macos-x86_64",
        ),
        // Binary name must match the expected target too.
        (
            "Linux",
            "x86_64",
            "x86_64",
            "togi-linux-x86_64.tar.gz",
            "togi.exe",
            "Resolved binary togi does not match expected togi.exe",
        ),
    ] {
        let output = run_assert_release_target(sys, machine, arch, archive, binary);
        assert!(
            !output.status.success(),
            "target binding unexpectedly succeeded for {expected_error}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "expected `{expected_error}` in stderr, got:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn release_identity_verification_enforces_tag_head_and_release_association() {
    if !bash_available() {
        eprintln!("skipping release identity test because bash is unavailable");
        return;
    }
    if !jq_available() {
        eprintln!("skipping release identity test because jq is unavailable");
        return;
    }

    let version = "v9.9.9";
    let tag_sha = "a".repeat(40);
    let peeled_sha = "b".repeat(40);
    let release_json =
        r#"{"tag_name":"v9.9.9","draft":false,"prerelease":false,"target_commitish":"main"}"#;

    // Annotated tag: peeled commit establishes source identity.
    let ls_remote =
        format!("{tag_sha}\trefs/tags/{version}\n{peeled_sha}\trefs/tags/{version}^{{}}");
    let output = run_release_identity(version, &peeled_sha, &ls_remote, Some(release_json));
    assert!(
        output.status.success(),
        "release identity verification failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(&format!("tag {version} resolves to commit {peeled_sha}")));
    assert!(stdout.contains("target_commitish metadata: main"));

    // Lightweight tag: the tag ref itself is the commit.
    let output = run_release_identity(
        version,
        &tag_sha,
        &format!("{tag_sha}\trefs/tags/{version}"),
        Some(release_json),
    );
    assert!(
        output.status.success(),
        "lightweight tag verification failed\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Moved tag: peeled commit no longer matches the workflow head.
    let output = run_release_identity(version, &tag_sha, &ls_remote, Some(release_json));
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not match workflow head"),
        "moved tag was not rejected: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Missing tag.
    let output = run_release_identity(version, &peeled_sha, "", Some(release_json));
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("does not resolve"),
        "missing tag was not rejected: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Missing public release association.
    let output = run_release_identity(version, &peeled_sha, &ls_remote, None);
    assert!(
        !output.status.success(),
        "missing release association unexpectedly succeeded"
    );

    for (json, expected_error) in [
        (
            r#"{"tag_name":"v9.9.8","draft":false,"prerelease":false,"target_commitish":"main"}"#,
            "does not match v9.9.9",
        ),
        (
            r#"{"tag_name":"v9.9.9","draft":true,"prerelease":false,"target_commitish":"main"}"#,
            "is a draft",
        ),
        (
            r#"{"tag_name":"v9.9.9","draft":false,"prerelease":true,"target_commitish":"main"}"#,
            "is a prerelease",
        ),
    ] {
        let output = run_release_identity(version, &peeled_sha, &ls_remote, Some(json));
        assert!(
            !output.status.success(),
            "release identity unexpectedly succeeded for {expected_error}"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "expected `{expected_error}` in stderr, got:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn run_assert_release_target(
    sys: &str,
    machine: &str,
    arch: &str,
    archive: &str,
    binary: &str,
) -> std::process::Output {
    let dir = TempDir::new().unwrap();
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    write_fake_uname(&bin_dir, sys, machine);
    let mut paths = vec![bin_dir];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::process::Command::new("bash")
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/scripts/assert-release-target.sh"))
        .env("TOGI_EXPECTED_ARCH", arch)
        .env("TOGI_EXPECTED_ARCHIVE", archive)
        .env("TOGI_EXPECTED_BINARY", binary)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env_remove("TOGI_OS")
        .env_remove("TOGI_ARCH")
        .output()
        .unwrap()
}

fn run_release_identity(
    version: &str,
    head: &str,
    ls_remote: &str,
    release_json: Option<&str>,
) -> std::process::Output {
    let dir = TempDir::new().unwrap();
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&bin_dir).unwrap();
    let fake_git = bin_dir.join("git");
    fs::write(&fake_git, FAKE_GIT).unwrap();
    chmod_executable(&fake_git);
    write_fake_curl(&bin_dir);

    let release_dir = dir.path().join("release");
    fs::create_dir_all(&release_dir).unwrap();
    if let Some(json) = release_json {
        fs::write(release_dir.join(version), json).unwrap();
    }

    let mut paths = vec![bin_dir];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::process::Command::new("bash")
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".github/scripts/verify-release-identity.sh"),
        )
        .env("TOGI_VERSION", version)
        .env("TOGI_WORKFLOW_HEAD", head)
        .env("FAKE_LS_REMOTE", ls_remote)
        .env("FAKE_RELEASE_DIR", &release_dir)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap()
}

const FAKE_GIT: &str = r#"#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = "ls-remote" ]; then
  if [ -n "${FAKE_LS_REMOTE:-}" ]; then
    printf '%s\n' "$FAKE_LS_REMOTE"
  fi
  exit 0
fi
echo "fake git: unexpected arguments: $*" >&2
exit 1
"#;

fn write_fake_uname(bin_dir: &Path, sys: &str, machine: &str) {
    let fake_uname = bin_dir.join("uname");
    fs::write(
        &fake_uname,
        format!(
            "#!/usr/bin/env bash\ncase \"${{1:-}}\" in\n  -s) printf '%s\\n' '{sys}' ;;\n  -m) printf '%s\\n' '{machine}' ;;\n  *) printf '%s\\n' '{sys}' ;;\nesac\n"
        ),
    )
    .unwrap();
    chmod_executable(&fake_uname);
}

const FAKE_CURL: &str = r#"#!/usr/bin/env bash
set -euo pipefail
: "${FAKE_RELEASE_DIR:?FAKE_RELEASE_DIR is required}"
out=""
url=""
while [ $# -gt 0 ]; do
  case "$1" in
    -*o)
      out="$2"
      shift 2
      ;;
    -*)
      shift
      ;;
    *)
      url="$1"
      shift
      ;;
  esac
done
[ -n "$url" ] || exit 2
src="${FAKE_RELEASE_DIR}/$(basename "$url")"
if [ ! -f "$src" ]; then
  echo "fake curl: no such release asset: $url" >&2
  exit 22
fi
if [ -n "$out" ]; then
  cp "$src" "$out"
else
  cat "$src"
fi
"#;

fn write_fake_curl(bin_dir: &Path) {
    let fake_curl = bin_dir.join("curl");
    fs::write(&fake_curl, FAKE_CURL).unwrap();
    chmod_executable(&fake_curl);
}

fn setup_fake_release(archive: &str, archive_bytes: &[u8], checksums: &str) -> TempDir {
    let dir = TempDir::new().unwrap();
    let release_dir = dir.path().join("release");
    let bin_dir = dir.path().join("bin");
    fs::create_dir_all(&release_dir).unwrap();
    fs::create_dir_all(&bin_dir).unwrap();
    fs::write(release_dir.join(archive), archive_bytes).unwrap();
    fs::write(release_dir.join("checksums.txt"), checksums).unwrap();
    write_fake_curl(&bin_dir);
    dir
}

fn run_release_asset_fetch(dir: &TempDir, version: &str, archive: &str) -> std::process::Output {
    let mut paths = vec![dir.path().join("bin")];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    std::process::Command::new("bash")
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(".github/scripts/fetch-togi-release-asset.sh"),
        )
        .env("TOGI_VERSION", version)
        .env("TOGI_ARCHIVE", archive)
        .env("TOGI_FETCH_DIR", dir.path().join("out"))
        .env("FAKE_RELEASE_DIR", dir.path().join("release"))
        .env("PATH", std::env::join_paths(paths).unwrap())
        .output()
        .unwrap()
}

fn command_succeeds(command: &str) -> bool {
    std::process::Command::new("bash")
        .arg("-c")
        .arg(command)
        .output()
        .unwrap()
        .status
        .success()
}

fn sha256_hex(path: &Path) -> String {
    sha256_hex_bytes(&fs::read(path).unwrap())
}

fn sha256_hex_bytes(bytes: &[u8]) -> String {
    use sha2::Digest;
    use std::fmt::Write;
    let digest = sha2::Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest.as_slice() {
        write!(hex, "{byte:02x}").unwrap();
    }
    hex
}

#[derive(Debug, PartialEq, Eq)]
struct ActionAsset {
    archive: String,
    binary: String,
}

fn action_asset(archive: &str, binary: &str) -> ActionAsset {
    ActionAsset {
        archive: archive.to_string(),
        binary: binary.to_string(),
    }
}

fn run_action_resolver(os: &str, arch: &str) -> std::process::Output {
    let helper =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/scripts/resolve-togi-asset.sh");
    std::process::Command::new("bash")
        .arg(helper)
        .env("TOGI_OS", os)
        .env("TOGI_ARCH", arch)
        .output()
        .unwrap()
}

fn resolve_action_asset(os: &str, arch: &str) -> ActionAsset {
    parse_action_asset(&run_action_resolver(os, arch))
}

fn host_action_asset() -> ActionAsset {
    let helper =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/scripts/resolve-togi-asset.sh");
    let output = std::process::Command::new("bash")
        .arg(helper)
        .env_remove("TOGI_OS")
        .env_remove("TOGI_ARCH")
        .output()
        .unwrap();
    parse_action_asset(&output)
}

fn parse_action_asset(output: &std::process::Output) -> ActionAsset {
    assert!(
        output.status.success(),
        "resolver failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut archive = None;
    let mut binary = None;
    for line in stdout.lines() {
        if let Some(value) = line.strip_prefix("TOGI_ARCHIVE=") {
            archive = Some(value.to_string());
        } else if let Some(value) = line.strip_prefix("TOGI_BINARY=") {
            binary = Some(value.to_string());
        } else {
            panic!("unexpected resolver output line: {line}");
        }
    }

    ActionAsset {
        archive: archive.expect("resolver did not emit TOGI_ARCHIVE"),
        binary: binary.expect("resolver did not emit TOGI_BINARY"),
    }
}

fn assert_action_installs_asset(asset: &ActionAsset, fake_cygpath: bool) {
    let dir = TempDir::new().unwrap();
    let payload_dir = dir.path().join("payload");
    fs::create_dir_all(&payload_dir).unwrap();
    fs::write(
        payload_dir.join(&asset.binary),
        "#!/usr/bin/env bash\nexit 0\n",
    )
    .unwrap();
    create_action_archive(asset, &payload_dir, &dir.path().join(&asset.archive));

    let github_path = dir.path().join("github_path");
    fs::write(&github_path, "").unwrap();

    let mut command = std::process::Command::new("bash");
    command
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/scripts/install-togi-archive.sh"))
        .env("RUNNER_TEMP", dir.path())
        .env("TOGI_ARCHIVE", &asset.archive)
        .env("TOGI_BINARY", &asset.binary)
        .env("GITHUB_PATH", &github_path);

    if fake_cygpath {
        let fake_bin = dir.path().join("fake-bin");
        fs::create_dir_all(&fake_bin).unwrap();
        let fake_cygpath_path = fake_bin.join("cygpath");
        fs::write(
            &fake_cygpath_path,
            "#!/usr/bin/env bash\ncase \"$1\" in\n  -u) printf '%s\\n' \"$2\" ;;\n  -w) printf 'C:\\\\togi\\\\%s\\n' \"$(basename \"$2\")\" ;;\n  *) exit 1 ;;\nesac\n",
        )
        .unwrap();
        chmod_executable(&fake_cygpath_path);

        let mut paths = vec![fake_bin];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        command.env("PATH", std::env::join_paths(paths).unwrap());
    }

    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "install helper failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let install_dir = dir.path().join("togi-bin");
    let installed = install_dir.join(&asset.binary);
    assert!(
        installed.exists(),
        "missing installed binary: {installed:?}"
    );
    assert_bash_test_x(&installed);

    let github_path_entry = fs::read_to_string(&github_path).unwrap();
    let github_path_entry = github_path_entry.trim();
    if fake_cygpath {
        assert_eq!(github_path_entry, "C:\\togi\\togi-bin");
    } else if cfg!(windows) {
        assert!(
            github_path_entry.contains("\\togi-bin") && !github_path_entry.starts_with('/'),
            "expected Windows GITHUB_PATH entry, got {github_path_entry}"
        );
    } else {
        assert_eq!(github_path_entry, install_dir.display().to_string());
    }
}

fn create_action_archive(asset: &ActionAsset, payload_dir: &Path, archive_path: &Path) {
    if asset.archive.ends_with(".tar.gz") {
        assert_command_success(
            std::process::Command::new("tar")
                .arg("czf")
                .arg(archive_path)
                .arg("-C")
                .arg(payload_dir)
                .arg(&asset.binary)
                .output()
                .unwrap(),
            "tar archive creation",
        );
    } else {
        create_zip_archive(payload_dir, archive_path, &asset.binary);
    }
}

fn create_zip_archive(payload_dir: &Path, archive_path: &Path, binary: &str) {
    if cfg!(windows) {
        assert_command_success(
            std::process::Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    "Compress-Archive -LiteralPath $env:TOGI_TEST_BINARY -DestinationPath $env:TOGI_TEST_ARCHIVE",
                ])
                .env("TOGI_TEST_BINARY", payload_dir.join(binary))
                .env("TOGI_TEST_ARCHIVE", archive_path)
                .output()
                .unwrap(),
            "zip archive creation",
        );
    } else {
        assert_command_success(
            std::process::Command::new("zip")
                .arg("-q")
                .arg(archive_path)
                .arg(binary)
                .current_dir(payload_dir)
                .output()
                .unwrap(),
            "zip archive creation",
        );
    }
}

fn chmod_executable(path: &Path) {
    assert_command_success(
        std::process::Command::new("bash")
            .arg("-c")
            .arg("chmod +x \"$1\"")
            .arg("chmod")
            .arg(path)
            .output()
            .unwrap(),
        "chmod +x",
    );
}

fn assert_bash_test_x(path: &Path) {
    assert_command_success(
        std::process::Command::new("bash")
            .arg("-c")
            .arg("path=$1; if command -v cygpath >/dev/null 2>&1; then path=$(cygpath -u \"$path\"); fi; test -x \"$path\"")
            .arg("test")
            .arg(path)
            .output()
            .unwrap(),
        "test -x",
    );
}

fn assert_command_success(output: std::process::Output, context: &str) {
    assert!(
        output.status.success(),
        "{context} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn explain_reads_json_report() {
    let dir = TempDir::new().unwrap();
    let report = r#"{
  "total": 2,
  "tested": 2,
  "killed": 1,
  "survived": 1,
  "timeout": 0,
  "build_errors": 0,
  "mutation_score": 0.0,
  "duration_ms": 10,
  "test_command": ["cargo", "test"],
  "build_command": ["cargo", "check"],
  "mutations": [
    {
      "id": 1,
      "file": "src/main.go",
      "line": 3,
      "operator": "plus_to_minus",
      "description": "Replace + with -",
      "result": "killed",
      "original": "+",
      "replacement": "-"
    },
    {
      "id": 2,
      "file": "src/main.go",
      "line": 4,
      "operator": "gt_to_gte",
      "description": "Replace > with >=",
      "result": "survived",
      "original": ">",
      "replacement": ">=",
      "diff": "--- a/src/main.go\n+++ b/src/main.go\n@@ -1 +1 @@\n-a > b\n+a >= b\n"
    }
  ]
}"#;
    let report_path = dir.path().join("togi-report.json");
    fs::write(&report_path, report).unwrap();

    togi()
        .args([
            "explain",
            "2",
            "--report",
            report_path.to_str().expect("report path should be utf-8"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Mutation #2"))
        .stdout(predicate::str::contains("src/main.go:4"))
        .stdout(predicate::str::contains(
            r#"Test command: ["cargo","test"]"#,
        ))
        .stdout(predicate::str::contains(
            r#"Build check: ["cargo","check"]"#,
        ))
        .stdout(predicate::str::contains("Why it survived"));

    togi()
        .args([
            "explain",
            "1",
            "--report",
            report_path.to_str().expect("report path should be utf-8"),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("Change: + -> -"))
        .stdout(predicate::str::contains("Why it was killed"));
}

#[test]
fn explain_reports_reused_cache_verdict_without_claiming_a_test_ran() {
    let dir = TempDir::new().unwrap();
    let report_path = dir.path().join("togi-report.json");
    fs::write(
        &report_path,
        r#"{
  "test_command": ["cargo", "test"],
  "mutations": [{
    "id": 1,
    "file": "src/main.go",
    "line": 3,
    "operator": "plus_to_minus",
    "description": "Replace + with -",
    "result": "survived",
    "execution": {"state": "exact_cache"}
  }]
}"#,
    )
    .unwrap();

    let output = togi()
        .args([
            "explain",
            "1",
            "--report",
            report_path.to_str().expect("report path should be utf-8"),
        ])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "explain cached verdict failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Execution: reused from exact cache"));
    assert!(stdout.contains("Test command: not run."));
    assert!(!stdout.contains(r#"Test command: ["cargo","test"]"#));
}

#[test]
fn explain_reports_incremental_history_and_structured_nonexecution() {
    let dir = TempDir::new().unwrap();

    for (index, (label, result, state, reason, execution_text)) in [
        (
            "incremental history",
            "survived",
            "incremental_history",
            None,
            "reused from incremental history",
        ),
        (
            "build error",
            "build_error",
            "not_executed",
            Some("build_error"),
            "not executed (build_error)",
        ),
        (
            "uncovered",
            "uncovered",
            "not_executed",
            Some("uncovered"),
            "not executed (uncovered)",
        ),
        (
            "subsumed",
            "subsumed",
            "not_executed",
            Some("subsumed"),
            "not executed (subsumed)",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let mut execution = serde_json::json!({"state": state});
        if let Some(reason) = reason {
            execution["reason"] = serde_json::Value::String(reason.into());
        }
        let report_path = dir.path().join(format!("report-{index}.json"));
        fs::write(
            &report_path,
            serde_json::to_string(&serde_json::json!({
                "test_command": ["cargo", "test"],
                "mutations": [{
                    "id": 1,
                    "file": "src/main.go",
                    "line": 3,
                    "operator": "plus_to_minus",
                    "description": label,
                    "result": result,
                    "execution": execution,
                }],
            }))
            .unwrap(),
        )
        .unwrap();

        let output = togi()
            .args([
                "explain",
                "1",
                "--report",
                report_path.to_str().expect("report path should be utf-8"),
            ])
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{label} explain failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains(&format!("Execution: {execution_text}")),
            "{label} stdout:\n{stdout}"
        );
        assert!(
            stdout.contains("Test command: not run."),
            "{label} stdout:\n{stdout}"
        );
        assert!(
            !stdout.contains(r#"Test command: ["cargo","test"]"#),
            "{label} stdout:\n{stdout}"
        );
    }
}

#[test]
fn check_invalid_config_exits_two() {
    let dir = setup_git_repo();
    fs::write(dir.path().join("togi.toml"), "invalid {{{{ toml").unwrap();

    togi()
        .args(["check", "--base", "HEAD"])
        .current_dir(dir.path())
        .assert()
        .code(2)
        .stderr(predicate::str::contains("Error"));
}

#[test]
fn clean_removes_cache_dir() {
    let dir = setup_git_repo();
    let cache_dir = dir.path().join(".togi-cache");
    fs::create_dir_all(&cache_dir).unwrap();
    fs::write(cache_dir.join("entry-1"), "{}").unwrap();
    fs::write(cache_dir.join("entry-2"), "{}").unwrap();
    assert!(cache_dir.exists());

    togi()
        .arg("clean")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Cache cleared"));

    assert!(!cache_dir.exists(), ".togi-cache should be removed");
}

#[test]
fn clean_succeeds_when_cache_missing() {
    let dir = setup_git_repo();
    assert!(!dir.path().join(".togi-cache").exists());

    togi()
        .arg("clean")
        .current_dir(dir.path())
        .assert()
        .success()
        .stdout(predicate::str::contains("Cache cleared"));
}

#[test]
fn list_operators_prints_known_ids_and_categories() {
    let output = togi().arg("list-operators").assert().success();
    let stdout = String::from_utf8(output.get_output().stdout.clone()).unwrap();

    // Known operator IDs (one per category) should appear in output.
    for id in [
        "lt_to_lte",        // binary
        "true_to_false",    // literal
        "plus_to_minus",    // boundary
        "remove_if_body",   // removal
        "remove_unary_not", // unary
        "remove_break",     // loop
        "negate_condition", // negate
        "return_empty",     // return
    ] {
        assert!(
            stdout.contains(id),
            "expected list-operators output to contain '{id}', got:\n{stdout}"
        );
    }

    // All expected category headers should appear.
    for cat in [
        "binary:",
        "literal:",
        "boundary:",
        "removal:",
        "unary:",
        "loop:",
        "negate:",
        "return:",
    ] {
        assert!(
            stdout.contains(cat),
            "expected list-operators output to contain category '{cat}', got:\n{stdout}"
        );
    }
}

const DOGFOOD_BADGE_LABEL: &str = "mutation score (dogfood: src/report/json.rs)";
const DOGFOOD_BADGE_ENDPOINT: &str =
    "https://raw.githubusercontent.com/Darkroom4364/togi/badges/mutation-score.json";
const DOGFOOD_BADGE_JOB_NAME: &str = "Update mutation score (dogfood: src/report/json.rs) badge";
const DOGFOOD_BADGE_STEP_NAME: &str = "Run mutation score (dogfood: src/report/json.rs) check";

struct DogfoodBadgeFixture {
    dir: TempDir,
    fake_togi: PathBuf,
    invocation_log: PathBuf,
    endpoint: PathBuf,
}

fn dogfood_badge_fixture() -> DogfoodBadgeFixture {
    let dir = TempDir::new().unwrap();
    let fake_togi = dir.path().join("fake-togi.sh");
    let invocation_log = dir.path().join("invocations.log");
    let endpoint = dir.path().join("mutation-score.json");
    fs::write(
        &fake_togi,
        r#"#!/usr/bin/env bash
set -euo pipefail
for argument in "$@"; do
  printf '<%s>\n' "$argument" >> "$FAKE_TOGI_LOG"
done
printf '%s\n' '--' >> "$FAKE_TOGI_LOG"
printf '%s\n' "${FAKE_TOGI_REPORT:?}"
exit "${FAKE_TOGI_STATUS:-0}"
"#,
    )
    .unwrap();
    let output = std::process::Command::new("bash")
        .args(["-c", "chmod +x \"$1\"", "--"])
        .arg(&fake_togi)
        .output()
        .unwrap();
    assert!(output.status.success());

    DogfoodBadgeFixture {
        dir,
        fake_togi,
        invocation_log,
        endpoint,
    }
}

fn run_dogfood_badge_generator(
    fixture: &DogfoodBadgeFixture,
    report: &str,
    status: i32,
) -> std::process::Output {
    std::process::Command::new("bash")
        .arg(
            Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/scripts/dogfood-mutation-score.sh"),
        )
        .arg(&fixture.endpoint)
        .current_dir(fixture.dir.path())
        .env("TOGI_BIN", &fixture.fake_togi)
        .env("RUNNER_TEMP", fixture.dir.path())
        .env("FAKE_TOGI_LOG", &fixture.invocation_log)
        .env("FAKE_TOGI_REPORT", report)
        .env("FAKE_TOGI_STATUS", status.to_string())
        .output()
        .unwrap()
}

fn dogfood_badge_invocation(fixture: &DogfoodBadgeFixture) -> Vec<String> {
    let log = fs::read_to_string(&fixture.invocation_log).unwrap();
    let mut lines = log.lines();
    let mut args = Vec::new();
    loop {
        let line = lines
            .next()
            .expect("fake togi invocation must terminate with `--`");
        if line == "--" {
            break;
        }
        assert!(
            line.starts_with('<') && line.ends_with('>'),
            "unexpected fake togi invocation line: {line}"
        );
        args.push(line[1..line.len() - 1].to_string());
    }
    assert!(
        lines.next().is_none(),
        "dogfood badge generator must invoke togi exactly once"
    );
    args
}

#[test]
fn dogfood_badge_generator_scopes_the_bounded_healthy_check() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping dogfood badge generator test because bash or jq is unavailable");
        return;
    }

    let fixture = dogfood_badge_fixture();
    let output = run_dogfood_badge_generator(
        &fixture,
        r#"{"kind":"mutation_report","schema_version":1,"tested":4,"timeout":0,"build_errors":0,"partial":false,"mutation_score":75.6,"survived":1}"#,
        1,
    );

    assert!(
        output.status.success(),
        "generator failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        dogfood_badge_invocation(&fixture),
        action_args(&[
            "check",
            "--all",
            "--path",
            "src/report/json.rs",
            "--test-cmd",
            "cargo test --locked",
            "--calibrate-timeout",
            "--timeout-multiplier",
            "4",
            "--timeout-slack",
            "2",
            "--format",
            "json",
        ])
    );
    let endpoint: serde_json::Value =
        serde_json::from_slice(&fs::read(&fixture.endpoint).unwrap()).unwrap();
    assert_eq!(
        endpoint,
        serde_json::json!({
            "schemaVersion": 1,
            "label": DOGFOOD_BADGE_LABEL,
            "message": "76% (4 tested)",
            "color": "brightgreen",
        })
    );
}

#[test]
fn dogfood_badge_generator_reports_dynamic_tested_count() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping dogfood badge generator test because bash or jq is unavailable");
        return;
    }

    let fixture = dogfood_badge_fixture();
    let output = run_dogfood_badge_generator(
        &fixture,
        r#"{"kind":"mutation_report","schema_version":1,"tested":17,"timeout":0,"build_errors":0,"partial":false,"mutation_score":75.6,"survived":4}"#,
        1,
    );

    assert!(
        output.status.success(),
        "generator failed:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let endpoint: serde_json::Value =
        serde_json::from_slice(&fs::read(&fixture.endpoint).unwrap()).unwrap();
    assert_eq!(
        endpoint.get("message").and_then(|message| message.as_str()),
        Some("76% (17 tested)")
    );
}

#[test]
fn dogfood_badge_generator_rejects_partial_or_invalid_reports_without_endpoint() {
    if !bash_available() || !jq_available() {
        eprintln!("skipping dogfood badge generator test because bash or jq is unavailable");
        return;
    }

    for (case, report) in [
        (
            "partial",
            r#"{"tested":4,"timeout":0,"build_errors":0,"partial":true,"mutation_score":75.6}"#,
        ),
        (
            "no tested mutations",
            r#"{"tested":0,"timeout":0,"build_errors":0,"partial":false,"mutation_score":75.6}"#,
        ),
        (
            "timeout",
            r#"{"tested":4,"timeout":1,"build_errors":0,"partial":false,"mutation_score":75.6}"#,
        ),
        (
            "build errors",
            r#"{"tested":4,"timeout":0,"build_errors":1,"partial":false,"mutation_score":75.6}"#,
        ),
        ("malformed JSON", "not JSON"),
    ] {
        let fixture = dogfood_badge_fixture();
        let output = run_dogfood_badge_generator(&fixture, report, 0);

        assert_eq!(
            output.status.code(),
            Some(1),
            "{case} report unexpectedly succeeded:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            !fixture.endpoint.exists(),
            "{case} report must not produce an endpoint"
        );
    }
}

#[test]
fn dogfood_badge_scope_contract_binds_docs_generator_workflow_and_public_route() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let readme = fs::read_to_string(root.join("README.md")).unwrap();
    let expected_badge = format!(
        "[![{DOGFOOD_BADGE_LABEL}](https://img.shields.io/endpoint?url={DOGFOOD_BADGE_ENDPOINT})]"
    );
    assert!(
        readme.contains(&expected_badge),
        "README must expose the scoped badge through its existing public route"
    );
    assert!(
        readme.contains(&format!(
            "The {DOGFOOD_BADGE_LABEL} badge presents both percentage and tested mutant count for its bounded `src/report/json.rs` selector; it is not a repository-wide score."
        )),
        "README must disclose the scoped percentage and tested mutant count"
    );

    let generator = fs::read_to_string(root.join(".github/scripts/dogfood-mutation-score.sh"))
        .unwrap()
        .replace("\r\n", "\n");
    let blocking_smoke = fs::read_to_string(root.join(".github/scripts/run-dogfood-smoke.sh"))
        .unwrap()
        .replace("\r\n", "\n");
    let bounded_selector = concat!(
        "  --all \\\n",
        "  --path src/report/json.rs \\\n",
        "  --test-cmd \"cargo test --locked\" \\\n",
        "  --calibrate-timeout \\\n",
        "  --timeout-multiplier 4 \\\n",
        "  --timeout-slack 2 \\\n",
        "  --format json \\\n",
    );
    for (script_name, script) in [
        ("badge generator", &generator),
        ("blocking dogfood smoke", &blocking_smoke),
    ] {
        assert!(
            script.contains(bounded_selector),
            "{script_name} must retain the bounded dogfood selector"
        );
    }
    assert!(
        generator.contains(&format!("label: \"{DOGFOOD_BADGE_LABEL}\"")),
        "generator must emit the same scoped badge label"
    );
    assert!(
        generator.contains(r#"output_path="${1:-mutation-score.json}""#),
        "generator must retain the published endpoint filename"
    );

    let workflow_text =
        fs::read_to_string(root.join(".github/workflows/dogfood-badge.yml")).unwrap();
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(&workflow_text).expect("dogfood badge workflow must parse as YAML");
    let job = workflow
        .get("jobs")
        .and_then(|jobs| jobs.get("mutation-score"))
        .expect("dogfood badge workflow must retain its mutation-score job");
    assert_eq!(
        job.get("name").and_then(|name| name.as_str()),
        Some(DOGFOOD_BADGE_JOB_NAME)
    );
    let steps = job_steps(&workflow, "mutation-score");
    let badge_step = steps
        .iter()
        .find(|step| {
            step.get("name").and_then(|name| name.as_str()) == Some(DOGFOOD_BADGE_STEP_NAME)
        })
        .expect("workflow must name its scoped badge generator step");
    assert_eq!(
        badge_step
            .get("run")
            .and_then(|run| run.as_str())
            .map(str::trim),
        Some("./.github/scripts/dogfood-mutation-score.sh \"$RUNNER_TEMP/mutation-score.json\"")
    );
    let publish_step = steps
        .iter()
        .find(|step| {
            step.get("name").and_then(|name| name.as_str()) == Some("Publish to badges branch")
        })
        .expect("workflow must retain the badge publication step");
    let publish_run = publish_step
        .get("run")
        .and_then(|run| run.as_str())
        .expect("badge publication step must run a script");
    for required in [
        "git checkout badges",
        "git checkout --orphan badges",
        "cp \"$RUNNER_TEMP/mutation-score.json\" mutation-score.json",
    ] {
        assert!(
            publish_run.contains(required),
            "badge publication must retain `{required}`"
        );
    }
}
