// Parallel test execution with timeouts

use crate::cache::{self, CacheKey};
use crate::config::BuildCommandOrigin;
use crate::replay::{DirectRecipeOrigin, RegularDirectRecipe};
use crate::source_identity::{
    normalized_project_relative_path, range_matches, resolve_normalized_project_relative_path,
    source_fingerprint,
};
use crate::{
    BuildErrorDiagnostic, Mutation, MutationExecution, MutationReport, MutationResult,
    SchemataFallbackReasonCount, SchemataReport, SurvivorConfirmation, TestSelectionProvenance,
};
use anyhow::{Context, bail};
#[cfg(not(windows))]
use cap_fs_ext::MetadataExt as CapMetadataExt;
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::ambient_authority;
use cap_std::fs::{Dir as CapDir, OpenOptions as CapOpenOptions};
use cap_tempfile::TempFile;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs;
use std::hash::Hasher;
use std::io::{IsTerminal, Read, Write};
use std::panic::{AssertUnwindSafe as PanicBoundary, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const CAPTURED_OUTPUT_LIMIT: usize = 1024 * 1024;
const CAPTURE_CLEANUP_TIMEOUT: Duration = Duration::from_millis(100);

/// Write mutation workspace content. Workspaces are disposable temp copies, so
/// durable temp-file + fsync writes would only add hot-loop I/O overhead.
fn write_workspace_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    fs::write(path, data)
}

/// Commands and timeouts used while evaluating mutations.
///
/// `command` is the default test command. Project and language commands can
/// override it for matching mutations unless `force_default_command` is set
/// by a CLI command override. An enabled build command runs before tests to
/// classify uncompilable mutations as build errors; its origin records whether
/// it was auto-detected or user-configured.
pub struct CommandConfig {
    /// Default test command, stored as argv.
    pub command: Vec<String>,
    /// Optional wrapper prefixed to every build and test command.
    pub sandbox_command: Vec<String>,
    /// True when the default command came from a CLI override and must win.
    pub force_default_command: bool,
    /// True when the default timeout came from a CLI override and must win.
    pub force_default_timeout: bool,
    /// Per-project test command and timeout overrides, longest path wins.
    pub project_commands: Vec<ProjectCommandConfig>,
    /// Per-language test command overrides keyed by `LanguageSupport::name()`.
    pub language_commands: HashMap<String, Vec<String>>,
    /// Optional build-check command, stored as argv.
    pub build_command: Vec<String>,
    /// How the effective build command was selected.
    pub build_command_origin: BuildCommandOrigin,
    /// Default per-mutation timeout.
    pub timeout: Duration,
    /// Per-language timeout overrides keyed by `LanguageSupport::name()`.
    pub language_timeouts: HashMap<String, Duration>,
    /// Optional source-line to test-name map used to narrow test commands.
    pub test_selection: Option<TestSelectionConfig>,
}

impl CommandConfig {
    fn has_build_command(&self) -> bool {
        self.build_command_origin.runs_before_tests() && !self.build_command.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCommandConfig {
    pub path: PathBuf,
    pub command: Option<Vec<String>>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Default)]
pub struct TestSelectionConfig {
    tests_by_line: HashMap<(String, usize), Vec<SelectedTest>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedTest {
    pub name: String,
    pub duration_ms: Option<u64>,
}

impl SelectedTest {
    pub fn new(name: impl Into<String>, duration_ms: Option<u64>) -> Self {
        Self {
            name: name.into(),
            duration_ms,
        }
    }
}

impl TestSelectionConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(
        &mut self,
        project_root: &Path,
        file: impl AsRef<Path>,
        line: usize,
        tests: Vec<String>,
    ) {
        self.insert_tests(
            project_root,
            file,
            line,
            tests
                .into_iter()
                .map(|name| SelectedTest::new(name, None))
                .collect(),
        );
    }

    pub fn insert_tests(
        &mut self,
        project_root: &Path,
        file: impl AsRef<Path>,
        line: usize,
        tests: Vec<SelectedTest>,
    ) {
        self.tests_by_line.insert(
            (normalized_cache_path(project_root, file.as_ref()), line),
            tests,
        );
    }

    fn tests_for(&self, project_root: &Path, mutation: &Mutation) -> Option<Vec<String>> {
        let mut tests = self
            .tests_by_line
            .get(&(
                normalized_cache_path(project_root, &mutation.file),
                mutation.line,
            ))?
            .iter()
            .collect::<Vec<_>>();
        tests.sort_by_key(|test| test.duration_ms.unwrap_or(u64::MAX));
        Some(tests.into_iter().map(|test| test.name.clone()).collect())
            .filter(|tests: &Vec<String>| !tests.is_empty())
    }
}

/// Optional conditions that stop scheduling new mutations once enough signal
/// has been observed for a PR gate.
#[derive(Debug, Clone, Default)]
pub struct EarlyStopConfig {
    /// Stop after at least this many survived mutants have completed.
    pub max_survivors: Option<usize>,
    /// Stop when even killing every remaining mutation could not satisfy this score.
    pub fail_under: Option<f64>,
}

impl EarlyStopConfig {
    fn is_enabled(&self) -> bool {
        self.max_survivors.is_some() || self.fail_under.is_some()
    }
}

/// Applies mutations, runs checks, restores files, and aggregates results.
///
/// The runner evaluates mutations in isolated workspace copies so whole-project
/// test commands do not observe another active mutation. It still honors
/// cancellation, cache lookups, output capture, and per-language command/timeout
/// selection.
pub struct TestRunner {
    /// Test/build commands and timeout configuration.
    pub commands: CommandConfig,
    /// Maximum number of scheduled mutation tasks.
    pub parallelism: usize,
    /// Repository root where commands run and mutation paths are resolved.
    pub project_root: PathBuf,
    /// Print every mutation result as it runs.
    pub verbose: bool,
    /// Capture and print output for survived mutations.
    pub show_output: bool,
    /// Optional cap on how many non-build-error mutations are tested.
    pub max_tested: Option<usize>,
    /// Optional stop conditions for gate-style partial runs.
    pub early_stop: EarlyStopConfig,
    /// Whether workspace copies should honor `.ignore`/`.gitignore` rules.
    pub respect_workspace_ignores: bool,
    /// Extra environment variables passed to every spawned command.
    pub env: HashMap<String, String>,
    /// Use structured incremental history in addition to exact cache entries.
    pub incremental_history: bool,
    /// Re-run mutations instead of trusting cache or incremental history hits.
    pub force_rerun: bool,
    /// Skip mutants subsumed by a shared recorded killer test (opt-in;
    /// requires `incremental_history`).
    pub learned_selection: bool,
    /// Set to true externally (e.g. Ctrl+C handler) to stop spawning new mutations.
    pub cancelled: Arc<AtomicBool>,
}

/// Result of a runner invocation.
///
/// `report` contains the completed mutation results. `cancelled` is true when
/// the runner observed cancellation before or during execution.
#[derive(Debug)]
pub struct RunOutcome {
    pub report: MutationReport,
    /// Captured direct recipes for regular one-mutation paths only. These are
    /// kept separate from the general report so non-JSON renderers never grow
    /// replay-specific data.
    pub replay_recipes: BTreeMap<u32, RegularDirectRecipe>,
    pub cancelled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineMeasurement {
    pub build_duration: Option<Duration>,
    pub test_duration: Duration,
}

pub struct BaselineTimingConfig<'a> {
    pub test_command: &'a [String],
    pub build_command: &'a [String],
    pub sandbox_command: &'a [String],
    pub build_command_origin: BuildCommandOrigin,
    pub timeout: Duration,
    pub env: &'a HashMap<String, String>,
    pub cancelled: &'a AtomicBool,
    pub respect_workspace_ignores: bool,
}

/// A deliberately narrow replay invocation. It never reaches normal runner
/// cache/history/selection/schemata paths.
pub struct ReplayRunConfig<'a> {
    pub test_command: Vec<String>,
    pub build_command: Option<Vec<String>>,
    pub timeout: Duration,
    pub env: HashMap<String, String>,
    pub respect_workspace_ignores: bool,
    pub source_revision: &'a str,
    pub source_fingerprint: &'a str,
    pub show_output: bool,
    pub cancelled: &'a AtomicBool,
}

#[derive(Debug)]
pub struct ReplayRunOutcome {
    pub result: MutationResult,
    pub test_output: Option<String>,
    pub cancelled: bool,
}
/// Configuration for validating unmutated test suites before mutation execution.
pub struct BaselineHealthConfig<'a> {
    /// Commands and routing used for the pending mutations.
    pub commands: &'a CommandConfig,
    /// More generous deadline used only while measuring routes that inherit the
    /// global timeout for calibration.
    pub default_measurement_timeout: Option<Duration>,
    /// Whether Go mutations may run through schemata, which requires an
    /// uncached `go test` command.
    pub schemata_enabled: bool,
    /// Extra environment variables passed to every spawned command.
    pub env: &'a HashMap<String, String>,
    /// Stops baseline execution when cancellation is requested.
    pub cancelled: &'a AtomicBool,
    /// Whether the isolated baseline workspace honors ignore files.
    pub respect_workspace_ignores: bool,
}

/// One passing unmutated test suite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineSuiteMeasurement {
    pub build_duration: Option<Duration>,
    pub test_command: Vec<String>,
    pub test_duration: Duration,
    pub uses_default_timeout: bool,
}

/// Passing baselines suitable for timeout calibration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineHealthMeasurement {
    pub suites: Vec<BaselineSuiteMeasurement>,
}

/// Which baseline command made a mutation run unsuitable for scoring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuiteFailurePhase {
    Build,
    Test,
}

impl SuiteFailurePhase {
    fn label(self) -> &'static str {
        match self {
            Self::Build => "baseline build command",
            Self::Test => "baseline test command",
        }
    }
}

/// Structured outcome for a failing unmutated baseline suite.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunSuiteFailureOutcome {
    Failed { output: Option<String> },
    TimedOut { timeout: Duration },
    CannotRun { detail: String },
}

/// A run-level failure that prevents mutation verdicts from being trustworthy.
///
/// `src/main.rs` can downcast a `check_baseline_health` error with
/// [`run_suite_failure`] and render this separately from mutation results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSuiteFailure {
    pub phase: SuiteFailurePhase,
    pub command: Vec<String>,
    pub outcome: RunSuiteFailureOutcome,
}

impl std::fmt::Display for RunSuiteFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let label = self.phase.label();
        let command = command_for_message(&self.command);
        match &self.outcome {
            RunSuiteFailureOutcome::Failed { output } => {
                write!(f, "{label} failed (`{command}`)")?;
                if let Some(output) = output {
                    write!(f, ":\n{output}")?;
                }
                Ok(())
            }
            RunSuiteFailureOutcome::TimedOut { timeout } => write!(
                f,
                "{label} timed out after {:.2}s (`{command}`)",
                timeout.as_secs_f64()
            ),
            RunSuiteFailureOutcome::CannotRun { detail } => {
                write!(f, "{label} could not run (`{command}`): {detail}")
            }
        }
    }
}

impl std::error::Error for RunSuiteFailure {}

/// Extract a structured run-level suite failure from baseline health checking.
pub fn run_suite_failure(error: &anyhow::Error) -> Option<&RunSuiteFailure> {
    error.downcast_ref::<RunSuiteFailure>()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SelectedTestCommand {
    argv: Vec<String>,
    /// Original route argv retained only when test selection changed it.
    unnarrowed_argv: Option<Vec<String>>,
    timeout: Duration,
    uses_default_timeout: bool,
    selected_tests: Vec<String>,
    selection_active: bool,
}

impl SelectedTestCommand {
    fn cache_context(
        &self,
        build_command: &[String],
        build_command_origin: BuildCommandOrigin,
        sandbox_command: &[String],
        env: &HashMap<String, String>,
    ) -> String {
        // Only configured checks participate in cache identity. A detected
        // suggestion cannot change execution, so it must share no-build keys.
        let build_str = match build_command_origin {
            BuildCommandOrigin::None | BuildCommandOrigin::AutoDetected => String::new(),
            BuildCommandOrigin::Configured => format!("{build_command:?}"),
        };
        let sandbox_str = if sandbox_command.is_empty() {
            String::new()
        } else {
            format!("{sandbox_command:?}")
        };
        let mut env_parts: Vec<String> = env.iter().map(|(k, v)| format!("{k}={v}")).collect();
        env_parts.sort();
        format!(
            "test={:?};build={};sandbox={};timeout={};env={}",
            self.argv,
            build_str,
            sandbox_str,
            self.timeout.as_millis(),
            env_parts.join(",")
        )
    }

    fn is_narrowed(&self) -> bool {
        self.unnarrowed_argv.is_some()
    }

    fn unnarrowed_argv(&self) -> Option<&[String]> {
        self.unnarrowed_argv.as_deref()
    }

    fn selection_provenance(
        &self,
        confirmation: SurvivorConfirmation,
    ) -> Option<TestSelectionProvenance> {
        if !self.selection_active {
            None
        } else if self.is_narrowed() {
            Some(TestSelectionProvenance::Narrowed { confirmation })
        } else {
            Some(TestSelectionProvenance::Full)
        }
    }
}

#[cfg(test)]
fn select_test_command(
    project_root: &Path,
    commands: &CommandConfig,
    mutation: &Mutation,
) -> SelectedTestCommand {
    select_test_command_with_history(project_root, commands, mutation, None)
}

fn select_test_command_with_history(
    project_root: &Path,
    commands: &CommandConfig,
    mutation: &Mutation,
    history: Option<&cache::IncrementalHistoryStore>,
) -> SelectedTestCommand {
    let mut selected = select_unnarrowed_test_command(project_root, commands, mutation);
    selected.selection_active = commands.test_selection.is_some();

    if let Some(test_selection) = &commands.test_selection {
        if let Some(mut tests) = test_selection.tests_for(project_root, mutation) {
            if let Some(preferred) = history.and_then(|history| {
                history.preferred_killer_test(
                    &cache_identity(project_root, mutation),
                    &mutation.description,
                    &tests,
                )
            }) {
                tests.sort_by_key(|test| if *test == preferred { 0 } else { 1 });
            }
            let narrowed = narrow_test_command(selected.argv.clone(), &tests);
            if narrowed != selected.argv {
                selected.unnarrowed_argv = Some(std::mem::replace(&mut selected.argv, narrowed));
            }
            selected.selected_tests = tests;
        }
    }

    selected
}

fn select_unnarrowed_test_command(
    project_root: &Path,
    commands: &CommandConfig,
    mutation: &Mutation,
) -> SelectedTestCommand {
    let project_info = matching_project_command(project_root, commands, mutation);
    let argv = project_info
        .filter(|_| !commands.force_default_command)
        .and_then(|project| project.command.as_ref())
        .or_else(|| {
            (!commands.force_default_command)
                .then(|| commands.language_commands.get(mutation.language.as_str()))
                .flatten()
        })
        .unwrap_or(&commands.command)
        .clone();

    let (timeout, uses_default_timeout) = if commands.force_default_timeout {
        (commands.timeout, true)
    } else if let Some(timeout) = project_info.and_then(|project| project.timeout) {
        (timeout, false)
    } else if let Some(timeout) = commands
        .language_timeouts
        .get(mutation.language.as_str())
        .copied()
    {
        (timeout, false)
    } else {
        (commands.timeout, true)
    };

    SelectedTestCommand {
        argv,
        unnarrowed_argv: None,
        timeout,
        uses_default_timeout,
        selected_tests: Vec::new(),
        selection_active: false,
    }
}

fn matching_project_command<'a>(
    project_root: &Path,
    commands: &'a CommandConfig,
    mutation: &Mutation,
) -> Option<&'a ProjectCommandConfig> {
    let mutation_path = normalized_cache_path(project_root, &mutation.file);
    let mutation_parts: Vec<&str> = mutation_path
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();

    commands
        .project_commands
        .iter()
        .filter_map(|project| {
            let project_path = normalized_cache_path(project_root, &project.path);
            let project_parts: Vec<&str> = project_path
                .split('/')
                .filter(|part| !part.is_empty())
                .collect();
            (!project_parts.is_empty()
                && project_parts.len() <= mutation_parts.len()
                && mutation_parts
                    .iter()
                    .zip(&project_parts)
                    .all(|(mutation, project)| mutation == project))
            .then_some((project_parts.len(), project))
        })
        .max_by_key(|(len, _)| *len)
        .map(|(_, project)| project)
}

fn narrow_test_command(argv: Vec<String>, tests: &[String]) -> Vec<String> {
    let narrowed = narrow_go_test_command(argv.clone(), tests);
    if narrowed != argv {
        return narrowed;
    }
    let narrowed = narrow_pytest_command(argv.clone(), tests);
    if narrowed != argv {
        return narrowed;
    }
    let narrowed = narrow_jest_or_vitest_command(argv.clone(), tests);
    if narrowed != argv {
        return narrowed;
    }
    let narrowed = narrow_cargo_test_command(argv.clone(), tests);
    if narrowed != argv {
        return narrowed;
    }
    let narrowed = narrow_maven_test_command(argv.clone(), tests);
    if narrowed != argv {
        return narrowed;
    }
    narrow_gradle_test_command(argv, tests)
}

fn narrow_go_test_command(mut argv: Vec<String>, tests: &[String]) -> Vec<String> {
    if argv.len() < 2 || argv[0] != "go" || argv[1] != "test" {
        return argv;
    }

    let pattern = format!(
        "^({})$",
        tests
            .iter()
            .map(|test| escape_go_test_regex(test))
            .collect::<Vec<_>>()
            .join("|")
    );

    for i in 2..argv.len() {
        if argv[i] == "-run" {
            if i + 1 < argv.len() {
                argv[i + 1] = pattern;
            } else {
                argv.push(pattern);
            }
            return argv;
        }
        if argv[i].starts_with("-run=") {
            argv[i] = format!("-run={pattern}");
            return argv;
        }
    }

    argv.splice(2..2, ["-run".to_string(), pattern]);
    argv
}

fn narrow_pytest_command(mut argv: Vec<String>, tests: &[String]) -> Vec<String> {
    if !is_pytest_command(&argv) {
        return argv;
    }
    if tests.iter().all(|test| is_pytest_node_id(test)) {
        argv.extend(tests.iter().cloned());
        return argv;
    }

    if !tests.iter().all(|test| is_simple_pytest_keyword(test)) {
        return argv;
    }
    let expression = tests.join(" or ");
    argv.extend(["-k".to_string(), expression]);
    argv
}

fn is_pytest_command(argv: &[String]) -> bool {
    matches!(argv.first().map(String::as_str), Some("pytest" | "py.test"))
        || matches!(
            (
                argv.first().map(String::as_str),
                argv.get(1).map(String::as_str),
                argv.get(2).map(String::as_str)
            ),
            (Some("python" | "python3"), Some("-m"), Some("pytest"))
        )
}

fn is_pytest_node_id(test: &str) -> bool {
    test.contains("::") || test.ends_with(".py")
}

fn is_simple_pytest_keyword(test: &str) -> bool {
    !test.is_empty()
        && test
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn narrow_jest_or_vitest_command(mut argv: Vec<String>, tests: &[String]) -> Vec<String> {
    if !is_jest_or_vitest_command(&argv) {
        return argv;
    }
    let pattern = exact_regex_pattern(tests);
    argv.extend(["-t".to_string(), pattern]);
    argv
}

fn is_jest_or_vitest_command(argv: &[String]) -> bool {
    matches!(argv.first().map(String::as_str), Some("jest" | "vitest"))
        || matches!(
            (
                argv.first().map(String::as_str),
                argv.get(1).map(String::as_str)
            ),
            (Some("npx"), Some("jest" | "vitest"))
        )
}

fn narrow_cargo_test_command(mut argv: Vec<String>, tests: &[String]) -> Vec<String> {
    if argv.len() < 2 || argv[0] != "cargo" || argv[1] != "test" || tests.len() != 1 {
        return argv;
    }
    insert_before_double_dash(&mut argv, tests[0].clone());
    argv
}

fn narrow_maven_test_command(mut argv: Vec<String>, tests: &[String]) -> Vec<String> {
    if argv.is_empty() || argv[0] != "mvn" || !argv.iter().any(|arg| arg == "test") {
        return argv;
    }
    let value = format!("-Dtest={}", tests.join(","));
    if let Some(existing) = argv.iter_mut().find(|arg| arg.starts_with("-Dtest=")) {
        *existing = value;
    } else {
        argv.push(value);
    }
    argv
}

fn narrow_gradle_test_command(mut argv: Vec<String>, tests: &[String]) -> Vec<String> {
    if argv.is_empty()
        || !matches!(argv[0].as_str(), "gradle" | "./gradlew" | "gradlew")
        || !argv.iter().any(|arg| arg == "test")
    {
        return argv;
    }
    for test in tests {
        argv.extend(["--tests".to_string(), test.clone()]);
    }
    argv
}

fn insert_before_double_dash(argv: &mut Vec<String>, value: String) {
    let insert_at = argv
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(argv.len());
    argv.insert(insert_at, value);
}

fn exact_regex_pattern(tests: &[String]) -> String {
    format!(
        "^({})$",
        tests
            .iter()
            .map(|test| escape_test_regex(test))
            .collect::<Vec<_>>()
            .join("|")
    )
}

fn escape_go_test_regex(test: &str) -> String {
    escape_test_regex(test)
}

fn escape_test_regex(test: &str) -> String {
    let mut escaped = String::new();
    for ch in test.chars() {
        if matches!(
            ch,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
        ) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

struct FileGuard {
    path: PathBuf,
    original: Vec<u8>,
}

impl Drop for FileGuard {
    fn drop(&mut self) {
        if let Err(e) = write_workspace_file(&self.path, &self.original) {
            eprintln!(
                "error: failed to restore {}: {} — file may be corrupted, check git status",
                self.path.display(),
                e
            );
        }
    }
}

pub(crate) struct WorkspaceCopy {
    _tempdir: tempfile::TempDir,
    root: PathBuf,
    reset_strategy: WorkspaceResetStrategy,
}

enum WorkspaceResetStrategy {
    Copy,
    GitWorktree {
        project_root: PathBuf,
        overlay: GitWorktreeOverlay,
    },
}

#[derive(Clone)]
struct GitWorktreeOverlay {
    copy_paths: Vec<PathBuf>,
    remove_paths: Vec<PathBuf>,
}

impl GitWorktreeOverlay {
    fn apply(&self, project_root: &Path, workspace_root: &Path) -> std::io::Result<()> {
        for relative in &self.remove_paths {
            remove_workspace_path(&workspace_root.join(relative))?;
        }
        for relative in &self.copy_paths {
            copy_overlay_file(project_root, workspace_root, relative)?;
        }
        Ok(())
    }

    fn apply_replay(
        &self,
        source_root: &CapDir,
        workspace: &ReplayWorkspace,
    ) -> std::io::Result<()> {
        #[cfg(windows)]
        ensure_replay_removals_need_no_directory(source_root, &self.remove_paths)?;
        for relative in &self.remove_paths {
            workspace.remove_relative(relative)?;
        }
        for relative in &self.copy_paths {
            if cap_relative_file_is_regular(source_root, relative)? {
                workspace.copy_regular_source(source_root, relative)?;
            } else {
                workspace.remove_relative(relative)?;
            }
        }
        Ok(())
    }
}

impl WorkspaceCopy {
    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    fn reset(&self, project_root: &Path, respect_ignores: bool) -> std::io::Result<()> {
        match &self.reset_strategy {
            WorkspaceResetStrategy::Copy => {
                reset_copied_workspace(project_root, &self.root, respect_ignores)
            }
            WorkspaceResetStrategy::GitWorktree {
                project_root,
                overlay,
            } => reset_git_worktree(project_root, &self.root, overlay),
        }
    }
}

#[cfg(windows)]
fn windows_normal_disk_root_letter(path: &Path) -> Option<u8> {
    use std::os::windows::ffi::OsStrExt;

    let mut units = path.as_os_str().encode_wide();
    let letter = u8::try_from(units.next()?).ok()?;
    if !letter.is_ascii_alphabetic()
        || units.next()? != u16::from(b':')
        || units.next()? != u16::from(b'\\')
        || units.next().is_some()
    {
        return None;
    }
    Some(letter)
}

/// Return the normal drive root containing the Windows system directory.
///
/// The system directory is the trusted OS-owned reference for replay's only
/// allowed temp volume. Keeping the unsafe calls here prevents a user-defined
/// DOS device such as `subst X:` from becoming a replay workspace root.
#[cfg(windows)]
fn windows_system_volume_root() -> std::io::Result<(PathBuf, u8)> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetVolumePathNameW;
    use windows_sys::Win32::System::SystemInformation::GetSystemWindowsDirectoryW;

    let mut directory_buffer = vec![0u16; 260];
    let system_directory = loop {
        let capacity = u32::try_from(directory_buffer.len())
            .map_err(|_| std::io::Error::other("Windows system directory path is too long"))?;
        // SAFETY: the initialized buffer is writable for `capacity` UTF-16
        // code units; the API writes at most that many and returns its length.
        let length = unsafe { GetSystemWindowsDirectoryW(directory_buffer.as_mut_ptr(), capacity) };
        if length == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let length = length as usize;
        if length < directory_buffer.len() {
            break PathBuf::from(OsString::from_wide(&directory_buffer[..length]));
        }
        let next_capacity = length
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("Windows system directory path is too long"))?
            .max(directory_buffer.len().saturating_mul(2));
        directory_buffer.resize(next_capacity, 0);
    };

    let mut system_directory_wide: Vec<u16> = system_directory.as_os_str().encode_wide().collect();
    system_directory_wide.push(0);
    let mut volume_buffer = vec![0u16; 260];
    let volume_root = loop {
        let capacity = u32::try_from(volume_buffer.len())
            .map_err(|_| std::io::Error::other("Windows volume path is too long"))?;
        // SAFETY: the input is NUL-terminated and both initialized UTF-16
        // buffers remain live for the call. The output capacity is supplied
        // exactly in UTF-16 code units.
        let success = unsafe {
            GetVolumePathNameW(
                system_directory_wide.as_ptr(),
                volume_buffer.as_mut_ptr(),
                capacity,
            )
        };
        if success != 0 {
            let length = volume_buffer
                .iter()
                .position(|unit| *unit == 0)
                .ok_or_else(|| std::io::Error::other("Windows volume path was not terminated"))?;
            break PathBuf::from(OsString::from_wide(&volume_buffer[..length]));
        }
        let error = std::io::Error::last_os_error();
        if volume_buffer.len() >= 32_768 {
            return Err(error);
        }
        volume_buffer.resize((volume_buffer.len() * 2).min(32_768), 0);
    };

    let Some(drive) = windows_normal_disk_root_letter(&volume_root) else {
        return Err(std::io::Error::other(format!(
            "Windows system volume {} is not a normal local drive root",
            volume_root.display()
        )));
    };
    Ok((volume_root, drive))
}

/// Owner of replay's trusted temp root: the validated lexical path plus a
/// pinned handle chain stabilizing it on Windows.
///
/// On Windows the lexical path handed to Git and `current_dir` must resolve
/// identically for the whole replay, so `pin` accepts only an absolute normal
/// disk path on the Windows system volume. It opens that volume root as the
/// explicit ambient trust anchor, then traverses every normal component with
/// `open_dir_nofollow`, retaining each handle. Any reparse point inside the
/// configured temp root is rejected fail-closed, and no component can be
/// renamed while held. On other platforms a single ambient open preserves the
/// established behavior.
struct ReplayTempRoot {
    lexical: PathBuf,
    root: CapDir,
    _ancestors: Vec<CapDir>,
}

impl ReplayTempRoot {
    #[cfg(windows)]
    fn pin(path: &Path) -> std::io::Result<Self> {
        let mut components = path.components();
        let drive = match components.next() {
            Some(std::path::Component::Prefix(prefix)) => match prefix.kind() {
                std::path::Prefix::Disk(letter) => letter,
                _ => {
                    return Err(std::io::Error::other(format!(
                        "replay temp root {} is not an absolute local disk path",
                        path.display()
                    )));
                }
            },
            _ => {
                return Err(std::io::Error::other(format!(
                    "replay temp root {} is not an absolute local disk path",
                    path.display()
                )));
            }
        };
        if !matches!(components.next(), Some(std::path::Component::RootDir)) {
            return Err(std::io::Error::other(format!(
                "replay temp root {} is not an absolute local disk path",
                path.display()
            )));
        }
        let mut parts = Vec::new();
        for component in components {
            let std::path::Component::Normal(part) = component else {
                return Err(std::io::Error::other(format!(
                    "replay temp root {} contains a non-normal path component",
                    path.display()
                )));
            };
            parts.push(part.to_os_string());
        }

        let (system_root, system_drive) = windows_system_volume_root()?;
        if !drive.eq_ignore_ascii_case(&system_drive) {
            return Err(std::io::Error::other(format!(
                "replay temp root {} is not on the Windows system volume {}",
                path.display(),
                system_root.display()
            )));
        }

        let mut current = CapDir::open_ambient_dir(&system_root, ambient_authority())?;
        let mut ancestors = Vec::new();
        for part in parts {
            let next = current.open_dir_nofollow(Path::new(&part))?;
            ancestors.push(current);
            current = next;
        }
        Ok(Self {
            lexical: path.to_path_buf(),
            root: current,
            _ancestors: ancestors,
        })
    }

    #[cfg(not(windows))]
    fn pin(path: &Path) -> std::io::Result<Self> {
        let root = CapDir::open_ambient_dir(path, ambient_authority())?;
        Ok(Self {
            lexical: path.to_path_buf(),
            root,
            _ancestors: Vec::new(),
        })
    }
}

/// Replay's capability-bounded view of its disposable clone.
///
/// `root` (the clone) and `_outer` (its TempDir ancestor) are held without
/// `FILE_SHARE_DELETE` on Windows, and `_temp_root` retains the pinned
/// temp-root chain, so no lexical path component can be renamed or rebound
/// while path-based Git and build/test subprocesses use `workspace.root()`.
/// Fields deliberately drop `root` first, then `_outer`, then `workspace`
/// runs TempDir cleanup while the chain is still held, and `_temp_root` last.
/// std's Windows `remove_dir_all` deletes handle-relative with no-reparse
/// opens (the CVE-2022-21658 fix), so normal TempDir cleanup never traverses
/// a junction swapped into the clone.
struct ReplayWorkspace {
    root: CapDir,
    _outer: CapDir,
    workspace: WorkspaceCopy,
    _temp_root: ReplayTempRoot,
}

impl ReplayWorkspace {
    /// Test construction: open an already-populated workspace root, retaining
    /// the same parent-first/no-follow hierarchy as the production clone
    /// target: pin the temp-root chain (on Windows every lexical component
    /// beneath the drive anchor), then open the outer and root leaves
    /// no-follow through their held parents.
    #[cfg(test)]
    fn open(workspace: WorkspaceCopy) -> std::io::Result<Self> {
        let outer_path = workspace
            .root()
            .parent()
            .ok_or_else(|| std::io::Error::other("replay workspace root has no parent"))?;
        let root_leaf = workspace
            .root()
            .file_name()
            .ok_or_else(|| std::io::Error::other("replay workspace root has no leaf name"))?;
        let temp_root_path = outer_path
            .parent()
            .ok_or_else(|| std::io::Error::other("replay workspace parent has no parent"))?;
        let outer_leaf = outer_path
            .file_name()
            .ok_or_else(|| std::io::Error::other("replay workspace parent has no leaf name"))?;
        let temp_root = ReplayTempRoot::pin(temp_root_path)?;
        let outer = temp_root.root.open_dir_nofollow(Path::new(outer_leaf))?;
        let root = outer.open_dir_nofollow(Path::new(root_leaf))?;
        Ok(Self {
            root,
            _outer: outer,
            workspace,
            _temp_root: temp_root,
        })
    }

    /// Production clone target under the ambient temp root.
    fn create_clone_target() -> std::io::Result<Self> {
        Self::create_clone_target_in(&std::env::temp_dir())
    }

    /// Pin the trusted temp-root chain, create the randomized outer TempDir
    /// beneath the validated lexical root, then re-open the outer basename
    /// no-follow through the pinned root before Git sees any pathname — a
    /// junction swapped in between creation and open fails closed here,
    /// before any Git spawn. The empty `workspace` child is then created and
    /// pinned the same way, with no retry accepting an existing child. An
    /// ordinary directory replacing the outer in that same window stays
    /// inside the trusted temp root; only reparse-point substitution is
    /// detected and rejected.
    fn create_clone_target_in(temp_root_path: &Path) -> std::io::Result<Self> {
        let temp_root = ReplayTempRoot::pin(temp_root_path)?;
        run_replay_temp_root_ready_hook();
        let tempdir = tempfile::Builder::new().tempdir_in(&temp_root.lexical)?;
        run_replay_outer_created_hook(tempdir.path());
        let outer_leaf = tempdir
            .path()
            .file_name()
            .ok_or_else(|| std::io::Error::other("replay tempdir has no leaf name"))?;
        let outer = temp_root.root.open_dir_nofollow(Path::new(outer_leaf))?;
        outer.create_dir(Path::new("workspace"))?;
        let root = outer.open_dir_nofollow(Path::new("workspace"))?;
        let root_path = tempdir.path().join("workspace");
        Ok(Self {
            root,
            _outer: outer,
            workspace: WorkspaceCopy {
                _tempdir: tempdir,
                root: root_path,
                reset_strategy: WorkspaceResetStrategy::Copy,
            },
            _temp_root: temp_root,
        })
    }

    /// This path is only for external Git and user-command working directories.
    fn root(&self) -> &Path {
        self.workspace.root()
    }

    fn ensure_directory(&self, relative: &Path) -> std::io::Result<()> {
        let (parents, leaf) = split_replay_relative_path(relative)?;
        let mut current = self.root.try_clone()?;
        for component in parents.into_iter().chain(std::iter::once(leaf)) {
            current = ensure_cap_directory_child(&current, &component)?;
        }
        Ok(())
    }

    fn remove_relative(&self, relative: &Path) -> std::io::Result<()> {
        let (parents, leaf) = split_replay_relative_path(relative)?;
        let mut current = self.root.try_clone()?;
        for component in parents {
            match current.symlink_metadata(&component) {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    current = current.open_dir_nofollow(&component)?;
                }
                Ok(_) => {
                    remove_cap_entry(&current, &component)?;
                    return Ok(());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            }
        }
        remove_cap_entry(&current, &leaf)
    }

    fn read_regular(&self, relative: &Path) -> std::io::Result<Vec<u8>> {
        let (parent, leaf) = open_cap_existing_parent(&self.root, relative)?;
        let metadata = parent.symlink_metadata(&leaf)?;
        if !metadata.file_type().is_file() {
            return Err(std::io::Error::other(format!(
                "replay workspace entry {} is not a regular file",
                relative.display()
            )));
        }
        let mut options = CapOpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let mut file = parent.open_with(&leaf, &options)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(std::io::Error::other(format!(
                "replay workspace entry {} changed from a regular file",
                relative.display()
            )));
        }
        let mut contents = Vec::new();
        file.read_to_end(&mut contents)?;
        Ok(contents)
    }

    fn copy_regular_source(&self, source_root: &CapDir, relative: &Path) -> std::io::Result<()> {
        let (source_parent, source_leaf) = open_cap_existing_parent(source_root, relative)?;
        let source_metadata = source_parent.symlink_metadata(&source_leaf)?;
        if !source_metadata.file_type().is_file() {
            return Err(std::io::Error::other(format!(
                "replay source entry {} is not a regular file",
                relative.display()
            )));
        }
        let mut options = CapOpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let mut source_file = source_parent.open_with(&source_leaf, &options)?;
        // Permissions and bytes come from this same held capability file.
        let source_metadata = source_file.metadata()?;
        if !source_metadata.file_type().is_file() {
            return Err(std::io::Error::other(format!(
                "replay source entry {} changed from a regular file",
                relative.display()
            )));
        }
        let permissions = source_metadata.permissions();
        #[cfg(not(windows))]
        if CapMetadataExt::nlink(&source_metadata) > 1 {
            // Recheck the opened inode before copying it into the replay clone.
            return Err(std::io::Error::other(format!(
                "replay source entry {} has multiple hard links",
                relative.display()
            )));
        }

        let (destination_parent, destination_leaf) = ensure_cap_parent(&self.root, relative)?;
        #[cfg(windows)]
        if let Ok(metadata) = destination_parent.symlink_metadata(&destination_leaf) {
            // A dir→file type-change needs directory removal, which stays
            // fail-closed on Windows; name the requirement before staging.
            if metadata.file_type().is_dir() {
                return Err(replay_windows_directory_removal_error(Path::new(
                    &destination_leaf,
                )));
            }
        }
        let mut staged = TempFile::new(&destination_parent)?;
        std::io::copy(&mut source_file, staged.as_file_mut())?;
        staged.as_file_mut().flush()?;
        staged.as_file().set_permissions(permissions)?;
        staged.replace(destination_leaf)
    }

    fn replace_regular(&self, relative: &Path, contents: &[u8]) -> std::io::Result<()> {
        let (parent, leaf) = ensure_cap_parent(&self.root, relative)?;
        let permissions = parent
            .symlink_metadata(&leaf)
            .ok()
            .filter(|metadata| metadata.file_type().is_file())
            .map(|metadata| metadata.permissions());
        let mut staged = TempFile::new(&parent)?;
        staged.as_file_mut().write_all(contents)?;
        staged.as_file_mut().flush()?;
        if let Some(permissions) = permissions {
            staged.as_file().set_permissions(permissions)?;
        }
        run_replay_publish_hook();
        staged.replace(leaf)
    }
}

fn split_replay_relative_path(relative: &Path) -> std::io::Result<(Vec<OsString>, OsString)> {
    if !is_safe_relative_path(relative) {
        return Err(invalid_workspace_relative_path(relative));
    }
    let mut components = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(component) => Ok(component.to_os_string()),
            _ => Err(invalid_workspace_relative_path(relative)),
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    let leaf = components
        .pop()
        .ok_or_else(|| invalid_workspace_relative_path(relative))?;
    Ok((components, leaf))
}

fn remove_cap_entry(parent: &CapDir, name: &OsString) -> std::io::Result<()> {
    match parent.symlink_metadata(name) {
        Ok(metadata) if metadata.file_type().is_dir() => {
            #[cfg(windows)]
            {
                Err(replay_windows_directory_removal_error(Path::new(name)))
            }
            #[cfg(not(windows))]
            {
                // Unix removal remains rooted in the held parent capability.
                parent.remove_dir_all(name)
            }
        }
        Ok(_) => parent.remove_file_or_symlink(name),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Windows cannot yet remove a directory beneath a held capability without a
/// path-based race (cap-primitives 4.0.2 drops the handle, then path-deletes).
/// Fail closed with a diagnostic naming the requirement; only file and
/// symlink overlay removals are supported there.
#[cfg(windows)]
fn replay_windows_directory_removal_error(name: &Path) -> std::io::Error {
    std::io::Error::other(format!(
        "replay cannot remove directory {} on Windows: safe disposable workspace setup requires race-free directory removal; only file and symlink overlay removals are supported",
        name.display()
    ))
}

/// Windows: removing a tracked leaf whose source ancestor disappeared or
/// became a non-directory would leave a stale directory behind in the clone,
/// so faithful replay requires removing that directory. Detect this through
/// no-follow capability lookups before applying any removals and fail closed
/// with the directory-removal diagnostic, before any mutation/test spawn.
#[cfg(windows)]
fn ensure_replay_removals_need_no_directory(
    source_root: &CapDir,
    remove_paths: &[PathBuf],
) -> std::io::Result<()> {
    for relative in remove_paths {
        let (parents, _leaf) = split_replay_relative_path(relative)?;
        let mut current = source_root.try_clone()?;
        let mut ancestor = PathBuf::new();
        for component in parents {
            ancestor.push(&component);
            match current.symlink_metadata(&component) {
                Ok(metadata) if metadata.file_type().is_dir() => {
                    current = current.open_dir_nofollow(&component)?;
                }
                Ok(_) => return Err(replay_windows_directory_removal_error(&ancestor)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err(replay_windows_directory_removal_error(&ancestor));
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn ensure_cap_directory_child(parent: &CapDir, name: &OsString) -> std::io::Result<CapDir> {
    loop {
        match parent.symlink_metadata(name) {
            Ok(metadata) if metadata.file_type().is_dir() => {
                return parent.open_dir_nofollow(name);
            }
            Ok(_) => remove_cap_entry(parent, name)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match parent.create_dir(name) {
            Ok(()) => return parent.open_dir_nofollow(name),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

fn ensure_cap_parent(root: &CapDir, relative: &Path) -> std::io::Result<(CapDir, OsString)> {
    let (parents, leaf) = split_replay_relative_path(relative)?;
    let mut current = root.try_clone()?;
    for component in parents {
        current = ensure_cap_directory_child(&current, &component)?;
    }
    Ok((current, leaf))
}

fn open_cap_existing_parent(root: &CapDir, relative: &Path) -> std::io::Result<(CapDir, OsString)> {
    let (parents, leaf) = split_replay_relative_path(relative)?;
    let mut current = root.try_clone()?;
    for component in parents {
        let metadata = current.symlink_metadata(&component)?;
        if !metadata.file_type().is_dir() {
            return Err(std::io::Error::other(format!(
                "capability path parent {} is not a directory",
                component.to_string_lossy()
            )));
        }
        current = current.open_dir_nofollow(&component)?;
    }
    Ok((current, leaf))
}

fn cap_relative_file_is_regular(root: &CapDir, relative: &Path) -> std::io::Result<bool> {
    let (parents, leaf) = match split_replay_relative_path(relative) {
        Ok(parts) => parts,
        Err(_) => return Ok(false),
    };
    let mut current = root.try_clone()?;
    for component in parents {
        let metadata = match current.symlink_metadata(&component) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_dir() {
            return Ok(false);
        }
        current = current.open_dir_nofollow(&component)?;
    }
    match current.symlink_metadata(&leaf) {
        Ok(metadata) => Ok(metadata.file_type().is_file()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
thread_local! {
    static REPLAY_PUBLISH_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_replay_publish_hook(hook: Option<Box<dyn FnOnce()>>) {
    REPLAY_PUBLISH_HOOK.with(|slot| *slot.borrow_mut() = hook);
}

fn run_replay_publish_hook() {
    #[cfg(test)]
    REPLAY_PUBLISH_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

#[cfg(test)]
type ReplayOuterCreatedHook = Box<dyn FnOnce(&Path)>;

#[cfg(test)]
thread_local! {
    static REPLAY_OUTER_CREATED_HOOK: std::cell::RefCell<Option<ReplayOuterCreatedHook>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_replay_outer_created_hook(hook: Option<ReplayOuterCreatedHook>) {
    REPLAY_OUTER_CREATED_HOOK.with(|slot| *slot.borrow_mut() = hook);
}

fn run_replay_outer_created_hook(outer: &Path) {
    #[cfg(test)]
    REPLAY_OUTER_CREATED_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook(outer);
        }
    });
    #[cfg(not(test))]
    let _ = outer;
}

#[cfg(test)]
thread_local! {
    static REPLAY_TEMP_ROOT_READY_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(all(test, windows))]
fn set_replay_temp_root_ready_hook(hook: Option<Box<dyn FnOnce()>>) {
    REPLAY_TEMP_ROOT_READY_HOOK.with(|slot| *slot.borrow_mut() = hook);
}

fn run_replay_temp_root_ready_hook() {
    #[cfg(test)]
    REPLAY_TEMP_ROOT_READY_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

/// Replay-only RAII restoration through the capability writer.
struct ReplayFileGuard<'a> {
    workspace: &'a ReplayWorkspace,
    relative: PathBuf,
    original: Vec<u8>,
}

impl Drop for ReplayFileGuard<'_> {
    fn drop(&mut self) {
        if let Err(error) = self
            .workspace
            .replace_regular(&self.relative, &self.original)
        {
            eprintln!(
                "error: failed to restore replay workspace {}: {error}",
                self.relative.display()
            );
        }
    }
}

impl Drop for WorkspaceCopy {
    fn drop(&mut self) {
        let WorkspaceResetStrategy::GitWorktree { project_root, .. } = &self.reset_strategy else {
            return;
        };
        if !self.root.exists() {
            return;
        }
        if let Err(e) = remove_git_worktree(project_root, &self.root) {
            eprintln!("warning: {e}");
        }
    }
}

/// Normal workspace-copy exclusions. Keep this compatible with ordinary
/// `togi check` workspace behavior.
pub(crate) fn should_skip_workspace_entry(relative: &Path) -> bool {
    relative.components().any(|component| {
        component.as_os_str().to_str().is_some_and(|name| {
            matches!(
                name,
                ".git"
                    | ".togi"
                    | ".togi-cache"
                    | ".togi.lock"
                    | ".codex"
                    | ".claude"
                    | "target"
                    | "node_modules"
                    | ".venv"
                    | "dist"
                    | "build"
            )
        })
    })
}

fn should_copy_workspace_entry(project_root: &Path, path: &Path) -> bool {
    path == project_root
        || path
            .strip_prefix(project_root)
            .is_ok_and(|relative| !should_skip_workspace_entry(relative))
}

/// Replay excludes control aliases case-insensitively in addition to normal
/// disposable-workspace artifacts.
fn should_skip_replay_workspace_entry(relative: &Path) -> bool {
    relative.components().any(|component| {
        component.as_os_str().to_str().is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            matches!(
                name.as_str(),
                ".git"
                    | ".togi"
                    | ".togi-cache"
                    | ".togi.lock"
                    | ".togi-baseline"
                    | ".codex"
                    | ".claude"
                    | "target"
                    | "node_modules"
                    | ".venv"
                    | "dist"
                    | "build"
            ) || name.starts_with(".togi-")
        })
    })
}

fn should_copy_replay_workspace_entry(project_root: &Path, path: &Path) -> bool {
    path == project_root
        || path
            .strip_prefix(project_root)
            .is_ok_and(|relative| !should_skip_replay_workspace_entry(relative))
}

/// Workspace directories kept across resets.
///
/// Stashes are created in `workspace_root.parent()` as `.togi-preserved-{name}`
/// so `fs::rename` stays on the same filesystem and remains metadata-only.
/// Any stale stash from an interrupted prior reset is reclaimed before rename.
const PRESERVED_WORKSPACE_DIRS: &[&str] = &["target"];

fn cache_context_fingerprint(project_root: &Path) -> u64 {
    git_cache_context_fingerprint(project_root)
        .unwrap_or_else(|| filesystem_cache_context_fingerprint(project_root))
}

fn filesystem_cache_context_fingerprint(project_root: &Path) -> u64 {
    let mut files = Vec::new();
    collect_cache_context_files(project_root, project_root, &mut files);
    files.sort_by_key(|path| normalized_cache_path(project_root, path));

    let mut hasher = StableCacheHasher::default();
    update_cache_hash(&mut hasher, b"filesystem");
    for relative in files {
        let path_key = normalized_cache_path(project_root, &relative);
        update_cache_hash(&mut hasher, path_key.as_bytes());
        if let Ok(content) = fs::read(project_root.join(&relative)) {
            update_cache_hash(&mut hasher, &content);
        }
    }
    hasher.finish()
}

fn git_cache_context_fingerprint(project_root: &Path) -> Option<u64> {
    if git_cache_context_is_dirty(project_root)? {
        return None;
    }

    let output = std::process::Command::new("git")
        .args(["ls-files", "-z", "-s", "--"])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let mut files = Vec::new();
    for entry in output.stdout.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let tab = entry.iter().position(|byte| *byte == b'\t')?;
        let metadata = String::from_utf8_lossy(&entry[..tab]);
        let mut fields = metadata.split_whitespace();
        let _mode = fields.next()?;
        let object_id = fields.next()?.to_string();
        let relative = PathBuf::from(String::from_utf8_lossy(&entry[tab + 1..]).into_owned());
        if is_cache_context_file(&relative) {
            files.push((relative, object_id));
        }
    }

    files.sort_by_key(|(path, _)| normalized_cache_path(project_root, path));

    let mut hasher = StableCacheHasher::default();
    update_cache_hash(&mut hasher, b"git-index");
    for (relative, object_id) in files {
        let path_key = normalized_cache_path(project_root, &relative);
        update_cache_hash(&mut hasher, path_key.as_bytes());
        update_cache_hash(&mut hasher, object_id.as_bytes());
    }
    Some(hasher.finish())
}

fn git_cache_context_is_dirty(project_root: &Path) -> Option<bool> {
    // No -M/--find-renames here: porcelain v1 -z then stays in the
    // single-token "XY path\0" form this parser expects.
    let output = std::process::Command::new("git")
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
        ])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    for entry in output.stdout.split(|byte| *byte == 0) {
        if entry.len() <= 3 || entry[2] != b' ' {
            continue;
        }
        let relative = PathBuf::from(String::from_utf8_lossy(&entry[3..]).into_owned());
        if is_cache_context_file(&relative) {
            return Some(true);
        }
    }
    Some(false)
}

fn collect_cache_context_files(project_root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };

    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let relative = path.strip_prefix(project_root).unwrap_or(&path);
        if should_skip_workspace_entry(relative) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            collect_cache_context_files(project_root, &path, out);
        } else if file_type.is_file() && is_cache_context_file(relative) {
            out.push(relative.to_path_buf());
        }
    }
}

/// Files whose content can change mutation verdicts beyond the mutated file
/// itself: build manifests, CI workflows, tests — and any source file, since
/// sources both shape which tests compile and run (e.g. Rust `mod` wiring) and
/// can carry colocated tests (`#[cfg(test)]` modules, specs). A verdict cached
/// before such a change must not be reused (#410).
fn is_cache_context_file(relative: &Path) -> bool {
    let path_key = normalized_cache_path(Path::new(""), relative).to_ascii_lowercase();
    let file_name = relative
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if has_source_context_extension(&file_name) {
        return true;
    }

    if matches!(
        file_name.as_str(),
        "togi.toml"
            | "cargo.toml"
            | "cargo.lock"
            | "go.mod"
            | "go.sum"
            | "package.json"
            | "package-lock.json"
            | "pnpm-lock.yaml"
            | "yarn.lock"
            | "bun.lock"
            | "bun.lockb"
            | "pyproject.toml"
            | "setup.py"
            | "setup.cfg"
            | "pytest.ini"
            | "tox.ini"
            | "pom.xml"
            | "build.gradle"
            | "build.gradle.kts"
            | "gemfile"
            | "gemfile.lock"
            | "cmakelists.txt"
    ) {
        return true;
    }

    if path_key.starts_with(".github/workflows/")
        && (file_name.ends_with(".yml") || file_name.ends_with(".yaml"))
    {
        return true;
    }

    let in_test_dir = relative.components().any(|component| {
        component.as_os_str().to_str().is_some_and(|part| {
            matches!(
                part.to_ascii_lowercase().as_str(),
                "test" | "tests" | "spec" | "specs" | "__tests__" | "__specs__"
            )
        })
    });
    if in_test_dir {
        return has_test_context_extension(&file_name);
    }

    file_name.starts_with("test_")
        || file_name.ends_with("_test.go")
        || file_name.ends_with("_test.rs")
        || file_name.ends_with("_test.py")
        || file_name.contains(".test.")
        || file_name.contains(".spec.")
        || file_name.ends_with("test.java")
        || file_name.ends_with("tests.java")
}

/// Extensions of source files togi can mutate. Any of them can influence test
/// compilation or execution, so all of them are verdict-cache context.
fn has_source_context_extension(file_name: &str) -> bool {
    matches!(
        Path::new(file_name)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or(""),
        "go" | "rs"
            | "py"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "java"
            | "c"
            | "cc"
            | "cpp"
            | "cxx"
            | "h"
            | "hpp"
            | "cs"
            | "rb"
    )
}

fn has_test_context_extension(file_name: &str) -> bool {
    matches!(
        Path::new(file_name)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or(""),
        "go" | "rs"
            | "py"
            | "ts"
            | "tsx"
            | "js"
            | "jsx"
            | "java"
            | "c"
            | "cc"
            | "cpp"
            | "cxx"
            | "h"
            | "hpp"
            | "cs"
            | "rb"
            | "toml"
            | "json"
            | "yaml"
            | "yml"
            | "xml"
    )
}

const STABLE_HASH_OFFSET: u64 = 0xcbf29ce484222325;
const STABLE_HASH_PRIME: u64 = 0x100000001b3;

struct StableCacheHasher {
    hash: u64,
}

impl Default for StableCacheHasher {
    fn default() -> Self {
        Self {
            hash: STABLE_HASH_OFFSET,
        }
    }
}

impl Hasher for StableCacheHasher {
    fn finish(&self) -> u64 {
        self.hash
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.hash ^= u64::from(*byte);
            self.hash = self.hash.wrapping_mul(STABLE_HASH_PRIME);
        }
    }
}

fn update_cache_hash(hasher: &mut impl Hasher, bytes: &[u8]) {
    hasher.write(&(bytes.len() as u64).to_le_bytes());
    hasher.write(bytes);
}

#[derive(Default)]
struct SourceContentCache {
    contents: Mutex<HashMap<String, Option<Vec<u8>>>>,
}

impl SourceContentCache {
    fn content_for(&self, project_root: &Path, mutation_file: &Path) -> Option<Vec<u8>> {
        let key = normalized_cache_path(project_root, mutation_file);
        let Ok(mut contents) = self.contents.lock() else {
            eprintln!("warning: source content cache mutex poisoned");
            return read_source_content(project_root, mutation_file);
        };
        if let Some(content) = contents.get(&key) {
            return content.clone();
        }

        let content = read_source_content(project_root, mutation_file);
        contents.insert(key, content.clone());
        content
    }

    #[cfg(test)]
    fn cached_entry_count(&self) -> usize {
        self.contents
            .lock()
            .expect("source content cache mutex poisoned")
            .len()
    }
}

#[derive(Default)]
struct TestContextIndex {
    files: Vec<TestContextFile>,
}

struct TestContextFile {
    key: String,
    content: Vec<u8>,
    text: Option<String>,
}

impl TestContextIndex {
    fn build(project_root: &Path) -> Self {
        let mut files = Vec::new();
        collect_cache_context_files(project_root, project_root, &mut files);
        files.sort_by_key(|path| normalized_cache_path(project_root, path));

        let files = files
            .into_iter()
            .filter_map(|relative| {
                let content = fs::read(project_root.join(&relative)).ok()?;
                let key = normalized_cache_path(project_root, &relative);
                let text = String::from_utf8(content.clone()).ok();
                Some(TestContextFile { key, content, text })
            })
            .collect();

        Self { files }
    }

    fn fingerprint_for_tests(&self, tests: &[String], fallback: u64) -> u64 {
        if tests.is_empty() || self.files.is_empty() {
            return fallback;
        }

        let mut matched = BTreeSet::new();
        for test in tests {
            let matches = self.files_for_test(test);
            if matches.is_empty() {
                return fallback;
            }
            matched.extend(matches);
        }

        let mut hasher = StableCacheHasher::default();
        update_cache_hash(&mut hasher, b"selected-test-context-v1");
        for test in tests {
            update_cache_hash(&mut hasher, test.as_bytes());
        }
        for index in matched {
            if let Some(file) = self.files.get(index) {
                update_cache_hash(&mut hasher, file.key.as_bytes());
                update_cache_hash(&mut hasher, &file.content);
            }
        }
        hasher.finish()
    }

    fn files_for_test(&self, test: &str) -> Vec<usize> {
        if let Some(path_key) = direct_test_path_key(test) {
            return self
                .files
                .iter()
                .enumerate()
                .filter_map(|(index, file)| (file.key == path_key).then_some(index))
                .collect();
        }

        let tokens = test_name_tokens(test);
        if tokens.is_empty() {
            return Vec::new();
        }

        self.files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| {
                let text = file.text.as_ref()?;
                tokens
                    .iter()
                    .all(|token| text.contains(token))
                    .then_some(index)
            })
            .collect()
    }
}

fn direct_test_path_key(test: &str) -> Option<String> {
    let path_part = test.split("::").next().unwrap_or(test);
    if !looks_like_test_path(path_part) {
        return None;
    }
    Some(normalized_cache_path(Path::new(""), Path::new(path_part)))
}

fn looks_like_test_path(value: &str) -> bool {
    (value.contains('/') || value.contains('\\'))
        && matches!(
            Path::new(value)
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or(""),
            "go" | "rs" | "py" | "ts" | "tsx" | "js" | "jsx" | "java" | "cs" | "rb"
        )
}

fn test_name_tokens(test: &str) -> Vec<String> {
    if test.chars().any(char::is_whitespace) {
        return (test.len() >= 3)
            .then(|| test.to_string())
            .into_iter()
            .collect();
    }

    let mut tokens = BTreeSet::new();
    for token in test.split([':', '#', '.']) {
        let token = token.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_');
        if token.len() >= 3 {
            tokens.insert(token.to_string());
        }
    }
    tokens.into_iter().collect()
}

fn read_source_content(project_root: &Path, mutation_file: &Path) -> Option<Vec<u8>> {
    let Ok(file_path) = validate_and_resolve_mutation_path(project_root, mutation_file) else {
        return None;
    };
    #[cfg(test)]
    notify_source_content_read(&file_path);
    fs::read(file_path).ok()
}

#[cfg(test)]
type SourceContentReadHook = Arc<dyn Fn(&Path) + Send + Sync + 'static>;

#[cfg(test)]
static SOURCE_CONTENT_READ_HOOK: std::sync::OnceLock<Mutex<Option<SourceContentReadHook>>> =
    std::sync::OnceLock::new();

#[cfg(test)]
fn set_source_content_read_hook(hook: Option<SourceContentReadHook>) {
    let lock = SOURCE_CONTENT_READ_HOOK.get_or_init(|| Mutex::new(None));
    *lock.lock().expect("source content read hook poisoned") = hook;
}

#[cfg(test)]
fn notify_source_content_read(path: &Path) {
    let Some(lock) = SOURCE_CONTENT_READ_HOOK.get() else {
        return;
    };
    let Ok(hook) = lock.lock() else {
        return;
    };
    if let Some(hook) = hook.as_ref() {
        hook(path);
    }
}

pub(crate) fn copy_workspace(project_root: &Path) -> std::io::Result<WorkspaceCopy> {
    copy_workspace_with_options(project_root, true)
}

fn copy_workspace_with_options(
    project_root: &Path,
    respect_ignores: bool,
) -> std::io::Result<WorkspaceCopy> {
    let tempdir = tempfile::tempdir()?;
    let root = tempdir.path().join("workspace");

    if respect_ignores {
        match create_git_worktree_workspace(project_root, &root) {
            Ok(Some(overlay)) => {
                return Ok(WorkspaceCopy {
                    _tempdir: tempdir,
                    root,
                    reset_strategy: WorkspaceResetStrategy::GitWorktree {
                        project_root: project_root.to_path_buf(),
                        overlay,
                    },
                });
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!(
                    "warning: could not create git worktree workspace: {e}; falling back to copy"
                );
            }
        }
    }

    finish_copy_workspace(project_root, tempdir, root, respect_ignores)
}

/// Create an independent Git snapshot for replay without registering a
/// worktree in the source repository. The source working tree is then overlaid
/// so dirty, untracked, and deleted paths retain normal workspace semantics.
fn copy_workspace_for_replay(
    project_root: &Path,
    expected_source_revision: &str,
    respect_ignores: bool,
) -> std::io::Result<ReplayWorkspace> {
    // Pin the trusted temp root and create the outer TempDir and empty clone
    // child through no-follow capabilities before any Git subprocess, so the
    // path Git and later build/test commands use cannot be rebound on
    // Windows.
    let workspace = ReplayWorkspace::create_clone_target()?;
    ensure_replay_snapshot_revision(project_root, expected_source_revision)?;
    clone_replay_snapshot(project_root, workspace.root(), expected_source_revision)?;

    let source_root = CapDir::open_ambient_dir(project_root, ambient_authority())?;
    ensure_replay_snapshot_revision(project_root, expected_source_revision)?;
    let overlay = collect_replay_git_worktree_overlay(project_root)?;
    remove_replay_snapshot_exclusions(&workspace)?;
    populate_replay_workspace(project_root, &source_root, &workspace, respect_ignores)?;
    overlay.apply_replay(&source_root, &workspace)?;
    ensure_replay_snapshot_revision(project_root, expected_source_revision)?;
    Ok(workspace)
}

fn git_snapshot_revision(project_root: &Path) -> std::io::Result<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(project_root)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "could not resolve Git HEAD for replay snapshot in {}\nstderr:\n{}",
            project_root.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let revision = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if revision.is_empty() {
        return Err(std::io::Error::other(
            "could not resolve a non-empty Git HEAD for replay snapshot",
        ));
    }
    Ok(revision)
}

fn ensure_replay_snapshot_revision(
    project_root: &Path,
    expected_source_revision: &str,
) -> std::io::Result<()> {
    let current = git_snapshot_revision(project_root)?;
    if current != expected_source_revision {
        return Err(std::io::Error::other(format!(
            "replay source Git HEAD changed: expected {expected_source_revision}, found {current}"
        )));
    }
    Ok(())
}

fn clone_replay_snapshot(
    project_root: &Path,
    root: &Path,
    source_revision: &str,
) -> std::io::Result<()> {
    let clone = std::process::Command::new("git")
        .args(["clone", "--no-local", "--quiet", "--no-checkout"])
        .arg(project_root)
        .arg(root)
        .output()?;
    if !clone.status.success() {
        return Err(std::io::Error::other(format!(
            "could not create isolated Git replay snapshot from {}\nstderr:\n{}",
            project_root.display(),
            String::from_utf8_lossy(&clone.stderr)
        )));
    }

    let checkout = std::process::Command::new("git")
        .args(["checkout", "--detach", "--quiet", source_revision])
        .current_dir(root)
        .output()?;
    if !checkout.status.success() {
        return Err(std::io::Error::other(format!(
            "could not check out replay snapshot revision {source_revision}\nstderr:\n{}",
            String::from_utf8_lossy(&checkout.stderr)
        )));
    }

    let git_dir = root.join(".git");
    let metadata = fs::symlink_metadata(&git_dir)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(std::io::Error::other(
            "isolated replay snapshot has a non-directory .git entry",
        ));
    }
    Ok(())
}

fn remove_replay_snapshot_exclusions(workspace: &ReplayWorkspace) -> std::io::Result<()> {
    fn remove_entries(
        workspace: &ReplayWorkspace,
        current: &CapDir,
        current_relative: &Path,
    ) -> std::io::Result<()> {
        for entry in current.entries()? {
            let entry = entry?;
            let relative = current_relative.join(entry.file_name());
            // The clone's own independent Git directory is the one excluded
            // entry that must remain for Git-dependent replay commands.
            if relative == Path::new(".git") {
                continue;
            }
            if should_skip_replay_workspace_entry(&relative) {
                workspace.remove_relative(&relative)?;
                continue;
            }
            if entry.file_type()?.is_dir() {
                let child = current.open_dir_nofollow(entry.file_name())?;
                remove_entries(workspace, &child, &relative)?;
            }
        }
        Ok(())
    }

    remove_entries(workspace, &workspace.root, Path::new(""))
}

fn finish_copy_workspace(
    project_root: &Path,
    tempdir: tempfile::TempDir,
    root: PathBuf,
    respect_ignores: bool,
) -> std::io::Result<WorkspaceCopy> {
    fs::create_dir(&root)?;
    populate_workspace(project_root, &root, respect_ignores)?;
    Ok(WorkspaceCopy {
        _tempdir: tempdir,
        root,
        reset_strategy: WorkspaceResetStrategy::Copy,
    })
}

pub fn measure_baseline_timing(
    project_root: &Path,
    config: BaselineTimingConfig<'_>,
) -> anyhow::Result<BaselineMeasurement> {
    if config.test_command.is_empty() {
        bail!("baseline test command is empty");
    }

    let workspace = copy_workspace_with_options(project_root, config.respect_workspace_ignores)
        .with_context(|| "could not create baseline timing workspace")?;
    let root = workspace.root();
    let build_duration =
        if config.build_command_origin.runs_before_tests() && !config.build_command.is_empty() {
            Some(measure_baseline_command(
                SuiteFailurePhase::Build,
                config.build_command,
                config.sandbox_command,
                root,
                config.timeout,
                config.env,
                config.cancelled,
            )?)
        } else {
            None
        };
    let test_duration = measure_baseline_command(
        SuiteFailurePhase::Test,
        config.test_command,
        config.sandbox_command,
        root,
        config.timeout,
        config.env,
        config.cancelled,
    )?;

    Ok(BaselineMeasurement {
        build_duration,
        test_duration,
    })
}
/// Run every unmutated test suite that the pending mutations would use.
///
/// Test selection is intentionally not applied: a narrowed command cannot
/// establish that the full effective suite is healthy. Each suite runs in a
/// fresh isolated workspace, with the enabled build command immediately
/// preceding its test command.
pub fn check_baseline_health(
    project_root: &Path,
    mutations: &[Mutation],
    config: BaselineHealthConfig<'_>,
) -> anyhow::Result<BaselineHealthMeasurement> {
    let suites = resolve_baseline_suites(
        project_root,
        config.commands,
        mutations,
        config.schemata_enabled,
    );
    if suites.is_empty() {
        bail!("no baseline test suites were selected");
    }

    let mut measurements = Vec::with_capacity(suites.len());
    for suite in suites {
        let workspace = copy_workspace_with_options(project_root, config.respect_workspace_ignores)
            .with_context(|| "could not create baseline health workspace")?;
        let root = workspace.root();
        let timeout = baseline_suite_timeout(&suite, config.default_measurement_timeout);
        let build_duration = if config.commands.has_build_command() {
            Some(measure_baseline_command(
                SuiteFailurePhase::Build,
                &config.commands.build_command,
                &config.commands.sandbox_command,
                root,
                timeout,
                config.env,
                config.cancelled,
            )?)
        } else {
            None
        };
        let test_duration = measure_baseline_command(
            SuiteFailurePhase::Test,
            &suite.argv,
            &config.commands.sandbox_command,
            root,
            timeout,
            config.env,
            config.cancelled,
        )?;
        measurements.push(BaselineSuiteMeasurement {
            build_duration,
            test_command: suite.argv,
            test_duration,
            uses_default_timeout: suite.default_timeout.is_some(),
        });
    }

    Ok(BaselineHealthMeasurement {
        suites: measurements,
    })
}

struct ResolvedBaselineSuite {
    argv: Vec<String>,
    default_timeout: Option<Duration>,
    explicit_timeout: Option<Duration>,
}

fn baseline_suite_timeout(
    suite: &ResolvedBaselineSuite,
    default_measurement_timeout: Option<Duration>,
) -> Duration {
    let default_timeout = suite
        .default_timeout
        .map(|timeout| default_measurement_timeout.unwrap_or(timeout));
    match (default_timeout, suite.explicit_timeout) {
        (Some(default), Some(explicit)) => default.min(explicit),
        (Some(timeout), None) | (None, Some(timeout)) => timeout,
        (None, None) => unreachable!("baseline suite has a timeout"),
    }
}

fn resolve_baseline_suites(
    project_root: &Path,
    commands: &CommandConfig,
    mutations: &[Mutation],
    schemata_enabled: bool,
) -> Vec<ResolvedBaselineSuite> {
    let mut suites = Vec::new();
    for mutation in mutations {
        let mut selected = select_unnarrowed_test_command(project_root, commands, mutation);
        if schemata_enabled && mutation.language == "go" {
            selected.argv = force_go_no_test_cache(selected.argv);
        }
        if let Some(existing) = suites
            .iter_mut()
            .find(|suite: &&mut ResolvedBaselineSuite| suite.argv == selected.argv)
        {
            let timeout = if selected.uses_default_timeout {
                &mut existing.default_timeout
            } else {
                &mut existing.explicit_timeout
            };
            *timeout =
                Some(timeout.map_or(selected.timeout, |current| current.min(selected.timeout)));
        } else {
            suites.push(ResolvedBaselineSuite {
                argv: selected.argv,
                default_timeout: selected.uses_default_timeout.then_some(selected.timeout),
                explicit_timeout: (!selected.uses_default_timeout).then_some(selected.timeout),
            });
        }
    }
    suites
}

fn measure_baseline_command(
    phase: SuiteFailurePhase,
    command: &[String],
    sandbox_command: &[String],
    cwd: &Path,
    timeout: Duration,
    env: &HashMap<String, String>,
    cancelled: &AtomicBool,
) -> anyhow::Result<Duration> {
    let started = Instant::now();
    let outcome = run_command(command, sandbox_command, cwd, timeout, true, env, cancelled);
    let duration = started.elapsed();
    if outcome.cancelled {
        bail!("baseline timing cancelled");
    }

    match outcome.result {
        MutationResult::Survived | MutationResult::Uncovered | MutationResult::Subsumed => {
            Ok(duration)
        }
        MutationResult::Killed => Err(RunSuiteFailure {
            phase,
            command: command.to_vec(),
            outcome: RunSuiteFailureOutcome::Failed {
                output: baseline_failure_output(outcome.test_output.as_deref()),
            },
        }
        .into()),
        MutationResult::Timeout => Err(RunSuiteFailure {
            phase,
            command: command.to_vec(),
            outcome: RunSuiteFailureOutcome::TimedOut { timeout },
        }
        .into()),
        MutationResult::BuildError => {
            let detail = outcome
                .build_error_detail
                .as_ref()
                .map(|detail| detail.message.clone())
                .unwrap_or_else(|| "command could not run".to_string());
            Err(RunSuiteFailure {
                phase,
                command: command.to_vec(),
                outcome: RunSuiteFailureOutcome::CannotRun { detail },
            }
            .into())
        }
    }
}

fn command_for_message(command: &[String]) -> String {
    if command.is_empty() {
        "<empty>".to_string()
    } else {
        command.join(" ")
    }
}

fn sandboxed_command(sandbox_command: &[String], command: &[String]) -> Vec<String> {
    if sandbox_command.is_empty() {
        return command.to_vec();
    }
    let mut argv = sandbox_command.to_vec();
    argv.extend_from_slice(command);
    argv
}

fn baseline_failure_output(output: Option<&str>) -> Option<String> {
    let output = output.map(str::trim).filter(|output| !output.is_empty())?;
    Some(output.lines().take(6).collect::<Vec<_>>().join("\n"))
}

fn create_git_worktree_workspace(
    project_root: &Path,
    root: &Path,
) -> std::io::Result<Option<GitWorktreeOverlay>> {
    if !git_worktree_workspace_is_available(project_root) {
        return Ok(None);
    }
    let overlay = collect_git_worktree_overlay(project_root)?;

    let output = std::process::Command::new("git")
        .args(["worktree", "add", "--detach", "--quiet"])
        .arg(root)
        .arg("HEAD")
        .current_dir(project_root)
        .output()?;
    if output.status.success() {
        if let Err(e) = overlay.apply(project_root, root) {
            let _ = remove_git_worktree(project_root, root);
            return Err(e);
        }
        return Ok(Some(overlay));
    }

    if root.exists() {
        let _ = fs::remove_dir_all(root);
    }
    Ok(None)
}

fn git_worktree_workspace_is_available(project_root: &Path) -> bool {
    let Some(top_level) = git_top_level(project_root) else {
        return false;
    };
    let Ok(project_root) = project_root.canonicalize() else {
        return false;
    };
    if top_level != project_root {
        return false;
    }

    std::process::Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(&project_root)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn git_top_level(project_root: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(project_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let top_level = String::from_utf8(output.stdout).ok()?;
    PathBuf::from(top_level.trim()).canonicalize().ok()
}

fn collect_git_worktree_overlay(project_root: &Path) -> std::io::Result<GitWorktreeOverlay> {
    let changed_paths = git_z_output_paths(
        project_root,
        &["diff", "--name-only", "-z", "--no-renames", "HEAD", "--"],
    )?;
    let untracked_paths = git_z_output_paths(
        project_root,
        &["ls-files", "-z", "--others", "--exclude-standard", "--"],
    )?;

    let mut copy_paths = BTreeSet::new();
    let mut remove_paths = BTreeSet::new();

    for relative in changed_paths {
        if !should_overlay_workspace_entry(&relative) {
            continue;
        }
        if project_root.join(&relative).is_file() {
            copy_paths.insert(relative);
        } else {
            remove_paths.insert(relative);
        }
    }

    for relative in untracked_paths {
        if !should_overlay_workspace_entry(&relative) {
            continue;
        }
        if project_root.join(&relative).is_file() {
            copy_paths.insert(relative);
        }
    }

    for copied in &copy_paths {
        remove_paths.remove(copied);
    }

    Ok(GitWorktreeOverlay {
        copy_paths: copy_paths.into_iter().collect(),
        remove_paths: remove_paths.into_iter().collect(),
    })
}

fn collect_replay_git_worktree_overlay(project_root: &Path) -> std::io::Result<GitWorktreeOverlay> {
    let changed_paths = git_z_output_paths(
        project_root,
        &["diff", "--name-only", "-z", "--no-renames", "HEAD", "--"],
    )?;
    let untracked_paths = git_z_output_paths(
        project_root,
        &["ls-files", "-z", "--others", "--exclude-standard", "--"],
    )?;

    let mut copy_paths = BTreeSet::new();
    let mut remove_paths = BTreeSet::new();

    for relative in changed_paths {
        if !should_overlay_replay_workspace_entry(&relative) {
            continue;
        }
        if replay_source_relative_file_is_regular(project_root, &relative)? {
            copy_paths.insert(relative);
        } else {
            remove_paths.insert(relative);
        }
    }

    for relative in untracked_paths {
        if !should_overlay_replay_workspace_entry(&relative) {
            continue;
        }
        if replay_source_relative_file_is_regular(project_root, &relative)? {
            copy_paths.insert(relative);
        }
    }

    for copied in &copy_paths {
        remove_paths.remove(copied);
    }

    Ok(GitWorktreeOverlay {
        copy_paths: copy_paths.into_iter().collect(),
        remove_paths: remove_paths.into_iter().collect(),
    })
}

/// Replay-only source classification without following symlinked parents into
/// source-root control state.
fn replay_source_relative_file_is_regular(
    project_root: &Path,
    relative: &Path,
) -> std::io::Result<bool> {
    if !is_safe_relative_path(relative) {
        return Ok(false);
    }
    let mut current = project_root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(component) = component else {
            return Ok(false);
        };
        current.push(component);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if components.peek().is_none() {
            return Ok(metadata.file_type().is_file());
        }
        if !metadata.file_type().is_dir() {
            return Ok(false);
        }
    }
    Ok(false)
}

fn git_z_output_paths(project_root: &Path, args: &[&str]) -> std::io::Result<Vec<PathBuf>> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(project_root)
        .output()?;
    if !output.status.success() {
        return Err(std::io::Error::other(format!(
            "git {} failed in {}\nstderr:\n{}",
            args.join(" "),
            project_root.display(),
            String::from_utf8_lossy(&output.stderr)
        )));
    }

    Ok(output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
        .filter_map(safe_git_relative_path)
        .collect())
}

fn should_overlay_workspace_entry(relative: &Path) -> bool {
    is_safe_relative_path(relative) && !should_skip_workspace_entry(relative)
}

fn should_overlay_replay_workspace_entry(relative: &Path) -> bool {
    is_safe_relative_path(relative) && !should_skip_replay_workspace_entry(relative)
}

fn safe_git_relative_path(raw: &[u8]) -> Option<PathBuf> {
    let raw = std::str::from_utf8(raw).ok()?;
    if raw.is_empty()
        || raw.starts_with('/')
        || raw.starts_with('\\')
        || raw.contains('\\')
        || raw.contains('\0')
    {
        return None;
    }

    let mut path = PathBuf::new();
    for part in raw.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return None;
        }
        path.push(part);
    }
    Some(path)
}

fn is_safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, std::path::Component::Normal(_)))
}

fn remove_git_worktree(project_root: &Path, root: &Path) -> std::io::Result<()> {
    let output = std::process::Command::new("git")
        .args(["worktree", "remove", "--force"])
        .arg(root)
        .current_dir(project_root)
        .output()?;
    if output.status.success() {
        return Ok(());
    }
    Err(std::io::Error::other(format!(
        "could not remove git worktree {}\nstderr:\n{}",
        root.display(),
        String::from_utf8_lossy(&output.stderr)
    )))
}

fn reset_copied_workspace(
    project_root: &Path,
    workspace_root: &Path,
    respect_ignores: bool,
) -> std::io::Result<()> {
    let preserved_dirs = preserve_workspace_dirs(workspace_root)?;
    if workspace_root.exists() {
        fs::remove_dir_all(workspace_root)?;
    }
    fs::create_dir(workspace_root)?;
    restore_workspace_dirs(workspace_root, preserved_dirs)?;
    populate_workspace(project_root, workspace_root, respect_ignores)
}

fn reset_git_worktree(
    project_root: &Path,
    workspace_root: &Path,
    overlay: &GitWorktreeOverlay,
) -> std::io::Result<()> {
    run_git_workspace_command(workspace_root, &["reset", "--hard", "--quiet", "HEAD"])?;

    let mut clean_args = vec!["clean", "-ffdx", "--quiet"];
    for preserved in PRESERVED_WORKSPACE_DIRS {
        clean_args.push("-e");
        clean_args.push(preserved);
    }
    run_git_workspace_command(workspace_root, &clean_args)?;
    overlay.apply(project_root, workspace_root)
}

fn run_git_workspace_command(workspace_root: &Path, args: &[&str]) -> std::io::Result<()> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(workspace_root)
        .output()?;
    if output.status.success() {
        return Ok(());
    }

    Err(std::io::Error::other(format!(
        "git {} failed in {}\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        workspace_root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )))
}

fn remove_workspace_path(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };

    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn ensure_workspace_root(workspace_root: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(workspace_root)?;
    if metadata.file_type().is_dir() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "workspace root {} is not a directory",
            workspace_root.display()
        )))
    }
}

fn invalid_workspace_relative_path(relative: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!(
            "workspace destination path {} is not a safe relative path",
            relative.display()
        ),
    )
}

/// Create one directory after replacing any non-directory entry at its leaf.
/// Callers first materialize all ancestor directories, so this never follows a
/// symlink while preparing a workspace destination.
fn materialize_workspace_directory(path: &Path) -> std::io::Result<()> {
    loop {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.file_type().is_dir() => return Ok(()),
            Ok(_) => remove_workspace_path(path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        match fs::create_dir(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
}

fn materialize_workspace_directory_at(
    workspace_root: &Path,
    relative: &Path,
) -> std::io::Result<()> {
    if !is_safe_relative_path(relative) {
        return Err(invalid_workspace_relative_path(relative));
    }
    ensure_workspace_root(workspace_root)?;
    let mut current = workspace_root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(component) = component else {
            return Err(invalid_workspace_relative_path(relative));
        };
        current.push(component);
        materialize_workspace_directory(&current)?;
    }
    Ok(())
}

fn materialize_workspace_parent(workspace_root: &Path, relative: &Path) -> std::io::Result<()> {
    if !is_safe_relative_path(relative) {
        return Err(invalid_workspace_relative_path(relative));
    }
    ensure_workspace_root(workspace_root)?;
    if let Some(parent) = relative
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        materialize_workspace_directory_at(workspace_root, parent)?;
    }
    Ok(())
}

/// Prepare a destination below a workspace without retaining a leaf symlink.
fn prepare_workspace_destination(
    workspace_root: &Path,
    relative: &Path,
) -> std::io::Result<PathBuf> {
    materialize_workspace_parent(workspace_root, relative)?;
    let destination = workspace_root.join(relative);
    remove_workspace_path(&destination)?;
    Ok(destination)
}

fn copy_regular_source_file_to_workspace(
    source: &Path,
    workspace_root: &Path,
    relative: &Path,
) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if !metadata.file_type().is_file() {
        return Err(std::io::Error::other(format!(
            "source entry {} is not a regular file",
            source.display()
        )));
    }
    let destination = prepare_workspace_destination(workspace_root, relative)?;
    fs::copy(source, destination)?;
    Ok(())
}

fn normal_overlay_source_is_control_path(relative: &Path) -> bool {
    relative.components().any(|component| {
        component.as_os_str().to_str().is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            name == ".git" || name == ".togi" || name == ".togi.lock" || name.starts_with(".togi-")
        })
    })
}

fn resolve_normal_overlay_source(
    project_root: &Path,
    relative: &Path,
) -> std::io::Result<Option<PathBuf>> {
    if !is_safe_relative_path(relative) {
        return Err(invalid_workspace_relative_path(relative));
    }
    let source = project_root.join(relative);
    if !source.is_file() {
        return Ok(None);
    }
    let normalized = normalized_project_relative_path(project_root, relative).ok_or_else(|| {
        std::io::Error::other(format!(
            "workspace overlay source {} is not project-relative",
            source.display()
        ))
    })?;
    let resolved =
        resolve_normalized_project_relative_path(project_root, &normalized).ok_or_else(|| {
            std::io::Error::other(format!(
                "workspace overlay source {} does not resolve within the project root",
                source.display()
            ))
        })?;
    let root = project_root.canonicalize()?;
    let resolved_relative = resolved.strip_prefix(root).map_err(|_| {
        std::io::Error::other(format!(
            "workspace overlay source {} does not resolve within the project root",
            source.display()
        ))
    })?;
    if normal_overlay_source_is_control_path(resolved_relative) {
        return Err(std::io::Error::other(format!(
            "workspace overlay source {} resolves into Togi or Git control state",
            source.display()
        )));
    }
    if !fs::metadata(&resolved)?.is_file() {
        return Ok(None);
    }
    Ok(Some(resolved))
}

fn copy_overlay_file(
    project_root: &Path,
    workspace_root: &Path,
    relative: &Path,
) -> std::io::Result<()> {
    let Some(source) = resolve_normal_overlay_source(project_root, relative)? else {
        remove_workspace_path(&workspace_root.join(relative))?;
        return Ok(());
    };
    let destination = prepare_workspace_destination(workspace_root, relative)?;
    fs::copy(source, destination)?;
    Ok(())
}

/// Move preserved dirs to sibling stashes before deleting the workspace.
///
/// The sibling stash location keeps rename on the same filesystem; removing an
/// existing stash is intentional recovery from an interrupted previous reset.
fn preserve_workspace_dirs(workspace_root: &Path) -> std::io::Result<Vec<(PathBuf, PathBuf)>> {
    let Some(parent) = workspace_root.parent() else {
        return Ok(Vec::new());
    };

    let mut preserved = Vec::new();
    for name in PRESERVED_WORKSPACE_DIRS {
        let source = workspace_root.join(name);
        if !source.exists() {
            continue;
        }

        let stash = parent.join(format!(".togi-preserved-{name}"));
        if stash.exists() {
            fs::remove_dir_all(&stash)?;
        }
        fs::rename(&source, &stash)?;
        preserved.push((PathBuf::from(name), stash));
    }
    Ok(preserved)
}

fn restore_workspace_dirs(
    workspace_root: &Path,
    preserved_dirs: Vec<(PathBuf, PathBuf)>,
) -> std::io::Result<()> {
    for (relative, stash) in preserved_dirs {
        let destination = prepare_workspace_destination(workspace_root, &relative)?;
        fs::rename(stash, destination)?;
    }
    Ok(())
}

fn populate_workspace(
    project_root: &Path,
    root: &Path,
    respect_ignores: bool,
) -> std::io::Result<()> {
    let project_root_for_filter = project_root.to_path_buf();

    let mut builder = ignore::WalkBuilder::new(project_root);
    builder
        .hidden(false)
        .ignore(respect_ignores)
        .git_ignore(respect_ignores)
        .git_exclude(respect_ignores)
        .git_global(respect_ignores)
        .parents(respect_ignores)
        .filter_entry(move |entry| {
            should_copy_workspace_entry(&project_root_for_filter, entry.path())
        });

    for entry in builder.build() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                return Err(std::io::Error::other(err));
            }
        };
        let path = entry.path();
        if path == project_root {
            continue;
        }
        let relative = match path.strip_prefix(project_root) {
            Ok(relative) => relative,
            Err(_) => continue,
        };

        if entry.file_type().is_some_and(|ft| ft.is_dir()) {
            materialize_workspace_directory_at(root, relative)?;
        } else if entry.file_type().is_some_and(|ft| ft.is_file()) {
            copy_regular_source_file_to_workspace(path, root, relative)?;
        }
    }

    Ok(())
}

/// Populate a replay clone through capabilities rather than destination paths.
fn populate_replay_workspace(
    project_root: &Path,
    source_root: &CapDir,
    workspace: &ReplayWorkspace,
    respect_ignores: bool,
) -> std::io::Result<()> {
    let project_root_for_filter = project_root.to_path_buf();
    let mut builder = ignore::WalkBuilder::new(project_root);
    builder
        .hidden(false)
        .ignore(respect_ignores)
        .git_ignore(respect_ignores)
        .git_exclude(respect_ignores)
        .git_global(respect_ignores)
        .parents(respect_ignores)
        .filter_entry(move |entry| {
            should_copy_replay_workspace_entry(&project_root_for_filter, entry.path())
        });

    for entry in builder.build() {
        let entry = entry.map_err(std::io::Error::other)?;
        let path = entry.path();
        if path == project_root {
            continue;
        }
        let relative = match path.strip_prefix(project_root) {
            Ok(relative) if is_safe_relative_path(relative) => relative,
            _ => continue,
        };
        if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_dir())
        {
            workspace.ensure_directory(relative)?;
        } else if entry
            .file_type()
            .is_some_and(|file_type| file_type.is_file())
        {
            if cap_relative_file_is_regular(source_root, relative)? {
                workspace.copy_regular_source(source_root, relative)?;
            } else {
                workspace.remove_relative(relative)?;
            }
        }
    }
    Ok(())
}

pub(crate) struct WorkspacePool {
    slots: Arc<Vec<WorkspaceCopy>>,
    free_slots: Arc<(Mutex<VecDeque<usize>>, Condvar)>,
    dirty_slots: Arc<Mutex<Vec<bool>>>,
}

impl WorkspacePool {
    pub(crate) fn new(project_root: &Path, slots: usize) -> std::io::Result<Self> {
        Self::new_with_options(project_root, slots, true)
    }

    fn new_with_options(
        project_root: &Path,
        slots: usize,
        respect_ignores: bool,
    ) -> std::io::Result<Self> {
        let slots = slots.max(1);
        let mut copies = Vec::with_capacity(slots);
        for _ in 0..slots {
            let copy = if respect_ignores {
                copy_workspace(project_root)?
            } else {
                copy_workspace_with_options(project_root, false)?
            };
            copies.push(copy);
        }

        let free_slots = (0..slots).collect();
        let dirty_slots = vec![false; slots];

        Ok(Self {
            slots: Arc::new(copies),
            free_slots: Arc::new((Mutex::new(free_slots), Condvar::new())),
            dirty_slots: Arc::new(Mutex::new(dirty_slots)),
        })
    }

    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn acquire(&self) -> WorkspaceSlot {
        let (lock, cvar) = &*self.free_slots;
        let mut free_slots = lock.lock().expect("workspace free-list mutex poisoned");
        let index = loop {
            if let Some(index) = free_slots.pop_front() {
                break index;
            }
            free_slots = cvar
                .wait(free_slots)
                .expect("workspace free-list mutex poisoned");
        };
        let needs_reset = {
            let mut dirty_slots = self
                .dirty_slots
                .lock()
                .expect("workspace dirty-list mutex poisoned");
            let needs_reset = dirty_slots[index];
            dirty_slots[index] = true;
            needs_reset
        };

        WorkspaceSlot {
            slots: self.slots.clone(),
            free_slots: self.free_slots.clone(),
            index,
            needs_reset,
        }
    }
}

pub(crate) struct WorkspaceSlot {
    slots: Arc<Vec<WorkspaceCopy>>,
    free_slots: Arc<(Mutex<VecDeque<usize>>, Condvar)>,
    index: usize,
    needs_reset: bool,
}

impl WorkspaceSlot {
    pub(crate) fn root(&self) -> &Path {
        self.slots[self.index].root()
    }

    fn needs_reset(&self) -> bool {
        self.needs_reset
    }

    fn reset(&self, project_root: &Path, respect_ignores: bool) -> std::io::Result<()> {
        self.slots[self.index].reset(project_root, respect_ignores)
    }
}

impl Drop for WorkspaceSlot {
    fn drop(&mut self) {
        let (lock, cvar) = &*self.free_slots;
        lock.lock()
            .expect("workspace free-list mutex poisoned")
            .push_back(self.index);
        cvar.notify_one();
    }
}

struct QueuedMutation {
    index: usize,
    mutation: Mutation,
    restore_checked: bool,
    primary_restore: Option<RestoredMutationResult>,
}

#[derive(Debug)]
struct MutationRunRecord {
    mutation: Mutation,
    result: MutationResult,
    execution: MutationExecution,
    selection: Option<TestSelectionProvenance>,
    build_error_diagnostic: Option<BuildErrorDiagnostic>,
    replay_recipe: Option<RegularDirectRecipe>,
}

impl MutationRunRecord {
    fn new(
        mutation: Mutation,
        result: MutationResult,
        build_error_diagnostic: Option<BuildErrorDiagnostic>,
    ) -> Self {
        Self {
            mutation,
            result,
            execution: MutationExecution::for_result(result),
            build_error_diagnostic,
            selection: None,
            replay_recipe: None,
        }
    }

    fn with_execution(mut self, execution: MutationExecution) -> Self {
        self.execution = execution;
        self
    }

    fn with_selection(mut self, selection: Option<TestSelectionProvenance>) -> Self {
        self.selection = selection;
        self
    }

    fn with_replay_recipe(mut self, replay_recipe: RegularDirectRecipe) -> Self {
        self.replay_recipe = Some(replay_recipe);
        self
    }
}

struct PreclassifiedMutations {
    fresh: Vec<QueuedMutation>,
    restored: Vec<(usize, MutationRunRecord)>,
}

#[derive(Default)]
struct SchemataRunSummary {
    fast_path: usize,
    fallback: usize,
    fallback_reasons: BTreeMap<String, usize>,
}

impl SchemataRunSummary {
    fn record_fallback(&mut self, reason: &str) {
        self.record_fallbacks(reason, 1);
    }

    fn record_fallbacks(&mut self, reason: &str, count: usize) {
        self.fallback += count;
        *self.fallback_reasons.entry(reason.to_string()).or_default() += count;
    }

    fn into_report(self) -> SchemataReport {
        SchemataReport {
            fast_path: self.fast_path,
            fallback: self.fallback,
            fallback_reasons: self
                .fallback_reasons
                .into_iter()
                .map(|(reason, count)| SchemataFallbackReasonCount { reason, count })
                .collect(),
        }
    }
}

#[derive(Default)]
struct EarlyStopCounts {
    fresh_eligible_total: usize,
    completed: usize,
    killed: usize,
    survived: usize,
    build_errors: usize,
    exact_cache_gate_total: usize,
    exact_cache_gate_killed: usize,
}

impl EarlyStopCounts {
    fn record_exact_cache(&mut self, result: MutationResult) {
        match result {
            MutationResult::Killed => {
                self.exact_cache_gate_total += 1;
                self.exact_cache_gate_killed += 1;
            }
            MutationResult::Survived | MutationResult::Timeout => {
                self.exact_cache_gate_total += 1;
            }
            MutationResult::BuildError | MutationResult::Uncovered | MutationResult::Subsumed => {}
        }
    }
}

struct EarlyStopState {
    config: EarlyStopConfig,
    stopped: AtomicBool,
    reason: Mutex<Option<String>>,
    counts: Mutex<EarlyStopCounts>,
}

impl EarlyStopState {
    fn new(config: EarlyStopConfig, planned_total: usize) -> Self {
        Self {
            config,
            stopped: AtomicBool::new(false),
            reason: Mutex::new(None),
            counts: Mutex::new(EarlyStopCounts {
                fresh_eligible_total: planned_total,
                ..Default::default()
            }),
        }
    }

    fn for_config(config: EarlyStopConfig, planned_total: usize) -> Option<Arc<Self>> {
        config
            .is_enabled()
            .then(|| Arc::new(Self::new(config, planned_total)))
    }

    fn should_stop(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    fn reason(&self) -> Option<String> {
        self.reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Fresh results are the only evidence for survivor-count early stopping.
    fn record_fresh(&self, result: MutationResult) {
        if self.should_stop() {
            return;
        }

        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        counts.completed += 1;
        match result {
            MutationResult::Killed => counts.killed += 1,
            MutationResult::Survived => counts.survived += 1,
            MutationResult::Timeout | MutationResult::Uncovered | MutationResult::Subsumed => {}
            MutationResult::BuildError => counts.build_errors += 1,
        }
        self.reevaluate(&counts);
    }

    /// Add all final exact-cache verdicts together before selecting workers.
    fn record_preclassified_exact_cache(&self, restored: &[(usize, MutationRunRecord)]) {
        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (_, record) in restored {
            if record.execution == MutationExecution::ExactCache {
                counts.record_exact_cache(record.result);
            }
        }
        self.reevaluate(&counts);
    }

    /// A late restored verdict no longer consumes fresh work; only an exact
    /// cache verdict contributes to the independent fail-under gate evidence.
    fn record_restored(&self, execution: MutationExecution, result: MutationResult) {
        if self.should_stop() {
            return;
        }

        let mut counts = self
            .counts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if execution.is_reused() {
            counts.fresh_eligible_total = counts.fresh_eligible_total.saturating_sub(1);
        }
        if execution == MutationExecution::ExactCache {
            counts.record_exact_cache(result);
        }
        self.reevaluate(&counts);
    }

    fn reevaluate(&self, counts: &EarlyStopCounts) {
        if self.should_stop() {
            return;
        }

        let remaining = counts.fresh_eligible_total.saturating_sub(counts.completed);
        if remaining == 0 {
            return;
        }

        if let Some(max_survivors) = self.config.max_survivors {
            if counts.survived >= max_survivors {
                self.stop(format!(
                    "--max-survivors {max_survivors} reached after {} completed mutation{}",
                    counts.completed,
                    if counts.completed == 1 { "" } else { "s" }
                ));
                return;
            }
        }

        if let Some(threshold) = self.config.fail_under {
            let fresh_tested = counts.completed.saturating_sub(counts.build_errors);
            let best_tested = counts.exact_cache_gate_total + fresh_tested + remaining;
            let best_killed = counts.exact_cache_gate_killed + counts.killed + remaining;
            let best_score = if best_tested > 0 {
                (best_killed as f64 / best_tested as f64) * 100.0
            } else if counts.fresh_eligible_total == 0 && counts.exact_cache_gate_total == 0 {
                100.0
            } else {
                0.0
            };
            if best_score < threshold {
                self.stop(format!(
                    "--fail-under {threshold:.1} cannot be reached; best possible fail-under gate score is {best_score:.1}%"
                ));
            }
        }
    }

    fn stop(&self, reason: String) {
        let mut current = self
            .reason
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if current.is_none() {
            *current = Some(reason);
            self.stopped.store(true, Ordering::Release);
        }
    }
}

struct RunShared<'a> {
    workspace_pool: &'a WorkspacePool,
    project_root: &'a Path,
    commands: &'a CommandConfig,
    build_command: &'a [String],
    build_command_origin: BuildCommandOrigin,
    env: &'a HashMap<String, String>,
    total: usize,
    verbose: bool,
    is_tty: bool,
    show_output: bool,
    counter: &'a AtomicUsize,
    cancelled: &'a AtomicBool,
    early_stop: Option<&'a Arc<EarlyStopState>>,
    respect_workspace_ignores: bool,
    cache_context_fingerprint: u64,
    test_context_index: &'a TestContextIndex,
    source_contents: &'a SourceContentCache,
    history: Option<&'a cache::IncrementalHistoryStore>,
    force_rerun: bool,
}

fn should_stop_early(early_stop: &Option<Arc<EarlyStopState>>) -> bool {
    early_stop.as_ref().is_some_and(|state| state.should_stop())
}

fn record_fresh_early_stop(shared: &RunShared<'_>, result: MutationResult) {
    if let Some(early_stop) = shared.early_stop {
        early_stop.record_fresh(result);
    }
}

fn record_restored_for_early_stop(
    early_stop: Option<&Arc<EarlyStopState>>,
    execution: MutationExecution,
    result: MutationResult,
) {
    if let Some(early_stop) = early_stop {
        early_stop.record_restored(execution, result);
    }
}

fn incremental_history_query(
    project_root: &Path,
    mutation: &Mutation,
    source_content: &[u8],
    command_context: &str,
    relevant_test_hash: u64,
) -> cache::IncrementalHistoryQuery {
    cache::IncrementalHistoryQuery {
        mutation_identity: cache_identity(project_root, mutation),
        mutation_description: mutation.description.clone(),
        source_hash: cache::hash_bytes(source_content),
        command_hash: cache::hash_str(command_context),
        relevant_test_hash,
    }
}

fn record_incremental_history(
    history: Option<&cache::IncrementalHistoryStore>,
    query: Option<&cache::IncrementalHistoryQuery>,
    selected_tests: &[String],
    result: MutationResult,
    previous_killer: Option<String>,
    failing_tests: &[String],
) {
    let (Some(history), Some(query)) = (history, query) else {
        return;
    };
    // Full-suite runs have no narrowed test list; fall back to failing tests
    // attributed from the captured output so later runs learn a killer (#428).
    let covering_tests: Vec<String> = if selected_tests.is_empty() {
        failing_tests.to_vec()
    } else {
        selected_tests.to_vec()
    };
    let killer_test = if result == MutationResult::Killed {
        previous_killer
            .filter(|killer| covering_tests.iter().any(|test| test == killer))
            .or_else(|| (selected_tests.len() == 1).then(|| selected_tests[0].clone()))
            .or_else(|| failing_tests.first().cloned())
    } else {
        None
    };
    history.record(cache::IncrementalHistoryEntry {
        mutation_identity: query.mutation_identity.clone(),
        mutation_description: query.mutation_description.clone(),
        result,
        source_hash: query.source_hash,
        command_hash: query.command_hash,
        relevant_test_hash: query.relevant_test_hash,
        covering_tests,
        killer_test,
    });
}

/// Attribute failing test names from captured full-suite test output (#428).
///
/// Narrowed runs know their tests up front; full-suite runs only learn which
/// test killed a mutant from the output. Supports the common formats:
/// cargo test (`test path::to::test ... FAILED`), go test (`--- FAIL: Name`),
/// pytest (`FAILED path::test - reason`), and unittest (`FAIL`/`ERROR:` lines).
fn attribute_failing_tests(test_output: &str) -> Vec<String> {
    let mut tests = Vec::new();
    let mut push = |name: &str| {
        if !name.is_empty() && !tests.iter().any(|test| test == name) {
            tests.push(name.to_string());
        }
    };
    for line in test_output.lines() {
        if let Some(name) = line
            .strip_prefix("test ")
            .and_then(|rest| rest.strip_suffix(" ... FAILED"))
        {
            push(name);
        } else if let Some(rest) = line.strip_prefix("--- FAIL: ") {
            if let Some(name) = rest.split_whitespace().next() {
                push(name);
            }
        } else if let Some(rest) = line
            .strip_prefix("FAILED ")
            .or_else(|| line.strip_prefix("FAIL: "))
            .or_else(|| line.strip_prefix("ERROR: "))
        {
            if let Some(name) = rest.split_whitespace().next() {
                push(name);
            }
        }
    }
    tests
}

struct PreparedMutationRun {
    selected_test: SelectedTestCommand,
    previous_killer: Option<String>,
    history_query: Option<cache::IncrementalHistoryQuery>,
    cache_key: Option<CacheKey>,
}

#[derive(Clone, Copy)]
struct RestoredMutationResult {
    result: MutationResult,
    execution: MutationExecution,
}

struct PreparedMutationContext<'a> {
    commands: &'a CommandConfig,
    history: Option<&'a cache::IncrementalHistoryStore>,
    source_contents: &'a SourceContentCache,
    cache_context_fingerprint: u64,
    test_context_index: &'a TestContextIndex,
    env: &'a HashMap<String, String>,
}

impl PreparedMutationRun {
    fn new(project_root: &Path, mutation: &Mutation, context: PreparedMutationContext<'_>) -> Self {
        let selected_test = select_test_command_with_history(
            project_root,
            context.commands,
            mutation,
            context.history,
        );
        let mutation_identity = cache_identity(project_root, mutation);
        let command_ctx = selected_test.cache_context(
            &context.commands.build_command,
            context.commands.build_command_origin,
            context.commands.sandbox_command.as_slice(),
            context.env,
        );
        let relevant_test_hash = context.test_context_index.fingerprint_for_tests(
            &selected_test.selected_tests,
            context.cache_context_fingerprint,
        );
        let previous_killer = context.history.and_then(|history| {
            history.preferred_killer_test(
                &mutation_identity,
                &mutation.description,
                &selected_test.selected_tests,
            )
        });
        let cache_ctx = format!(
            "{command_ctx};context={:016x}",
            context.cache_context_fingerprint
        );
        let source_content = context
            .source_contents
            .content_for(project_root, &mutation.file);
        let history_query = source_content.as_deref().map(|content| {
            incremental_history_query(
                project_root,
                mutation,
                content,
                &command_ctx,
                relevant_test_hash,
            )
        });
        let cache_key = source_content.as_ref().map(|content| {
            CacheKey::new(
                content,
                &mutation_identity,
                &mutation.description,
                &cache_ctx,
            )
        });

        Self {
            selected_test,
            previous_killer,
            history_query,
            cache_key,
        }
    }

    /// Capture the final direct argv before cache/history lookup. The recipe
    /// deliberately contains no configuration lookup inputs: replay must not
    /// resolve current routes, selection, or sandbox settings again.
    fn direct_recipe(
        &self,
        commands: &CommandConfig,
        env: &HashMap<String, String>,
        respect_workspace_ignores: bool,
        origin: DirectRecipeOrigin,
    ) -> RegularDirectRecipe {
        self.direct_recipe_for(
            &self.selected_test.argv,
            commands,
            env,
            respect_workspace_ignores,
            origin,
        )
    }

    fn direct_recipe_for(
        &self,
        test_command: &[String],
        commands: &CommandConfig,
        env: &HashMap<String, String>,
        respect_workspace_ignores: bool,
        origin: DirectRecipeOrigin,
    ) -> RegularDirectRecipe {
        let build_command = commands
            .has_build_command()
            .then(|| sandboxed_command(&commands.sandbox_command, &commands.build_command));
        let build_command_origin = build_command
            .as_ref()
            .map(|_| commands.build_command_origin)
            .unwrap_or(BuildCommandOrigin::None);
        RegularDirectRecipe {
            test_command: sandboxed_command(&commands.sandbox_command, test_command),
            build_command,
            build_command_origin,
            timeout_ms: u64::try_from(self.selected_test.timeout.as_millis()).unwrap_or(u64::MAX),
            env: env
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
            respect_workspace_ignores,
            origin,
        }
    }

    fn restore_result(
        &self,
        project_root: &Path,
        history: Option<&cache::IncrementalHistoryStore>,
        force_rerun: bool,
    ) -> Option<RestoredMutationResult> {
        if force_rerun {
            return None;
        }
        if let Some(key) = &self.cache_key {
            if let Some(result) = cache::lookup(project_root, key) {
                self.record_history(history, result, None);
                return Some(RestoredMutationResult {
                    result,
                    execution: MutationExecution::ExactCache,
                });
            }
        }
        if let (Some(history), Some(query)) = (history, self.history_query.as_ref()) {
            if let Some(result) = history.lookup(query) {
                if let Some(key) = &self.cache_key {
                    cache::store(project_root, key, result);
                }
                self.record_history(Some(history), result, None);
                return Some(RestoredMutationResult {
                    result,
                    execution: MutationExecution::IncrementalHistory,
                });
            }
        }
        None
    }

    fn store_cache(&self, project_root: &Path, result: MutationResult) {
        if let Some(key) = &self.cache_key {
            cache::store(project_root, key, result);
        }
    }

    fn record_history(
        &self,
        history: Option<&cache::IncrementalHistoryStore>,
        result: MutationResult,
        test_output: Option<&str>,
    ) {
        let failing_tests = if result == MutationResult::Killed {
            test_output.map(attribute_failing_tests).unwrap_or_default()
        } else {
            Vec::new()
        };
        record_incremental_history(
            history,
            self.history_query.as_ref(),
            &self.selected_test.selected_tests,
            result,
            self.previous_killer.clone(),
            &failing_tests,
        );
    }
}

fn needs_survivor_confirmation(selected: &SelectedTestCommand, result: MutationResult) -> bool {
    selected.is_narrowed() && result == MutationResult::Survived
}

fn confirmation_from_result(result: MutationResult) -> SurvivorConfirmation {
    match result {
        MutationResult::Survived => SurvivorConfirmation::ConfirmedSurvived,
        MutationResult::Killed => SurvivorConfirmation::Killed,
        MutationResult::Timeout => SurvivorConfirmation::TimedOut,
        MutationResult::BuildError => SurvivorConfirmation::BuildError,
        MutationResult::Uncovered | MutationResult::Subsumed => SurvivorConfirmation::BuildError,
    }
}

fn confirmation_workspace_reset_error(
    workspace_root: &Path,
    error: std::io::Error,
) -> MutationOutcome {
    MutationOutcome::build_error_with(
        "confirmation_workspace_reset",
        vec![],
        format!(
            "could not reset isolated mutation workspace {} before full-suite confirmation: {error}",
            workspace_root.display()
        ),
    )
}

fn run_queued_mutation(
    queued: QueuedMutation,
    reservation: TestSlotReservation,
    shared: RunShared<'_>,
) -> Option<(usize, MutationRunRecord)> {
    let QueuedMutation {
        index,
        mutation,
        restore_checked,
        primary_restore,
    } = queued;

    if shared.cancelled.load(Ordering::Relaxed) {
        return None;
    }

    let prepared = PreparedMutationRun::new(
        shared.project_root,
        &mutation,
        PreparedMutationContext {
            commands: shared.commands,
            history: shared.history,
            source_contents: shared.source_contents,
            cache_context_fingerprint: shared.cache_context_fingerprint,
            test_context_index: shared.test_context_index,
            env: shared.env,
        },
    );
    let primary_direct_recipe = prepared.direct_recipe(
        shared.commands,
        shared.env,
        shared.respect_workspace_ignores,
        DirectRecipeOrigin::Executed,
    );
    let restored = primary_restore.or_else(|| {
        (!restore_checked)
            .then(|| {
                prepared.restore_result(shared.project_root, shared.history, shared.force_rerun)
            })
            .flatten()
    });

    if let Some(restored) = restored {
        if !needs_survivor_confirmation(&prepared.selected_test, restored.result) {
            reservation.release();
            record_restored_for_early_stop(shared.early_stop, restored.execution, restored.result);
            record_progress(&shared, &mutation, restored.result, None, true);
            let mut record = MutationRunRecord::new(mutation, restored.result, None)
                .with_execution(restored.execution)
                .with_selection(
                    prepared
                        .selected_test
                        .selection_provenance(SurvivorConfirmation::NotNeeded),
                );
            if matches!(
                restored.result,
                MutationResult::Killed | MutationResult::Survived | MutationResult::Timeout
            ) {
                if let Some(origin) = DirectRecipeOrigin::from_execution(restored.execution) {
                    let mut recipe = primary_direct_recipe.clone();
                    recipe.origin = origin;
                    record = record.with_replay_recipe(recipe);
                }
            }
            return Some((index, record));
        }
    }

    let primary_was_restored = restored.is_some();
    let workspace_slot = shared.workspace_pool.acquire();
    let workspace_root = workspace_slot.root().to_path_buf();
    let primary_outcome = if let Some(restored) = restored {
        MutationOutcome::new(restored.result, None)
    } else {
        if workspace_slot.needs_reset() {
            if let Err(e) =
                workspace_slot.reset(shared.project_root, shared.respect_workspace_ignores)
            {
                eprintln!(
                    "warning: could not reset isolated mutation workspace {}: {e}",
                    workspace_root.display()
                );
                reservation.release();
                record_progress(&shared, &mutation, MutationResult::BuildError, None, false);
                record_fresh_early_stop(&shared, MutationResult::BuildError);
                let diagnostic = BuildErrorDiagnostic::new(
                    mutation.id,
                    "regular",
                    "workspace_reset",
                    vec![],
                    format!(
                        "could not reset isolated mutation workspace {}: {e}",
                        workspace_root.display()
                    ),
                );
                return Some((
                    index,
                    MutationRunRecord::new(mutation, MutationResult::BuildError, Some(diagnostic))
                        .with_selection(
                            prepared
                                .selected_test
                                .selection_provenance(SurvivorConfirmation::NotNeeded),
                        ),
                ));
            }
        }
        let workspace_target =
            ResolvedMutation::new_for_execution(shared.project_root, &workspace_root, &mutation);
        run_single_mutation(
            &prepared.selected_test.argv,
            shared.commands.sandbox_command.as_slice(),
            BuildCommand {
                argv: shared.build_command,
                origin: shared.build_command_origin,
            },
            prepared.selected_test.timeout,
            &workspace_root,
            workspace_target,
            shared.show_output || shared.history.is_some(),
            shared.env,
            shared.cancelled,
        )
    };

    if primary_outcome.cancelled {
        return None;
    }
    if !primary_was_restored {
        prepared.store_cache(shared.project_root, primary_outcome.result);
        prepared.record_history(
            shared.history,
            primary_outcome.result,
            primary_outcome.test_output.as_deref(),
        );
    }

    let primary_result = primary_outcome.result;
    let mut outcome = primary_outcome;
    let mut execution = restored
        .map(|restored| restored.execution)
        .unwrap_or_else(|| MutationExecution::for_result(outcome.result));
    let mut direct_recipe = primary_direct_recipe;
    let confirmation = if needs_survivor_confirmation(&prepared.selected_test, primary_result) {
        match prepared.selected_test.unnarrowed_argv() {
            Some(full_argv) => {
                outcome = match workspace_slot
                    .reset(shared.project_root, shared.respect_workspace_ignores)
                {
                    Ok(()) => {
                        let workspace_target = ResolvedMutation::new_for_execution(
                            shared.project_root,
                            &workspace_root,
                            &mutation,
                        );
                        run_single_mutation(
                            full_argv,
                            shared.commands.sandbox_command.as_slice(),
                            BuildCommand {
                                argv: shared.build_command,
                                origin: shared.build_command_origin,
                            },
                            prepared.selected_test.timeout,
                            &workspace_root,
                            workspace_target,
                            shared.show_output,
                            shared.env,
                            shared.cancelled,
                        )
                    }
                    Err(error) => confirmation_workspace_reset_error(&workspace_root, error),
                };
                if outcome.cancelled {
                    return None;
                }
                execution = MutationExecution::for_result(outcome.result);
                direct_recipe = prepared.direct_recipe_for(
                    full_argv,
                    shared.commands,
                    shared.env,
                    shared.respect_workspace_ignores,
                    DirectRecipeOrigin::Executed,
                );
                confirmation_from_result(outcome.result)
            }
            None => {
                outcome = MutationOutcome::build_error_with(
                    "confirmation_full_route",
                    vec![],
                    "narrowed test command is missing its full route",
                );
                execution = MutationExecution::for_result(outcome.result);
                confirmation_from_result(outcome.result)
            }
        }
    } else {
        SurvivorConfirmation::NotNeeded
    };

    if primary_was_restored {
        if outcome.result == MutationResult::BuildError {
            reservation.release();
        } else {
            reservation.commit();
        }
    } else if primary_result == MutationResult::BuildError {
        reservation.release();
    } else {
        // A primary test already consumed this candidate's slot even if the
        // full confirmation later cannot run.
        reservation.commit();
    }

    let result = outcome.result;
    record_progress(
        &shared,
        &mutation,
        result,
        outcome.test_output.as_deref(),
        false,
    );
    record_fresh_early_stop(&shared, result);
    let diagnostic = build_error_diagnostic_from_outcome(&mutation, "regular", &outcome);
    let mut record = MutationRunRecord::new(mutation, result, diagnostic)
        .with_execution(execution)
        .with_selection(prepared.selected_test.selection_provenance(confirmation));
    if matches!(
        result,
        MutationResult::Killed | MutationResult::Survived | MutationResult::Timeout
    ) {
        record = record.with_replay_recipe(direct_recipe);
    }
    Some((index, record))
}

struct TestSlotReservation {
    counter: Option<Arc<AtomicUsize>>,
}

impl TestSlotReservation {
    fn try_reserve(max_tested: Option<usize>, counter: &Arc<AtomicUsize>) -> Option<Self> {
        let Some(max) = max_tested else {
            return Some(Self { counter: None });
        };

        counter
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < max).then_some(current + 1)
            })
            .ok()
            .map(|_| Self {
                counter: Some(counter.clone()),
            })
    }

    fn commit(mut self) {
        self.counter = None;
    }

    fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if let Some(counter) = self.counter.take() {
            counter.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

impl Drop for TestSlotReservation {
    fn drop(&mut self) {
        self.release_inner();
    }
}

#[allow(clippy::manual_is_multiple_of)]
fn record_progress(
    shared: &RunShared<'_>,
    mutation: &Mutation,
    result: MutationResult,
    test_output: Option<&str>,
    cached: bool,
) {
    let n = shared.counter.fetch_add(1, Ordering::Relaxed) + 1;
    if shared.verbose {
        if cached {
            eprintln!(
                "  [{}/{}] \u{21bb} cached  {}:{} \u{2014} {}",
                n,
                shared.total,
                mutation.file.display(),
                mutation.line,
                mutation.operator
            );
        } else {
            let symbol = match result {
                MutationResult::Killed => "\u{2713} killed",
                MutationResult::Survived => "\u{2717} survived",
                MutationResult::Timeout => "⧖ timeout",
                MutationResult::BuildError => "⚠ build error",
                MutationResult::Uncovered => "◌ uncovered",
                MutationResult::Subsumed => "◌ subsumed",
            };
            eprintln!(
                "  [{}/{}] {}  {}:{} \u{2014} {}",
                n,
                shared.total,
                symbol,
                mutation.file.display(),
                mutation.line,
                mutation.operator
            );
        }
    } else if shared.is_tty {
        eprint!("\r  [{}/{}] testing mutations...", n, shared.total);
        let _ = std::io::stderr().flush();
    } else if n == shared.total || (shared.total >= 4 && n % (shared.total / 4) == 0) {
        eprintln!("  [{}/{}] testing mutations...", n, shared.total);
    }

    if shared.show_output && result == MutationResult::Survived {
        if let Some(output) = test_output {
            eprintln!(
                "  ┌─ test output for {}:{} ({})",
                mutation.file.display(),
                mutation.line,
                mutation.operator
            );
            for line in output.lines() {
                eprintln!("  │ {}", line);
            }
            eprintln!("  └─");
        }
    }
}

/// Merge history-subsumed mutants into the report as `Subsumed`.
///
/// Mirrors `merge_uncovered` in main.rs: they count toward
/// `total`/`planned_total` (they were part of the scheduled work) but stay
/// out of the tested denominator; see `MutationReport::tested_count`.
fn merge_subsumed(report: &mut MutationReport, subsumed: Vec<Mutation>) {
    let n = subsumed.len();
    if n == 0 {
        return;
    }
    report
        .results
        .extend(subsumed.into_iter().map(|m| (m, MutationResult::Subsumed)));
    report.total += n;
    report.planned_total += n;
}

impl TestRunner {
    /// Resolve reusable verdicts before early-stop decisions so the gate sees
    /// the complete fresh-work denominator rather than only cache hits
    /// encountered so far by workers.
    fn preclassify_for_early_stop(
        &self,
        mutations: &[Mutation],
        schema_candidate_ids: &HashSet<u32>,
    ) -> PreclassifiedMutations {
        if !self.early_stop.is_enabled() {
            return PreclassifiedMutations {
                fresh: mutations
                    .iter()
                    .cloned()
                    .enumerate()
                    .map(|(index, mutation)| QueuedMutation {
                        index,
                        mutation,
                        restore_checked: false,
                        primary_restore: None,
                    })
                    .collect(),
                restored: Vec::new(),
            };
        }

        let source_contents = SourceContentCache::default();
        let cache_context_hash = cache_context_fingerprint(&self.project_root);
        let test_context_index = TestContextIndex::build(&self.project_root);
        let history = self
            .incremental_history
            .then(|| cache::IncrementalHistoryStore::load(&self.project_root));
        let mut fresh = Vec::with_capacity(mutations.len());
        let mut restored = Vec::new();
        let mut cancelled = false;

        for (index, mutation) in mutations.iter().enumerate() {
            if cancelled || self.cancelled.load(Ordering::Acquire) {
                cancelled = true;
                fresh.push(QueuedMutation {
                    index,
                    mutation: mutation.clone(),
                    restore_checked: false,
                    primary_restore: None,
                });
                continue;
            }

            let prepared = PreparedMutationRun::new(
                &self.project_root,
                mutation,
                PreparedMutationContext {
                    commands: &self.commands,
                    history: history.as_ref(),
                    source_contents: &source_contents,
                    cache_context_fingerprint: cache_context_hash,
                    test_context_index: &test_context_index,
                    env: &self.env,
                },
            );
            // Capture before cache/history lookup even when early-stop
            // preclassification restores the verdict.
            let direct_recipe = prepared.direct_recipe(
                &self.commands,
                &self.env,
                self.respect_workspace_ignores,
                DirectRecipeOrigin::Executed,
            );
            if let Some(restored_result) =
                prepared.restore_result(&self.project_root, history.as_ref(), self.force_rerun)
            {
                if needs_survivor_confirmation(&prepared.selected_test, restored_result.result)
                    || (restored_result.result == MutationResult::Survived
                        && schema_candidate_ids.contains(&mutation.id))
                {
                    fresh.push(QueuedMutation {
                        index,
                        mutation: mutation.clone(),
                        restore_checked: true,
                        primary_restore: Some(restored_result),
                    });
                    continue;
                }
                let mut record =
                    MutationRunRecord::new(mutation.clone(), restored_result.result, None)
                        .with_execution(restored_result.execution)
                        .with_selection(
                            prepared
                                .selected_test
                                .selection_provenance(SurvivorConfirmation::NotNeeded),
                        );
                if matches!(
                    restored_result.result,
                    MutationResult::Killed | MutationResult::Survived | MutationResult::Timeout
                ) {
                    if let Some(origin) =
                        DirectRecipeOrigin::from_execution(restored_result.execution)
                    {
                        let mut recipe = direct_recipe;
                        recipe.origin = origin;
                        record = record.with_replay_recipe(recipe);
                    }
                }
                restored.push((index, record));
            } else {
                fresh.push(QueuedMutation {
                    index,
                    mutation: mutation.clone(),
                    restore_checked: true,
                    primary_restore: None,
                });
            }
        }

        PreclassifiedMutations { fresh, restored }
    }

    #[allow(clippy::manual_is_multiple_of)]
    pub fn run(&self, mutations: Vec<Mutation>) -> RunOutcome {
        let (mutations, subsumed) = self.split_subsumed(mutations);
        let planned_total = mutations.len();
        let preclassified = self.preclassify_for_early_stop(&mutations, &HashSet::new());
        let early_stop =
            EarlyStopState::for_config(self.early_stop.clone(), preclassified.fresh.len());
        if let Some(early_stop) = &early_stop {
            early_stop.record_preclassified_exact_cache(&preclassified.restored);
        }
        let mut outcome = self.run_regular_with_state(
            preclassified.fresh,
            preclassified.restored,
            early_stop,
            planned_total,
            Arc::new(AtomicUsize::new(0)),
        );
        merge_subsumed(&mut outcome.report, subsumed);
        outcome
    }

    pub fn run_with_schemata(&self, mutations: Vec<Mutation>) -> RunOutcome {
        // Learned selection runs before schemata planning so subsumed
        // mutants never end up in schema rewrites.
        let (mutations, subsumed) = self.split_subsumed(mutations);
        let mut outcome = self.run_with_schemata_planned(mutations);
        merge_subsumed(&mut outcome.report, subsumed);
        outcome
    }

    /// Partition mutations into those to execute and those subsumed by a
    /// shared recorded killer test (opt-in learned selection). Conservative:
    /// without the flag or without incremental history every mutation
    /// executes, exactly as before. `--force-rerun` still bypasses verdict
    /// restore for the canonical mutants but does not disable clustering.
    fn split_subsumed(&self, mutations: Vec<Mutation>) -> (Vec<Mutation>, Vec<Mutation>) {
        if !self.learned_selection || !self.incremental_history {
            return (mutations, Vec::new());
        }
        let history = cache::IncrementalHistoryStore::load(&self.project_root);
        let source_contents = SourceContentCache::default();
        let partition = crate::learned::partition_subsumed(mutations, |mutation| {
            let source = source_contents.content_for(&self.project_root, &mutation.file)?;
            let selected = select_test_command_with_history(
                &self.project_root,
                &self.commands,
                mutation,
                Some(&history),
            );
            let command_ctx = selected.cache_context(
                &self.commands.build_command,
                self.commands.build_command_origin,
                &self.commands.sandbox_command,
                &self.env,
            );
            history.learned_killer_test(
                &cache_identity(&self.project_root, mutation),
                &mutation.description,
                cache::hash_bytes(&source),
                cache::hash_str(&command_ctx),
            )
        });
        if !partition.subsumed.is_empty() {
            eprintln!(
                "learned selection: {} mutants subsumed into {} canonical mutants",
                partition.subsumed.len(),
                partition.clusters
            );
        }
        (partition.to_run, partition.subsumed)
    }

    fn run_with_schemata_planned(&self, mutations: Vec<Mutation>) -> RunOutcome {
        let start = Instant::now();
        if mutations.is_empty() {
            return self.outcome_from_records(Vec::new(), start.elapsed());
        }
        let planned_total = mutations.len();
        let index_by_id: HashMap<u32, usize> = mutations
            .iter()
            .enumerate()
            .map(|(index, mutation)| (mutation.id, index))
            .collect();
        let plan = crate::schemata::plan(&self.project_root, mutations.clone());
        let schema_candidate_ids: HashSet<u32> = plan
            .selected
            .iter()
            .filter(|schema_mutation| {
                matches!(
                    schema_mutation.mutation.language.as_str(),
                    "c" | "cpp" | "go" | "java" | "rust"
                )
            })
            .map(|schema_mutation| schema_mutation.mutation.id)
            .collect();
        let preclassified = self.preclassify_for_early_stop(&mutations, &schema_candidate_ids);
        let pending_restores: HashMap<u32, RestoredMutationResult> = preclassified
            .fresh
            .iter()
            .filter_map(|queued| {
                queued
                    .primary_restore
                    .map(|restored| (queued.mutation.id, restored))
            })
            .collect();
        let restored_ids: HashSet<u32> = preclassified
            .restored
            .iter()
            .map(|(_, record)| record.mutation.id)
            .collect();
        let restore_checked = self.early_stop.is_enabled();
        let early_stop =
            EarlyStopState::for_config(self.early_stop.clone(), preclassified.fresh.len());
        if let Some(early_stop) = &early_stop {
            early_stop.record_preclassified_exact_cache(&preclassified.restored);
        }
        let tested_counter = Arc::new(AtomicUsize::new(0));

        let mut schema_by_language = HashMap::<String, Vec<crate::schemata::SchemaMutation>>::new();
        let mut fallback_mutations = Vec::new();
        let mut schemata_summary = SchemataRunSummary::default();
        let mut all_records = preclassified
            .restored
            .into_iter()
            .map(|(_, record)| record)
            .collect::<Vec<_>>();
        // Cache/history restoration before schema planning is not enough
        // evidence to publish a direct replay recipe for a schema candidate.
        // Known regular fallbacks keep their captured direct recipe.
        for record in &mut all_records {
            if schema_candidate_ids.contains(&record.mutation.id) {
                record.replay_recipe = None;
            }
        }

        for schema_mutation in plan.selected {
            match schema_mutation.mutation.language.as_str() {
                "c" | "cpp" | "go" | "java" | "rust" => {
                    if !restored_ids.contains(&schema_mutation.mutation.id) {
                        schema_by_language
                            .entry(schema_mutation.mutation.language.clone())
                            .or_default()
                            .push(schema_mutation);
                    }
                }
                _ => {
                    schemata_summary.record_fallback("unsupported_runner");
                    if !restored_ids.contains(&schema_mutation.mutation.id) {
                        fallback_mutations.push(schema_mutation.mutation);
                    }
                }
            }
        }
        for fallback in plan.fallback {
            schemata_summary.record_fallback(fallback.reason.as_str());
            if !restored_ids.contains(&fallback.mutation.id) {
                fallback_mutations.push(fallback.mutation);
            }
        }
        for (language, schema_mutations) in schema_by_language {
            if should_stop_early(&early_stop) {
                break;
            }
            let mutation_count = schema_mutations.len();
            match self.run_schema_mutations(
                &language,
                &schema_mutations,
                early_stop.clone(),
                tested_counter.clone(),
                restore_checked,
                &pending_restores,
            ) {
                Ok((records, demoted)) => {
                    schemata_summary.fast_path += records
                        .iter()
                        .filter(|record| record.execution.is_tested())
                        .count();
                    all_records.extend(records);
                    if !demoted.is_empty() {
                        schemata_summary.record_fallbacks("schema_build_failure", demoted.len());
                        fallback_mutations.extend(demoted);
                    }
                }
                Err(err) => {
                    eprintln!("warning: could not run {language} schemata: {err} — falling back");
                    schemata_summary.record_fallbacks("rewrite_error", mutation_count);
                    fallback_mutations.extend(
                        schema_mutations
                            .into_iter()
                            .map(|schema_mutation| schema_mutation.mutation),
                    );
                }
            }
        }

        if !self.cancelled.load(Ordering::Acquire)
            && !fallback_mutations.is_empty()
            && !should_stop_early(&early_stop)
        {
            let fallback = self.run_regular_with_state(
                fallback_mutations
                    .into_iter()
                    .enumerate()
                    .map(|(index, mutation)| QueuedMutation {
                        index,
                        // A demoted schema candidate still needs direct
                        // execution; its cached verdict is not confirmation.
                        primary_restore: pending_restores
                            .get(&mutation.id)
                            .copied()
                            .filter(|_| !schema_candidate_ids.contains(&mutation.id)),
                        restore_checked: restore_checked
                            || schema_candidate_ids.contains(&mutation.id),
                        mutation,
                    })
                    .collect(),
                Vec::new(),
                early_stop.clone(),
                planned_total,
                tested_counter,
            );
            all_records.extend(records_from_report(
                fallback.report,
                fallback.replay_recipes,
            ));
        }

        all_records.sort_by_key(|record| {
            index_by_id
                .get(&record.mutation.id)
                .copied()
                .unwrap_or(usize::MAX)
        });
        let mut outcome = self.outcome_from_records_with_status(
            all_records,
            start.elapsed(),
            planned_total,
            early_stop.as_ref().and_then(|state| state.reason()),
        );
        outcome.report.schemata = Some(schemata_summary.into_report());
        outcome
    }

    #[allow(clippy::manual_is_multiple_of)]
    fn run_regular_with_state(
        &self,
        mutations: Vec<QueuedMutation>,
        mut restored: Vec<(usize, MutationRunRecord)>,
        early_stop: Option<Arc<EarlyStopState>>,
        planned_total: usize,
        tested_counter: Arc<AtomicUsize>,
    ) -> RunOutcome {
        let start = Instant::now();
        let fresh_total = mutations.len();
        let total = fresh_total + restored.len();
        if fresh_total == 0
            || self.cancelled.load(Ordering::Acquire)
            || should_stop_early(&early_stop)
        {
            restored.sort_by_key(|(index, _)| *index);
            return self.outcome_from_records_with_status(
                restored.into_iter().map(|(_, record)| record).collect(),
                start.elapsed(),
                planned_total,
                early_stop.as_ref().and_then(|state| state.reason()),
            );
        }

        let counter = Arc::new(AtomicUsize::new(restored.len()));
        let verbose = self.verbose;
        let is_tty = std::io::stderr().is_terminal();
        let workspace_slots = workspace_pool_slot_count(self.parallelism, fresh_total);

        let workspace_pool_result = if self.respect_workspace_ignores {
            WorkspacePool::new(&self.project_root, workspace_slots)
        } else {
            WorkspacePool::new_with_options(&self.project_root, workspace_slots, false)
        };
        let workspace_pool = match workspace_pool_result {
            Ok(pool) => Arc::new(pool),
            Err(e) => {
                eprintln!("warning: could not create isolated mutation workspaces: {e}");
                restored.extend(mutations.into_iter().map(|queued| {
                    let diagnostic = BuildErrorDiagnostic::new(
                        queued.mutation.id,
                        "regular",
                        "workspace_pool",
                        vec![],
                        format!("could not create isolated mutation workspaces: {e}"),
                    );
                    (
                        queued.index,
                        MutationRunRecord::new(
                            queued.mutation,
                            MutationResult::BuildError,
                            Some(diagnostic),
                        ),
                    )
                }));
                restored.sort_by_key(|(index, _)| *index);
                return self.outcome_from_records_with_status(
                    restored.into_iter().map(|(_, record)| record).collect(),
                    start.elapsed(),
                    planned_total,
                    early_stop.as_ref().and_then(|state| state.reason()),
                );
            }
        };

        let source_contents = SourceContentCache::default();
        let project_root = Arc::new(self.project_root.clone());
        let build_command = Arc::new(self.commands.build_command.clone());
        let build_command_origin = self.commands.build_command_origin;
        let queue = Arc::new(Mutex::new(mutations.into_iter().collect::<VecDeque<_>>()));
        let results = Arc::new(Mutex::new(restored));
        let worker_count = workspace_pool.len().min(fresh_total).max(1);
        let cache_context_hash = cache_context_fingerprint(&self.project_root);
        let test_context_index = TestContextIndex::build(&self.project_root);
        let history = self
            .incremental_history
            .then(|| cache::IncrementalHistoryStore::load(&self.project_root));

        thread::scope(|scope| {
            for _ in 0..worker_count {
                let queue = queue.clone();
                let results = results.clone();
                let workspace_pool = workspace_pool.clone();
                let project_root = project_root.clone();
                let build_command = build_command.clone();
                let counter = counter.clone();
                let tested_counter = tested_counter.clone();
                let cancelled = self.cancelled.clone();
                let commands = &self.commands;
                let env = &self.env;
                let max_tested = self.max_tested;
                let show_output = self.show_output;
                let source_contents = &source_contents;
                let test_context_index = &test_context_index;
                let history = history.as_ref();
                let force_rerun = self.force_rerun;
                let early_stop = early_stop.clone();

                scope.spawn(move || {
                    loop {
                        if cancelled.load(Ordering::Relaxed) || should_stop_early(&early_stop) {
                            break;
                        }

                        let Some(reservation) =
                            TestSlotReservation::try_reserve(max_tested, &tested_counter)
                        else {
                            break;
                        };
                        if should_stop_early(&early_stop) {
                            reservation.release();
                            break;
                        }
                        let next = match queue.lock() {
                            Ok(mut queue) => queue.pop_front(),
                            Err(_) => {
                                eprintln!("warning: mutation queue mutex poisoned");
                                break;
                            }
                        };
                        let Some(queued) = next else {
                            break;
                        };

                        let outcome = catch_unwind(PanicBoundary(|| {
                            run_queued_mutation(
                                queued,
                                reservation,
                                RunShared {
                                    workspace_pool: workspace_pool.as_ref(),
                                    project_root: project_root.as_ref().as_path(),
                                    commands,
                                    build_command: build_command.as_ref().as_slice(),
                                    build_command_origin,
                                    env,
                                    total,
                                    verbose,
                                    is_tty,
                                    show_output,
                                    counter: &counter,
                                    cancelled: &cancelled,
                                    early_stop: early_stop.as_ref(),
                                    respect_workspace_ignores: self.respect_workspace_ignores,
                                    cache_context_fingerprint: cache_context_hash,
                                    test_context_index,
                                    source_contents,
                                    history,
                                    force_rerun,
                                },
                            )
                        }));

                        match outcome {
                            Ok(Some(result)) => match results.lock() {
                                Ok(mut results) => results.push(result),
                                Err(_) => {
                                    eprintln!("warning: mutation results mutex poisoned");
                                    break;
                                }
                            },
                            Ok(None) => {}
                            Err(_) => eprintln!("warning: mutation task panicked"),
                        }
                    }
                });
            }
        });

        let mut indexed_results = Arc::try_unwrap(results)
            .expect("all result handles should be dropped")
            .into_inner()
            .expect("mutation results mutex poisoned");
        indexed_results.sort_by_key(|(index, _)| *index);
        let all_records = indexed_results
            .into_iter()
            .map(|(_, record)| record)
            .collect();

        // Clear progress line on TTY
        if !verbose && is_tty {
            eprint!("\r                                        \r");
            let _ = std::io::stderr().flush();
        }

        self.outcome_from_records_with_status(
            all_records,
            start.elapsed(),
            planned_total,
            early_stop.as_ref().and_then(|state| state.reason()),
        )
    }

    fn run_schema_mutations(
        &self,
        language: &str,
        schema_mutations: &[crate::schemata::SchemaMutation],
        early_stop: Option<Arc<EarlyStopState>>,
        tested_counter: Arc<AtomicUsize>,
        restore_checked: bool,
        pending_restores: &HashMap<u32, RestoredMutationResult>,
    ) -> Result<(Vec<MutationRunRecord>, Vec<Mutation>), crate::schemata::SchemaRewriteError> {
        self.run_schema_mutations_inner(
            language,
            schema_mutations,
            early_stop,
            tested_counter,
            restore_checked,
            pending_restores,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_schema_mutations_inner(
        &self,
        language: &str,
        schema_mutations: &[crate::schemata::SchemaMutation],
        early_stop: Option<Arc<EarlyStopState>>,
        tested_counter: Arc<AtomicUsize>,
        restore_checked: bool,
        pending_restores: &HashMap<u32, RestoredMutationResult>,
        allow_build_bisection: bool,
    ) -> Result<(Vec<MutationRunRecord>, Vec<Mutation>), crate::schemata::SchemaRewriteError> {
        let rewrites =
            schema_rewrites_for_language(&self.project_root, language, schema_mutations)?;
        let workspace =
            copy_workspace_with_options(&self.project_root, self.respect_workspace_ignores)
                .map_err(|e| {
                    crate::schemata::SchemaRewriteError::new(format!(
                        "could not create schema workspace: {e}"
                    ))
                })?;

        let mut results = Vec::with_capacity(schema_mutations.len());
        let source_contents = SourceContentCache::default();
        let cache_context_hash = cache_context_fingerprint(&self.project_root);
        let test_context_index = TestContextIndex::build(&self.project_root);
        let history = self
            .incremental_history
            .then(|| cache::IncrementalHistoryStore::load(&self.project_root));
        let mut workspace_needs_reset = false;
        let mut demoted = Vec::new();
        for (position, schema_mutation) in schema_mutations.iter().enumerate() {
            if self.cancelled.load(Ordering::Acquire) || should_stop_early(&early_stop) {
                break;
            }
            let Some(reservation) =
                TestSlotReservation::try_reserve(self.max_tested, &tested_counter)
            else {
                break;
            };
            if should_stop_early(&early_stop) {
                reservation.release();
                break;
            }
            let mutation = &schema_mutation.mutation;
            let mut env = self.env.clone();
            env.insert("TOGI_MUTANT".to_string(), mutation.id.to_string());
            // Cache keys must not embed the per-mutant TOGI_MUTANT id: ids are
            // reassigned whenever the mutation set changes, which would orphan
            // every cached verdict from earlier runs.
            let prepared = PreparedMutationRun::new(
                &self.project_root,
                mutation,
                PreparedMutationContext {
                    commands: &self.commands,
                    history: history.as_ref(),
                    source_contents: &source_contents,
                    cache_context_fingerprint: cache_context_hash,
                    test_context_index: &test_context_index,
                    env: &self.env,
                },
            );
            let argv = if language == "go" {
                force_go_no_test_cache(prepared.selected_test.argv.clone())
            } else {
                prepared.selected_test.argv.clone()
            };
            let primary_restore = pending_restores.get(&mutation.id).copied().or_else(|| {
                if restore_checked {
                    None
                } else {
                    prepared.restore_result(&self.project_root, history.as_ref(), self.force_rerun)
                }
            });
            if let Some(restored) = primary_restore {
                if restored.result != MutationResult::Survived {
                    reservation.release();
                    record_restored_for_early_stop(
                        early_stop.as_ref(),
                        restored.execution,
                        restored.result,
                    );
                    if self.verbose {
                        eprintln!(
                            "  [schema] ↻ cached  {}:{} — {}",
                            mutation.file.display(),
                            mutation.line,
                            mutation.operator
                        );
                    }
                    results.push(
                        MutationRunRecord::new(mutation.clone(), restored.result, None)
                            .with_execution(restored.execution)
                            .with_selection(
                                prepared
                                    .selected_test
                                    .selection_provenance(SurvivorConfirmation::NotNeeded),
                            ),
                    );
                    continue;
                }
            }

            let primary_was_restored = primary_restore.is_some();
            let mut cacheable = true;
            let outcome = if let Some(restored) = primary_restore {
                MutationOutcome::new(restored.result, None)
            } else if workspace_needs_reset {
                if let Err(e) = workspace.reset(&self.project_root, self.respect_workspace_ignores)
                {
                    cacheable = false;
                    MutationOutcome::build_error_with(
                        "schema_workspace_reset",
                        vec![],
                        format!(
                            "could not reset schema workspace {}: {e}",
                            workspace.root().display()
                        ),
                    )
                } else {
                    workspace_needs_reset = true;
                    run_schema_workspace_mutation(
                        self,
                        workspace.root(),
                        &rewrites,
                        &argv,
                        prepared.selected_test.timeout,
                        &env,
                        &mut cacheable,
                    )
                }
            } else {
                workspace_needs_reset = true;
                run_schema_workspace_mutation(
                    self,
                    workspace.root(),
                    &rewrites,
                    &argv,
                    prepared.selected_test.timeout,
                    &env,
                    &mut cacheable,
                )
            };
            if outcome.cancelled {
                break;
            }
            if outcome.result == MutationResult::BuildError
                && outcome
                    .build_error_detail
                    .as_ref()
                    .is_some_and(|detail| detail.phase == "schema_build")
            {
                reservation.release();
                let remaining = &schema_mutations[position..];
                let error_line = outcome
                    .build_error_detail
                    .as_ref()
                    .and_then(|detail| {
                        detail
                            .message
                            .lines()
                            .find(|line| line.starts_with("error"))
                            .map(str::to_owned)
                    })
                    .unwrap_or_else(|| "no compiler diagnostic captured".into());

                if allow_build_bisection {
                    match self.bisect_schema_build_batch(language, remaining) {
                        Ok((salvaged_batches, newly_demoted)) => {
                            let salvaged_count: usize =
                                salvaged_batches.iter().map(|batch| batch.len()).sum();
                            let demoted_count = newly_demoted.len();
                            eprintln!(
                                "warning: schema build failed ({error_line}) — salvaging {salvaged_count} {language} mutants in smaller schema batches and demoting {demoted_count} to regular runs"
                            );

                            for batch in salvaged_batches {
                                if self.cancelled.load(Ordering::Acquire)
                                    || should_stop_early(&early_stop)
                                {
                                    break;
                                }
                                match self.run_schema_mutations_inner(
                                    language,
                                    &batch,
                                    early_stop.clone(),
                                    tested_counter.clone(),
                                    restore_checked,
                                    pending_restores,
                                    false,
                                ) {
                                    Ok((batch_records, batch_demoted)) => {
                                        results.extend(batch_records);
                                        demoted.extend(batch_demoted);
                                    }
                                    Err(err) => {
                                        eprintln!(
                                            "warning: could not run salvaged {language} schema batch: {err} — falling back"
                                        );
                                        demoted.extend(
                                            batch
                                                .into_iter()
                                                .map(|schema_mutation| schema_mutation.mutation),
                                        );
                                    }
                                }
                            }
                            demoted.extend(newly_demoted);
                        }
                        Err(err) => {
                            eprintln!(
                                "warning: schema build failed ({error_line}) — could not bisect {language} schema batch: {err}; demoting {} mutants to regular runs",
                                remaining.len()
                            );
                            demoted.extend(
                                remaining
                                    .iter()
                                    .map(|schema_mutation| schema_mutation.mutation.clone()),
                            );
                        }
                    }
                } else {
                    eprintln!(
                        "warning: schema build failed ({error_line}) — demoting {} {language} mutants to regular runs",
                        remaining.len()
                    );
                    demoted.extend(
                        remaining
                            .iter()
                            .map(|schema_mutation| schema_mutation.mutation.clone()),
                    );
                }
                break;
            }
            if !primary_was_restored
                && (outcome.result != MutationResult::Survived
                    || prepared.selected_test.is_narrowed())
            {
                if cacheable {
                    prepared.store_cache(&self.project_root, outcome.result);
                }
                prepared.record_history(
                    history.as_ref(),
                    outcome.result,
                    outcome.test_output.as_deref(),
                );
            }

            let primary_result = outcome.result;
            let mut final_outcome = outcome;
            let mut execution = primary_restore
                .map(|restored| restored.execution)
                .unwrap_or(MutationExecution::Executed);
            let mut direct_recipe = None;
            let confirmation = if primary_result == MutationResult::Survived {
                // Confirm the concrete edit, not the instrumented schema, on
                // the full route. Only this fresh execution earns a recipe.
                let full_argv = prepared
                    .selected_test
                    .unnarrowed_argv()
                    .unwrap_or(&prepared.selected_test.argv);
                let full_argv = if language == "go" {
                    force_go_no_test_cache(full_argv.to_vec())
                } else {
                    full_argv.to_vec()
                };
                final_outcome =
                    match workspace.reset(&self.project_root, self.respect_workspace_ignores) {
                        Ok(()) => {
                            workspace_needs_reset = true;
                            let target = ResolvedMutation::new_for_execution(
                                &self.project_root,
                                workspace.root(),
                                mutation,
                            );
                            run_single_mutation(
                                &full_argv,
                                &self.commands.sandbox_command,
                                BuildCommand {
                                    argv: &self.commands.build_command,
                                    origin: self.commands.build_command_origin,
                                },
                                prepared.selected_test.timeout,
                                workspace.root(),
                                target,
                                self.show_output || history.is_some(),
                                &self.env,
                                &self.cancelled,
                            )
                        }
                        Err(error) => confirmation_workspace_reset_error(workspace.root(), error),
                    };
                if final_outcome.cancelled {
                    break;
                }
                execution = MutationExecution::for_result(final_outcome.result);
                if matches!(
                    final_outcome.result,
                    MutationResult::Killed | MutationResult::Survived | MutationResult::Timeout
                ) {
                    direct_recipe = Some(prepared.direct_recipe_for(
                        &full_argv,
                        &self.commands,
                        &self.env,
                        self.respect_workspace_ignores,
                        DirectRecipeOrigin::Executed,
                    ));
                }
                if !prepared.selected_test.is_narrowed() {
                    // Replace stale schema evidence even when confirmation
                    // cannot build; it must not reappear as a cached survivor.
                    prepared.store_cache(&self.project_root, final_outcome.result);
                    prepared.record_history(
                        history.as_ref(),
                        final_outcome.result,
                        final_outcome.test_output.as_deref(),
                    );
                }
                confirmation_from_result(final_outcome.result)
            } else {
                SurvivorConfirmation::NotNeeded
            };

            if primary_was_restored {
                if final_outcome.result == MutationResult::BuildError {
                    reservation.release();
                } else {
                    reservation.commit();
                }
            } else if primary_result == MutationResult::BuildError {
                reservation.release();
            } else {
                reservation.commit();
            }
            if let Some(early_stop) = &early_stop {
                early_stop.record_fresh(final_outcome.result);
            }
            if self.verbose {
                let symbol = match final_outcome.result {
                    MutationResult::Killed => "✓ killed",
                    MutationResult::Survived => "✗ survived",
                    MutationResult::Timeout => "⧖ timeout",
                    MutationResult::BuildError => "⚠ build error",
                    MutationResult::Uncovered => "◌ uncovered",
                    MutationResult::Subsumed => "◌ subsumed",
                };
                eprintln!(
                    "  [schema] {}  {}:{} — {}",
                    symbol,
                    mutation.file.display(),
                    mutation.line,
                    mutation.operator
                );
            }
            if self.show_output && final_outcome.result == MutationResult::Survived {
                if let Some(output) = final_outcome.test_output.as_deref() {
                    eprintln!(
                        "  ┌─ test output for {}:{} ({})",
                        mutation.file.display(),
                        mutation.line,
                        mutation.operator
                    );
                    for line in output.lines() {
                        eprintln!("  │ {}", line);
                    }
                    eprintln!("  └─");
                }
            }
            let diagnostic =
                build_error_diagnostic_from_outcome(mutation, "schemata", &final_outcome);
            let mut record =
                MutationRunRecord::new(mutation.clone(), final_outcome.result, diagnostic)
                    .with_execution(execution)
                    .with_selection(prepared.selected_test.selection_provenance(confirmation));
            record.replay_recipe = direct_recipe;
            results.push(record);
        }

        Ok((results, demoted))
    }

    fn bisect_schema_build_batch(
        &self,
        language: &str,
        schema_mutations: &[crate::schemata::SchemaMutation],
    ) -> Result<
        (Vec<Vec<crate::schemata::SchemaMutation>>, Vec<Mutation>),
        crate::schemata::SchemaRewriteError,
    > {
        let workspace =
            copy_workspace_with_options(&self.project_root, self.respect_workspace_ignores)
                .map_err(|e| {
                    crate::schemata::SchemaRewriteError::new(format!(
                        "could not create schema bisection workspace: {e}"
                    ))
                })?;
        let mut salvaged_batches = Vec::new();
        let mut demoted = Vec::new();
        self.partition_schema_build_batch(
            language,
            schema_mutations,
            &workspace,
            &mut salvaged_batches,
            &mut demoted,
        )?;
        Ok((salvaged_batches, demoted))
    }

    fn partition_schema_build_batch(
        &self,
        language: &str,
        schema_mutations: &[crate::schemata::SchemaMutation],
        workspace: &WorkspaceCopy,
        salvaged_batches: &mut Vec<Vec<crate::schemata::SchemaMutation>>,
        demoted: &mut Vec<Mutation>,
    ) -> Result<(), crate::schemata::SchemaRewriteError> {
        if self.cancelled.load(Ordering::Acquire) {
            return Ok(());
        }
        if schema_mutations.len() == 1 {
            demoted.push(schema_mutations[0].mutation.clone());
            return Ok(());
        }

        let (left, right) = schema_mutations.split_at(schema_mutations.len() / 2);
        for batch in [left, right] {
            if self.cancelled.load(Ordering::Acquire) {
                return Ok(());
            }
            match self.schema_batch_builds(language, batch, workspace)? {
                Some(true) => salvaged_batches.push(batch.to_vec()),
                Some(false) => self.partition_schema_build_batch(
                    language,
                    batch,
                    workspace,
                    salvaged_batches,
                    demoted,
                )?,
                None => return Ok(()),
            }
        }
        Ok(())
    }

    fn schema_batch_builds(
        &self,
        language: &str,
        schema_mutations: &[crate::schemata::SchemaMutation],
        workspace: &WorkspaceCopy,
    ) -> Result<Option<bool>, crate::schemata::SchemaRewriteError> {
        workspace
            .reset(&self.project_root, self.respect_workspace_ignores)
            .map_err(|e| {
                crate::schemata::SchemaRewriteError::new(format!(
                    "could not reset schema bisection workspace {}: {e}",
                    workspace.root().display()
                ))
            })?;
        let rewrites =
            schema_rewrites_for_language(&self.project_root, language, schema_mutations)?;
        apply_schema_rewrites_to_workspace(&self.project_root, workspace.root(), &rewrites)?;

        let timeout = schema_mutations
            .iter()
            .map(|schema_mutation| {
                select_unnarrowed_test_command(
                    &self.project_root,
                    &self.commands,
                    &schema_mutation.mutation,
                )
                .timeout
            })
            .max()
            .unwrap_or(self.commands.timeout);
        let build = run_command(
            &self.commands.build_command,
            &self.commands.sandbox_command,
            workspace.root(),
            timeout,
            true,
            &self.env,
            &self.cancelled,
        );
        if build.cancelled {
            return Ok(None);
        }
        Ok(Some(build.result == MutationResult::Survived))
    }

    fn outcome_from_records(
        &self,
        all_records: Vec<MutationRunRecord>,
        duration: Duration,
    ) -> RunOutcome {
        let planned_total = all_records.len();
        self.outcome_from_records_with_status(all_records, duration, planned_total, None)
    }

    fn outcome_from_records_with_status(
        &self,
        all_records: Vec<MutationRunRecord>,
        duration: Duration,
        planned_total: usize,
        early_stop_reason: Option<String>,
    ) -> RunOutcome {
        let (report, replay_recipes) =
            self.report_from_records(all_records, duration, planned_total, early_stop_reason);
        RunOutcome {
            report,
            replay_recipes,
            cancelled: self.cancelled.load(Ordering::Acquire),
        }
    }

    fn report_from_records(
        &self,
        all_records: Vec<MutationRunRecord>,
        duration: Duration,
        planned_total: usize,
        early_stop_reason: Option<String>,
    ) -> (MutationReport, BTreeMap<u32, RegularDirectRecipe>) {
        let mut results = Vec::with_capacity(all_records.len());
        let mut build_error_diagnostics = Vec::new();
        let mut execution_provenance = BTreeMap::new();
        let mut selection_provenance = BTreeMap::new();
        let mut replay_recipes = BTreeMap::new();
        let mut total = 0;
        let mut killed = 0;
        let mut survived = 0;
        let mut timeout_count = 0;
        let mut build_errors = 0;

        for record in all_records {
            let MutationRunRecord {
                mutation,
                result,
                execution,
                build_error_diagnostic,
                selection,
                replay_recipe,
            } = record;
            total += 1;
            match result {
                MutationResult::Killed => killed += 1,
                MutationResult::Survived => survived += 1,
                MutationResult::Timeout => timeout_count += 1,
                MutationResult::BuildError => build_errors += 1,
                // Uncovered mutants never reach the runner; they are merged
                // into the report by the caller and derived from `results`.
                // Subsumed mutants are merged the same way by `merge_subsumed`
                // after the run; neither counts toward any tally here.
                MutationResult::Uncovered | MutationResult::Subsumed => {}
            }
            if execution != MutationExecution::for_result(result) {
                execution_provenance.insert(mutation.id, execution);
            }
            if let Some(selection) = selection {
                selection_provenance.insert(mutation.id, selection);
            }
            if let Some(diagnostic) = build_error_diagnostic {
                debug_assert_eq!(result, MutationResult::BuildError);
                build_error_diagnostics.push(diagnostic);
            }
            if let Some(recipe) = replay_recipe {
                if matches!(
                    result,
                    MutationResult::Killed | MutationResult::Survived | MutationResult::Timeout
                ) {
                    replay_recipes.insert(mutation.id, recipe);
                }
            }
            results.push((mutation, result));
        }

        (
            MutationReport {
                results,
                execution_provenance,
                selection_provenance,
                build_error_diagnostics,
                schemata: None,
                baseline_timing: None,
                duration,
                test_command: if self.commands.language_commands.is_empty()
                    && self.commands.project_commands.is_empty()
                {
                    Some(sandboxed_command(
                        &self.commands.sandbox_command,
                        &self.commands.command,
                    ))
                } else {
                    None
                },
                build_command: if self.commands.has_build_command() {
                    sandboxed_command(&self.commands.sandbox_command, &self.commands.build_command)
                } else {
                    vec![]
                },
                planned_total,
                early_stop_reason,
                total,
                killed,
                survived,
                timeout: timeout_count,
                build_errors,
            },
            replay_recipes,
        )
    }
}

fn schema_rewrites_for_language(
    project_root: &Path,
    language: &str,
    schema_mutations: &[crate::schemata::SchemaMutation],
) -> Result<Vec<crate::schemata::SchemaFileRewrite>, crate::schemata::SchemaRewriteError> {
    match language {
        "c" => crate::schemata::rewrite_c_files(project_root, schema_mutations),
        "cpp" => crate::schemata::rewrite_cpp_files(project_root, schema_mutations),
        "go" => crate::schemata::rewrite_go_files(project_root, schema_mutations),
        "java" => crate::schemata::rewrite_java_files(project_root, schema_mutations),
        "rust" => crate::schemata::rewrite_rust_files(project_root, schema_mutations),
        _ => Err(crate::schemata::SchemaRewriteError::new(format!(
            "{language} schemata execution is not available"
        ))),
    }
}

fn apply_schema_rewrites_to_workspace(
    project_root: &Path,
    workspace_root: &Path,
    rewrites: &[crate::schemata::SchemaFileRewrite],
) -> Result<(), crate::schemata::SchemaRewriteError> {
    for rewrite in rewrites {
        let relative = project_relative_path(project_root, &rewrite.file).map_err(|_| {
            crate::schemata::SchemaRewriteError::new(format!(
                "could not resolve rewritten file {}",
                rewrite.file.display()
            ))
        })?;
        let target =
            validate_and_resolve_mutation_path(workspace_root, &relative).map_err(|_| {
                crate::schemata::SchemaRewriteError::new(format!(
                    "rewritten file {} is not in schema workspace",
                    rewrite.file.display()
                ))
            })?;
        write_workspace_file(&target, &rewrite.content).map_err(|e| {
            crate::schemata::SchemaRewriteError::new(format!(
                "could not write schema file {}: {e}",
                target.display()
            ))
        })?;
    }

    Ok(())
}

fn run_schema_workspace_mutation(
    runner: &TestRunner,
    workspace_root: &Path,
    rewrites: &[crate::schemata::SchemaFileRewrite],
    argv: &[String],
    timeout: Duration,
    env: &HashMap<String, String>,
    cacheable: &mut bool,
) -> MutationOutcome {
    if let Err(e) =
        apply_schema_rewrites_to_workspace(&runner.project_root, workspace_root, rewrites)
    {
        *cacheable = false;
        return MutationOutcome::build_error_with(
            "schema_rewrite",
            vec![],
            format!("could not apply schema rewrites: {e}"),
        );
    }

    if runner.commands.has_build_command() {
        let build = run_command(
            &runner.commands.build_command,
            &runner.commands.sandbox_command,
            workspace_root,
            timeout,
            true,
            &runner.env,
            &runner.cancelled,
        );
        if build.cancelled {
            return build;
        }
        if build.result != MutationResult::Survived {
            return MutationOutcome::build_error_with(
                "schema_build",
                runner.commands.build_command.clone(),
                build_error_message_from_outcome(
                    "schema build command",
                    &runner.commands.build_command,
                    workspace_root,
                    &build,
                ),
            );
        }
    }

    run_command(
        argv,
        &runner.commands.sandbox_command,
        workspace_root,
        timeout,
        runner.show_output || runner.incremental_history,
        env,
        &runner.cancelled,
    )
}

fn records_from_report(
    report: MutationReport,
    mut replay_recipes: BTreeMap<u32, RegularDirectRecipe>,
) -> Vec<MutationRunRecord> {
    let MutationReport {
        results,
        execution_provenance,
        selection_provenance,
        build_error_diagnostics,
        ..
    } = report;
    let mut diagnostics: HashMap<u32, BuildErrorDiagnostic> = build_error_diagnostics
        .into_iter()
        .map(|diagnostic| (diagnostic.mutation_id, diagnostic))
        .collect();
    results
        .into_iter()
        .map(|(mutation, result)| {
            let execution = execution_provenance
                .get(&mutation.id)
                .copied()
                .unwrap_or_else(|| MutationExecution::for_result(result));
            let diagnostic = diagnostics.remove(&mutation.id);
            let selection = selection_provenance.get(&mutation.id).copied();
            let mut record = MutationRunRecord::new(mutation, result, diagnostic)
                .with_execution(execution)
                .with_selection(selection);
            if let Some(recipe) = replay_recipes.remove(&record.mutation.id) {
                record = record.with_replay_recipe(recipe);
            }
            record
        })
        .collect()
}

fn workspace_pool_slot_count(parallelism: usize, total: usize) -> usize {
    debug_assert!(total > 0);
    parallelism.max(1).min(total)
}

struct MutationOutcome {
    result: MutationResult,
    test_output: Option<String>,
    build_error_detail: Option<BuildErrorDetail>,
    cancelled: bool,
}

struct BuildErrorDetail {
    phase: String,
    command: Vec<String>,
    message: String,
}

impl BuildErrorDetail {
    fn new(phase: impl Into<String>, command: Vec<String>, message: impl Into<String>) -> Self {
        Self {
            phase: phase.into(),
            command,
            message: message.into(),
        }
    }
}

impl MutationOutcome {
    fn new(result: MutationResult, test_output: Option<String>) -> Self {
        Self {
            result,
            test_output,
            build_error_detail: None,
            cancelled: false,
        }
    }

    fn build_error_with(
        phase: impl Into<String>,
        command: Vec<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            result: MutationResult::BuildError,
            test_output: None,
            build_error_detail: Some(BuildErrorDetail::new(phase, command, message)),
            cancelled: false,
        }
    }

    fn cancelled() -> Self {
        Self {
            result: MutationResult::BuildError,
            test_output: None,
            build_error_detail: None,
            cancelled: true,
        }
    }
}

struct BuildCommand<'a> {
    argv: &'a [String],
    origin: BuildCommandOrigin,
}

fn build_error_diagnostic_from_outcome(
    mutation: &Mutation,
    runner: &str,
    outcome: &MutationOutcome,
) -> Option<BuildErrorDiagnostic> {
    if outcome.result != MutationResult::BuildError {
        return None;
    }
    let Some(detail) = outcome.build_error_detail.as_ref() else {
        return Some(BuildErrorDiagnostic::new(
            mutation.id,
            runner,
            "unknown",
            vec![],
            "build error diagnostic unavailable",
        ));
    };
    Some(BuildErrorDiagnostic::new(
        mutation.id,
        runner,
        detail.phase.clone(),
        detail.command.clone(),
        detail.message.clone(),
    ))
}

fn build_error_message_from_outcome(
    context: &str,
    command: &[String],
    cwd: &Path,
    outcome: &MutationOutcome,
) -> String {
    if let Some(output) = outcome.test_output.as_deref() {
        if !output.trim().is_empty() {
            return output.to_string();
        }
    }
    if let Some(detail) = outcome.build_error_detail.as_ref() {
        if !detail.message.trim().is_empty() {
            return detail.message.clone();
        }
    }
    let command = if command.is_empty() {
        "<empty>".to_string()
    } else {
        command.join(" ")
    };
    format!(
        "{context} returned {}; command: {command}; cwd: {}",
        outcome.result,
        cwd.display()
    )
}

fn mutation_file_path(project_root: &Path, mutation_file: &Path) -> PathBuf {
    if mutation_file.is_absolute() {
        mutation_file.to_path_buf()
    } else {
        project_root.join(mutation_file)
    }
}

fn project_relative_path(project_root: &Path, file: &Path) -> Result<PathBuf, ()> {
    if file.is_absolute() {
        let canonical = file.canonicalize().map_err(|_| ())?;
        let root = project_root.canonicalize().map_err(|_| ())?;
        canonical
            .strip_prefix(root)
            .map(PathBuf::from)
            .map_err(|_| ())
    } else {
        Ok(file.to_path_buf())
    }
}

fn force_go_no_test_cache(mut argv: Vec<String>) -> Vec<String> {
    if argv.len() < 2 || argv[0] != "go" || argv[1] != "test" {
        return argv;
    }
    if argv
        .iter()
        .skip(2)
        .any(|arg| arg == "-count" || arg.starts_with("-count="))
    {
        return argv;
    }
    argv.insert(2, "-count=1".to_string());
    argv
}

fn cache_identity(project_root: &Path, mutation: &Mutation) -> String {
    format!(
        "{}:{}..{}:{}:{}=>{}",
        normalized_cache_path(project_root, &mutation.file),
        mutation.byte_range.start,
        mutation.byte_range.end,
        mutation.operator,
        mutation.original,
        mutation.replacement
    )
}

fn normalized_cache_path(project_root: &Path, mutation_file: &Path) -> String {
    let relative = if mutation_file.is_absolute() {
        mutation_file
            .canonicalize()
            .ok()
            .and_then(|path| {
                project_root
                    .canonicalize()
                    .ok()
                    .and_then(|root| path.strip_prefix(root).ok().map(PathBuf::from))
            })
            .unwrap_or_else(|| mutation_file.to_path_buf())
    } else {
        mutation_file.to_path_buf()
    };

    relative
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn validate_and_resolve_mutation_path(
    project_root: &Path,
    mutation_file: &Path,
) -> Result<PathBuf, ()> {
    let file_path = mutation_file_path(project_root, mutation_file);

    let canonical = match file_path.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "warning: path traversal blocked: cannot resolve {}: {e}",
                mutation_file.display()
            );
            return Err(());
        }
    };
    let root = match project_root.canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("warning: path traversal blocked: cannot resolve project root: {e}");
            return Err(());
        }
    };
    if !canonical.starts_with(&root) {
        eprintln!(
            "warning: path traversal blocked: {} escapes project root",
            mutation_file.display()
        );
        return Err(());
    }

    Ok(file_path)
}

struct ResolvedMutation<'a> {
    mutation: &'a Mutation,
    file_path: Result<PathBuf, ()>,
    relative_path: Result<PathBuf, ()>,
}

impl<'a> ResolvedMutation<'a> {
    #[cfg(any(test, feature = "fuzzing"))]
    fn new(project_root: &Path, mutation: &'a Mutation) -> Self {
        Self {
            mutation,
            file_path: validate_and_resolve_mutation_path(project_root, &mutation.file),
            relative_path: project_relative_path(project_root, &mutation.file).and_then(
                |relative| {
                    is_safe_relative_path(&relative)
                        .then_some(relative)
                        .ok_or(())
                },
            ),
        }
    }

    fn new_for_execution(
        original_root: &Path,
        execution_root: &Path,
        mutation: &'a Mutation,
    ) -> Self {
        let relative_path = if mutation.file.is_absolute() {
            mutation.file.canonicalize().ok().and_then(|canonical| {
                original_root
                    .canonicalize()
                    .ok()
                    .and_then(|root| canonical.strip_prefix(root).ok().map(PathBuf::from))
            })
        } else {
            Some(mutation.file.clone())
        }
        .filter(|relative| is_safe_relative_path(relative))
        .ok_or(());
        let file_path = relative_path
            .as_ref()
            .map_err(|_| ())
            .and_then(|relative| validate_and_resolve_mutation_path(execution_root, relative));

        Self {
            mutation,
            file_path,
            relative_path,
        }
    }

    fn new_for_replay(source_root: &Path, workspace_root: &Path, mutation: &'a Mutation) -> Self {
        let relative_path =
            project_relative_path(source_root, &mutation.file).and_then(|relative| {
                is_safe_relative_path(&relative)
                    .then_some(relative)
                    .ok_or(())
            });
        let file_path = relative_path
            .as_ref()
            .map(|relative| workspace_root.join(relative))
            .map_err(|_| ());
        Self {
            mutation,
            file_path,
            relative_path,
        }
    }
}

fn validate_replay_snapshot_target(
    workspace: &ReplayWorkspace,
    target: &ResolvedMutation<'_>,
    expected_source_fingerprint: &str,
) -> anyhow::Result<()> {
    let relative = target.relative_path.as_ref().map_err(|()| {
        anyhow::anyhow!(
            "could not derive a safe replay source path for {}",
            target.mutation.file.display()
        )
    })?;
    let source = workspace.read_regular(relative).with_context(|| {
        format!(
            "could not read replay source {} in isolated snapshot",
            target.mutation.file.display()
        )
    })?;
    if source_fingerprint(&source) != expected_source_fingerprint {
        anyhow::bail!("replay snapshot source fingerprint does not match the report");
    }
    if !range_matches(
        &source,
        target.mutation.byte_range.start,
        target.mutation.byte_range.end,
        &target.mutation.original,
    ) {
        anyhow::bail!("replay snapshot mutation byte range and original bytes do not match");
    }
    Ok(())
}

/// Verify against the current suite only after its unmutated route passes in
/// a separate disposable workspace, so baseline side effects cannot kill the mutant.
pub fn run_repair_verification(
    project_root: &Path,
    mutation: &Mutation,
    config: ReplayRunConfig<'_>,
) -> anyhow::Result<ReplayRunOutcome> {
    let frozen_mutant = {
        let workspace = copy_workspace_for_replay(
            project_root,
            config.source_revision,
            config.respect_workspace_ignores,
        )
        .context("could not create repair baseline workspace")?;
        let target = ResolvedMutation::new_for_replay(project_root, workspace.root(), mutation);
        validate_replay_snapshot_target(&workspace, &target, config.source_fingerprint)?;
        // Freeze both inputs before any commands run. Recopying the live
        // worktree afterwards could mistake a newly failing test for a kill.
        let frozen_mutant = copy_workspace_for_replay(
            workspace.root(),
            config.source_revision,
            config.respect_workspace_ignores,
        )
        .context("could not freeze repair mutation workspace")?;
        // Both clones must retain the same stable Git origin. The baseline
        // directory is disposable and must not become a test dependency.
        let origin = std::process::Command::new("git")
            .args(["remote", "set-url", "origin"])
            .arg(project_root)
            .current_dir(frozen_mutant.root())
            .output()
            .context("could not preserve repair workspace Git origin")?;
        if !origin.status.success() {
            bail!(
                "could not preserve repair workspace Git origin: {}",
                String::from_utf8_lossy(&origin.stderr)
            );
        }
        for (phase, command) in config
            .build_command
            .as_deref()
            .map(|command| (SuiteFailurePhase::Build, command))
            .into_iter()
            .chain(std::iter::once((
                SuiteFailurePhase::Test,
                config.test_command.as_slice(),
            )))
        {
            measure_baseline_command(
                phase,
                command,
                &[],
                workspace.root(),
                config.timeout,
                &config.env,
                config.cancelled,
            )
            .context("repair not verified: current unmutated suite must pass first")?;
        }
        frozen_mutant
    };
    run_replay_in_workspace(project_root, mutation, config, &frozen_mutant)
}

/// Execute one validated replay in a disposable workspace without consulting
/// or updating any normal-run cache/history state.
pub fn run_replay_mutation(
    project_root: &Path,
    mutation: &Mutation,
    config: ReplayRunConfig<'_>,
) -> anyhow::Result<ReplayRunOutcome> {
    // File-only replay is Windows-safe: clone, populate, validation, and the
    // staged-rename mutation publish are all capability-bounded beneath pinned
    // parent handles. cap-primitives 4.0.2 cannot atomically remove an opened
    // directory on Windows, so `remove_cap_entry` fails closed on directory
    // removals during workspace setup, before any mutation/test command spawns.
    let workspace = copy_workspace_for_replay(
        project_root,
        config.source_revision,
        config.respect_workspace_ignores,
    )
    .with_context(|| "could not create replay workspace")?;
    run_replay_in_workspace(project_root, mutation, config, &workspace)
}

fn run_replay_in_workspace(
    project_root: &Path,
    mutation: &Mutation,
    config: ReplayRunConfig<'_>,
    workspace: &ReplayWorkspace,
) -> anyhow::Result<ReplayRunOutcome> {
    let target = ResolvedMutation::new_for_replay(project_root, workspace.root(), mutation);
    validate_replay_snapshot_target(workspace, &target, config.source_fingerprint)?;
    let build_command = config.build_command.as_deref().unwrap_or(&[]);
    let outcome = run_single_mutation_with_replay_access(
        &config.test_command,
        &[],
        BuildCommand {
            argv: build_command,
            origin: if config.build_command.is_some() {
                BuildCommandOrigin::Configured
            } else {
                BuildCommandOrigin::None
            },
        },
        config.timeout,
        workspace.root(),
        target,
        config.show_output,
        &config.env,
        config.cancelled,
        Some(workspace),
    );
    Ok(ReplayRunOutcome {
        result: outcome.result,
        test_output: outcome.test_output,
        cancelled: outcome.cancelled,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_single_mutation(
    command: &[String],

    sandbox_command: &[String],
    build_command: BuildCommand<'_>,
    timeout: Duration,
    project_root: &Path,
    target: ResolvedMutation<'_>,
    capture_output: bool,
    env: &HashMap<String, String>,
    cancelled: &AtomicBool,
) -> MutationOutcome {
    run_single_mutation_with_replay_access(
        command,
        sandbox_command,
        build_command,
        timeout,
        project_root,
        target,
        capture_output,
        env,
        cancelled,
        None,
    )
}

/// Exercise the normal FileGuard-backed mutation path for libFuzzer.
///
/// This is deliberately feature-gated: cargo-fuzz is the sole consumer and
/// stable builds expose no fuzz-only runner API.
#[cfg(feature = "fuzzing")]
pub fn fuzz_apply_and_restore(project_root: &Path, mutation: &Mutation) -> anyhow::Result<()> {
    let cancelled = AtomicBool::new(false);
    let command = if cfg!(windows) {
        vec!["cmd".to_string(), "/C".to_string(), "exit 0".to_string()]
    } else {
        vec!["true".to_string()]
    };
    let outcome = run_single_mutation(
        &command,
        &[],
        BuildCommand {
            argv: &[],
            origin: BuildCommandOrigin::None,
        },
        Duration::from_secs(1),
        project_root,
        ResolvedMutation::new(project_root, mutation),
        false,
        &HashMap::new(),
        &cancelled,
    );
    if outcome.result == MutationResult::BuildError {
        let detail = outcome
            .build_error_detail
            .map(|detail| detail.message)
            .unwrap_or_else(|| "mutation execution was cancelled".to_string());
        anyhow::bail!("{detail}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_single_mutation_with_replay_access(
    command: &[String],
    sandbox_command: &[String],
    build_command: BuildCommand<'_>,
    timeout: Duration,
    project_root: &Path,
    target: ResolvedMutation<'_>,
    capture_output: bool,
    env: &HashMap<String, String>,
    cancelled: &AtomicBool,
    replay_workspace: Option<&ReplayWorkspace>,
) -> MutationOutcome {
    if cancelled.load(Ordering::Acquire) {
        return MutationOutcome::cancelled();
    }

    let mutation = target.mutation;
    let replay_relative = target.relative_path.as_ref().ok().cloned();
    if replay_workspace.is_some() && replay_relative.is_none() {
        return MutationOutcome::build_error_with(
            "resolve_mutation_path",
            vec![],
            format!(
                "could not derive a safe mutation path {} in replay workspace",
                mutation.file.display()
            ),
        );
    }
    let file_path = match target.file_path {
        Ok(path) => path,
        Err(()) => {
            return MutationOutcome::build_error_with(
                "resolve_mutation_path",
                vec![],
                format!(
                    "could not resolve mutation path {} in workspace {}",
                    mutation.file.display(),
                    project_root.display()
                ),
            );
        }
    };

    // Replay reads and restores through its clone capability; normal runs keep
    // their established ambient-path behavior.
    let (original, _normal_guard, _replay_guard): (
        Vec<u8>,
        Option<FileGuard>,
        Option<ReplayFileGuard<'_>>,
    ) = if let (Some(workspace), Some(relative)) = (replay_workspace, replay_relative.as_deref()) {
        match workspace.read_regular(relative) {
            Ok(content) => {
                let guard = ReplayFileGuard {
                    workspace,
                    relative: relative.to_path_buf(),
                    original: content.clone(),
                };
                (content, None, Some(guard))
            }
            Err(error) => {
                eprintln!("warning: could not read {}: {error}", file_path.display());
                return MutationOutcome::build_error_with(
                    "read_source",
                    vec![],
                    format!("could not read {}: {error}", file_path.display()),
                );
            }
        }
    } else {
        match std::fs::read(&file_path) {
            Ok(content) => {
                let guard = FileGuard {
                    path: file_path.clone(),
                    original: content.clone(),
                };
                (content, Some(guard), None)
            }
            Err(error) => {
                eprintln!("warning: could not read {}: {error}", file_path.display());
                return MutationOutcome::build_error_with(
                    "read_source",
                    vec![],
                    format!("could not read {}: {error}", file_path.display()),
                );
            }
        }
    };

    // Apply mutation
    let mut mutated = original.clone();
    let range = mutation.byte_range.clone();
    if range.start > range.end || range.end > mutated.len() {
        return MutationOutcome::build_error_with(
            "apply_mutation",
            vec![],
            format!(
                "mutation byte range {}..{} is outside {} bytes in {}",
                range.start,
                range.end,
                mutated.len(),
                file_path.display()
            ),
        );
    }
    if !crate::source_identity::range_matches(&original, range.start, range.end, &mutation.original)
    {
        return MutationOutcome::build_error_with(
            "apply_mutation",
            vec![],
            format!(
                "mutation original bytes do not match {}..{} in {}",
                range.start,
                range.end,
                file_path.display()
            ),
        );
    }
    mutated.splice(range, mutation.replacement.as_bytes().iter().copied());

    let write_result = match (replay_workspace, replay_relative.as_deref()) {
        (Some(workspace), Some(relative)) => workspace.replace_regular(relative, &mutated),
        (Some(_), None) => Err(std::io::Error::other(
            "missing replay mutation path after validation",
        )),
        (None, _) => write_workspace_file(&file_path, &mutated),
    };
    if let Err(error) = write_result {
        eprintln!("warning: could not write {}: {error}", file_path.display());
        return MutationOutcome::build_error_with(
            "write_source",
            vec![],
            format!("could not write {}: {error}", file_path.display()),
        );
    }

    // Configured build checks skip expensive tests when the mutation does not compile.
    if build_command.origin.runs_before_tests() && !build_command.argv.is_empty() {
        let build_outcome = run_command(
            build_command.argv,
            sandbox_command,
            project_root,
            timeout,
            true,
            env,
            cancelled,
        );
        if build_outcome.cancelled {
            return build_outcome;
        }
        if build_outcome.result != MutationResult::Survived {
            return MutationOutcome::build_error_with(
                "build_command",
                build_command.argv.to_vec(),
                build_error_message_from_outcome(
                    "build command",
                    build_command.argv,
                    project_root,
                    &build_outcome,
                ),
            );
        }
    }

    if cancelled.load(Ordering::Acquire) {
        return MutationOutcome::cancelled();
    }

    // Run test command; guard will restore the file on drop
    run_command(
        command,
        sandbox_command,
        project_root,
        timeout,
        capture_output,
        env,
        cancelled,
    )
}

fn run_command(
    command: &[String],
    sandbox_command: &[String],
    cwd: &Path,
    timeout_dur: Duration,
    capture_output: bool,
    env: &HashMap<String, String>,
    cancelled: &AtomicBool,
) -> MutationOutcome {
    if cancelled.load(Ordering::Acquire) {
        return MutationOutcome::cancelled();
    }

    if command.is_empty() {
        return MutationOutcome::build_error_with("command", vec![], "command is empty");
    }

    let command = sandboxed_command(sandbox_command, command);
    // foxguard: ignore[rs/no-command-injection]
    // User-provided argv is executed directly without a shell; this is the
    // core feature of the runner, not a string interpolation sink.
    let mut cmd = std::process::Command::new(&command[0]);
    cmd.args(&command[1..]).current_dir(cwd).envs(env);
    configure_command_for_process_tree(&mut cmd);

    let mut stdout_capture = None;
    let mut stderr_capture = None;
    if capture_output {
        stdout_capture = match OutputCapture::new("stdout") {
            Ok(capture) => Some(capture),
            Err(e) => {
                eprintln!("warning: could not create stdout capture file: {e}");
                return MutationOutcome::build_error_with(
                    "command",
                    command.to_vec(),
                    format!("could not create stdout capture file: {e}"),
                );
            }
        };
        stderr_capture = match OutputCapture::new("stderr") {
            Ok(capture) => Some(capture),
            Err(e) => {
                eprintln!("warning: could not create stderr capture file: {e}");
                return MutationOutcome::build_error_with(
                    "command",
                    command.to_vec(),
                    format!("could not create stderr capture file: {e}"),
                );
            }
        };
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());
    } else {
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::null());
    }

    if cancelled.load(Ordering::Acquire) {
        return MutationOutcome::cancelled();
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("warning: could not spawn command {:?}: {e}", command[0]);
            return MutationOutcome::build_error_with(
                "command",
                command.to_vec(),
                format!("could not spawn command {:?}: {e}", command[0]),
            );
        }
    };
    let mut process_tree = ProcessTreeGuard::attach(&child);

    if capture_output {
        let Some(stdout) = child.stdout.take() else {
            terminate_and_wait(&mut child, &mut process_tree);
            return MutationOutcome::build_error_with(
                "command",
                command.to_vec(),
                "could not capture command stdout",
            );
        };
        if let Some(capture) = stdout_capture.as_mut() {
            if let Err(e) = capture.start(stdout) {
                eprintln!("warning: could not open stdout capture file: {e}");
                terminate_and_wait(&mut child, &mut process_tree);
                return MutationOutcome::build_error_with(
                    "command",
                    command.to_vec(),
                    format!("could not open stdout capture file: {e}"),
                );
            }
        }

        let Some(stderr) = child.stderr.take() else {
            terminate_and_wait(&mut child, &mut process_tree);
            finish_capture_threads(
                &mut stdout_capture,
                &mut stderr_capture,
                capture_cleanup_deadline(),
            );
            return MutationOutcome::build_error_with(
                "command",
                command.to_vec(),
                "could not capture command stderr",
            );
        };
        if let Some(capture) = stderr_capture.as_mut() {
            if let Err(e) = capture.start(stderr) {
                eprintln!("warning: could not open stderr capture file: {e}");
                terminate_and_wait(&mut child, &mut process_tree);
                finish_capture_threads(
                    &mut stdout_capture,
                    &mut stderr_capture,
                    capture_cleanup_deadline(),
                );
                return MutationOutcome::build_error_with(
                    "command",
                    command.to_vec(),
                    format!("could not open stderr capture file: {e}"),
                );
            }
        }
    }

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if cancelled.load(Ordering::Acquire) {
                    terminate_and_wait(&mut child, &mut process_tree);
                    finish_capture_threads(
                        &mut stdout_capture,
                        &mut stderr_capture,
                        capture_cleanup_deadline(),
                    );
                    return MutationOutcome::cancelled();
                }
                if started.elapsed() >= timeout_dur {
                    terminate_and_wait(&mut child, &mut process_tree);
                    finish_capture_threads(
                        &mut stdout_capture,
                        &mut stderr_capture,
                        capture_cleanup_deadline(),
                    );
                    return MutationOutcome::new(MutationResult::Timeout, None);
                }
                let remaining = timeout_dur.saturating_sub(started.elapsed());
                thread::sleep(remaining.min(Duration::from_millis(10)));
            }
            Err(e) => {
                terminate_and_wait(&mut child, &mut process_tree);
                finish_capture_threads(
                    &mut stdout_capture,
                    &mut stderr_capture,
                    capture_cleanup_deadline(),
                );
                return MutationOutcome::build_error_with(
                    "command",
                    command.to_vec(),
                    format!("could not wait for command: {e}"),
                );
            }
        }
    };

    // Defense-in-depth: the child was reaped but process-group
    // descendants may still hold pipes open. Check the cancellation
    // flag before any result or side-effect propagation.
    if cancelled.load(Ordering::Acquire) {
        process_tree.terminate(child.id());
        finish_capture_threads(
            &mut stdout_capture,
            &mut stderr_capture,
            capture_cleanup_deadline(),
        );
        return MutationOutcome::cancelled();
    }

    process_tree.terminate(child.id());

    let result = if status.success() {
        MutationResult::Survived
    } else {
        MutationResult::Killed
    };
    let test_output = if capture_output {
        let capture_deadline = capture_cleanup_deadline();
        let stdout = stdout_capture
            .as_mut()
            .expect("stdout capture file should exist")
            .finish(capture_deadline);
        let stderr = stderr_capture
            .as_mut()
            .expect("stderr capture file should exist")
            .finish(capture_deadline);
        let mut combined = String::from_utf8_lossy(&stdout.bytes).into_owned();
        append_truncation_notice(&mut combined, stdout.truncated, "stdout");

        let stderr_text = String::from_utf8_lossy(&stderr.bytes);
        if !stderr_text.is_empty() {
            if !combined.is_empty() {
                combined.push('\n');
            }
            combined.push_str(&stderr_text);
        }
        append_truncation_notice(&mut combined, stderr.truncated, "stderr");
        Some(combined)
    } else {
        None
    };

    MutationOutcome::new(result, test_output)
}

fn terminate_and_wait(child: &mut std::process::Child, process_tree: &mut ProcessTreeGuard) {
    process_tree.terminate(child.id());
    let _ = child.wait();
}

#[cfg(unix)]
struct ProcessTreeGuard;

#[cfg(unix)]
impl ProcessTreeGuard {
    fn attach(_child: &std::process::Child) -> Self {
        Self
    }

    fn terminate(&mut self, pid: u32) {
        terminate_process_tree(pid);
    }
}

#[cfg(windows)]
struct ProcessTreeGuard {
    job: Option<WindowsJobHandle>,
}

#[cfg(windows)]
impl ProcessTreeGuard {
    fn attach(child: &std::process::Child) -> Self {
        match WindowsJobHandle::new_kill_on_close().and_then(|job| {
            job.assign(child)?;
            Ok(job)
        }) {
            Ok(job) => Self { job: Some(job) },
            Err(e) => {
                eprintln!("warning: could not attach command to Windows job object: {e}");
                Self { job: None }
            }
        }
    }

    fn terminate(&mut self, pid: u32) {
        if self.job.take().is_none() {
            terminate_process_tree(pid);
        }
    }
}

#[cfg(windows)]
struct WindowsJobHandle {
    handle: WindowsHandle,
}

#[cfg(windows)]
impl WindowsJobHandle {
    fn new_kill_on_close() -> std::io::Result<Self> {
        use std::mem::size_of;
        use std::ptr::null;

        let handle = unsafe { CreateJobObjectW(null(), null()) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }

        let job = Self { handle };
        let mut info = WindowsJobExtendedLimitInformation::default();
        info.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let configured = unsafe {
            SetInformationJobObject(
                job.handle,
                JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS,
                &info as *const _ as *const std::ffi::c_void,
                size_of::<WindowsJobExtendedLimitInformation>() as u32,
            )
        };
        if configured == 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(job)
    }

    fn assign(&self, child: &std::process::Child) -> std::io::Result<()> {
        use std::os::windows::io::AsRawHandle;

        let assigned = unsafe {
            AssignProcessToJobObject(self.handle, child.as_raw_handle() as WindowsHandle)
        };
        if assigned == 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WindowsJobHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}

#[cfg(windows)]
type WindowsHandle = *mut std::ffi::c_void;

#[cfg(windows)]
const INVALID_HANDLE_VALUE: WindowsHandle = -1isize as WindowsHandle;
#[cfg(windows)]
const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION_CLASS: i32 = 9;
#[cfg(windows)]
const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: u32 = 0x0000_2000;

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct WindowsJobBasicLimitInformation {
    per_process_user_time_limit: i64,
    per_job_user_time_limit: i64,
    limit_flags: u32,
    minimum_working_set_size: usize,
    maximum_working_set_size: usize,
    active_process_limit: u32,
    affinity: usize,
    priority_class: u32,
    scheduling_class: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct WindowsIoCounters {
    read_operation_count: u64,
    write_operation_count: u64,
    other_operation_count: u64,
    read_transfer_count: u64,
    write_transfer_count: u64,
    other_transfer_count: u64,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Default)]
struct WindowsJobExtendedLimitInformation {
    basic_limit_information: WindowsJobBasicLimitInformation,
    io_info: WindowsIoCounters,
    process_memory_limit: usize,
    job_memory_limit: usize,
    peak_process_memory_used: usize,
    peak_job_memory_used: usize,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(job_attributes: *const std::ffi::c_void, name: *const u16)
    -> WindowsHandle;
    fn SetInformationJobObject(
        job: WindowsHandle,
        information_class: i32,
        information: *const std::ffi::c_void,
        information_length: u32,
    ) -> i32;
    fn AssignProcessToJobObject(job: WindowsHandle, process: WindowsHandle) -> i32;
    fn CloseHandle(handle: WindowsHandle) -> i32;
}

#[cfg(not(any(unix, windows)))]
struct ProcessTreeGuard;

#[cfg(not(any(unix, windows)))]
impl ProcessTreeGuard {
    fn attach(_child: &std::process::Child) -> Self {
        Self
    }

    fn terminate(&mut self, _pid: u32) {}
}

#[cfg(unix)]
fn configure_command_for_process_tree(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0);
}

#[cfg(windows)]
fn configure_command_for_process_tree(cmd: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn configure_command_for_process_tree(_cmd: &mut std::process::Command) {}

#[cfg(unix)]
fn terminate_process_tree(pid: u32) {
    let pgid = pid as libc::pid_t;
    unsafe {
        let _ = libc::killpg(pgid, libc::SIGKILL);
    }
}

#[cfg(windows)]
fn terminate_process_tree(pid: u32) {
    let _ = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_tree(_pid: u32) {}

struct OutputCapture {
    file: tempfile::NamedTempFile,
    reader: Option<thread::JoinHandle<bool>>,
    stream: &'static str,
}

impl OutputCapture {
    fn new(stream: &'static str) -> std::io::Result<Self> {
        Ok(Self {
            file: tempfile::NamedTempFile::new()?,
            reader: None,
            stream,
        })
    }

    fn start<R>(&mut self, reader: R) -> std::io::Result<()>
    where
        R: Read + Send + 'static,
    {
        let writer = self.file.reopen()?;
        self.reader = Some(spawn_output_capture(reader, writer, self.stream));
        Ok(())
    }

    fn finish(&mut self, deadline: Instant) -> CapturedOutput {
        let truncated = self
            .reader
            .take()
            .is_some_and(|reader| finish_capture_reader(reader, deadline, self.stream));
        read_captured_output(self.file.path(), self.stream, truncated)
    }
}

fn capture_cleanup_deadline() -> Instant {
    Instant::now() + CAPTURE_CLEANUP_TIMEOUT
}

fn finish_capture_reader(
    reader: thread::JoinHandle<bool>,
    deadline: Instant,
    stream: &'static str,
) -> bool {
    loop {
        if reader.is_finished() {
            return match reader.join() {
                Ok(truncated) => truncated,
                Err(_) => {
                    eprintln!("warning: {stream} capture thread panicked");
                    true
                }
            };
        }

        let now = Instant::now();
        if now >= deadline {
            eprintln!("warning: {stream} capture thread did not finish before cleanup deadline");
            return false;
        }

        thread::sleep((deadline - now).min(Duration::from_millis(5)));
    }
}

fn finish_capture_threads(
    stdout_capture: &mut Option<OutputCapture>,
    stderr_capture: &mut Option<OutputCapture>,
    deadline: Instant,
) {
    if let Some(capture) = stdout_capture.as_mut() {
        let _ = capture.finish(deadline);
    }
    if let Some(capture) = stderr_capture.as_mut() {
        let _ = capture.finish(deadline);
    }
}

struct CapturedOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

fn spawn_output_capture<R>(
    mut reader: R,
    mut writer: fs::File,
    stream: &'static str,
) -> thread::JoinHandle<bool>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut written = 0usize;
        let mut truncated = false;
        let mut buffer = [0; 8192];

        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let available = CAPTURED_OUTPUT_LIMIT.saturating_sub(written);
                    let keep = n.min(available);
                    if keep > 0 {
                        if let Err(e) = writer.write_all(&buffer[..keep]) {
                            eprintln!("warning: could not write {stream} capture output: {e}");
                            break;
                        }
                        written += keep;
                    }
                    if keep < n {
                        truncated = true;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    eprintln!("warning: could not read {stream} capture output: {e}");
                    break;
                }
            }
        }
        let _ = writer.flush();
        truncated
    })
}

fn read_captured_output(path: &Path, stream: &str, already_truncated: bool) -> CapturedOutput {
    let mut file = match fs::File::open(path) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("warning: could not read {stream} capture file: {e}");
            return CapturedOutput {
                bytes: Vec::new(),
                truncated: false,
            };
        }
    };
    let mut bytes = Vec::new();
    let mut limited = std::io::Read::by_ref(&mut file).take((CAPTURED_OUTPUT_LIMIT + 1) as u64);
    if let Err(e) = limited.read_to_end(&mut bytes) {
        eprintln!("warning: could not read {stream} capture output: {e}");
        return CapturedOutput {
            bytes: Vec::new(),
            truncated: false,
        };
    }
    let truncated = already_truncated || bytes.len() > CAPTURED_OUTPUT_LIMIT;
    if truncated {
        bytes.truncate(CAPTURED_OUTPUT_LIMIT);
    }
    CapturedOutput { bytes, truncated }
}

fn append_truncation_notice(output: &mut String, truncated: bool, stream: &str) {
    if !truncated {
        return;
    }
    if !output.is_empty() && !output.ends_with('\n') {
        output.push('\n');
    }
    output.push_str(&format!(
        "[{stream} truncated after {CAPTURED_OUTPUT_LIMIT} bytes]"
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutation;

    #[cfg(windows)]
    struct SubstDrive {
        name: String,
    }

    #[cfg(windows)]
    impl SubstDrive {
        fn allocate(target: &Path, system_drive: u8) -> std::io::Result<Self> {
            for letter in (b'A'..=b'Z').rev() {
                if letter.eq_ignore_ascii_case(&system_drive) {
                    continue;
                }
                let name = format!("{}:", char::from(letter));
                let output = std::process::Command::new("subst")
                    .arg(&name)
                    .arg(target)
                    .output()?;
                if output.status.success() {
                    return Ok(Self { name });
                }
            }
            Err(std::io::Error::other(
                "could not allocate a free non-system drive with subst",
            ))
        }

        fn root(&self) -> PathBuf {
            PathBuf::from(format!("{}\\", self.name))
        }
    }

    #[cfg(windows)]
    impl Drop for SubstDrive {
        fn drop(&mut self) {
            let _ = std::process::Command::new("subst")
                .arg(&self.name)
                .arg("/D")
                .output();
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_normal_disk_root_rejects_non_normal_roots() {
        assert_eq!(
            windows_normal_disk_root_letter(Path::new(r"C:\")),
            Some(b'C')
        );
        assert_eq!(
            windows_normal_disk_root_letter(Path::new(r"c:\")),
            Some(b'c')
        );
        assert_eq!(windows_normal_disk_root_letter(Path::new(r"C:\\")), None);
        assert_eq!(windows_normal_disk_root_letter(Path::new(r"C:\temp")), None);
        assert_eq!(
            windows_normal_disk_root_letter(Path::new(r"\\server\share")),
            None
        );
        assert_eq!(windows_normal_disk_root_letter(Path::new(r"\\?\C:\")), None);
    }

    fn successful_command() -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["cmd".into(), "/C".into(), "exit 0".into()]
        }
        #[cfg(not(windows))]
        {
            vec!["sh".into(), "-c".into(), "true".into()]
        }
    }

    fn failing_command() -> Vec<String> {
        #[cfg(windows)]
        {
            vec!["cmd".into(), "/C".into(), "exit 1".into()]
        }
        #[cfg(not(windows))]
        {
            vec!["sh".into(), "-c".into(), "false".into()]
        }
    }

    /// Append the exact bytes `invoked` to a log file so tests can prove a
    /// command spawned (or did not) across platforms.
    fn appending_log_command(log_path: &Path) -> Vec<String> {
        #[cfg(windows)]
        {
            vec![
                "powershell".into(),
                "-NoProfile".into(),
                "-Command".into(),
                format!(
                    "[System.IO.File]::AppendAllText('{}', 'invoked')",
                    // PowerShell single-quoted literals escape an apostrophe
                    // by doubling it.
                    log_path.display().to_string().replace('\'', "''")
                ),
            ]
        }
        #[cfg(not(windows))]
        {
            vec![
                "sh".into(),
                "-c".into(),
                "printf invoked >> \"$1\"".into(),
                "replay".into(),
                log_path.display().to_string(),
            ]
        }
    }

    #[test]
    fn file_guard_restores_content_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, b"original").unwrap();

        {
            let _guard = FileGuard {
                path: path.clone(),
                original: b"original".to_vec(),
            };
            std::fs::write(&path, b"modified").unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), b"modified");
        }

        assert_eq!(std::fs::read(&path).unwrap(), b"original");
    }

    #[test]
    fn file_guard_restores_content_on_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.txt");
        std::fs::write(&path, b"original").unwrap();
        let path_for_closure = path.clone();

        let result = std::panic::catch_unwind(move || {
            let _guard = FileGuard {
                path: path_for_closure.clone(),
                original: b"original".to_vec(),
            };
            std::fs::write(&path_for_closure, b"mutated").unwrap();
            assert_eq!(std::fs::read(&path_for_closure).unwrap(), b"mutated");
            panic!("simulated panic mid-mutation");
        });

        assert!(
            result.is_err(),
            "panic should propagate out of catch_unwind"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"original",
            "FileGuard must restore original content even when unwinding"
        );
    }

    fn make_test_mutation(file: &std::path::Path) -> Mutation {
        Mutation {
            id: 1,
            file: file.to_path_buf(),
            language: String::new(),
            line: 1,
            column: 1,
            operator: "test".into(),
            description: "test mutation".into(),
            original: "hello".into(),
            replacement: "world".into(),
            byte_range: 0..5,
        }
    }

    /// Creates a tempdir with a "test.txt" containing "hello world" and a matching mutation.
    fn make_test_setup() -> (tempfile::TempDir, PathBuf, Mutation) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();
        let mutation = make_test_mutation(&file);
        (dir, file, mutation)
    }

    fn make_relative_test_setup() -> (tempfile::TempDir, PathBuf, Mutation) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();
        let mutation = make_test_mutation(Path::new("test.txt"));
        (dir, file, mutation)
    }

    #[cfg(unix)]
    #[test]
    fn replay_snapshot_target_rejects_fifo_before_reading() -> anyhow::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let root = tempdir.path().to_path_buf();
        let fifo = root.join("target");
        let status = std::process::Command::new("mkfifo").arg(&fifo).status()?;
        assert!(status.success());

        let mutation = make_test_mutation(&fifo);
        let target = ResolvedMutation::new(&root, &mutation);
        let workspace = ReplayWorkspace::open(WorkspaceCopy {
            _tempdir: tempdir,
            root,
            reset_strategy: WorkspaceResetStrategy::Copy,
        })?;
        let error =
            validate_replay_snapshot_target(&workspace, &target, "sha256:expected").unwrap_err();
        assert!(format!("{error:#}").contains("not a regular file"));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replay_workspace_publish_does_not_follow_replaced_leaf_symlink() -> anyhow::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let root = tempdir.path().to_path_buf();
        let target = root.join("target");
        std::fs::write(&target, b"original")?;
        let sentinel_dir = tempfile::tempdir()?;
        let sentinel = sentinel_dir.path().join("sentinel");
        std::fs::write(&sentinel, b"outside")?;
        let replacement_target = target.clone();
        let replacement_sentinel = sentinel.clone();
        let workspace = ReplayWorkspace::open(WorkspaceCopy {
            _tempdir: tempdir,
            root,
            reset_strategy: WorkspaceResetStrategy::Copy,
        })?;

        set_replay_publish_hook(Some(Box::new(move || {
            std::fs::remove_file(&replacement_target).unwrap();
            std::os::unix::fs::symlink(&replacement_sentinel, &replacement_target).unwrap();
        })));
        workspace.replace_regular(Path::new("target"), b"mutated")?;

        assert_eq!(std::fs::read(&sentinel)?, b"outside");
        assert_eq!(workspace.read_regular(Path::new("target"))?, b"mutated");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replay_workspace_publish_does_not_write_hardlink_target() -> anyhow::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let root = tempdir.path().to_path_buf();
        let sentinel_dir = tempfile::tempdir()?;
        let sentinel = sentinel_dir.path().join("sentinel");
        std::fs::write(&sentinel, b"outside")?;
        std::fs::hard_link(&sentinel, root.join("target"))?;
        let workspace = ReplayWorkspace::open(WorkspaceCopy {
            _tempdir: tempdir,
            root,
            reset_strategy: WorkspaceResetStrategy::Copy,
        })?;

        workspace.replace_regular(Path::new("target"), b"mutated")?;

        assert_eq!(std::fs::read(&sentinel)?, b"outside");
        assert_eq!(workspace.read_regular(Path::new("target"))?, b"mutated");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replay_workspace_copies_only_regular_sources_and_preserves_permissions() -> anyhow::Result<()>
    {
        use std::os::unix::fs::PermissionsExt;

        let source_tempdir = tempfile::tempdir()?;
        let source_root = CapDir::open_ambient_dir(source_tempdir.path(), ambient_authority())?;
        let source = source_tempdir.path().join("source");
        std::fs::write(&source, b"source bytes")?;
        std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640))?;
        let workspace_tempdir = tempfile::tempdir()?;
        let root = workspace_tempdir.path().to_path_buf();
        let workspace = ReplayWorkspace::open(WorkspaceCopy {
            _tempdir: workspace_tempdir,
            root,
            reset_strategy: WorkspaceResetStrategy::Copy,
        })?;

        workspace.copy_regular_source(&source_root, Path::new("source"))?;
        let copied = workspace.read_regular(Path::new("source"))?;
        assert_eq!(copied, b"source bytes");
        let copied_mode = std::fs::metadata(workspace.root().join("source"))?
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(copied_mode, 0o640);

        let sentinel = source_tempdir.path().join("sentinel");
        std::fs::write(&sentinel, b"outside")?;
        let source_link = source_tempdir.path().join("link");
        std::os::unix::fs::symlink(&sentinel, &source_link)?;
        assert!(
            workspace
                .copy_regular_source(&source_root, Path::new("link"))
                .is_err()
        );
        let source_hardlink = source_tempdir.path().join("hardlink");
        std::fs::hard_link(&sentinel, &source_hardlink)?;
        assert!(
            workspace
                .copy_regular_source(&source_root, Path::new("hardlink"))
                .is_err()
        );
        assert_eq!(std::fs::read(&sentinel)?, b"outside");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replay_workspace_replaces_parent_symlink_inside_clone() -> anyhow::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let root = tempdir.path().to_path_buf();
        let sentinel_dir = tempfile::tempdir()?;
        let sentinel = sentinel_dir.path().join("sentinel");
        std::fs::write(&sentinel, b"outside")?;
        std::os::unix::fs::symlink(sentinel_dir.path(), root.join("alias"))?;
        let workspace = ReplayWorkspace::open(WorkspaceCopy {
            _tempdir: tempdir,
            root,
            reset_strategy: WorkspaceResetStrategy::Copy,
        })?;

        workspace.replace_regular(Path::new("alias/sentinel"), b"mutated")?;

        assert_eq!(std::fs::read(&sentinel)?, b"outside");
        assert_eq!(
            workspace.read_regular(Path::new("alias/sentinel"))?,
            b"mutated"
        );
        Ok(())
    }

    #[cfg(windows)]
    fn create_junction(link: &Path, target: &Path) {
        let status = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .status()
            .expect("failed to spawn mklink /J");
        assert!(status.success(), "mklink /J failed for {}", link.display());
    }

    #[cfg(windows)]
    fn windows_replay_workspace(tempdir: tempfile::TempDir) -> std::io::Result<ReplayWorkspace> {
        let root = tempdir.path().to_path_buf();
        ReplayWorkspace::open(WorkspaceCopy {
            _tempdir: tempdir,
            root,
            reset_strategy: WorkspaceResetStrategy::Copy,
        })
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_directory_overlay_removal_fails_closed_before_spawning() -> anyhow::Result<()>
    {
        if !git_available() {
            return Ok(());
        }
        let (dir, file, mutation) = make_relative_test_setup();
        run_git(dir.path(), &["init"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Togi Test"]);
        // A committed excluded directory forces a directory removal while
        // sanitizing the clone.
        std::fs::create_dir_all(dir.path().join("target"))?;
        std::fs::write(dir.path().join("target/artifact.bin"), b"pinned")?;
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "initial"]);
        let source_revision = git_snapshot_revision(dir.path())?;
        let expected_source_fingerprint = source_fingerprint(&std::fs::read(&file)?);

        let log_dir = tempfile::tempdir()?;
        let log_path = log_dir.path().join("replay.log");
        let cancelled = AtomicBool::new(false);
        let error = run_replay_mutation(
            dir.path(),
            &mutation,
            ReplayRunConfig {
                test_command: appending_log_command(&log_path),
                build_command: None,
                timeout: Duration::from_secs(30),
                env: HashMap::new(),
                respect_workspace_ignores: true,
                source_revision: &source_revision,
                source_fingerprint: &expected_source_fingerprint,
                show_output: false,
                cancelled: &cancelled,
            },
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("race-free directory removal"),
            "diagnostic must name the directory-removal requirement: {message}"
        );
        assert!(
            !log_path.exists(),
            "directory removal must fail before any command spawn"
        );
        assert_eq!(std::fs::read(&file)?, b"hello world");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_dir_to_file_type_change_fails_closed_before_spawning() -> anyhow::Result<()> {
        if !git_available() {
            return Ok(());
        }
        let (dir, file, mutation) = make_relative_test_setup();
        run_git(dir.path(), &["init"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Togi Test"]);
        std::fs::create_dir_all(dir.path().join("slot"))?;
        std::fs::write(dir.path().join("slot/nested.txt"), b"nested")?;
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "initial"]);
        let source_revision = git_snapshot_revision(dir.path())?;
        let expected_source_fingerprint = source_fingerprint(&std::fs::read(&file)?);
        // Working tree replaces the tracked directory with a regular file.
        std::fs::remove_dir_all(dir.path().join("slot"))?;
        std::fs::write(dir.path().join("slot"), b"file")?;

        let log_dir = tempfile::tempdir()?;
        let log_path = log_dir.path().join("replay.log");
        let cancelled = AtomicBool::new(false);
        let error = run_replay_mutation(
            dir.path(),
            &mutation,
            ReplayRunConfig {
                test_command: appending_log_command(&log_path),
                build_command: None,
                timeout: Duration::from_secs(30),
                env: HashMap::new(),
                respect_workspace_ignores: true,
                source_revision: &source_revision,
                source_fingerprint: &expected_source_fingerprint,
                show_output: false,
                cancelled: &cancelled,
            },
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("race-free directory removal"),
            "diagnostic must name the directory-removal requirement: {message}"
        );
        assert!(
            !log_path.exists(),
            "type-change must fail before any command spawn"
        );
        assert_eq!(std::fs::read(&file)?, b"hello world");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_deleted_tracked_directory_fails_closed_before_spawning() -> anyhow::Result<()>
    {
        if !git_available() {
            return Ok(());
        }
        let (dir, file, mutation) = make_relative_test_setup();
        run_git(dir.path(), &["init"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Togi Test"]);
        std::fs::create_dir_all(dir.path().join("gone"))?;
        std::fs::write(dir.path().join("gone/file.txt"), b"nested")?;
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "initial"]);
        let source_revision = git_snapshot_revision(dir.path())?;
        let expected_source_fingerprint = source_fingerprint(&std::fs::read(&file)?);
        // Source deletes the whole tracked directory; the overlay only names
        // the tracked leaf, so faithful replay would require removing the
        // stale clone directory.
        std::fs::remove_dir_all(dir.path().join("gone"))?;

        let log_dir = tempfile::tempdir()?;
        let log_path = log_dir.path().join("replay.log");
        let cancelled = AtomicBool::new(false);
        let error = run_replay_mutation(
            dir.path(),
            &mutation,
            ReplayRunConfig {
                test_command: appending_log_command(&log_path),
                build_command: None,
                timeout: Duration::from_secs(30),
                env: HashMap::new(),
                respect_workspace_ignores: true,
                source_revision: &source_revision,
                source_fingerprint: &expected_source_fingerprint,
                show_output: false,
                cancelled: &cancelled,
            },
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("race-free directory removal"),
            "diagnostic must name the directory-removal requirement: {message}"
        );
        assert!(
            message.contains("gone"),
            "diagnostic must name the stale directory: {message}"
        );
        assert!(
            !log_path.exists(),
            "stale directory removal must fail before any command spawn"
        );
        assert_eq!(std::fs::read(&file)?, b"hello world");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_file_to_dir_type_change_fails_closed_before_spawning() -> anyhow::Result<()> {
        if !git_available() {
            return Ok(());
        }
        let (dir, file, mutation) = make_relative_test_setup();
        run_git(dir.path(), &["init"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Togi Test"]);
        std::fs::write(dir.path().join("slot"), b"tracked file")?;
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "initial"]);
        let source_revision = git_snapshot_revision(dir.path())?;
        let expected_source_fingerprint = source_fingerprint(&std::fs::read(&file)?);
        // Source replaces the tracked file with a directory; mirroring it
        // would require removing the clone-side directory after populate.
        std::fs::remove_file(dir.path().join("slot"))?;
        std::fs::create_dir(dir.path().join("slot"))?;
        std::fs::write(dir.path().join("slot/new.rs"), b"new")?;

        let log_dir = tempfile::tempdir()?;
        let log_path = log_dir.path().join("replay.log");
        let cancelled = AtomicBool::new(false);
        let error = run_replay_mutation(
            dir.path(),
            &mutation,
            ReplayRunConfig {
                test_command: appending_log_command(&log_path),
                build_command: None,
                timeout: Duration::from_secs(30),
                env: HashMap::new(),
                respect_workspace_ignores: true,
                source_revision: &source_revision,
                source_fingerprint: &expected_source_fingerprint,
                show_output: false,
                cancelled: &cancelled,
            },
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("race-free directory removal"),
            "diagnostic must name the directory-removal requirement: {message}"
        );
        assert!(
            !log_path.exists(),
            "file-to-dir type change must fail before any command spawn"
        );
        assert_eq!(std::fs::read(&file)?, b"hello world");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_file_removal_survives_and_keeps_removed_file_absent() -> anyhow::Result<()> {
        if !git_available() {
            return Ok(());
        }
        let (dir, file, mutation) = make_relative_test_setup();
        run_git(dir.path(), &["init"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Togi Test"]);
        std::fs::write(dir.path().join("removed.txt"), b"removed content")?;
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "initial"]);
        let source_revision = git_snapshot_revision(dir.path())?;
        let expected_source_fingerprint = source_fingerprint(&std::fs::read(&file)?);
        std::fs::remove_file(dir.path().join("removed.txt"))?;

        let log_dir = tempfile::tempdir()?;
        let log_path = log_dir.path().join("replay.log");
        let cancelled = AtomicBool::new(false);
        let mut env = HashMap::new();
        env.insert("TOGI_REPLAY_LOG".into(), log_path.display().to_string());
        let outcome = run_replay_mutation(
            dir.path(),
            &mutation,
            ReplayRunConfig {
                test_command: vec![
                    "powershell".into(),
                    "-NoProfile".into(),
                    "-Command".into(),
                    "if (Test-Path -LiteralPath 'removed.txt') { exit 1 }; [System.IO.File]::AppendAllText($env:TOGI_REPLAY_LOG, 'invoked')".into(),
                ],
                build_command: None,
                timeout: Duration::from_secs(30),
                env,
                respect_workspace_ignores: true,
                source_revision: &source_revision,
                source_fingerprint: &expected_source_fingerprint,
                show_output: false,
                cancelled: &cancelled,
            },
        )?;
        assert_eq!(outcome.result, MutationResult::Survived);
        assert_eq!(std::fs::read(&log_path)?, b"invoked");
        assert_eq!(std::fs::read(&file)?, b"hello world");
        assert!(!dir.path().join("removed.txt").exists());
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_clone_target_pins_parent_and_child_and_accepts_git_clone()
    -> anyhow::Result<()> {
        if !git_available() {
            return Ok(());
        }
        let source = tempfile::tempdir()?;
        run_git(source.path(), &["init"]);
        run_git(source.path(), &["config", "user.email", "test@example.com"]);
        run_git(source.path(), &["config", "user.name", "Togi Test"]);
        std::fs::write(source.path().join("committed.txt"), b"committed")?;
        run_git(source.path(), &["add", "."]);
        run_git(source.path(), &["commit", "-m", "initial"]);
        let source_revision = git_snapshot_revision(source.path())?;

        let trusted_root = tempfile::tempdir()?;
        let workspace = ReplayWorkspace::create_clone_target_in(trusted_root.path())?;
        let outer_path = workspace.workspace._tempdir.path().to_path_buf();
        let child_path = workspace.root().to_path_buf();

        // Another process cannot rename the pinned temp root, the pinned
        // outer TempDir, or the pinned clone child, so the path Git and
        // later commands use cannot be rebound.
        for path in [trusted_root.path(), &outer_path, &child_path] {
            let status = std::process::Command::new("cmd")
                .args(["/C", "ren"])
                .arg(path)
                .arg("moved")
                .status()?;
            assert!(!status.success(), "{} must be pinned", path.display());
            assert!(path.is_dir());
        }
        assert!(!outer_path.join("moved").exists());

        // Git accepts the pre-created empty child as the clone destination
        // and checks out the committed content through the pinned path.
        clone_replay_snapshot(source.path(), workspace.root(), &source_revision)?;
        assert_eq!(
            workspace.read_regular(Path::new("committed.txt"))?,
            b"committed"
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_clone_target_rejects_outer_junction_swap_before_git() -> std::io::Result<()> {
        let trusted_root = tempfile::tempdir()?;
        let outside_dir = tempfile::tempdir()?;
        let sentinel = outside_dir.path().join("sentinel.txt");
        std::fs::write(&sentinel, b"outside")?;

        // Simulate a same-user racer: replace the just-created outer TempDir
        // with a junction to an outside directory before the constructor
        // re-opens the outer basename through the pinned trusted root.
        let outside_path = outside_dir.path().to_path_buf();
        set_replay_outer_created_hook(Some(Box::new(move |outer: &Path| {
            let moved = outer.with_file_name("outer-moved");
            std::fs::rename(outer, &moved).unwrap();
            create_junction(outer, &outside_path);
        })));
        let result = ReplayWorkspace::create_clone_target_in(trusted_root.path());
        set_replay_outer_created_hook(None);

        // The no-follow open rejects the junction before any Git subprocess;
        // the outside target is never traversed or modified.
        assert!(result.is_err());
        assert!(trusted_root.path().join("outer-moved").is_dir());
        assert_eq!(std::fs::read(&sentinel)?, b"outside");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_clone_target_rejects_junction_temp_root_before_any_setup()
    -> std::io::Result<()> {
        let trusted_base = tempfile::tempdir()?;
        let outside_dir = tempfile::tempdir()?;
        let sentinel = outside_dir.path().join("sentinel.txt");
        std::fs::write(&sentinel, b"outside")?;
        create_junction(
            &trusted_base.path().join("junction-temp"),
            outside_dir.path(),
        );

        // A configured temp root that is itself a reparse point is rejected
        // while pinning the lexical chain, before any TempDir or Git setup.
        let result =
            ReplayWorkspace::create_clone_target_in(&trusted_base.path().join("junction-temp"));
        assert!(result.is_err());
        assert_eq!(std::fs::read(&sentinel)?, b"outside");
        assert_eq!(std::fs::read_dir(outside_dir.path())?.count(), 1);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_clone_target_rejects_subst_temp_root_before_any_setup() -> std::io::Result<()>
    {
        let safe_target = tempfile::tempdir()?;
        // The test source is itself a normal, accepted system-volume temp
        // directory before it is exposed through a user-defined DOS device.
        ReplayTempRoot::pin(safe_target.path())?;
        let (_, system_drive) = windows_system_volume_root()?;
        let mapping = SubstDrive::allocate(safe_target.path(), system_drive)?;
        let outside_dir = tempfile::tempdir()?;
        let sentinel = outside_dir.path().join("sentinel.txt");
        std::fs::write(&sentinel, b"outside")?;
        assert_eq!(std::fs::read_dir(safe_target.path())?.count(), 0);

        let hook_called = std::rc::Rc::new(std::cell::Cell::new(false));
        let hook_called_clone = std::rc::Rc::clone(&hook_called);
        set_replay_temp_root_ready_hook(Some(Box::new(move || {
            hook_called_clone.set(true);
        })));
        let result = ReplayWorkspace::create_clone_target_in(&mapping.root().join("togi-replay"));
        set_replay_temp_root_ready_hook(None);

        let error = result
            .err()
            .expect("a SUBST-mapped temp root must be rejected");
        assert!(
            error
                .to_string()
                .contains("is not on the Windows system volume")
        );
        assert!(!hook_called.get(), "must reject before temp-root setup");
        assert_eq!(std::fs::read(&sentinel)?, b"outside");
        assert_eq!(std::fs::read_dir(safe_target.path())?.count(), 0);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_temp_root_chain_blocks_external_rebind_then_clone_succeeds()
    -> anyhow::Result<()> {
        if !git_available() {
            return Ok(());
        }
        let source = tempfile::tempdir()?;
        run_git(source.path(), &["init"]);
        run_git(source.path(), &["config", "user.email", "test@example.com"]);
        run_git(source.path(), &["config", "user.name", "Togi Test"]);
        std::fs::write(source.path().join("committed.txt"), b"committed")?;
        run_git(source.path(), &["add", "."]);
        run_git(source.path(), &["commit", "-m", "initial"]);
        let source_revision = git_snapshot_revision(source.path())?;

        let trusted_root = tempfile::tempdir()?;
        let trusted_path = trusted_root.path().to_path_buf();
        // After the chain is pinned but before the outer TempDir is created,
        // a same-user racer cannot rename the pinned temp root or its pinned
        // parent; every lexical component stays bound for Git/current_dir.
        let hook_path = trusted_path.clone();
        set_replay_temp_root_ready_hook(Some(Box::new(move || {
            let rename = |path: &Path, name: &str| {
                std::process::Command::new("cmd")
                    .args(["/C", "ren"])
                    .arg(path)
                    .arg(name)
                    .status()
                    .expect("spawn ren")
            };
            assert!(!rename(&hook_path, "moved").success());
            let parent = hook_path.parent().unwrap().to_path_buf();
            assert!(!rename(&parent, "moved-parent").success());
        })));
        let workspace = ReplayWorkspace::create_clone_target_in(trusted_root.path())?;
        set_replay_temp_root_ready_hook(None);
        assert!(trusted_path.is_dir());

        clone_replay_snapshot(source.path(), workspace.root(), &source_revision)?;
        assert_eq!(
            workspace.read_regular(Path::new("committed.txt"))?,
            b"committed"
        );
        Ok(())
    }

    #[test]
    fn replay_clone_target_contains_ordinary_outer_replacement_in_trusted_root()
    -> std::io::Result<()> {
        let trusted_root = tempfile::tempdir()?;
        let outside_dir = tempfile::tempdir()?;
        let sentinel = outside_dir.path().join("sentinel.txt");
        std::fs::write(&sentinel, b"outside")?;

        // The residual create-to-open window can substitute an ordinary
        // directory (not a reparse point) for the outer TempDir. That
        // replacement stays inside the trusted temp root; nothing escapes
        // and no atomic-identity claim is made.
        set_replay_outer_created_hook(Some(Box::new(|outer: &Path| {
            let moved = outer.with_file_name("outer-moved");
            std::fs::rename(outer, &moved).unwrap();
            std::fs::create_dir(outer).unwrap();
        })));
        let workspace = ReplayWorkspace::create_clone_target_in(trusted_root.path())?;
        set_replay_outer_created_hook(None);

        assert!(trusted_root.path().join("outer-moved").is_dir());
        assert!(workspace.root().starts_with(trusted_root.path()));
        assert!(workspace.root().is_dir());
        assert_eq!(std::fs::read_dir(workspace.root())?.count(), 0);
        assert_eq!(std::fs::read(&sentinel)?, b"outside");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_cleanup_removes_workspace_without_traversing_junction() -> std::io::Result<()>
    {
        let outside_dir = tempfile::tempdir()?;
        let sentinel = outside_dir.path().join("sentinel.txt");
        std::fs::write(&sentinel, b"outside")?;

        let workspace_tempdir = tempfile::tempdir()?;
        let temp_root = workspace_tempdir.path().to_path_buf();
        let root = temp_root.join("workspace");
        std::fs::create_dir(&root)?;
        std::fs::write(root.join("file.txt"), b"clone")?;
        create_junction(&root.join("alias"), outside_dir.path());
        {
            let _workspace = ReplayWorkspace::open(WorkspaceCopy {
                _tempdir: workspace_tempdir,
                root: root.clone(),
                reset_strategy: WorkspaceResetStrategy::Copy,
            })?;
        }

        // Normal TempDir cleanup removes the disposable clone; std's Windows
        // removal is handle-relative with no-reparse opens, so the junction is
        // removed as a reparse point and its outside target is never touched.
        assert!(!temp_root.exists());
        assert_eq!(std::fs::read(&sentinel)?, b"outside");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_remove_relative_removes_junction_leaf_without_touching_target()
    -> std::io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let root = tempdir.path().to_path_buf();
        let outside_dir = tempfile::tempdir()?;
        let sentinel = outside_dir.path().join("sentinel.txt");
        std::fs::write(&sentinel, b"outside")?;
        create_junction(&root.join("alias"), outside_dir.path());
        let workspace = windows_replay_workspace(tempdir)?;

        workspace.remove_relative(Path::new("alias"))?;

        assert!(std::fs::symlink_metadata(root.join("alias")).is_err());
        assert_eq!(std::fs::read(&sentinel)?, b"outside");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_remove_relative_never_traverses_midpath_junction() -> std::io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let root = tempdir.path().to_path_buf();
        let outside_dir = tempfile::tempdir()?;
        let sentinel = outside_dir.path().join("sentinel.txt");
        std::fs::write(&sentinel, b"outside")?;
        create_junction(&root.join("alias"), outside_dir.path());
        let workspace = windows_replay_workspace(tempdir)?;

        // The junction mid-path component is removed as a reparse point, never
        // opened, so the outside target keeps its contents.
        workspace.remove_relative(Path::new("alias/sentinel.txt"))?;

        assert!(std::fs::symlink_metadata(root.join("alias")).is_err());
        assert_eq!(std::fs::read(&sentinel)?, b"outside");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_publish_replaces_midpath_junction_inside_clone() -> std::io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let root = tempdir.path().to_path_buf();
        let outside_dir = tempfile::tempdir()?;
        let sentinel = outside_dir.path().join("sentinel.txt");
        std::fs::write(&sentinel, b"outside")?;
        create_junction(&root.join("alias"), outside_dir.path());
        let workspace = windows_replay_workspace(tempdir)?;

        workspace.replace_regular(Path::new("alias/sentinel.txt"), b"mutated")?;

        assert_eq!(std::fs::read(&sentinel)?, b"outside");
        assert_eq!(
            workspace.read_regular(Path::new("alias/sentinel.txt"))?,
            b"mutated"
        );
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_remove_relative_removes_file_symlink_leaf() -> std::io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let root = tempdir.path().to_path_buf();
        let outside_dir = tempfile::tempdir()?;
        let sentinel = outside_dir.path().join("sentinel.txt");
        std::fs::write(&sentinel, b"outside")?;
        if std::os::windows::fs::symlink_file(&sentinel, root.join("alias")).is_err() {
            eprintln!("skipping: symlink creation requires SeCreateSymbolicLinkPrivilege");
            return Ok(());
        }
        let workspace = windows_replay_workspace(tempdir)?;

        workspace.remove_relative(Path::new("alias"))?;

        assert!(std::fs::symlink_metadata(root.join("alias")).is_err());
        assert_eq!(std::fs::read(&sentinel)?, b"outside");
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn replay_windows_held_parent_capability_blocks_external_rename_but_allows_leaf_rename()
    -> std::io::Result<()> {
        let tempdir = tempfile::tempdir()?;
        let parent = tempdir.path().join("parent");
        std::fs::create_dir(&parent)?;
        std::fs::write(parent.join("leaf.txt"), b"leaf")?;
        // cap-primitives opens directories without FILE_SHARE_DELETE.
        let _held = CapDir::open_ambient_dir(&parent, ambient_authority())?;

        // Another process cannot rename the pinned parent out from under us.
        let status = std::process::Command::new("cmd")
            .args(["/C", "ren"])
            .arg(&parent)
            .arg("moved")
            .status()?;
        assert!(!status.success(), "held parent must not be renamable");
        assert!(parent.is_dir());
        assert!(!tempdir.path().join("moved").exists());

        // A leaf beneath the pinned parent remains freely renamable.
        std::fs::rename(parent.join("leaf.txt"), parent.join("renamed.txt"))?;
        assert_eq!(std::fs::read(parent.join("renamed.txt"))?, b"leaf");
        Ok(())
    }

    #[test]
    fn replay_runner_uses_only_workspace_and_validates_original_bytes() -> anyhow::Result<()> {
        let (dir, file, mutation) = make_relative_test_setup();
        if !git_available() {
            return Ok(());
        }
        run_git(dir.path(), &["init"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Togi Test"]);
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "initial"]);
        let source_revision = git_snapshot_revision(dir.path())?;
        let expected_source_fingerprint = source_fingerprint(&std::fs::read(&file)?);
        let cache_dir = dir.path().join(".togi-cache");
        std::fs::create_dir_all(&cache_dir)?;
        let history_path = cache_dir.join("history.json");
        std::fs::write(&history_path, "sentinel history")?;
        let cache_before = std::fs::read(&history_path)?;
        let log_dir = tempfile::tempdir()?;
        // An apostrophe in the log path exercises the shell-quoting of the
        // platform log command (PowerShell single-quote doubling on Windows).
        let log_path = log_dir.path().join("re'play.log");
        let cancelled = AtomicBool::new(false);
        let config = ReplayRunConfig {
            test_command: appending_log_command(&log_path),
            build_command: None,
            timeout: Duration::from_secs(30),
            env: HashMap::new(),
            respect_workspace_ignores: true,
            source_revision: &source_revision,
            source_fingerprint: &expected_source_fingerprint,
            show_output: false,
            cancelled: &cancelled,
        };

        let outcome = run_replay_mutation(dir.path(), &mutation, config)?;
        assert_eq!(outcome.result, MutationResult::Survived);
        assert_eq!(std::fs::read(&log_path)?, b"invoked");
        assert_eq!(std::fs::read(&file)?, b"hello world");
        assert_eq!(std::fs::read(&history_path)?, cache_before);
        assert!(!dir.path().join(".togi.lock").exists());

        let mut mismatched = mutation;
        mismatched.original = "nope".into();
        let rejected_log = log_dir.path().join("rejected.log");
        let rejected = run_replay_mutation(
            dir.path(),
            &mismatched,
            ReplayRunConfig {
                test_command: appending_log_command(&rejected_log),
                build_command: None,
                timeout: Duration::from_secs(30),
                env: HashMap::new(),
                respect_workspace_ignores: true,
                source_revision: &source_revision,
                source_fingerprint: &expected_source_fingerprint,
                show_output: false,
                cancelled: &cancelled,
            },
        );
        assert!(rejected.is_err());
        assert!(!rejected_log.exists());
        assert_eq!(std::fs::read(&file)?, b"hello world");
        assert_eq!(std::fs::read(&history_path)?, cache_before);
        Ok(())
    }

    #[test]
    fn replay_runner_requires_git_snapshot_before_spawning_commands() -> anyhow::Result<()> {
        let (dir, _file, mutation) = make_relative_test_setup();
        let log_dir = tempfile::tempdir()?;
        let log_path = log_dir.path().join("replay.log");
        let cancelled = AtomicBool::new(false);

        let result = run_replay_mutation(
            dir.path(),
            &mutation,
            ReplayRunConfig {
                test_command: appending_log_command(&log_path),
                build_command: None,
                timeout: Duration::from_secs(30),
                env: HashMap::new(),
                respect_workspace_ignores: true,
                source_revision: "expected",
                source_fingerprint: "expected",
                show_output: false,
                cancelled: &cancelled,
            },
        );
        assert!(result.is_err());
        assert!(!log_path.exists());
        Ok(())
    }

    #[test]
    fn replay_runner_rejects_snapshot_fingerprint_mismatch_before_spawning_commands()
    -> anyhow::Result<()> {
        if !git_available() {
            return Ok(());
        }

        let (dir, file, mutation) = make_relative_test_setup();
        run_git(dir.path(), &["init"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Togi Test"]);
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "initial"]);
        let source_revision = git_snapshot_revision(dir.path())?;
        let expected_source_fingerprint = source_fingerprint(&std::fs::read(&file)?);
        std::fs::write(&file, b"changed source")?;

        let log_dir = tempfile::tempdir()?;
        let log_path = log_dir.path().join("replay.log");
        let cancelled = AtomicBool::new(false);
        let result = run_replay_mutation(
            dir.path(),
            &mutation,
            ReplayRunConfig {
                test_command: appending_log_command(&log_path),
                build_command: None,
                timeout: Duration::from_secs(30),
                env: HashMap::new(),
                respect_workspace_ignores: true,
                source_revision: &source_revision,
                source_fingerprint: &expected_source_fingerprint,
                show_output: false,
                cancelled: &cancelled,
            },
        );
        assert!(result.is_err());
        assert!(!log_path.exists());
        assert_eq!(std::fs::read(&file)?, b"changed source");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replay_overlay_replaces_tracked_symlink_without_touching_source_target() -> anyhow::Result<()>
    {
        if !git_available() {
            return Ok(());
        }

        let (dir, file, mutation) = make_relative_test_setup();
        run_git(dir.path(), &["init"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Togi Test"]);
        let victim = dir.path().join(".togi-cache/history.json");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&victim, &link)?;
        let nested = dir.path().join("nested");
        std::os::unix::fs::symlink(victim.parent().unwrap(), &nested)?;
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "initial"]);
        let source_revision = git_snapshot_revision(dir.path())?;
        let expected_source_fingerprint = source_fingerprint(&std::fs::read(&file)?);

        std::fs::create_dir_all(victim.parent().unwrap())?;
        std::fs::write(&victim, b"source cache sentinel")?;
        std::fs::remove_file(&link)?;
        std::fs::write(&link, b"dirty payload")?;
        std::fs::remove_file(&nested)?;
        std::fs::create_dir(&nested)?;
        std::fs::write(nested.join("payload"), b"nested dirty payload")?;

        let log_dir = tempfile::tempdir()?;
        let log_path = log_dir.path().join("replay.log");
        let cancelled = AtomicBool::new(false);
        let outcome = run_replay_mutation(
            dir.path(),
            &mutation,
            ReplayRunConfig {
                test_command: vec![
                    "sh".into(),
                    "-c".into(),
                    "printf invoked >> \"$1\"".into(),
                    "replay".into(),
                    log_path.display().to_string(),
                ],
                build_command: None,
                timeout: Duration::from_secs(1),
                env: HashMap::new(),
                respect_workspace_ignores: true,
                source_revision: &source_revision,
                source_fingerprint: &expected_source_fingerprint,
                show_output: false,
                cancelled: &cancelled,
            },
        )?;
        assert_eq!(outcome.result, MutationResult::Survived);
        assert_eq!(std::fs::read(&log_path)?, b"invoked");
        assert_eq!(std::fs::read(&victim)?, b"source cache sentinel");
        assert_eq!(std::fs::read(&link)?, b"dirty payload");
        assert_eq!(
            std::fs::read(nested.join("payload"))?,
            b"nested dirty payload"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replay_overlay_skips_untracked_source_symlink_to_cache() -> anyhow::Result<()> {
        if !git_available() {
            return Ok(());
        }

        let (dir, file, mutation) = make_relative_test_setup();
        run_git(dir.path(), &["init"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Togi Test"]);
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "initial"]);
        let source_revision = git_snapshot_revision(dir.path())?;
        let expected_source_fingerprint = source_fingerprint(&std::fs::read(&file)?);

        let cached_source = dir.path().join(".togi-cache/history.json");
        std::fs::create_dir_all(cached_source.parent().unwrap())?;
        std::fs::write(&cached_source, b"source cache sentinel")?;
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&cached_source, &alias)?;
        let replay_overlay = collect_replay_git_worktree_overlay(dir.path())?;
        assert!(
            replay_overlay
                .copy_paths
                .iter()
                .all(|relative| relative != Path::new("alias"))
        );

        let log_dir = tempfile::tempdir()?;
        let log_path = log_dir.path().join("replay.log");
        let cancelled = AtomicBool::new(false);
        let outcome = run_replay_mutation(
            dir.path(),
            &mutation,
            ReplayRunConfig {
                test_command: vec![
                    "sh".into(),
                    "-c".into(),
                    "test ! -e alias && printf invoked >> \"$1\"".into(),
                    "replay".into(),
                    log_path.display().to_string(),
                ],
                build_command: None,
                timeout: Duration::from_secs(1),
                env: HashMap::new(),
                respect_workspace_ignores: true,
                source_revision: &source_revision,
                source_fingerprint: &expected_source_fingerprint,
                show_output: false,
                cancelled: &cancelled,
            },
        )?;
        assert_eq!(outcome.result, MutationResult::Survived);
        assert_eq!(std::fs::read(&log_path)?, b"invoked");
        assert_eq!(std::fs::read(&cached_source)?, b"source cache sentinel");
        assert!(std::fs::symlink_metadata(&alias)?.file_type().is_symlink());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn replay_overlay_skips_source_parent_symlink_to_cache() -> anyhow::Result<()> {
        if !git_available() {
            return Ok(());
        }

        let (dir, file, mutation) = make_relative_test_setup();
        let replaced_dir = dir.path().join("dir");
        std::fs::create_dir(&replaced_dir)?;
        std::fs::write(replaced_dir.join("file"), b"tracked source")?;
        run_git(dir.path(), &["init"]);
        run_git(dir.path(), &["config", "user.email", "test@example.com"]);
        run_git(dir.path(), &["config", "user.name", "Togi Test"]);
        run_git(dir.path(), &["add", "."]);
        run_git(dir.path(), &["commit", "-m", "initial"]);
        let source_revision = git_snapshot_revision(dir.path())?;
        let expected_source_fingerprint = source_fingerprint(&std::fs::read(&file)?);

        let cached_source = dir.path().join(".togi-cache/file");
        std::fs::create_dir_all(cached_source.parent().unwrap())?;
        std::fs::write(&cached_source, b"source cache sentinel")?;
        std::fs::remove_dir_all(&replaced_dir)?;
        std::os::unix::fs::symlink(".togi-cache", &replaced_dir)?;
        assert!(!replay_source_relative_file_is_regular(
            dir.path(),
            Path::new("dir/file")
        )?);
        let overlay = collect_replay_git_worktree_overlay(dir.path())?;
        assert!(
            overlay
                .copy_paths
                .iter()
                .all(|relative| !relative.starts_with("dir"))
        );

        let log_dir = tempfile::tempdir()?;
        let log_path = log_dir.path().join("replay.log");
        let cancelled = AtomicBool::new(false);
        let outcome = run_replay_mutation(
            dir.path(),
            &mutation,
            ReplayRunConfig {
                test_command: vec![
                    "sh".into(),
                    "-c".into(),
                    "test ! -e dir/file && printf invoked >> \"$1\"".into(),
                    "replay".into(),
                    log_path.display().to_string(),
                ],
                build_command: None,
                timeout: Duration::from_secs(1),
                env: HashMap::new(),
                respect_workspace_ignores: true,
                source_revision: &source_revision,
                source_fingerprint: &expected_source_fingerprint,
                show_output: false,
                cancelled: &cancelled,
            },
        )?;
        assert_eq!(outcome.result, MutationResult::Survived);
        assert_eq!(std::fs::read(&log_path)?, b"invoked");
        assert_eq!(std::fs::read(&cached_source)?, b"source cache sentinel");
        assert!(
            std::fs::symlink_metadata(&replaced_dir)?
                .file_type()
                .is_symlink()
        );
        Ok(())
    }

    #[test]
    fn regular_direct_recipes_cover_fresh_cache_history_and_schema_fallback() -> anyhow::Result<()>
    {
        for (reuse, expected_origin) in [
            (None, DirectRecipeOrigin::Executed),
            (
                Some(ReuseSource::ExactCache),
                DirectRecipeOrigin::ExactCache,
            ),
            (
                Some(ReuseSource::IncrementalHistory),
                DirectRecipeOrigin::IncrementalHistory,
            ),
        ] {
            let (dir, _file, mutation) = make_relative_test_setup();
            let mut commands = test_command_config();
            commands.command = successful_command();
            if let Some(reuse) = reuse {
                seed_reused_survivor(dir.path(), &commands, &mutation, reuse)?;
            }
            let runner = TestRunner {
                commands,
                parallelism: 1,
                project_root: dir.path().to_path_buf(),
                verbose: false,
                show_output: false,
                max_tested: None,
                early_stop: EarlyStopConfig::default(),
                respect_workspace_ignores: true,
                env: HashMap::new(),
                incremental_history: true,
                force_rerun: false,
                learned_selection: false,
                cancelled: Arc::new(AtomicBool::new(false)),
            };
            let outcome = runner.run(vec![mutation.clone()]);
            let recipe = outcome
                .replay_recipes
                .get(&mutation.id)
                .expect("regular result should capture a direct replay recipe");
            assert_eq!(recipe.origin, expected_origin);
            assert_eq!(recipe.test_command, successful_command());
            assert_eq!(recipe.build_command, None);
            assert_eq!(recipe.build_command_origin, BuildCommandOrigin::None);
        }

        let (dir, _file, mut mutation) = make_relative_test_setup();
        mutation.language = "unsupported".into();
        let mut commands = test_command_config();
        commands.command = successful_command();
        let runner = TestRunner {
            commands,
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: EarlyStopConfig::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: false,
            force_rerun: true,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let outcome = runner.run_with_schemata(vec![mutation.clone()]);
        assert!(outcome.report.schemata.is_some());
        assert_eq!(
            outcome
                .replay_recipes
                .get(&mutation.id)
                .map(|recipe| &recipe.origin),
            Some(&DirectRecipeOrigin::Executed)
        );
        Ok(())
    }

    #[cfg(not(windows))]
    #[test]
    fn direct_recipe_omits_sandboxed_auto_build_suggestion() -> anyhow::Result<()> {
        let (dir, _file, mutation) = make_relative_test_setup();
        let mut commands = test_command_config();
        commands.command = successful_command();
        commands.build_command = successful_command();
        commands.build_command_origin = BuildCommandOrigin::AutoDetected;
        commands.sandbox_command = vec!["env".into()];
        let mut expected_command = commands.sandbox_command.clone();
        expected_command.extend(successful_command());
        let runner = TestRunner {
            commands,
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: EarlyStopConfig::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: false,
            force_rerun: true,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let outcome = runner.run(vec![mutation.clone()]);
        let recipe = outcome
            .replay_recipes
            .get(&mutation.id)
            .expect("regular result should capture a direct replay recipe");
        assert_eq!(recipe.test_command, expected_command);
        assert_eq!(recipe.build_command, None);
        assert_eq!(recipe.build_command_origin, BuildCommandOrigin::None);
        Ok(())
    }

    fn go_operator_mutation(id: u32, file: &str, source: &str, nth: usize) -> Mutation {
        let mut offset = 0usize;
        for index in 0..=nth {
            let next = source[offset..]
                .find("==")
                .expect("source should contain ==");
            offset += next;
            if index == nth {
                break;
            }
            offset += 2;
        }
        Mutation {
            id,
            file: PathBuf::from(file),
            language: "go".into(),
            line: 1,
            column: 1,
            operator: "eq_to_neq".into(),
            description: "Replace == with !=".into(),
            original: "==".into(),
            replacement: "!=".into(),
            byte_range: offset..offset + 2,
        }
    }

    #[derive(Clone, Copy)]
    enum ReuseSource {
        ExactCache,
        IncrementalHistory,
    }

    fn seed_reused_survivor(
        project_root: &Path,
        commands: &CommandConfig,
        mutation: &Mutation,
        reuse_source: ReuseSource,
    ) -> anyhow::Result<()> {
        seed_reused_result(
            project_root,
            commands,
            mutation,
            reuse_source,
            MutationResult::Survived,
        )
    }

    fn seed_reused_result(
        project_root: &Path,
        commands: &CommandConfig,
        mutation: &Mutation,
        reuse_source: ReuseSource,
        result: MutationResult,
    ) -> anyhow::Result<()> {
        seed_reused_result_with_env(
            project_root,
            commands,
            mutation,
            reuse_source,
            result,
            &HashMap::new(),
        )
    }

    fn seed_reused_survivor_with_env(
        project_root: &Path,
        commands: &CommandConfig,
        mutation: &Mutation,
        reuse_source: ReuseSource,
        env: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        seed_reused_result_with_env(
            project_root,
            commands,
            mutation,
            reuse_source,
            MutationResult::Survived,
            env,
        )
    }

    fn seed_reused_result_with_env(
        project_root: &Path,
        commands: &CommandConfig,
        mutation: &Mutation,
        reuse_source: ReuseSource,
        result: MutationResult,
        env: &HashMap<String, String>,
    ) -> anyhow::Result<()> {
        let selected = select_test_command(project_root, commands, mutation);
        let command_context = selected.cache_context(
            &commands.build_command,
            commands.build_command_origin,
            &commands.sandbox_command,
            env,
        );
        let context_hash = cache_context_fingerprint(project_root);
        let source_path = if mutation.file.is_absolute() {
            mutation.file.clone()
        } else {
            project_root.join(&mutation.file)
        };
        let source = std::fs::read(source_path)?;

        match reuse_source {
            ReuseSource::ExactCache => {
                let key = CacheKey::new(
                    &source,
                    &cache_identity(project_root, mutation),
                    &mutation.description,
                    &format!("{command_context};context={context_hash:016x}"),
                );
                cache::store(project_root, &key, result);
            }
            ReuseSource::IncrementalHistory => {
                let test_context_index = TestContextIndex::build(project_root);
                let query = incremental_history_query(
                    project_root,
                    mutation,
                    &source,
                    &command_context,
                    test_context_index
                        .fingerprint_for_tests(&selected.selected_tests, context_hash),
                );
                cache::IncrementalHistoryStore::load(project_root).record(
                    cache::IncrementalHistoryEntry {
                        mutation_identity: query.mutation_identity,
                        mutation_description: query.mutation_description,
                        result,
                        source_hash: query.source_hash,
                        command_hash: query.command_hash,
                        relevant_test_hash: query.relevant_test_hash,
                        covering_tests: vec![],
                        killer_test: None,
                    },
                );
            }
        }

        Ok(())
    }

    #[cfg(unix)]
    fn first_run_survives_second_kills_command(state_dir: &Path) -> Vec<String> {
        vec![
            "sh".into(),
            "-c".into(),
            r#"
state_dir=$1
runs=0
if [ -f "$state_dir/runs" ]; then runs=$(cat "$state_dir/runs"); fi
runs=$((runs + 1))
printf '%s\n' "$runs" > "$state_dir/runs"
test "$runs" -eq 1
"#
            .into(),
            "state".into(),
            state_dir.display().to_string(),
        ]
    }

    #[cfg(unix)]
    fn fake_selection_command(
        project_root: &Path,
        command_name: &str,
        narrowed_marker: &str,
        full_status: i32,
    ) -> (HashMap<String, String>, PathBuf) {
        use std::os::unix::fs::PermissionsExt;

        let bin = project_root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let command = bin.join(command_name);
        let log = project_root.join("confirmation-command.log");
        std::fs::write(
            &command,
            format!(
                "#!/bin/sh\nprintf '<%s>\\n' \"$*\" >> \"$TOGI_CONFIRMATION_LOG\"\ncase \" $* \" in\n  *\" {narrowed_marker} \"*) exit 0 ;;\n  *) exit {full_status} ;;\nesac\n"
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&command).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&command, permissions).unwrap();

        let mut env = HashMap::new();
        env.insert(
            "PATH".into(),
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        env.insert("TOGI_CONFIRMATION_LOG".into(), log.display().to_string());
        (env, log)
    }

    #[cfg(unix)]
    fn confirmation_runner(
        project_root: &Path,
        commands: CommandConfig,
        env: HashMap<String, String>,
    ) -> TestRunner {
        TestRunner {
            commands,
            parallelism: 1,
            project_root: project_root.to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: EarlyStopConfig::default(),
            respect_workspace_ignores: true,
            env,
            incremental_history: false,
            force_rerun: true,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn c_operator_mutation(id: u32, file: &str, source: &str, nth: usize) -> Mutation {
        let mut mutation = go_operator_mutation(id, file, source, nth);
        mutation.language = "c".into();
        mutation
    }

    fn cpp_operator_mutation(id: u32, file: &str, source: &str, nth: usize) -> Mutation {
        let mut mutation = go_operator_mutation(id, file, source, nth);
        mutation.language = "cpp".into();
        mutation
    }

    fn rust_operator_mutation(id: u32, file: &str, source: &str, nth: usize) -> Mutation {
        let mut mutation = go_operator_mutation(id, file, source, nth);
        mutation.language = "rust".into();
        mutation
    }

    fn java_operator_mutation(id: u32, file: &str, source: &str, nth: usize) -> Mutation {
        let mut mutation = go_operator_mutation(id, file, source, nth);
        mutation.language = "java".into();
        mutation
    }

    #[test]
    fn cache_context_fingerprint_changes_for_tests_and_config() {
        let dir = tempfile::tempdir().unwrap();
        let initial = cache_context_fingerprint(dir.path());

        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            b"pub fn value() -> i32 { 1 }",
        )
        .unwrap();
        assert_ne!(
            cache_context_fingerprint(dir.path()),
            initial,
            "source files are verdict context: they shape test compilation and module wiring (#410)"
        );

        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        let test_file = dir.path().join("tests/calc_test.go");
        std::fs::write(&test_file, b"package calc\nfunc TestValue() {}\n").unwrap();
        let with_test = cache_context_fingerprint(dir.path());
        assert_ne!(with_test, initial);

        std::fs::write(&test_file, b"package calc\nfunc TestValueChanged() {}\n").unwrap();
        let changed_test = cache_context_fingerprint(dir.path());
        assert_ne!(changed_test, with_test);

        std::fs::write(dir.path().join("togi.toml"), b"[test]\ntimeout = 10\n").unwrap();
        assert_ne!(cache_context_fingerprint(dir.path()), changed_test);
    }

    #[test]
    fn source_content_cache_reads_each_file_once_lazily() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("src.txt");
        std::fs::write(&file, b"hello world").unwrap();

        let mut first = make_test_mutation(&file);
        first.id = 1;
        let mut second = make_test_mutation(&file);
        second.id = 2;
        second.byte_range = 6..11;

        let cache = SourceContentCache::default();

        assert_eq!(cache.cached_entry_count(), 0);
        assert_eq!(
            cache.content_for(dir.path(), &first.file).unwrap(),
            b"hello world"
        );
        assert_eq!(
            cache.content_for(dir.path(), &second.file).unwrap(),
            b"hello world"
        );
        assert_eq!(cache.cached_entry_count(), 1);
    }

    #[test]
    fn git_cache_context_uses_index_when_context_is_clean() {
        if !git_available() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init"]);
        run_git(root, &["config", "user.email", "test@example.com"]);
        run_git(root, &["config", "user.name", "Test"]);

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("Cargo.toml"), b"[package]\nname = \"fixture\"\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub fn value() -> i32 { 1 }\n").unwrap();
        std::fs::write(
            root.join("tests/value_test.rs"),
            b"#[test]\nfn value() {}\n",
        )
        .unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "initial"]);

        let clean = git_cache_context_fingerprint(root).expect("clean git fingerprint");
        std::fs::write(root.join("src/lib.rs"), b"pub fn value() -> i32 { 2 }\n").unwrap();
        assert!(
            git_cache_context_fingerprint(root).is_none(),
            "dirty source files should fall back to filesystem hashing"
        );
        assert_ne!(
            cache_context_fingerprint(root),
            clean,
            "source edits must change the cache context fingerprint"
        );

        std::fs::write(
            root.join("tests/value_test.rs"),
            b"#[test]\nfn changed() {}\n",
        )
        .unwrap();
        assert!(
            git_cache_context_fingerprint(root).is_none(),
            "dirty test files should fall back to filesystem hashing"
        );
    }

    #[test]
    fn cache_context_fingerprint_changes_for_new_source_file() {
        if !git_available() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init"]);
        run_git(root, &["config", "user.email", "test@example.com"]);
        run_git(root, &["config", "user.name", "Test"]);

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), b"[package]\nname = \"fixture\"\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub fn value() -> i32 { 1 }\n").unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "initial"]);

        let before = cache_context_fingerprint(root);
        // A brand-new source file joining the diff (tracked via `git add -N`)
        // must invalidate verdicts cached before it existed (#410).
        std::fs::write(root.join("src/codec.rs"), b"pub fn decode() -> i32 { 1 }\n").unwrap();
        assert_ne!(cache_context_fingerprint(root), before);
    }

    #[test]
    fn cache_context_fingerprint_changes_when_src_test_module_edited() {
        if !git_available() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run_git(root, &["init"]);
        run_git(root, &["config", "user.email", "test@example.com"]);
        run_git(root, &["config", "user.name", "Test"]);

        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), b"[package]\nname = \"fixture\"\n").unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            b"pub fn value() -> i32 { 1 }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn value() {\n        assert_eq!(super::value(), 1);\n    }\n}\n",
        )
        .unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "initial"]);

        let before = cache_context_fingerprint(root);
        // Tests colocated in a source file are verdict context too (#410).
        std::fs::write(
            root.join("src/lib.rs"),
            b"pub fn value() -> i32 { 1 }\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn value() {\n        assert_eq!(super::value(), 2);\n    }\n}\n",
        )
        .unwrap();
        assert_ne!(cache_context_fingerprint(root), before);
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    fn run_git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap_or_else(|e| panic!("failed to run git {args:?}: {e}"));
        assert!(
            output.status.success(),
            "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_clean_git_fixture(root: &Path) {
        run_git(root, &["init"]);
        run_git(root, &["config", "user.email", "test@example.com"]);
        run_git(root, &["config", "user.name", "Test"]);
        run_git(root, &["config", "core.autocrlf", "false"]);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub fn f() {}\n").unwrap();
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "initial"]);
    }

    fn test_command_config() -> CommandConfig {
        CommandConfig {
            command: vec!["cargo".into(), "test".into()],
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec![],
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::None,
            timeout: Duration::from_secs(30),
            language_timeouts: HashMap::new(),
            test_selection: None,
        }
    }

    #[test]
    fn select_test_command_uses_default_command_and_timeout() {
        let commands = test_command_config();
        let mutation = make_test_mutation(Path::new("src/lib.rs"));

        let selected = select_test_command(Path::new("/repo"), &commands, &mutation);

        assert_eq!(selected.argv, vec!["cargo", "test"]);
        assert_eq!(selected.timeout, Duration::from_secs(30));
    }

    #[test]
    fn select_test_command_uses_language_command_and_timeout() {
        let mut commands = test_command_config();
        commands.language_commands.insert(
            "go".into(),
            vec!["go".into(), "test".into(), "./...".into()],
        );
        commands
            .language_timeouts
            .insert("go".into(), Duration::from_secs(5));
        let mut mutation = make_test_mutation(Path::new("calc.go"));
        mutation.language = "go".into();

        let selected = select_test_command(Path::new("/repo"), &commands, &mutation);

        assert_eq!(selected.argv, vec!["go", "test", "./..."]);
        assert_eq!(selected.timeout, Duration::from_secs(5));
    }

    #[test]
    fn select_test_command_uses_project_command_and_timeout() {
        let mut commands = test_command_config();
        commands.project_commands.push(ProjectCommandConfig {
            path: PathBuf::from("services/api"),
            command: Some(vec![
                "cargo".into(),
                "test".into(),
                "-p".into(),
                "api".into(),
            ]),
            timeout: Some(Duration::from_secs(12)),
        });
        let mutation = make_test_mutation(Path::new("services/api/src/lib.rs"));

        let selected = select_test_command(Path::new("/repo"), &commands, &mutation);

        assert_eq!(selected.argv, vec!["cargo", "test", "-p", "api"]);
        assert_eq!(selected.timeout, Duration::from_secs(12));
    }

    #[test]
    fn select_test_command_project_longest_prefix_wins() {
        let mut commands = test_command_config();
        commands.project_commands.push(ProjectCommandConfig {
            path: PathBuf::from("services"),
            command: Some(vec!["make".into(), "test-services".into()]),
            timeout: Some(Duration::from_secs(10)),
        });
        commands.project_commands.push(ProjectCommandConfig {
            path: PathBuf::from("services/api"),
            command: Some(vec!["make".into(), "test-api".into()]),
            timeout: Some(Duration::from_secs(20)),
        });
        let mutation = make_test_mutation(Path::new("services/api/src/lib.rs"));

        let selected = select_test_command(Path::new("/repo"), &commands, &mutation);

        assert_eq!(selected.argv, vec!["make", "test-api"]);
        assert_eq!(selected.timeout, Duration::from_secs(20));
    }

    #[test]
    fn select_test_command_project_overrides_language() {
        let mut commands = test_command_config();
        commands.language_commands.insert(
            "go".into(),
            vec!["go".into(), "test".into(), "./...".into()],
        );
        commands
            .language_timeouts
            .insert("go".into(), Duration::from_secs(5));
        commands.project_commands.push(ProjectCommandConfig {
            path: PathBuf::from("services/api"),
            command: Some(vec![
                "go".into(),
                "test".into(),
                "./services/api/...".into(),
            ]),
            timeout: Some(Duration::from_secs(9)),
        });
        let mut mutation = make_test_mutation(Path::new("services/api/calc.go"));
        mutation.language = "go".into();

        let selected = select_test_command(Path::new("/repo"), &commands, &mutation);

        assert_eq!(selected.argv, vec!["go", "test", "./services/api/..."]);
        assert_eq!(selected.timeout, Duration::from_secs(9));
    }

    #[test]
    fn select_test_command_project_timeout_can_override_language_command() {
        let mut commands = test_command_config();
        commands.language_commands.insert(
            "go".into(),
            vec!["go".into(), "test".into(), "./...".into()],
        );
        commands
            .language_timeouts
            .insert("go".into(), Duration::from_secs(5));
        commands.project_commands.push(ProjectCommandConfig {
            path: PathBuf::from("services/api"),
            command: None,
            timeout: Some(Duration::from_secs(11)),
        });
        let mut mutation = make_test_mutation(Path::new("services/api/calc.go"));
        mutation.language = "go".into();

        let selected = select_test_command(Path::new("/repo"), &commands, &mutation);

        assert_eq!(selected.argv, vec!["go", "test", "./..."]);
        assert_eq!(selected.timeout, Duration::from_secs(11));
    }

    #[test]
    fn select_test_command_cli_override_keeps_project_timeout() {
        let mut commands = test_command_config();
        commands.command = vec!["make".into(), "ci".into()];
        commands.timeout = Duration::from_secs(30);
        commands.force_default_command = true;
        commands.language_commands.insert(
            "go".into(),
            vec!["go".into(), "test".into(), "./...".into()],
        );
        commands
            .language_timeouts
            .insert("go".into(), Duration::from_secs(5));
        commands.project_commands.push(ProjectCommandConfig {
            path: PathBuf::from("services/api"),
            command: Some(vec![
                "go".into(),
                "test".into(),
                "./services/api/...".into(),
            ]),
            timeout: Some(Duration::from_secs(9)),
        });
        let mut mutation = make_test_mutation(Path::new("services/api/calc.go"));
        mutation.language = "go".into();

        let selected = select_test_command(Path::new("/repo"), &commands, &mutation);

        assert_eq!(selected.argv, vec!["make", "ci"]);
        assert_eq!(selected.timeout, Duration::from_secs(9));
    }

    #[test]
    fn select_test_command_cli_timeout_overrides_project_and_language_timeout() {
        let mut commands = test_command_config();
        commands.timeout = Duration::from_secs(30);
        commands.force_default_timeout = true;
        commands
            .language_timeouts
            .insert("go".into(), Duration::from_secs(5));
        commands.project_commands.push(ProjectCommandConfig {
            path: PathBuf::from("services/api"),
            command: Some(vec![
                "go".into(),
                "test".into(),
                "./services/api/...".into(),
            ]),
            timeout: Some(Duration::from_secs(9)),
        });
        let mut mutation = make_test_mutation(Path::new("services/api/calc.go"));
        mutation.language = "go".into();

        let selected = select_test_command(Path::new("/repo"), &commands, &mutation);

        assert_eq!(selected.argv, vec!["go", "test", "./services/api/..."]);
        assert_eq!(selected.timeout, Duration::from_secs(30));
    }

    #[test]
    fn selected_test_command_cache_context_elides_auto_build_suggestions() {
        let selected = SelectedTestCommand {
            argv: vec!["cargo test".into()],
            unnarrowed_argv: None,
            selection_active: false,
            timeout: Duration::from_secs(2),
            uses_default_timeout: false,
            selected_tests: Vec::new(),
        };
        let ambiguous = SelectedTestCommand {
            argv: vec!["cargo".into(), "test".into()],
            unnarrowed_argv: None,
            selection_active: false,
            timeout: Duration::from_secs(2),
            uses_default_timeout: false,
            selected_tests: Vec::new(),
        };

        assert_ne!(
            selected.cache_context(&[], BuildCommandOrigin::None, &[], &HashMap::new()),
            ambiguous.cache_context(&[], BuildCommandOrigin::None, &[], &HashMap::new())
        );

        let no_build = selected.cache_context(&[], BuildCommandOrigin::None, &[], &HashMap::new());
        let auto_build = selected.cache_context(
            &["go".into(), "build".into(), "./...".into()],
            BuildCommandOrigin::AutoDetected,
            &[],
            &HashMap::new(),
        );
        let configured_build = selected.cache_context(
            &["go".into(), "build".into(), "./...".into()],
            BuildCommandOrigin::Configured,
            &[],
            &HashMap::new(),
        );
        assert_eq!(no_build, auto_build);
        assert_ne!(no_build, configured_build);
        assert_eq!(
            CacheKey::new(b"source", "mutation", "description", &no_build).test_command_hash,
            CacheKey::new(b"source", "mutation", "description", &auto_build).test_command_hash
        );
    }

    #[cfg(unix)]
    #[test]
    fn auto_build_suggestion_reuses_an_exact_cache_verdict() -> anyhow::Result<()> {
        let (dir, _file, mutation) = make_relative_test_setup();
        let build_log = dir.path().join("build.log");
        let test_log = dir.path().join("test.log");
        let test_command = vec![
            "sh".into(),
            "-c".into(),
            "printf invoked >> \"$1\"; exit 1".into(),
            "test".into(),
            test_log.display().to_string(),
        ];
        let no_build_commands = CommandConfig {
            command: test_command.clone(),
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec![],
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::None,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: None,
        };
        seed_reused_survivor(
            dir.path(),
            &no_build_commands,
            &mutation,
            ReuseSource::ExactCache,
        )?;

        let reused = TestRunner {
            commands: no_build_commands,
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: false,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
        .run(vec![mutation.clone()])
        .report;
        assert_eq!(
            reused.execution_for(mutation.id, MutationResult::Survived),
            MutationExecution::ExactCache
        );
        assert!(!test_log.exists(), "the seeded verdict should be reused");

        let auto_build_commands = CommandConfig {
            command: test_command,
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: appending_log_command(&build_log),
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::AutoDetected,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: None,
        };
        let reused_auto = TestRunner {
            commands: auto_build_commands,
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: false,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
        .run(vec![mutation.clone()])
        .report;

        assert_eq!(reused_auto.results.len(), 1);
        assert_eq!(reused_auto.results[0].1, MutationResult::Survived);
        assert_eq!(
            reused_auto.execution_for(mutation.id, MutationResult::Survived),
            MutationExecution::ExactCache
        );
        assert!(!build_log.exists(), "a detected suggestion must not run");
        assert!(!test_log.exists(), "the seeded verdict should be reused");
        Ok(())
    }

    #[test]
    fn sandboxed_command_prefixes_wrapper_argv() {
        let sandbox = vec!["bwrap".into(), "--".into()];
        let command = vec!["cargo".into(), "test".into()];

        assert_eq!(
            sandboxed_command(&sandbox, &command),
            vec!["bwrap", "--", "cargo", "test"]
        );
        assert_eq!(sandboxed_command(&[], &command), command);
    }

    #[test]
    fn select_test_command_narrows_go_command_when_tests_cover_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        let file = tmp.path().join("src/calc.go");
        std::fs::write(&file, b"package calc\n").unwrap();

        let mut selection = TestSelectionConfig::new();
        selection.insert(
            tmp.path(),
            &file,
            7,
            vec!["TestAdd".into(), "TestMax+Fast".into()],
        );

        let mut commands = test_command_config();
        commands.command = vec!["go".into(), "test".into(), "./...".into()];
        commands.test_selection = Some(selection);

        let mut mutation = make_test_mutation(Path::new("src/calc.go"));
        mutation.line = 7;

        let selected = select_test_command(tmp.path(), &commands, &mutation);

        assert_eq!(
            selected.argv,
            vec!["go", "test", "-run", "^(TestAdd|TestMax\\+Fast)$", "./..."]
        );
    }

    #[test]
    fn select_test_command_replaces_go_run_flag_value() {
        let tmp = tempfile::tempdir().unwrap();
        let mut selection = TestSelectionConfig::new();
        selection.insert(tmp.path(), Path::new("calc.go"), 4, vec!["TestAdd".into()]);

        let mut commands = test_command_config();
        commands.command = vec![
            "go".into(),
            "test".into(),
            "-count=1".into(),
            "-run".into(),
            "OldPattern".into(),
            "./...".into(),
        ];
        commands.test_selection = Some(selection);

        let mut mutation = make_test_mutation(Path::new("calc.go"));
        mutation.line = 4;

        let selected = select_test_command(tmp.path(), &commands, &mutation);

        assert_eq!(
            selected.argv,
            vec!["go", "test", "-count=1", "-run", "^(TestAdd)$", "./..."]
        );
    }

    #[test]
    fn select_test_command_replaces_go_run_equals_flag_value() {
        let tmp = tempfile::tempdir().unwrap();
        let mut selection = TestSelectionConfig::new();
        selection.insert(tmp.path(), Path::new("calc.go"), 4, vec!["TestAdd".into()]);

        let mut commands = test_command_config();
        commands.command = vec![
            "go".into(),
            "test".into(),
            "-count=1".into(),
            "-run=OldPattern".into(),
            "./...".into(),
        ];
        commands.test_selection = Some(selection);

        let mut mutation = make_test_mutation(Path::new("calc.go"));
        mutation.line = 4;

        let selected = select_test_command(tmp.path(), &commands, &mutation);

        assert_eq!(
            selected.argv,
            vec!["go", "test", "-count=1", "-run=^(TestAdd)$", "./..."]
        );
    }

    #[test]
    fn force_go_no_test_cache_inserts_count_once() {
        assert_eq!(
            force_go_no_test_cache(vec!["go".into(), "test".into(), "./...".into()]),
            vec!["go", "test", "-count=1", "./..."]
        );
        assert_eq!(
            force_go_no_test_cache(vec![
                "go".into(),
                "test".into(),
                "-count=1".into(),
                "./...".into()
            ]),
            vec!["go", "test", "-count=1", "./..."]
        );
        assert_eq!(
            force_go_no_test_cache(vec!["cargo".into(), "test".into()]),
            vec!["cargo", "test"]
        );
    }

    #[test]
    fn select_test_command_falls_back_when_no_tests_cover_mutation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut commands = test_command_config();
        commands.command = vec!["go".into(), "test".into(), "./...".into()];
        commands.test_selection = Some(TestSelectionConfig::new());
        let mutation = make_test_mutation(Path::new("src/calc.go"));

        let selected = select_test_command(tmp.path(), &commands, &mutation);

        assert_eq!(selected.argv, vec!["go", "test", "./..."]);
    }

    #[test]
    fn select_test_command_orders_selected_tests_by_duration() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut selection = TestSelectionConfig::new();
        selection.insert_tests(
            tmp.path(),
            Path::new("src/calc.py"),
            3,
            vec![
                SelectedTest::new("slow_test", Some(50)),
                SelectedTest::new("fast_test", Some(5)),
                SelectedTest::new("unknown_test", None),
            ],
        );

        let mut commands = test_command_config();
        commands.command = vec!["pytest".into()];
        commands.test_selection = Some(selection);

        let mut mutation = make_test_mutation(Path::new("src/calc.py"));
        mutation.line = 3;

        let selected = select_test_command(tmp.path(), &commands, &mutation);

        assert_eq!(
            selected.argv,
            vec!["pytest", "-k", "fast_test or slow_test or unknown_test"]
        );
        Ok(())
    }

    #[test]
    fn select_test_command_narrows_pytest_node_ids() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            tmp.path(),
            Path::new("src/calc.py"),
            3,
            vec![
                "tests/test_calc.py::test_add".into(),
                "tests/test_calc.py::test_max".into(),
            ],
        );

        let mut commands = test_command_config();
        commands.command = vec!["python".into(), "-m".into(), "pytest".into()];
        commands.test_selection = Some(selection);

        let mut mutation = make_test_mutation(Path::new("src/calc.py"));
        mutation.line = 3;

        let selected = select_test_command(tmp.path(), &commands, &mutation);

        assert_eq!(
            selected.argv,
            vec![
                "python",
                "-m",
                "pytest",
                "tests/test_calc.py::test_add",
                "tests/test_calc.py::test_max"
            ]
        );
        Ok(())
    }

    #[test]
    fn select_test_command_falls_back_for_unsupported_pytest_names() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            tmp.path(),
            Path::new("src/calc.py"),
            3,
            vec!["test_add".into(), "test with space".into()],
        );

        let mut commands = test_command_config();
        commands.command = vec!["pytest".into()];
        commands.test_selection = Some(selection);

        let mut mutation = make_test_mutation(Path::new("src/calc.py"));
        mutation.line = 3;

        let selected = select_test_command(tmp.path(), &commands, &mutation);

        assert_eq!(selected.argv, vec!["pytest"]);
        Ok(())
    }

    #[test]
    fn select_test_command_narrows_jest_and_vitest_names() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            tmp.path(),
            Path::new("src/calc.test.ts"),
            3,
            vec!["adds numbers".into(), "handles max+".into()],
        );

        let mut commands = test_command_config();
        commands.command = vec!["npx".into(), "vitest".into(), "run".into()];
        commands.test_selection = Some(selection);

        let mut mutation = make_test_mutation(Path::new("src/calc.test.ts"));
        mutation.line = 3;

        let selected = select_test_command(tmp.path(), &commands, &mutation);

        assert_eq!(
            selected.argv,
            vec![
                "npx",
                "vitest",
                "run",
                "-t",
                "^(adds numbers|handles max\\+)$"
            ]
        );
        Ok(())
    }

    #[test]
    fn select_test_command_narrows_single_cargo_test_filter() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            tmp.path(),
            Path::new("src/lib.rs"),
            3,
            vec!["math::adds".into()],
        );

        let mut commands = test_command_config();
        commands.command = vec![
            "cargo".into(),
            "test".into(),
            "--workspace".into(),
            "--".into(),
            "--nocapture".into(),
        ];
        commands.test_selection = Some(selection);

        let mut mutation = make_test_mutation(Path::new("src/lib.rs"));
        mutation.line = 3;

        let selected = select_test_command(tmp.path(), &commands, &mutation);

        assert_eq!(
            selected.argv,
            vec![
                "cargo",
                "test",
                "--workspace",
                "math::adds",
                "--",
                "--nocapture"
            ]
        );
        Ok(())
    }

    #[test]
    fn narrowed_routes_retain_their_full_command_for_confirmation() {
        for (path, command, test) in [
            (
                "src/calc.go",
                vec!["go".into(), "test".into(), "./...".into()],
                "TestAdd",
            ),
            ("src/calc.py", vec!["pytest".into()], "selected_test"),
            (
                "src/calc.test.ts",
                vec!["vitest".into(), "run".into()],
                "selected_test",
            ),
            (
                "src/lib.rs",
                vec!["cargo".into(), "test".into()],
                "selected_test",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut selection = TestSelectionConfig::new();
            selection.insert(root.path(), Path::new(path), 1, vec![test.into()]);
            let mut commands = test_command_config();
            let full_command = command.clone();
            commands.command = command;
            commands.test_selection = Some(selection);
            let mut mutation = make_test_mutation(Path::new(path));
            mutation.line = 1;

            let selected = select_test_command(root.path(), &commands, &mutation);

            assert_eq!(
                selected.unnarrowed_argv(),
                Some(full_command.as_slice()),
                "{path} must retain its original route"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn confirmation_kills_a_narrowed_survivor_on_its_full_route() {
        let (dir, _, mutation) = make_relative_test_setup();
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            dir.path(),
            Path::new("test.txt"),
            1,
            vec!["selected_test".into()],
        );
        let (env, log) = fake_selection_command(dir.path(), "pytest", "-k selected_test", 1);
        let mut commands = test_command_config();
        commands.command = vec!["pytest".into()];
        commands.test_selection = Some(selection);

        let report = confirmation_runner(dir.path(), commands, env)
            .run(vec![mutation.clone()])
            .report;

        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].0.id, mutation.id);
        assert_eq!(report.results[0].1, MutationResult::Killed);
        assert_eq!(report.survived, 0);
        assert_eq!(
            report.selection_for(mutation.id),
            Some(TestSelectionProvenance::Narrowed {
                confirmation: SurvivorConfirmation::Killed,
            })
        );
        assert_eq!(
            std::fs::read_to_string(log)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["<-k selected_test>", "<>"],
        );
    }

    #[cfg(unix)]
    #[test]
    fn confirmation_keeps_a_narrowed_survivor_that_survives_its_full_route() {
        let (dir, _, mutation) = make_relative_test_setup();
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            dir.path(),
            Path::new("test.txt"),
            1,
            vec!["selected_test".into()],
        );
        let (env, log) = fake_selection_command(dir.path(), "pytest", "-k selected_test", 0);
        let mut commands = test_command_config();
        commands.command = vec!["pytest".into()];
        commands.test_selection = Some(selection);

        let report = confirmation_runner(dir.path(), commands, env)
            .run(vec![mutation.clone()])
            .report;

        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].0.id, mutation.id);
        assert_eq!(report.results[0].1, MutationResult::Survived);
        assert_eq!(report.survived, 1);
        assert_eq!(
            report.selection_for(mutation.id),
            Some(TestSelectionProvenance::Narrowed {
                confirmation: SurvivorConfirmation::ConfirmedSurvived,
            })
        );
        assert_eq!(
            std::fs::read_to_string(log)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["<-k selected_test>", "<>"],
        );
    }

    #[cfg(unix)]
    #[test]
    fn confirmation_rechecks_a_cached_narrowed_survivor_without_rewriting_its_cache() {
        let (dir, _, mutation) = make_relative_test_setup();
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            dir.path(),
            Path::new("test.txt"),
            1,
            vec!["selected_test".into()],
        );
        let (env, log) = fake_selection_command(dir.path(), "pytest", "-k selected_test", 1);
        let mut commands = test_command_config();
        commands.command = vec!["pytest".into()];
        commands.test_selection = Some(selection);
        seed_reused_survivor_with_env(
            dir.path(),
            &commands,
            &mutation,
            ReuseSource::ExactCache,
            &env,
        )
        .unwrap();

        let mut runner = confirmation_runner(dir.path(), commands, env);
        runner.force_rerun = false;
        let report = runner.run(vec![mutation.clone()]).report;

        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].0.id, mutation.id);
        assert_eq!(report.results[0].1, MutationResult::Killed);
        assert_eq!(
            report.execution_for(mutation.id, MutationResult::Killed),
            MutationExecution::Executed
        );
        assert_eq!(
            report.selection_for(mutation.id),
            Some(TestSelectionProvenance::Narrowed {
                confirmation: SurvivorConfirmation::Killed,
            })
        );
        assert_eq!(
            std::fs::read_to_string(log)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["<>"],
        );
        let selected = select_test_command(dir.path(), &runner.commands, &mutation);
        let source = std::fs::read(dir.path().join("test.txt")).unwrap();
        let key = CacheKey::new(
            &source,
            &cache_identity(dir.path(), &mutation),
            &mutation.description,
            &format!(
                "{};context={:016x}",
                selected.cache_context(
                    &runner.commands.build_command,
                    runner.commands.build_command_origin,
                    &runner.commands.sandbox_command,
                    &runner.env,
                ),
                cache_context_fingerprint(dir.path())
            ),
        );
        assert_eq!(
            cache::lookup(dir.path(), &key),
            Some(MutationResult::Survived)
        );
    }

    #[test]
    fn select_test_command_falls_back_for_multiple_cargo_tests() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            tmp.path(),
            Path::new("src/lib.rs"),
            3,
            vec!["math::adds".into(), "math::max".into()],
        );

        let mut commands = test_command_config();
        commands.command = vec!["cargo".into(), "test".into()];
        commands.test_selection = Some(selection);

        let mut mutation = make_test_mutation(Path::new("src/lib.rs"));
        mutation.line = 3;

        let selected = select_test_command(tmp.path(), &commands, &mutation);

        assert_eq!(selected.argv, vec!["cargo", "test"]);
        Ok(())
    }

    #[test]
    fn select_test_command_narrows_maven_tests() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            tmp.path(),
            Path::new("src/main/java/Calc.java"),
            3,
            vec!["CalcTest#adds".into(), "CalcTest#max".into()],
        );

        let mut commands = test_command_config();
        commands.command = vec!["mvn".into(), "-q".into(), "test".into()];
        commands.test_selection = Some(selection);

        let mut mutation = make_test_mutation(Path::new("src/main/java/Calc.java"));
        mutation.line = 3;

        let selected = select_test_command(tmp.path(), &commands, &mutation);

        assert_eq!(
            selected.argv,
            vec!["mvn", "-q", "test", "-Dtest=CalcTest#adds,CalcTest#max"]
        );
        Ok(())
    }

    #[test]
    fn select_test_command_narrows_gradle_tests() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            tmp.path(),
            Path::new("src/main/java/Calc.java"),
            3,
            vec!["CalcTest.adds".into(), "CalcTest.max".into()],
        );

        let mut commands = test_command_config();
        commands.command = vec!["./gradlew".into(), "test".into()];
        commands.test_selection = Some(selection);

        let mut mutation = make_test_mutation(Path::new("src/main/java/Calc.java"));
        mutation.line = 3;

        let selected = select_test_command(tmp.path(), &commands, &mutation);

        assert_eq!(
            selected.argv,
            vec![
                "./gradlew",
                "test",
                "--tests",
                "CalcTest.adds",
                "--tests",
                "CalcTest.max"
            ]
        );
        Ok(())
    }

    #[test]
    fn select_test_command_falls_back_for_unsupported_command() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            tmp.path(),
            Path::new("src/lib.rs"),
            3,
            vec!["test_name".into()],
        );

        let mut commands = test_command_config();
        commands.command = vec!["make".into(), "test".into()];
        commands.test_selection = Some(selection);

        let mut mutation = make_test_mutation(Path::new("src/lib.rs"));
        mutation.line = 3;

        let selected = select_test_command(tmp.path(), &commands, &mutation);

        assert_eq!(selected.argv, vec!["make", "test"]);
        Ok(())
    }

    #[test]
    fn select_test_command_prioritizes_history_killer() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            tmp.path(),
            Path::new("src/calc.py"),
            3,
            vec!["test_slow".into(), "test_fast".into()],
        );

        let mut commands = test_command_config();
        commands.command = vec!["pytest".into()];
        commands.test_selection = Some(selection);

        let mut mutation = make_test_mutation(Path::new("src/calc.py"));
        mutation.line = 3;
        let history = cache::IncrementalHistoryStore::load(tmp.path());
        history.record(cache::IncrementalHistoryEntry {
            mutation_identity: cache_identity(tmp.path(), &mutation),
            mutation_description: mutation.description.clone(),
            result: MutationResult::Killed,
            source_hash: 1,
            command_hash: 2,
            relevant_test_hash: 3,
            covering_tests: vec!["test_slow".into(), "test_fast".into()],
            killer_test: Some("test_fast".into()),
        });

        let selected =
            select_test_command_with_history(tmp.path(), &commands, &mutation, Some(&history));

        assert_eq!(
            selected.argv,
            vec!["pytest", "-k", "test_fast or test_slow"]
        );
        Ok(())
    }

    #[test]
    fn incremental_history_reuses_result_and_force_rerun_bypasses_it() -> anyhow::Result<()> {
        let (dir, file, mutation) = make_test_setup();
        std::fs::create_dir_all(dir.path().join("tests"))?;
        std::fs::write(
            dir.path().join("tests/test_calc.rs"),
            "fn test_add() { assert!(true); }\n",
        )?;

        let mut selection = TestSelectionConfig::new();
        selection.insert(dir.path(), &file, mutation.line, vec!["test_add".into()]);
        let commands = || CommandConfig {
            command: failing_command(),
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec![],
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::None,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: Some(selection.clone()),
        };

        let command_config = commands();
        let selected =
            select_test_command_with_history(dir.path(), &command_config, &mutation, None);
        let command_ctx = selected.cache_context(
            &command_config.build_command,
            BuildCommandOrigin::None,
            &command_config.sandbox_command,
            &HashMap::new(),
        );
        let context_hash = cache_context_fingerprint(dir.path());
        let test_context_index = TestContextIndex::build(dir.path());
        let relevant_test_hash =
            test_context_index.fingerprint_for_tests(&selected.selected_tests, context_hash);
        let source = std::fs::read(&file)?;
        let query = incremental_history_query(
            dir.path(),
            &mutation,
            &source,
            &command_ctx,
            relevant_test_hash,
        );
        cache::IncrementalHistoryStore::load(dir.path()).record(cache::IncrementalHistoryEntry {
            mutation_identity: query.mutation_identity.clone(),
            mutation_description: query.mutation_description.clone(),
            result: MutationResult::Survived,
            source_hash: query.source_hash,
            command_hash: query.command_hash,
            relevant_test_hash: query.relevant_test_hash,
            covering_tests: selected.selected_tests.clone(),
            killer_test: None,
        });

        let cached_runner = TestRunner {
            commands: commands(),
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let cached = cached_runner.run(vec![mutation.clone()]).report;
        assert_eq!(cached.results[0].1, MutationResult::Survived);
        assert_eq!(
            cached.execution_for(mutation.id, MutationResult::Survived),
            MutationExecution::IncrementalHistory
        );
        assert_eq!(cached.tested_count(), 0);
        assert_eq!(cached.execution_counts().incremental_history_reused, 1);

        let forced_runner = TestRunner {
            commands: commands(),
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: true,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let forced = forced_runner.run(vec![mutation]).report;
        assert_eq!(forced.results[0].1, MutationResult::Killed);
        assert_eq!(
            forced.execution_for(forced.results[0].0.id, MutationResult::Killed),
            MutationExecution::Executed
        );
        assert_eq!(forced.tested_count(), 1);
        Ok(())
    }

    #[test]
    fn restored_cache_and_history_verdicts_keep_provenance_without_fake_diagnostics()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mutation = |id: u32, file: &str, description: &str| Mutation {
            id,
            file: PathBuf::from(file),
            language: String::new(),
            line: 1,
            column: 1,
            operator: "test".into(),
            description: description.into(),
            original: "hello".into(),
            replacement: "world".into(),
            byte_range: 0..5,
        };
        let cached = mutation(0, "cached.txt", "cached build error");
        let historical = mutation(1, "history.txt", "historical survivor");
        std::fs::write(dir.path().join(&cached.file), "hello")?;
        std::fs::write(dir.path().join(&historical.file), "hello")?;
        let commands = CommandConfig {
            command: successful_command(),
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec![],
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::None,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: None,
        };
        let env = HashMap::new();
        let context_hash = cache_context_fingerprint(dir.path());

        let selected = select_test_command(dir.path(), &commands, &cached);
        let context = selected.cache_context(
            &commands.build_command,
            commands.build_command_origin,
            &commands.sandbox_command,
            &env,
        );
        let key = CacheKey::new(
            &std::fs::read(dir.path().join(&cached.file))?,
            &cache_identity(dir.path(), &cached),
            &cached.description,
            &format!("{context};context={context_hash:016x}"),
        );
        cache::store(dir.path(), &key, MutationResult::BuildError);

        let selected = select_test_command(dir.path(), &commands, &historical);
        let command_context = selected.cache_context(
            &commands.build_command,
            commands.build_command_origin,
            &commands.sandbox_command,
            &env,
        );
        let test_context_index = TestContextIndex::build(dir.path());
        let source = std::fs::read(dir.path().join(&historical.file))?;
        let query = incremental_history_query(
            dir.path(),
            &historical,
            &source,
            &command_context,
            test_context_index.fingerprint_for_tests(&selected.selected_tests, context_hash),
        );
        cache::IncrementalHistoryStore::load(dir.path()).record(cache::IncrementalHistoryEntry {
            mutation_identity: query.mutation_identity,
            mutation_description: query.mutation_description,
            result: MutationResult::Survived,
            source_hash: query.source_hash,
            command_hash: query.command_hash,
            relevant_test_hash: query.relevant_test_hash,
            covering_tests: vec![],
            killer_test: None,
        });

        let runner = TestRunner {
            commands,
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env,
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let report = runner.run(vec![cached, historical]).report;

        assert_eq!(
            report
                .results
                .iter()
                .map(|(_, result)| *result)
                .collect::<Vec<_>>(),
            vec![MutationResult::BuildError, MutationResult::Survived]
        );
        assert_eq!(report.tested_count(), 0);
        assert!(report.build_error_diagnostics.is_empty());
        assert_eq!(
            report.execution_for(0, MutationResult::BuildError),
            MutationExecution::ExactCache
        );
        assert_eq!(
            report.execution_for(1, MutationResult::Survived),
            MutationExecution::IncrementalHistory
        );

        let json: serde_json::Value =
            serde_json::from_str(&crate::report::json::to_json_string(&report)?)?;
        assert_eq!(json["tested"], 0);
        assert_eq!(json["mutations"][0]["result"], "build_error");
        assert_eq!(json["mutations"][0]["execution"]["state"], "exact_cache");
        assert_eq!(json["mutations"][1]["result"], "survived");
        assert_eq!(
            json["mutations"][1]["execution"]["state"],
            "incremental_history"
        );
        assert_eq!(json["build_error_groups"], serde_json::json!([]));
        Ok(())
    }

    #[test]
    fn incremental_history_invalidated_by_source_context_change() -> anyhow::Result<()> {
        let (dir, file, mutation) = make_test_setup();

        let commands = || CommandConfig {
            command: failing_command(),
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec![],
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::None,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: None,
        };

        let command_config = commands();
        let selected = select_test_command(dir.path(), &command_config, &mutation);
        let command_ctx = selected.cache_context(
            &command_config.build_command,
            BuildCommandOrigin::None,
            &command_config.sandbox_command,
            &HashMap::new(),
        );
        let context_hash = cache_context_fingerprint(dir.path());
        let test_context_index = TestContextIndex::build(dir.path());
        let relevant_test_hash =
            test_context_index.fingerprint_for_tests(&selected.selected_tests, context_hash);
        let source = std::fs::read(&file)?;
        let query = incremental_history_query(
            dir.path(),
            &mutation,
            &source,
            &command_ctx,
            relevant_test_hash,
        );
        cache::IncrementalHistoryStore::load(dir.path()).record(cache::IncrementalHistoryEntry {
            mutation_identity: query.mutation_identity.clone(),
            mutation_description: query.mutation_description.clone(),
            result: MutationResult::Survived,
            source_hash: query.source_hash,
            command_hash: query.command_hash,
            relevant_test_hash: query.relevant_test_hash,
            covering_tests: selected.selected_tests.clone(),
            killer_test: None,
        });

        // A source file added after the verdict was recorded — e.g. a module
        // wiring change or a colocated test — must invalidate the restore (#410).
        std::fs::create_dir_all(dir.path().join("src"))?;
        std::fs::write(
            dir.path().join("src/codec.rs"),
            "pub fn decode() -> i32 { 1 }\n",
        )?;

        let runner = TestRunner {
            commands: commands(),
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let report = runner.run(vec![mutation]).report;
        assert_eq!(report.results[0].1, MutationResult::Killed);
        Ok(())
    }

    #[test]
    fn attribute_failing_tests_parses_cargo_output() {
        let output = "running 3 tests\n\
                      test my::passing::test ... ok\n\
                      test my::failing::test ... FAILED\n\
                      \n\
                      failures:\n\
                      \n\
                      ---- my::failing::test stdout ----\n\
                      boom\n\
                      \n\
                      failures:\n\
                      \x20   my::failing::test\n\
                      \n\
                      test result: FAILED. 2 passed; 1 failed; 0 ignored;\n";
        assert_eq!(
            attribute_failing_tests(output),
            vec!["my::failing::test".to_string()]
        );
    }

    #[test]
    fn attribute_failing_tests_parses_go_output() {
        let output = "=== RUN   TestAdd\n\
                      --- FAIL: TestAdd (0.00s)\n\
                      \x20   calc_test.go:12: want 5, got 4\n\
                      --- FAIL: TestSub/fast (0.00s)\n\
                      FAIL\n\
                      FAIL\texample.com/calc\t0.01s\n";
        assert_eq!(
            attribute_failing_tests(output),
            vec!["TestAdd".to_string(), "TestSub/fast".to_string()]
        );
    }

    #[test]
    fn attribute_failing_tests_parses_pytest_and_unittest_output() {
        let pytest = "tests/test_calc.py::test_add FAILED [ 50%]\n\
                      FAILED tests/test_calc.py::test_sub - assert 1 == 2\n";
        assert_eq!(
            attribute_failing_tests(pytest),
            vec!["tests/test_calc.py::test_sub".to_string()]
        );

        let unittest = "FAIL: test_discount (test_calc.TestCalc)\n\
                        ERROR: test_parse (test_calc.TestCalc)\n";
        assert_eq!(
            attribute_failing_tests(unittest),
            vec!["test_discount".to_string(), "test_parse".to_string()]
        );
    }

    #[test]
    fn attribute_failing_tests_ignores_noise_and_empty_output() {
        assert!(attribute_failing_tests("").is_empty());
        assert!(attribute_failing_tests("all tests passed\nok  \texample.com/calc\n").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn full_suite_kill_records_killer_test_from_output() -> anyhow::Result<()> {
        let (dir, _file, mutation) = make_test_setup();
        let script = "echo 'running 2 tests'\n\
                      echo 'test my::passing::test ... ok'\n\
                      echo 'test my::failing::test ... FAILED'\n\
                      echo 'failures:'\n\
                      echo '    my::failing::test'\n\
                      echo 'test result: FAILED. 1 passed; 1 failed;'\n\
                      exit 1\n";
        let commands = CommandConfig {
            command: vec!["sh".into(), "-c".into(), script.into()],
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec![],
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::None,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: None,
        };
        let runner = TestRunner {
            commands,
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: true,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(vec![mutation]).report;
        assert_eq!(report.results[0].1, MutationResult::Killed);

        // Even without --show-output, history is active, so the failing test
        // was captured from output and recorded as the killer (#428).
        let data = std::fs::read_to_string(dir.path().join(".togi-cache/history.json"))?;
        let history: serde_json::Value = serde_json::from_str(&data)?;
        let entry = &history["entries"][0];
        assert_eq!(entry["killer_test"], "my::failing::test");
        assert_eq!(
            entry["covering_tests"],
            serde_json::json!(["my::failing::test"])
        );
        Ok(())
    }

    /// Three applicable mutations in one file with distinct identities.
    fn make_killer_cluster_setup() -> (tempfile::TempDir, PathBuf, Vec<Mutation>) {
        let (dir, file, base) = make_test_setup();
        let mut second = base.clone();
        second.id = 2;
        second.byte_range = 6..11;
        second.original = "world".into();
        second.replacement = "hello".into();
        second.operator = "test_b".into();
        second.description = "mutation b".into();
        let mut third = base.clone();
        third.id = 3;
        third.replacement = "WORLD".into();
        third.operator = "test_c".into();
        third.description = "mutation c".into();
        (dir, file, vec![base, second, third])
    }

    /// Seed a Killed entry with a shared killer test for each mutation.
    ///
    /// `relevant_test_hash` is deliberately stale so verdict restore misses:
    /// the canonical mutant really executes, making the subsumed siblings'
    /// skipped execution observable in the report.
    fn seed_killer_cluster_history(dir: &Path, file: &Path, mutations: &[Mutation]) {
        let commands = CommandConfig {
            command: failing_command(),
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec![],
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::None,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: None,
        };
        let store = cache::IncrementalHistoryStore::load(dir);
        let source = std::fs::read(file).expect("source file should exist");
        for mutation in mutations {
            let selected = select_test_command(dir, &commands, mutation);
            let command_ctx = selected.cache_context(
                &commands.build_command,
                BuildCommandOrigin::None,
                &commands.sandbox_command,
                &HashMap::new(),
            );
            store.record(cache::IncrementalHistoryEntry {
                mutation_identity: cache_identity(dir, mutation),
                mutation_description: mutation.description.clone(),
                result: MutationResult::Killed,
                source_hash: cache::hash_bytes(&source),
                command_hash: cache::hash_str(&command_ctx),
                relevant_test_hash: 3,
                covering_tests: vec!["test_shared".into()],
                killer_test: Some("test_shared".into()),
            });
        }
    }

    fn killer_cluster_runner(dir: &Path, learned_selection: bool) -> TestRunner {
        TestRunner {
            commands: CommandConfig {
                command: failing_command(),
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn learned_selection_subsumes_shared_killer_cluster() -> anyhow::Result<()> {
        let (dir, file, mutations) = make_killer_cluster_setup();
        seed_killer_cluster_history(dir.path(), &file, &mutations);

        let report = killer_cluster_runner(dir.path(), true)
            .run(mutations)
            .report;

        // Only the canonical mutant (first in run order) executed; its two
        // cluster siblings were classified Subsumed without execution.
        assert_eq!(report.total, 3);
        assert_eq!(report.killed, 1);
        assert_eq!(report.subsumed_count(), 2);
        assert_eq!(report.tested_count(), 1);
        let result_for = |id: u32| {
            report
                .results
                .iter()
                .find(|(mutation, _)| mutation.id == id)
                .map(|(_, result)| *result)
        };
        assert_eq!(result_for(1), Some(MutationResult::Killed));
        assert_eq!(result_for(2), Some(MutationResult::Subsumed));
        assert_eq!(result_for(3), Some(MutationResult::Subsumed));
        // Subsumed mutants stay out of the score denominator: 1/1 → 100%.
        assert_eq!(crate::report::mutation_score(&report), 100.0);
        Ok(())
    }

    #[test]
    fn learned_selection_disabled_by_default_executes_everything() -> anyhow::Result<()> {
        let (dir, file, mutations) = make_killer_cluster_setup();
        seed_killer_cluster_history(dir.path(), &file, &mutations);

        let report = killer_cluster_runner(dir.path(), false)
            .run(mutations)
            .report;

        assert_eq!(report.total, 3);
        assert_eq!(report.tested_count(), 3);
        assert_eq!(report.killed, 3);
        assert_eq!(report.subsumed_count(), 0);
        assert!(
            report
                .results
                .iter()
                .all(|(_, result)| *result == MutationResult::Killed)
        );
        Ok(())
    }

    #[test]
    fn mutation_file_path_resolves_relative_paths_against_project_root() {
        let root = Path::new("/repo");

        assert_eq!(
            mutation_file_path(root, Path::new("src/lib.rs")),
            PathBuf::from("/repo/src/lib.rs")
        );
        assert_eq!(
            mutation_file_path(root, Path::new("/tmp/src/lib.rs")),
            PathBuf::from("/tmp/src/lib.rs")
        );
    }

    #[test]
    fn cache_identity_normalizes_absolute_and_relative_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/lib.rs");
        std::fs::write(&file, b"hello world").unwrap();

        let relative = make_test_mutation(Path::new("src/lib.rs"));
        let absolute = make_test_mutation(&file);

        assert_eq!(
            cache_identity(root, &relative),
            cache_identity(root, &absolute)
        );
    }

    #[test]
    fn validate_and_resolve_mutation_path_rejects_parent_escape() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(tmp.path().join("secret.txt"), b"secret").unwrap();

        assert!(validate_and_resolve_mutation_path(&root, Path::new("../secret.txt")).is_err());
    }

    #[test]
    fn replay_workspace_control_exclusions_are_ascii_case_insensitive() {
        for path in [
            ".GIT/config",
            ".TOGI/state",
            ".TOGI-CACHE/history.json",
            ".TOGI.LOCK",
            ".TOGI-BASELINE",
            ".TOGI-OTHER/state",
        ] {
            assert!(
                should_skip_replay_workspace_entry(Path::new(path)),
                "{path}"
            );
        }
    }

    #[test]
    fn normal_workspace_exclusions_remain_case_sensitive() {
        for path in ["Target/cache", "Build/artifact", ".Togi-Custom/state"] {
            assert!(!should_skip_workspace_entry(Path::new(path)), "{path}");
        }
    }

    #[test]
    fn copy_workspace_copies_regular_files_and_skips_internal_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(root.join(".venv")).unwrap();
        std::fs::create_dir_all(root.join("dist")).unwrap();
        std::fs::create_dir_all(root.join("build")).unwrap();
        std::fs::create_dir_all(root.join("services/api/src")).unwrap();
        std::fs::create_dir_all(root.join("services/api/node_modules/pkg")).unwrap();
        std::fs::create_dir_all(root.join("services/api/build")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::create_dir_all(root.join(".togi")).unwrap();
        std::fs::create_dir_all(root.join(".togi-cache")).unwrap();
        std::fs::create_dir_all(root.join(".codex")).unwrap();
        std::fs::create_dir_all(root.join(".claude")).unwrap();
        std::fs::write(root.join("Cargo.toml"), b"[package]\n").unwrap();
        std::fs::write(root.join(".ignore"), b"src/ignored_by_ignore.rs\n").unwrap();
        std::fs::write(root.join(".gitignore"), b"ignored-by-gitignore.txt\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub fn f() {}\n").unwrap();
        std::fs::write(
            root.join("src/ignored_by_ignore.rs"),
            b"pub fn ignored() {}\n",
        )
        .unwrap();
        std::fs::write(root.join("ignored-by-gitignore.txt"), b"skip").unwrap();
        std::fs::write(root.join("node_modules/pkg/index.js"), b"skip").unwrap();
        std::fs::write(root.join(".venv/pyvenv.cfg"), b"skip").unwrap();
        std::fs::write(root.join("dist/bundle.js"), b"skip").unwrap();
        std::fs::write(root.join("build/artifact"), b"skip").unwrap();
        std::fs::write(root.join("services/api/src/lib.rs"), b"pub fn api() {}\n").unwrap();
        std::fs::write(root.join("services/api/node_modules/pkg/index.js"), b"skip").unwrap();
        std::fs::write(root.join("services/api/build/artifact"), b"skip").unwrap();
        std::fs::write(root.join("target/debug/build-artifact"), b"skip").unwrap();
        std::fs::write(root.join(".git/HEAD"), b"skip").unwrap();
        std::fs::write(root.join(".togi/cache"), b"skip").unwrap();
        std::fs::write(root.join(".togi-cache/cache-entry"), b"skip").unwrap();
        std::fs::write(root.join(".togi.lock"), b"skip").unwrap();
        std::fs::write(root.join(".codex/session"), b"skip").unwrap();
        std::fs::write(root.join(".claude/session"), b"skip").unwrap();

        let copy = copy_workspace(root).unwrap();

        assert_eq!(
            std::fs::read(copy.root().join("src/lib.rs")).unwrap(),
            b"pub fn f() {}\n"
        );
        assert!(copy.root().join("Cargo.toml").exists());
        assert!(!copy.root().join("src/ignored_by_ignore.rs").exists());
        assert!(!copy.root().join("ignored-by-gitignore.txt").exists());
        assert!(!copy.root().join("node_modules").exists());
        assert!(!copy.root().join(".venv").exists());
        assert!(!copy.root().join("dist").exists());
        assert!(!copy.root().join("build").exists());
        assert!(copy.root().join("services/api/src/lib.rs").exists());
        assert!(!copy.root().join("services/api/node_modules").exists());
        assert!(!copy.root().join("services/api/build").exists());
        assert!(!copy.root().join("target").exists());
        assert!(!copy.root().join(".git").exists());
        assert!(!copy.root().join(".togi").exists());
        assert!(!copy.root().join(".togi-cache").exists());
        assert!(!copy.root().join(".togi.lock").exists());
        assert!(!copy.root().join(".codex").exists());
        assert!(!copy.root().join(".claude").exists());
    }

    #[test]
    fn normal_workspace_copy_retains_mixed_case_unrelated_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        for (relative, contents) in [
            ("Target/cache", b"target" as &[u8]),
            ("Build/artifact", b"build"),
            (".Togi-Custom/state", b"custom"),
        ] {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }

        let copy = copy_workspace_with_options(root, false).unwrap();

        assert_eq!(
            std::fs::read(copy.root().join("Target/cache")).unwrap(),
            b"target"
        );
        assert_eq!(
            std::fs::read(copy.root().join("Build/artifact")).unwrap(),
            b"build"
        );
        assert_eq!(
            std::fs::read(copy.root().join(".Togi-Custom/state")).unwrap(),
            b"custom"
        );
    }

    #[test]
    fn copy_workspace_can_include_ignored_files_when_requested() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::write(root.join(".gitignore"), b"ignored.txt\n").unwrap();
        std::fs::write(root.join("ignored.txt"), b"copy me").unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub fn f() {}\n").unwrap();
        std::fs::write(root.join("target/debug/build-artifact"), b"skip").unwrap();

        let copy = copy_workspace_with_options(root, false).unwrap();

        assert_eq!(
            std::fs::read(copy.root().join("ignored.txt")).unwrap(),
            b"copy me"
        );
        assert!(copy.root().join("src/lib.rs").exists());
        assert!(!copy.root().join("target").exists());
    }

    #[test]
    fn reset_workspace_preserves_target_cache_and_removes_other_side_effects() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("source");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub fn f() {}\n").unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(workspace.join("target/debug")).unwrap();
        std::fs::write(workspace.join("target/debug/cache"), b"keep").unwrap();
        std::fs::write(workspace.join("side-effect"), b"drop").unwrap();

        reset_copied_workspace(&root, &workspace, true).unwrap();

        assert_eq!(
            std::fs::read(workspace.join("target/debug/cache")).unwrap(),
            b"keep"
        );
        assert!(workspace.join("src/lib.rs").exists());
        assert!(!workspace.join("side-effect").exists());
    }

    #[test]
    fn reset_workspace_handles_missing_workspace_target() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("source");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub fn f() {}\n").unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(workspace.join("side-effect"), b"drop").unwrap();

        reset_copied_workspace(&root, &workspace, true).unwrap();

        assert!(workspace.join("src/lib.rs").exists());
        assert!(!workspace.join("side-effect").exists());
        assert!(!workspace.join("target/debug/cache").exists());
    }

    #[test]
    fn copy_workspace_uses_git_worktree_for_clean_repo_root() {
        if !git_available() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_clean_git_fixture(root);

        let copy = copy_workspace(root).unwrap();

        assert!(copy.root().join(".git").exists());
        assert_eq!(
            std::fs::read(copy.root().join("src/lib.rs")).unwrap(),
            b"pub fn f() {}\n"
        );

        std::fs::write(copy.root().join("src/lib.rs"), b"pub fn f() -> i32 { 2 }\n").unwrap();
        std::fs::write(copy.root().join("side-effect"), b"drop").unwrap();
        std::fs::create_dir_all(copy.root().join("target/debug")).unwrap();
        std::fs::write(copy.root().join("target/debug/cache"), b"keep").unwrap();

        copy.reset(root, true).unwrap();

        assert_eq!(
            std::fs::read(copy.root().join("src/lib.rs")).unwrap(),
            b"pub fn f() {}\n"
        );
        assert!(!copy.root().join("side-effect").exists());
        assert_eq!(
            std::fs::read(copy.root().join("target/debug/cache")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn replay_workspace_is_independent_git_snapshot_without_source_admin_residue() {
        if !git_available() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_clean_git_fixture(root);
        std::fs::write(root.join(".TOGI-BASELINE"), b"control").unwrap();
        std::fs::write(root.join("src/deleted.rs"), b"stale").unwrap();
        run_git(root, &["add", ".TOGI-BASELINE", "src/deleted.rs"]);
        run_git(root, &["commit", "-m", "add snapshot controls"]);
        let source_revision = git_snapshot_revision(root).unwrap();
        std::fs::remove_file(root.join("src/deleted.rs")).unwrap();
        std::fs::create_dir_all(root.join(".TOGI-CACHE")).unwrap();
        std::fs::write(root.join(".TOGI-CACHE/cache"), b"control").unwrap();
        let worktrees = root.join(".git/worktrees");
        assert!(!worktrees.exists());

        let workspace_root;
        {
            let copy = copy_workspace_for_replay(root, &source_revision, true).unwrap();
            workspace_root = copy.root().to_path_buf();
            let git_dir = copy.root().join(".git");
            let metadata = std::fs::symlink_metadata(&git_dir).unwrap();
            assert!(metadata.file_type().is_dir());
            assert!(!metadata.file_type().is_symlink());
            assert_eq!(
                std::fs::read(copy.root().join("src/lib.rs")).unwrap(),
                b"pub fn f() {}\n"
            );
            assert!(!copy.root().join(".TOGI-BASELINE").exists());
            assert!(!copy.root().join(".TOGI-CACHE").exists());
            assert!(!copy.root().join("src/deleted.rs").exists());
            assert!(!worktrees.exists());
        }

        assert!(!workspace_root.exists());
        assert!(!worktrees.exists());
    }

    #[test]
    fn replay_snapshot_rejects_mismatched_expected_revision() {
        if !git_available() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        init_clean_git_fixture(root);
        let actual = git_snapshot_revision(root).unwrap();
        let expected = "0".repeat(actual.len());
        assert_ne!(actual, expected);
        assert!(copy_workspace_for_replay(root, &expected, true).is_err());
        assert!(!root.join(".git/worktrees").exists());
    }

    #[test]
    fn copy_workspace_uses_git_worktree_with_dirty_overlay() -> std::io::Result<()> {
        if !git_available() {
            return Ok(());
        }

        let tmp = tempfile::tempdir()?;
        let root = tmp.path();
        init_clean_git_fixture(root);
        std::fs::write(root.join("src/old.rs"), b"pub fn old() {}\n")?;
        std::fs::write(root.join("src/replaced"), b"old file\n")?;
        run_git(root, &["add", "."]);
        run_git(root, &["commit", "-m", "add old file"]);

        std::fs::write(root.join("src/lib.rs"), b"pub fn f() -> i32 { 1 }\n")?;
        std::fs::remove_file(root.join("src/old.rs"))?;
        std::fs::remove_file(root.join("src/replaced"))?;
        std::fs::create_dir(root.join("src/replaced"))?;
        std::fs::write(root.join("src/replaced/nested.rs"), b"pub fn nested() {}\n")?;
        std::fs::write(root.join("local.txt"), b"copy me")?;

        let copy = copy_workspace(root)?;

        assert!(copy.root().join(".git").exists());
        assert_eq!(
            std::fs::read(copy.root().join("src/lib.rs"))?,
            b"pub fn f() -> i32 { 1 }\n"
        );
        assert_eq!(std::fs::read(copy.root().join("local.txt"))?, b"copy me");
        assert!(!copy.root().join("src/old.rs").exists());
        assert_eq!(
            std::fs::read(copy.root().join("src/replaced/nested.rs"))?,
            b"pub fn nested() {}\n"
        );

        std::fs::write(copy.root().join("src/lib.rs"), b"side effect")?;
        std::fs::write(copy.root().join("local.txt"), b"changed")?;
        std::fs::write(copy.root().join("src/old.rs"), b"stale")?;
        std::fs::remove_dir_all(copy.root().join("src/replaced"))?;
        std::fs::write(copy.root().join("src/replaced"), b"stale file")?;
        std::fs::write(copy.root().join("side-effect"), b"drop")?;
        std::fs::create_dir_all(copy.root().join("target/debug"))?;
        std::fs::write(copy.root().join("target/debug/cache"), b"keep")?;

        copy.reset(root, true)?;

        assert_eq!(
            std::fs::read(copy.root().join("src/lib.rs"))?,
            b"pub fn f() -> i32 { 1 }\n"
        );
        assert_eq!(std::fs::read(copy.root().join("local.txt"))?, b"copy me");
        assert!(!copy.root().join("src/old.rs").exists());

        assert_eq!(
            std::fs::read(copy.root().join("src/replaced/nested.rs"))?,
            b"pub fn nested() {}\n"
        );
        assert!(!copy.root().join("side-effect").exists());
        assert_eq!(
            std::fs::read(copy.root().join("target/debug/cache"))?,
            b"keep"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn normal_git_worktree_overlay_follows_dirty_and_untracked_regular_source_symlinks()
    -> std::io::Result<()> {
        use std::os::unix::fs::symlink;

        if !git_available() {
            return Ok(());
        }

        let tmp = tempfile::tempdir()?;
        let root = tmp.path();
        init_clean_git_fixture(root);
        std::fs::write(root.join("target.txt"), b"linked contents")?;
        std::fs::write(root.join("tracked.txt"), b"original contents")?;
        run_git(root, &["add", "target.txt", "tracked.txt"]);
        run_git(root, &["commit", "-m", "add overlay files"]);

        std::fs::remove_file(root.join("tracked.txt"))?;
        symlink("target.txt", root.join("tracked.txt"))?;
        symlink("target.txt", root.join("untracked-link.txt"))?;

        let copy = copy_workspace(root)?;

        for relative in ["tracked.txt", "untracked-link.txt"] {
            let destination = copy.root().join(relative);
            assert_eq!(std::fs::read(&destination)?, b"linked contents");
            assert!(
                !std::fs::symlink_metadata(destination)?
                    .file_type()
                    .is_symlink(),
                "{relative} should retain normal overlay copy semantics"
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn copy_workspace_falls_back_for_escaping_untracked_symlink() -> std::io::Result<()> {
        use std::os::unix::fs::symlink;

        if !git_available() {
            return Ok(());
        }

        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("repo");
        std::fs::create_dir(&root)?;
        init_clean_git_fixture(&root);
        let outside = tmp.path().join("outside.rs");
        let link = root.join("untracked-link.rs");
        std::fs::write(&outside, b"outside")?;
        symlink(&outside, &link)?;

        let copy = copy_workspace(&root)?;

        assert!(matches!(&copy.reset_strategy, WorkspaceResetStrategy::Copy));
        assert!(!copy.root().join("untracked-link.rs").exists());
        assert_eq!(std::fs::read(&outside)?, b"outside");
        assert!(std::fs::symlink_metadata(&link)?.file_type().is_symlink());
        assert!(!root.join(".git/worktrees").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn normal_git_worktree_overlay_replaces_destination_symlink() -> std::io::Result<()> {
        use std::os::unix::fs::symlink;

        if !git_available() {
            return Ok(());
        }

        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("repo");
        std::fs::create_dir(&root)?;
        init_clean_git_fixture(&root);
        let sentinel = tmp.path().join("outside");
        std::fs::write(&sentinel, b"outside sentinel")?;
        let link = root.join("link.txt");
        symlink(&sentinel, &link)?;
        run_git(&root, &["add", "link.txt"]);
        run_git(&root, &["commit", "-m", "add linked file"]);

        std::fs::remove_file(&link)?;
        std::fs::write(&link, b"overlay contents")?;

        let copy = copy_workspace(&root)?;
        let destination = copy.root().join("link.txt");
        assert_eq!(std::fs::read(&sentinel)?, b"outside sentinel");
        assert!(
            std::fs::symlink_metadata(&destination)?
                .file_type()
                .is_file()
        );
        assert_eq!(std::fs::read(destination)?, b"overlay contents");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn normal_overlay_rejects_escaping_and_control_symlink_sources() -> std::io::Result<()> {
        use std::os::unix::fs::symlink;

        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("project");
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir(&root)?;
        std::fs::create_dir(&workspace)?;
        let outside = tmp.path().join("outside");
        std::fs::write(&outside, b"outside sentinel")?;
        symlink(&outside, root.join("outside-link"))?;

        let error = copy_overlay_file(&root, &workspace, Path::new("outside-link")).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("does not resolve within the project root")
        );
        assert_eq!(std::fs::read(&outside)?, b"outside sentinel");

        let control = root.join(".TOGI-CACHE/cache");
        std::fs::create_dir_all(control.parent().unwrap())?;
        std::fs::write(&control, b"control sentinel")?;
        symlink(".TOGI-CACHE/cache", root.join("control-link"))?;

        let error = copy_overlay_file(&root, &workspace, Path::new("control-link")).unwrap_err();
        assert!(error.to_string().contains("Togi or Git control state"));
        assert_eq!(std::fs::read(control)?, b"control sentinel");
        Ok(())
    }

    #[test]
    fn baseline_timing_measures_build_and_test_in_workspace() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("source.txt"), "clean")?;
        let command = successful_command();

        let measurement = measure_baseline_timing(
            dir.path(),
            BaselineTimingConfig {
                test_command: &command,
                build_command: &command,
                sandbox_command: &[],
                build_command_origin: BuildCommandOrigin::Configured,
                timeout: Duration::from_secs(5),
                env: &HashMap::new(),
                cancelled: &AtomicBool::new(false),
                respect_workspace_ignores: false,
            },
        )?;

        assert!(measurement.build_duration.is_some());
        assert!(measurement.test_duration <= Duration::from_secs(5));
        Ok(())
    }
    #[test]
    fn baseline_suites_ignore_line_to_test_narrowing() {
        let dir = tempfile::tempdir().unwrap();
        let mut commands = test_command_config();
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            dir.path(),
            Path::new("src/lib.rs"),
            1,
            vec!["only_this_test".into()],
        );
        commands.test_selection = Some(selection);
        let mutation = make_test_mutation(Path::new("src/lib.rs"));

        let suites = resolve_baseline_suites(dir.path(), &commands, &[mutation], false);

        assert_eq!(suites.len(), 1);
        assert_eq!(suites[0].argv, vec!["cargo", "test"]);
        assert!(suites[0].default_timeout.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn baseline_health_isolates_source_and_target_between_routes() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("project");
        let log = dir.path().join("baseline.log");
        std::fs::create_dir_all(root.join("services/api"))?;
        std::fs::create_dir_all(root.join("src"))?;
        std::fs::write(root.join("src/lib.rs"), "clean")?;
        std::fs::write(root.join("worker.go"), "package worker")?;
        std::fs::write(root.join("services/api/main.go"), "package api")?;

        let global_command = vec![
            "sh".into(),
            "-c".into(),
            "test \"$(cat target/build-state)\" = generated || exit 1; echo global >> \"$BASELINE_LOG\"; printf changed > src/lib.rs; printf leaked > target/from-global".into(),
        ];
        let language_command = vec![
            "sh".into(),
            "-c".into(),
            "test \"$(cat src/lib.rs)\" = clean && test \"$(cat target/build-state)\" = generated && test ! -e target/from-global || exit 1; echo language >> \"$BASELINE_LOG\"; printf leaked > target/from-language".into(),
        ];
        let project_command = vec![
            "sh".into(),
            "-c".into(),
            "test \"$(cat src/lib.rs)\" = clean && test \"$(cat target/build-state)\" = generated && test ! -e target/from-global && test ! -e target/from-language || exit 1; echo project >> \"$BASELINE_LOG\"".into(),
        ];
        let build_command = vec![
            "sh".into(),
            "-c".into(),
            "mkdir -p target; printf generated > target/build-state; echo build >> \"$BASELINE_LOG\"".into(),
        ];
        let mut commands = test_command_config();
        commands.command = global_command.clone();
        commands
            .language_commands
            .insert("go".into(), language_command);
        commands.project_commands.push(ProjectCommandConfig {
            path: PathBuf::from("services/api"),
            command: Some(project_command),
            timeout: None,
        });
        commands.build_command = build_command;
        commands.build_command_origin = BuildCommandOrigin::Configured;

        let mut env = HashMap::new();
        env.insert(
            "BASELINE_LOG".to_string(),
            log.to_string_lossy().into_owned(),
        );
        let mut language_mutation = make_test_mutation(Path::new("worker.go"));
        language_mutation.language = "go".into();
        let mut project_mutation = make_test_mutation(Path::new("services/api/main.go"));
        project_mutation.language = "go".into();
        let mutations = vec![
            make_test_mutation(Path::new("src/lib.rs")),
            language_mutation.clone(),
            project_mutation,
            language_mutation,
        ];

        let measurement = check_baseline_health(
            &root,
            &mutations,
            BaselineHealthConfig {
                commands: &commands,
                default_measurement_timeout: None,
                schemata_enabled: false,
                env: &env,
                cancelled: &AtomicBool::new(false),
                respect_workspace_ignores: false,
            },
        )?;

        assert_eq!(measurement.suites.len(), 3);
        assert_eq!(measurement.suites[0].test_command, global_command);
        assert_eq!(
            std::fs::read_to_string(log)?.lines().collect::<Vec<_>>(),
            vec!["build", "global", "build", "language", "build", "project"]
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn baseline_health_exposes_structured_run_suite_failure() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("subject.rs");
        std::fs::write(&file, "fn subject() {}").unwrap();
        let mutation = make_test_mutation(&file);
        let mut commands = test_command_config();
        commands.command = failing_command();
        let cancelled = AtomicBool::new(false);

        let error = check_baseline_health(
            dir.path(),
            &[mutation],
            BaselineHealthConfig {
                commands: &commands,
                default_measurement_timeout: None,
                schemata_enabled: false,
                env: &HashMap::new(),
                cancelled: &cancelled,
                respect_workspace_ignores: true,
            },
        )
        .unwrap_err();

        let failure = run_suite_failure(&error).expect("baseline failure should be structured");
        assert_eq!(failure.phase, SuiteFailurePhase::Test);
        assert_eq!(failure.command, failing_command());
        assert!(matches!(
            &failure.outcome,
            RunSuiteFailureOutcome::Failed { .. }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn baseline_health_uses_explicit_route_timeout_while_calibrating() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("worker.go"), "package worker").unwrap();
        let mut commands = test_command_config();
        commands.language_commands.insert(
            "go".into(),
            vec!["sh".into(), "-c".into(), "sleep 1".into()],
        );
        commands
            .language_timeouts
            .insert("go".into(), Duration::from_millis(50));
        let mut mutation = make_test_mutation(Path::new("worker.go"));
        mutation.language = "go".into();

        let err = check_baseline_health(
            dir.path(),
            &[mutation],
            BaselineHealthConfig {
                commands: &commands,
                default_measurement_timeout: Some(Duration::from_secs(60)),
                schemata_enabled: false,
                env: &HashMap::new(),
                cancelled: &AtomicBool::new(false),
                respect_workspace_ignores: false,
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("timed out after 0.05s"));
    }

    #[cfg(unix)]
    #[test]
    fn baseline_health_checks_identical_commands_at_each_effective_deadline() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("default.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("worker.go"), "package worker").unwrap();
        let command = vec!["sh".into(), "-c".into(), "sleep 2".into()];
        let mut commands = test_command_config();
        commands.command = command.clone();
        commands.timeout = Duration::from_secs(1);
        commands.language_commands.insert("go".into(), command);
        commands
            .language_timeouts
            .insert("go".into(), Duration::from_secs(5));
        let mut language_mutation = make_test_mutation(Path::new("worker.go"));
        language_mutation.language = "go".into();

        let err = check_baseline_health(
            dir.path(),
            &[
                make_test_mutation(Path::new("default.rs")),
                language_mutation,
            ],
            BaselineHealthConfig {
                commands: &commands,
                default_measurement_timeout: None,
                schemata_enabled: false,
                env: &HashMap::new(),
                cancelled: &AtomicBool::new(false),
                respect_workspace_ignores: false,
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("timed out after 1.00s"));
    }

    #[cfg(unix)]
    #[test]
    fn baseline_health_keeps_explicit_identical_deadline_while_calibrating_in_reverse_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("default.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("worker.go"), "package worker").unwrap();
        let command = vec!["sh".into(), "-c".into(), "sleep 1".into()];
        let mut commands = test_command_config();
        commands.command = command.clone();
        commands.language_commands.insert("go".into(), command);
        commands
            .language_timeouts
            .insert("go".into(), Duration::from_millis(50));
        let mut language_mutation = make_test_mutation(Path::new("worker.go"));
        language_mutation.language = "go".into();

        let err = check_baseline_health(
            dir.path(),
            &[
                language_mutation,
                make_test_mutation(Path::new("default.rs")),
            ],
            BaselineHealthConfig {
                commands: &commands,
                default_measurement_timeout: Some(Duration::from_secs(60)),
                schemata_enabled: false,
                env: &HashMap::new(),
                cancelled: &AtomicBool::new(false),
                respect_workspace_ignores: false,
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("timed out after 0.05s"));
    }

    #[cfg(unix)]
    #[test]
    fn baseline_health_uses_explicit_project_timeout_while_calibrating() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("services/api")).unwrap();
        std::fs::write(dir.path().join("services/api/main.go"), "package api").unwrap();
        let mut commands = test_command_config();
        commands.project_commands.push(ProjectCommandConfig {
            path: PathBuf::from("services/api"),
            command: Some(vec!["sh".into(), "-c".into(), "sleep 1".into()]),
            timeout: Some(Duration::from_millis(50)),
        });

        let err = check_baseline_health(
            dir.path(),
            &[make_test_mutation(Path::new("services/api/main.go"))],
            BaselineHealthConfig {
                commands: &commands,
                default_measurement_timeout: Some(Duration::from_secs(60)),
                schemata_enabled: false,
                env: &HashMap::new(),
                cancelled: &AtomicBool::new(false),
                respect_workspace_ignores: false,
            },
        )
        .unwrap_err();

        assert!(err.to_string().contains("timed out after 0.05s"));
    }

    #[test]
    fn workspace_pool_creates_slots_and_reuses_after_drop() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub fn f() {}\n").unwrap();

        let pool = WorkspacePool::new(root, 2).unwrap();
        assert_eq!(pool.len(), 2);

        let first = pool.acquire();
        let second = pool.acquire();
        assert!(!first.needs_reset());
        assert!(!second.needs_reset());
        assert_ne!(first.root(), second.root());
        assert!(first.root().join("src/lib.rs").exists());
        assert!(second.root().join("src/lib.rs").exists());

        let first_root = first.root().to_path_buf();
        let second_root = second.root().to_path_buf();
        drop(second);

        let third = pool.acquire();
        assert!(third.needs_reset());
        assert_eq!(third.root(), second_root.as_path());
        assert_ne!(third.root(), first_root.as_path());
    }

    #[test]
    fn workspace_pool_uses_at_least_one_slot() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file.txt"), b"content").unwrap();

        let pool = WorkspacePool::new(tmp.path(), 0).unwrap();

        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn workspace_pool_slot_count_is_bounded_by_total_mutations() {
        assert_eq!(workspace_pool_slot_count(0, 3), 1);
        assert_eq!(workspace_pool_slot_count(1, 3), 1);
        assert_eq!(workspace_pool_slot_count(2, 5), 2);
        assert_eq!(workspace_pool_slot_count(8, 3), 3);
    }

    #[test]
    fn command_succeeds_returns_survived() {
        let (dir, file, mutation) = make_relative_test_setup();

        let outcome = run_single_mutation(
            &["true".to_string()],
            &[],
            BuildCommand {
                argv: &[],
                origin: BuildCommandOrigin::None,
            },
            Duration::from_secs(5),
            dir.path(),
            ResolvedMutation::new(dir.path(), &mutation),
            false,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::Survived);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[test]
    fn command_fails_returns_killed() {
        let (dir, file, mutation) = make_test_setup();

        let outcome = run_single_mutation(
            &["false".to_string()],
            &[],
            BuildCommand {
                argv: &[],
                origin: BuildCommandOrigin::None,
            },
            Duration::from_secs(5),
            dir.path(),
            ResolvedMutation::new(dir.path(), &mutation),
            false,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::Killed);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[test]
    fn fuzz_diff_to_workspace_boundary_preserves_sources() {
        let seed_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/diffs");
        let mut seeds: Vec<Vec<u8>> = std::fs::read_dir(seed_dir)
            .unwrap()
            .map(|entry| std::fs::read(entry.unwrap().path()).unwrap())
            .collect();
        seeds.sort();
        assert!(!seeds.is_empty());

        for seed in seeds {
            let tmp = tempfile::tempdir().unwrap();
            let case_root = tmp.path();
            let project = case_root.join("project");
            std::fs::create_dir_all(&project).unwrap();
            let sentinel = case_root.join("outside.go");
            let sentinel_bytes = b"package main // sentinel\n";
            std::fs::write(&sentinel, sentinel_bytes).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(&sentinel, project.join("link.go")).unwrap();

            let changed = crate::diff::parse_diff(&String::from_utf8_lossy(&seed));
            for file in &changed {
                assert!(crate::source_identity::is_normalized_project_relative_path(
                    &file.path.to_string_lossy()
                ));
            }

            let source = b"package main\n\nfunc f() int {\n\treturn 1 + 2\n}\n";
            std::fs::write(project.join("main.go"), source).unwrap();
            let mut files = vec![crate::ChangedFile {
                path: PathBuf::from("main.go"),
                hunks: vec![crate::LineRange { start: 1, end: 5 }],
            }];
            files.extend(changed.into_iter().take(3));
            let mutations = crate::mutator::generate_mutations(&files, &project, 32, 0, &[])
                .expect("parsed inputs must not abort mutation generation");
            assert!(!mutations.is_empty());

            for mutation in mutations.iter().take(4) {
                let path = project.join(&mutation.file);
                let original = std::fs::read(&path).unwrap();
                assert_eq!(
                    &original[mutation.byte_range.clone()],
                    mutation.original.as_bytes()
                );
                let outcome = run_single_mutation(
                    &["true".to_string()],
                    &[],
                    BuildCommand {
                        argv: &[],
                        origin: BuildCommandOrigin::None,
                    },
                    Duration::from_secs(5),
                    &project,
                    ResolvedMutation::new(&project, mutation),
                    false,
                    &HashMap::new(),
                    &AtomicBool::new(false),
                );
                assert_eq!(outcome.result, MutationResult::Survived);
                assert_eq!(std::fs::read(path).unwrap(), original);
            }

            for path in [
                PathBuf::from("../outside.go"),
                sentinel.clone(),
                PathBuf::from("link.go"),
            ] {
                let mutation = Mutation {
                    id: 1,
                    file: path,
                    language: "Go".into(),
                    line: 1,
                    column: 1,
                    operator: "boundary_fuzz".into(),
                    description: "adversarial".into(),
                    original: "package".into(),
                    replacement: "PWNED!!".into(),
                    byte_range: 0..7,
                };
                let outcome = run_single_mutation(
                    &["true".to_string()],
                    &[],
                    BuildCommand {
                        argv: &[],
                        origin: BuildCommandOrigin::None,
                    },
                    Duration::from_secs(5),
                    &project,
                    ResolvedMutation::new(&project, &mutation),
                    false,
                    &HashMap::new(),
                    &AtomicBool::new(false),
                );
                assert_eq!(outcome.result, MutationResult::BuildError);
            }
            assert_eq!(std::fs::read(sentinel).unwrap(), sentinel_bytes);
        }
    }

    #[test]
    fn empty_replacement_splices_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();

        // Replace "hello" with "" (empty), making the file shorter
        let mutation = Mutation {
            id: 1,
            file: file.clone(),
            language: String::new(),
            line: 1,
            column: 1,
            operator: "removal".into(),
            description: "remove text".into(),
            original: "hello".into(),
            replacement: "".into(),
            byte_range: 0..5,
        };

        let outcome = run_single_mutation(
            &["true".to_string()],
            &[],
            BuildCommand {
                argv: &[],
                origin: BuildCommandOrigin::None,
            },
            Duration::from_secs(5),
            dir.path(),
            ResolvedMutation::new(dir.path(), &mutation),
            false,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::Survived);
        // File should be restored to original
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[test]
    fn command_not_found_returns_build_error() {
        let (dir, file, mutation) = make_test_setup();

        let outcome = run_single_mutation(
            &["nonexistent_binary_xyz_12345".to_string()],
            &[],
            BuildCommand {
                argv: &[],
                origin: BuildCommandOrigin::None,
            },
            Duration::from_secs(5),
            dir.path(),
            ResolvedMutation::new(dir.path(), &mutation),
            false,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::BuildError);
        // File should be restored
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[test]
    fn command_timeout_returns_timeout() {
        let (dir, file, mutation) = make_test_setup();

        let outcome = run_single_mutation(
            &["sleep".to_string(), "10".to_string()],
            &[],
            BuildCommand {
                argv: &[],
                origin: BuildCommandOrigin::None,
            },
            Duration::from_millis(100),
            dir.path(),
            ResolvedMutation::new(dir.path(), &mutation),
            false,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::Timeout);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[test]
    fn empty_build_error_output_falls_back_to_command_context() {
        let outcome = MutationOutcome::new(MutationResult::Killed, Some(String::new()));
        let message = build_error_message_from_outcome(
            "build command",
            &["cargo".to_string(), "check".to_string()],
            Path::new("/tmp/workspace"),
            &outcome,
        );

        assert!(message.contains("build command returned killed"));
        assert!(message.contains("command: cargo check"));
        assert!(message.contains("cwd: /tmp/workspace"));
    }

    #[cfg(unix)]
    #[test]
    fn build_check_failure_skips_test() {
        let (dir, file, mutation) = make_test_setup();

        let marker = dir.path().join("test_ran.marker");
        // Build fails → should return BuildError without running test
        let outcome = run_single_mutation(
            &[
                "sh".to_string(),
                "-c".to_string(),
                format!("touch {}", marker.display()),
            ],
            &[],
            BuildCommand {
                argv: &[
                    "sh".to_string(),
                    "-c".to_string(),
                    "echo compile nope >&2; exit 1".to_string(),
                ],
                origin: BuildCommandOrigin::Configured,
            },
            Duration::from_secs(5),
            dir.path(),
            ResolvedMutation::new(dir.path(), &mutation),
            false,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::BuildError);
        let detail = outcome
            .build_error_detail
            .as_ref()
            .expect("build failure should carry diagnostics");
        assert_eq!(detail.phase, "build_command");
        assert!(detail.message.contains("compile nope"));
        assert!(!marker.exists(), "test command should not have run");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[test]
    fn build_check_success_runs_test() {
        let (dir, _file, mutation) = make_test_setup();

        // Build succeeds → test runs and fails → Killed
        let outcome = run_single_mutation(
            &["false".to_string()], // test fails = killed
            &[],
            BuildCommand {
                argv: &["true".to_string()], // build succeeds
                origin: BuildCommandOrigin::Configured,
            },
            Duration::from_secs(5),
            dir.path(),
            ResolvedMutation::new(dir.path(), &mutation),
            false,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::Killed);
    }

    #[cfg(unix)]
    #[test]
    fn auto_detected_build_command_does_not_pre_filter() {
        let (dir, _file, mutation) = make_test_setup();

        let build_marker = dir.path().join("build_ran.marker");
        let test_marker = dir.path().join("test_ran.marker");

        let outcome = run_single_mutation(
            &[
                "sh".to_string(),
                "-c".to_string(),
                format!("touch {}; exit 1", test_marker.display()),
            ],
            &[],
            BuildCommand {
                argv: &[
                    "sh".to_string(),
                    "-c".to_string(),
                    format!("touch {}; exit 1", build_marker.display()),
                ],
                origin: BuildCommandOrigin::AutoDetected,
            },
            Duration::from_secs(5),
            dir.path(),
            ResolvedMutation::new(dir.path(), &mutation),
            false,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::Killed);
        assert!(
            !build_marker.exists(),
            "auto-detected build suggestion must not run"
        );
        assert!(test_marker.exists(), "test command should run directly");
    }

    #[cfg(unix)]
    #[test]
    fn configured_go_build_separates_compile_errors_from_test_kills() {
        if std::process::Command::new("go")
            .arg("version")
            .output()
            .is_err()
        {
            eprintln!("skipping Go build classification test because go is unavailable");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("go.mod"), "module example.com/calculator\n").unwrap();
        let source = "package calculator\n\nfunc Value() int { return 1 }\n";
        let file = dir.path().join("calculator.go");
        std::fs::write(&file, source).unwrap();
        for (path, content) in [
            ("one/shared/shared.go", "package shared\nfunc One() {}\n"),
            ("two/shared/shared.go", "package shared\nfunc Two() {}\n"),
        ] {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let test_only_source = r#"package main

import (
    "fmt"
    "os"
    "testing"
)

func mark() {
    if marker := os.Getenv("TOGI_GO_TEST_MARKER"); marker != "" {
        _ = os.WriteFile(marker, []byte("ran"), 0o600)
    }
}

func init() {
    mark()
}

func TestMain(m *testing.M) {
    mark()
    os.Exit(m.Run())
}

func TestVetOnly(t *testing.T) {
    fmt.Printf("%d", "not-an-int")
}
"#;
        let test_only_file = dir.path().join("testonly/main_test.go");
        std::fs::create_dir_all(test_only_file.parent().unwrap()).unwrap();
        std::fs::write(&test_only_file, test_only_source).unwrap();

        let offset = source.rfind('1').unwrap();
        let mutation = |id: u32, replacement: &str| Mutation {
            id,
            file: PathBuf::from("calculator.go"),
            language: "Go".into(),
            line: 3,
            column: offset + 1,
            operator: "binary".into(),
            description: format!("replace 1 with {replacement}"),
            original: "1".into(),
            replacement: replacement.into(),
            byte_range: offset..offset + 1,
        };
        let mut config = crate::config::Config::default();
        assert_eq!(
            config.resolve_build_command(dir.path()),
            BuildCommandOrigin::AutoDetected
        );
        assert_eq!(
            config.test.build_command,
            crate::config::AUTO_GO_COMPILE_COMMAND
                .iter()
                .map(|argument| (*argument).to_string())
                .collect::<Vec<_>>()
        );
        let build_command = config.test.build_command.clone();
        config.test.set_build_command(build_command);
        let origin = config.test.build_command_origin;
        assert_eq!(origin, BuildCommandOrigin::Configured);

        let test_marker = dir.path().join("test_ran.marker");
        let go_runtime_marker = dir.path().join("go_runtime.marker");
        let failing_test = vec![
            "sh".into(),
            "-c".into(),
            format!("touch {}; exit 1", test_marker.display()),
        ];
        let mut env = HashMap::new();
        env.insert(
            "TOGI_GO_TEST_MARKER".into(),
            go_runtime_marker.display().to_string(),
        );

        let invalid = mutation(1, "+");
        let invalid_outcome = run_single_mutation(
            &failing_test,
            &[],
            BuildCommand {
                argv: &config.test.build_command,
                origin,
            },
            Duration::from_secs(30),
            dir.path(),
            ResolvedMutation::new(dir.path(), &invalid),
            false,
            &env,
            &AtomicBool::new(false),
        );
        assert_eq!(invalid_outcome.result, MutationResult::BuildError);
        assert_eq!(
            invalid_outcome
                .build_error_detail
                .as_ref()
                .map(|detail| detail.phase.as_str()),
            Some("build_command")
        );
        assert!(
            !test_marker.exists(),
            "the test command must not be credited for a source-invalid mutant"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), source);

        let test_literal = "\"not-an-int\"";
        let test_offset = test_only_source.find(test_literal).unwrap();
        let invalid_test = Mutation {
            id: 2,
            file: PathBuf::from("testonly/main_test.go"),
            language: "Go".into(),
            line: 24,
            column: test_offset + 1,
            operator: "string".into(),
            description: "break test-only source".into(),
            original: test_literal.into(),
            replacement: "\"".into(),
            byte_range: test_offset..test_offset + test_literal.len(),
        };
        let invalid_test_outcome = run_single_mutation(
            &failing_test,
            &[],
            BuildCommand {
                argv: &config.test.build_command,
                origin,
            },
            Duration::from_secs(30),
            dir.path(),
            ResolvedMutation::new(dir.path(), &invalid_test),
            false,
            &env,
            &AtomicBool::new(false),
        );
        assert_eq!(invalid_test_outcome.result, MutationResult::BuildError);
        assert!(
            !test_marker.exists(),
            "the test command must not be credited for a test-invalid mutant"
        );
        assert_eq!(
            std::fs::read_to_string(&test_only_file).unwrap(),
            test_only_source
        );

        let valid = mutation(3, "2");
        let valid_outcome = run_single_mutation(
            &failing_test,
            &[],
            BuildCommand {
                argv: &config.test.build_command,
                origin,
            },
            Duration::from_secs(30),
            dir.path(),
            ResolvedMutation::new(dir.path(), &valid),
            false,
            &env,
            &AtomicBool::new(false),
        );
        assert_eq!(valid_outcome.result, MutationResult::Killed);
        assert!(
            test_marker.exists(),
            "a compilable mutant must reach the test command"
        );
        assert!(
            !go_runtime_marker.exists(),
            "the compile check must not execute init or TestMain"
        );
        assert!(
            dir.path().read_dir().unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".test")),
            "the compile check must not leave Go test binaries in the workspace"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), source);
    }

    #[cfg(unix)]
    #[test]
    fn configured_dotnet_build_restores_ignored_assets_in_isolated_workspaces() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let source = "class Calculator { static bool Value() => true; }\n";
        std::fs::write(
            dir.path().join("Calculator.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\" />\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("Calculator.cs"), source).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "obj/\n").unwrap();
        let source_assets = dir.path().join("obj/project.assets.json");
        std::fs::create_dir_all(source_assets.parent().unwrap()).unwrap();
        std::fs::write(&source_assets, "source-assets").unwrap();

        let bin = dir.path().join("fake-dotnet-bin");
        std::fs::create_dir(&bin).unwrap();
        let fake_dotnet = bin.join("dotnet");
        std::fs::write(
            &fake_dotnet,
            r#"#!/bin/sh
case "$1" in
  build)
    case " $* " in
      *" --no-restore "*) exit 31 ;;
    esac
    if [ -e obj/project.assets.json ]; then
      printf 'cached\n' >> "$TOGI_DOTNET_LOG"
    else
      printf 'restored\n' >> "$TOGI_DOTNET_LOG"
      mkdir -p obj
      : > obj/project.assets.json
    fi
    ;;
  test)
    test -f obj/project.assets.json || exit 32
    if grep -q 'false' Calculator.cs; then
      exit 1
    fi
    ;;
  *) exit 33 ;;
esac
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_dotnet).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&fake_dotnet, permissions).unwrap();

        if !git_available() {
            eprintln!("skipping dotnet isolated workspace test because git is unavailable");
            return;
        }
        init_clean_git_fixture(dir.path());

        let mut config = crate::config::Config::default();
        assert_eq!(
            config.resolve_build_command(dir.path()),
            BuildCommandOrigin::AutoDetected
        );
        assert_eq!(
            config.test.build_command,
            vec!["dotnet", "build", "Calculator.csproj"]
        );
        let build_command = config.test.build_command.clone();
        config.test.set_build_command(build_command);
        let origin = config.test.build_command_origin;
        assert_eq!(origin, BuildCommandOrigin::Configured);
        assert_eq!(
            crate::config::detect_test_command(dir.path()),
            vec!["dotnet", "test", "Calculator.csproj"]
        );

        let mut env = HashMap::new();
        env.insert(
            "PATH".into(),
            format!(
                "{}:{}",
                bin.display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        );
        let log = dir.path().join("dotnet.log");
        env.insert("TOGI_DOTNET_LOG".into(), log.display().to_string());
        let commands = CommandConfig {
            command: vec!["dotnet".into(), "test".into()],
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: config.test.build_command,
            sandbox_command: vec![],
            build_command_origin: origin,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: None,
        };

        let baseline_cancelled = AtomicBool::new(false);
        let timing = measure_baseline_timing(
            dir.path(),
            BaselineTimingConfig {
                test_command: &commands.command,
                build_command: &commands.build_command,
                sandbox_command: &commands.sandbox_command,
                build_command_origin: commands.build_command_origin,
                timeout: commands.timeout,
                env: &env,
                cancelled: &baseline_cancelled,
                respect_workspace_ignores: true,
            },
        )
        .unwrap();
        assert!(timing.build_duration.is_some());

        let offset = source.find("true").unwrap();
        let mutation = Mutation {
            id: 0,
            file: PathBuf::from("Calculator.cs"),
            language: "c_sharp".into(),
            line: 1,
            column: offset + 1,
            operator: "boolean".into(),
            description: "replace true with false".into(),
            original: "true".into(),
            replacement: "false".into(),
            byte_range: offset..offset + "true".len(),
        };
        let report = TestRunner {
            commands,
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env,
            incremental_history: false,
            force_rerun: true,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
        .run(vec![mutation.clone()])
        .report;

        assert_eq!(report.results.len(), 1);
        assert_eq!(report.results[0].0.id, mutation.id);
        assert_eq!(report.results[0].1, MutationResult::Killed);
        assert_eq!(report.build_errors, 0);
        assert_eq!(
            std::fs::read_to_string(log)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["restored", "restored"],
            "both isolated baseline and mutation workspaces must restore ignored assets"
        );
        assert_eq!(
            std::fs::read_to_string(source_assets).unwrap(),
            "source-assets"
        );
    }

    #[test]
    fn out_of_range_byte_range_returns_build_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hi").unwrap();

        let mut mutation = make_test_mutation(&file);
        mutation.byte_range = 0..100; // way past end

        let outcome = run_single_mutation(
            &["true".to_string()],
            &[],
            BuildCommand {
                argv: &[],
                origin: BuildCommandOrigin::None,
            },
            Duration::from_secs(5),
            dir.path(),
            ResolvedMutation::new(dir.path(), &mutation),
            false,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::BuildError);
    }

    #[test]
    fn missing_file_returns_build_error() {
        let dir = tempfile::tempdir().unwrap();
        let mutation = make_test_mutation(&dir.path().join("nonexistent.txt"));

        let outcome = run_single_mutation(
            &["true".to_string()],
            &[],
            BuildCommand {
                argv: &[],
                origin: BuildCommandOrigin::None,
            },
            Duration::from_secs(5),
            dir.path(),
            ResolvedMutation::new(dir.path(), &mutation),
            false,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::BuildError);
    }

    #[test]
    fn empty_command_returns_build_error() {
        let outcome = run_command(
            &[],
            &[],
            &PathBuf::from("."),
            Duration::from_secs(5),
            false,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::BuildError);
    }

    #[cfg(unix)]
    #[test]
    fn capture_output_collects_stdout_stderr() {
        let outcome = run_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo out; echo err >&2".to_string(),
            ],
            &[],
            &PathBuf::from("."),
            Duration::from_secs(5),
            true,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::Survived);
        let output = outcome.test_output.unwrap();
        assert!(output.contains("out"), "should capture stdout");
        assert!(output.contains("err"), "should capture stderr");
    }

    #[cfg(unix)]
    #[test]
    fn capture_output_truncates_and_drains_large_stdout() {
        let bytes_to_write = CAPTURED_OUTPUT_LIMIT + 256 * 1024;
        let outcome = run_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                format!("yes x | head -c {bytes_to_write}"),
            ],
            &[],
            &PathBuf::from("."),
            Duration::from_secs(5),
            true,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::Survived);
        let output = outcome.test_output.unwrap();
        assert!(
            output.contains("[stdout truncated after"),
            "large stdout should be marked as truncated"
        );
        assert!(
            output.len() < CAPTURED_OUTPUT_LIMIT + 128,
            "captured output should stay close to the configured cap"
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_output_does_not_timeout_when_background_writer_holds_stdout() {
        let started = Instant::now();
        let outcome = run_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "printf out; (sleep 1; printf late) &".to_string(),
            ],
            &[],
            &PathBuf::from("."),
            Duration::from_secs(5),
            true,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::Survived);
        assert!(
            started.elapsed() < Duration::from_millis(900),
            "finished child should not wait for background writer EOF"
        );
        assert!(
            outcome.test_output.unwrap().contains("out"),
            "captured output should include immediate stdout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_output_timeout_returns_promptly_with_background_writer() {
        let started = Instant::now();
        let outcome = run_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "(sleep 1; printf late) & sleep 10".to_string(),
            ],
            &[],
            &PathBuf::from("."),
            Duration::from_millis(100),
            true,
            &HashMap::new(),
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::Timeout);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "timeout path should not wait for descendant-held stdout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn capture_output_timeout_returns_promptly_when_setsid_descendant_holds_stdout() {
        let setsid_available = match std::process::Command::new("setsid").arg("true").status() {
            Ok(status) => status.success(),
            Err(_) => false,
        };
        if !setsid_available {
            eprintln!("skipping setsid capture cleanup test because setsid is unavailable");
            return;
        }

        struct EscapedProcessGuard(PathBuf);

        impl Drop for EscapedProcessGuard {
            fn drop(&mut self) {
                let Ok(pid) = std::fs::read_to_string(&self.0) else {
                    return;
                };
                let Ok(pid) = pid.trim().parse::<libc::pid_t>() else {
                    return;
                };
                unsafe {
                    let _ = libc::kill(pid, libc::SIGKILL);
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("escaped.pid");
        let _guard = EscapedProcessGuard(pid_file.clone());
        let mut env = HashMap::new();
        env.insert("ESCAPED_PID".to_string(), pid_file.display().to_string());

        let started = Instant::now();
        let outcome = run_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                r#"setsid sh -c 'printf "%s\n" "$$" > "$ESCAPED_PID"; while :; do sleep 1; done' &
while [ ! -s "$ESCAPED_PID" ]; do sleep 0.01; done
sleep 10"#
                    .to_string(),
            ],
            &[],
            dir.path(),
            Duration::from_millis(200),
            true,
            &env,
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::Timeout);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "timeout cleanup should not wait for a setsid descendant-held stdout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn no_capture_normal_completion_terminates_background_descendants() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("descendant.marker");
        let mut env = HashMap::new();
        env.insert("MARKER".to_string(), marker.display().to_string());

        let outcome = run_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "(sleep 0.5; touch \"$MARKER\") &".to_string(),
            ],
            &[],
            dir.path(),
            Duration::from_secs(5),
            false,
            &env,
            &AtomicBool::new(false),
        );

        assert_eq!(outcome.result, MutationResult::Survived);
        thread::sleep(Duration::from_millis(800));
        assert!(
            !marker.exists(),
            "background descendant should be killed even when output is not captured"
        );
    }

    #[cfg(unix)]
    #[test]
    fn runner_cancellation_stops_promptly_and_returns_cancelled() {
        let (dir, file, mutation) = make_test_setup();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_from_thread = cancelled.clone();

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["sleep".into(), "10".into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(30),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled,
        };

        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            cancel_from_thread.store(true, Ordering::Release);
        });

        let started = Instant::now();
        let outcome = runner.run(vec![mutation]);
        canceller.join().unwrap();

        assert!(outcome.cancelled);
        assert_eq!(outcome.report.total, 0);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "runner should not wait for the command timeout after cancellation"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[cfg(unix)]
    #[test]
    fn run_with_schemata_reports_unsupported_runner_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let source = "def same(a, b):\n    return a == b\n";
        std::fs::write(dir.path().join("app.py"), source).unwrap();
        let start = source.find("a == b").unwrap();
        let mutation = Mutation {
            id: 0,
            file: PathBuf::from("app.py"),
            language: "python".into(),
            line: 2,
            column: 12,
            operator: "eq_to_neq".into(),
            description: "Replace == with !=".into(),
            original: "a == b".into(),
            replacement: "a != b".into(),
            byte_range: start..start + "a == b".len(),
        };

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["true".into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run_with_schemata(vec![mutation]).report;
        let schemata = report.schemata.expect("schemata run should report stats");

        assert_eq!(report.total, 1);
        assert_eq!(report.survived, 1);
        assert_eq!(schemata.fast_path, 0);
        assert_eq!(schemata.fallback, 1);
        assert_eq!(schemata.fallback_reasons.len(), 1);
        assert_eq!(schemata.fallback_reasons[0].reason, "unsupported_runner");
        assert_eq!(schemata.fallback_reasons[0].count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn run_with_schemata_executes_c_mutation_with_active_env() {
        if std::process::Command::new("cc")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping C schemata runner test because cc is unavailable");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let source = "\
int same(int a, int b) {
    return a == b;
}

int main(void) {
    if (!same(1, 1)) {
        return 1;
    }
    if (same(1, 2)) {
        return 2;
    }
    return 0;
}
";
        std::fs::write(dir.path().join("calc.c"), source).unwrap();
        let mutation = c_operator_mutation(0, "calc.c", source, 0);

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["./calc".into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec!["cc".into(), "calc.c".into(), "-o".into(), "calc".into()],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::Configured,
                timeout: Duration::from_secs(30),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run_with_schemata(vec![mutation]).report;

        assert_eq!(report.total, 1);
        assert_eq!(report.results[0].1, MutationResult::Killed);
    }

    #[cfg(unix)]
    #[test]
    fn run_with_schemata_executes_cpp_mutation_with_active_env() {
        if std::process::Command::new("c++")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("skipping C++ schemata runner test because c++ is unavailable");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let source = "\
bool same(int a, int b) {
    return a == b;
}

int main() {
    if (!same(1, 1)) {
        return 1;
    }
    if (same(1, 2)) {
        return 2;
    }
    return 0;
}
";
        std::fs::write(dir.path().join("calc.cpp"), source).unwrap();
        let mutation = cpp_operator_mutation(0, "calc.cpp", source, 0);

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["./calc".into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![
                    "c++".into(),
                    "calc.cpp".into(),
                    "-std=c++11".into(),
                    "-o".into(),
                    "calc".into(),
                ],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::Configured,
                timeout: Duration::from_secs(30),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run_with_schemata(vec![mutation]).report;

        assert_eq!(report.total, 1);
        assert_eq!(report.results[0].1, MutationResult::Killed);
    }

    #[cfg(unix)]
    #[test]
    fn run_with_schemata_executes_go_mutations_with_active_env() {
        let dir = tempfile::tempdir().unwrap();
        let source = "\
package calc
func first(a, b int) bool { return a == b }
func second(c, d int) bool { return c == d }
";
        std::fs::write(dir.path().join("calc.go"), source).unwrap();
        let first = go_operator_mutation(0, "calc.go", source, 0);
        let second = go_operator_mutation(1, "calc.go", source, 1);
        let script = r#"
if [ -z "${TOGI_MUTANT:-}" ]; then
  case "$(cat calc.go)" in *'return c != d'*) exit 0 ;; *) exit 2 ;; esac
fi
case "$(cat calc.go)" in
  *__togi_active*) ;;
  *) exit 2 ;;
esac
case "$TOGI_MUTANT" in
  0) exit 1 ;;
  1) exit 0 ;;
  *) exit 2 ;;
esac
"#;

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["sh".into(), "-c".into(), script.into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run_with_schemata(vec![first, second]).report;

        assert_eq!(report.total, 2);
        assert_eq!(report.results[0].1, MutationResult::Killed);
        assert_eq!(report.results[1].1, MutationResult::Survived);
    }

    #[cfg(unix)]
    #[test]
    fn schemata_build_uses_project_route_timeout() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("services/api")).unwrap();
        let source = "package api\nfunc same(a, b int) bool { return a == b }\n";
        std::fs::write(dir.path().join("services/api/calc.go"), source).unwrap();
        let mutation = go_operator_mutation(0, "services/api/calc.go", source, 0);
        let test_command = vec![
            "sh".into(),
            "-c".into(),
            "case \"$(cat services/api/calc.go)\" in *__togi_active*) exit 1 ;; *) exit 2 ;; esac"
                .into(),
        ];
        let runner = TestRunner {
            commands: CommandConfig {
                command: successful_command(),
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![ProjectCommandConfig {
                    path: PathBuf::from("services/api"),
                    command: Some(test_command),
                    timeout: Some(Duration::from_secs(2)),
                }],
                language_commands: HashMap::new(),
                build_command: vec!["sh".into(), "-c".into(), "sleep 1".into()],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::Configured,
                timeout: Duration::from_millis(50),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run_with_schemata(vec![mutation]).report;

        assert_eq!(report.results[0].1, MutationResult::Killed);
        assert!(
            report
                .schemata
                .expect("schemata report")
                .fallback_reasons
                .iter()
                .all(|reason| reason.reason != "schema_build_failure")
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_with_schemata_resets_workspace_between_schema_mutants() {
        let dir = tempfile::tempdir().unwrap();
        let source = "\
package calc
func first(a, b int) bool { return a == b }
func second(c, d int) bool { return c == d }
";
        std::fs::write(dir.path().join("calc.go"), source).unwrap();
        let first = go_operator_mutation(0, "calc.go", source, 0);
        let second = go_operator_mutation(1, "calc.go", source, 1);
        let script = r#"
case "$(cat calc.go)" in
  *__togi_active*|*'!='*) ;;
  *) exit 2 ;;
esac
test ! -f side_effect || exit 1
touch side_effect
"#;

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["sh".into(), "-c".into(), script.into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run_with_schemata(vec![first, second]).report;

        assert_eq!(report.total, 2);
        assert_eq!(report.survived, 2);
        assert_eq!(report.killed, 0);
    }

    #[cfg(unix)]
    #[test]
    fn run_with_schemata_uses_cache_and_releases_max_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let source = "\
package calc
func first(a, b int) bool { return a == b }
func second(c, d int) bool { return c == d }
";
        std::fs::write(dir.path().join("calc.go"), source).unwrap();
        let first = go_operator_mutation(0, "calc.go", source, 0);
        let second = go_operator_mutation(1, "calc.go", source, 1);
        let script = r#"
runs=0
if [ -f "$STATE_DIR/runs" ]; then runs=$(cat "$STATE_DIR/runs"); fi
runs=$((runs + 1))
printf '%s\n' "$runs" > "$STATE_DIR/runs"
case "$TOGI_MUTANT" in
  1|'') exit 0 ;;
  *) exit 2 ;;
esac
"#;
        let mut env = HashMap::new();
        env.insert("STATE_DIR".to_string(), state.path().display().to_string());
        let commands = CommandConfig {
            command: vec!["sh".into(), "-c".into(), script.into()],
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec![],
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::None,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: None,
        };
        // The seeded entry is computed without TOGI_MUTANT; the schemata runner
        // sets it per mutant at execution time but must still hit the cache.
        let cache_env = env.clone();
        let selected = select_test_command(dir.path(), &commands, &first);
        let cache_selected = SelectedTestCommand {
            argv: selected.argv,
            unnarrowed_argv: None,
            selection_active: false,
            timeout: selected.timeout,
            uses_default_timeout: selected.uses_default_timeout,
            selected_tests: selected.selected_tests,
        };
        let cache_ctx = cache_selected.cache_context(
            &commands.build_command,
            commands.build_command_origin,
            &commands.sandbox_command,
            &cache_env,
        );
        let cache_ctx = format!(
            "{cache_ctx};context={:016x}",
            cache_context_fingerprint(dir.path())
        );
        let key = CacheKey::new(
            source.as_bytes(),
            &cache_identity(dir.path(), &first),
            &first.description,
            &cache_ctx,
        );
        cache::store(dir.path(), &key, MutationResult::Killed);

        let runner = TestRunner {
            commands,
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: Some(1),
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env,
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run_with_schemata(vec![first, second]).report;
        let runs: usize = std::fs::read_to_string(state.path().join("runs"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        assert_eq!(report.total, 2);
        assert_eq!(report.results[0].1, MutationResult::Killed);
        assert_eq!(report.results[1].1, MutationResult::Survived);
        assert_eq!(
            report.execution_for(report.results[0].0.id, MutationResult::Killed),
            MutationExecution::ExactCache
        );
        assert_eq!(
            report.execution_for(report.results[1].0.id, MutationResult::Survived),
            MutationExecution::Executed
        );
        assert_eq!(report.tested_count(), 1);
        assert_eq!(
            report
                .schemata
                .as_ref()
                .expect("schemata summary")
                .fast_path,
            report.tested_count()
        );
        assert_eq!(report.execution_counts().exact_cache_reused, 1);
        assert_eq!(runs, 2, "the fresh survivor also needs direct confirmation");
    }

    #[cfg(unix)]
    #[test]
    fn run_with_schemata_marks_incremental_history_reuse() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = "package calc\nfunc equal(a, b int) bool { return a == b }\n";
        std::fs::write(dir.path().join("calc.go"), source)?;
        let mutation = go_operator_mutation(0, "calc.go", source, 0);
        let commands = CommandConfig {
            command: failing_command(),
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec![],
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::None,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: None,
        };
        let selected = select_test_command(dir.path(), &commands, &mutation);
        let command_context = selected.cache_context(
            &commands.build_command,
            commands.build_command_origin,
            &commands.sandbox_command,
            &HashMap::new(),
        );
        let source_content = std::fs::read(dir.path().join("calc.go"))?;
        let context_hash = cache_context_fingerprint(dir.path());
        let test_context_index = TestContextIndex::build(dir.path());
        let relevant_test_hash =
            test_context_index.fingerprint_for_tests(&selected.selected_tests, context_hash);
        let query = incremental_history_query(
            dir.path(),
            &mutation,
            &source_content,
            &command_context,
            relevant_test_hash,
        );
        cache::IncrementalHistoryStore::load(dir.path()).record(cache::IncrementalHistoryEntry {
            mutation_identity: query.mutation_identity,
            mutation_description: query.mutation_description,
            result: MutationResult::Killed,
            source_hash: query.source_hash,
            command_hash: query.command_hash,
            relevant_test_hash: query.relevant_test_hash,
            covering_tests: vec![],
            killer_test: None,
        });

        let runner = TestRunner {
            commands,
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run_with_schemata(vec![mutation]).report;
        assert_eq!(report.results[0].1, MutationResult::Killed);
        assert_eq!(
            report.execution_for(report.results[0].0.id, MutationResult::Killed),
            MutationExecution::IncrementalHistory
        );
        assert_eq!(
            report
                .schemata
                .as_ref()
                .expect("schemata summary")
                .fast_path,
            report.tested_count()
        );
        assert_eq!(report.tested_count(), 0);
        assert_eq!(report.execution_counts().incremental_history_reused, 1);
        Ok(())
    }

    #[cfg(unix)]
    fn exact_cached_kill_keeps_fail_under_reachable(use_schemata: bool) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = tempfile::tempdir()?;
        let source = "\
package calc
func first(a, b int) bool { return a == b }
func second(a, b int) bool { return a == b }
func third(a, b int) bool { return a == b }
";
        std::fs::write(dir.path().join("calc.go"), source)?;
        let mutations = (0..3)
            .map(|id| go_operator_mutation(id, "calc.go", source, id as usize))
            .collect::<Vec<_>>();
        let commands = CommandConfig {
            command: vec![
                "sh".into(),
                "-c".into(),
                r#"
runs=0
if [ -f "$1/runs" ]; then runs=$(cat "$1/runs"); fi
printf '%s\n' "$((runs + 1))" > "$1/runs"
if [ -n "${TOGI_MUTANT:-}" ]; then test "$TOGI_MUTANT" = 1; exit $?; fi
case "$(cat calc.go)" in
  *'func second(a, b int) bool { return a != b }'*) exit 0 ;;
  *) exit 1 ;;
esac
"#
                .into(),
                "gate".into(),
                state.path().display().to_string(),
            ],
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec![],
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::None,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: None,
        };
        seed_reused_result(
            dir.path(),
            &commands,
            &mutations[0],
            ReuseSource::ExactCache,
            MutationResult::Killed,
        )?;
        let runner = TestRunner {
            commands,
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: EarlyStopConfig {
                max_survivors: None,
                fail_under: Some(60.0),
            },
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: false,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = if use_schemata {
            runner.run_with_schemata(mutations).report
        } else {
            runner.run(mutations).report
        };

        assert_eq!(report.planned_total, 3);
        assert_eq!(report.total, 3);
        assert_eq!(report.killed, 2);
        assert_eq!(report.survived, 1);
        assert_eq!(report.tested_count(), 2);
        assert_eq!(
            report.execution_for(0, MutationResult::Killed),
            MutationExecution::ExactCache
        );
        assert_eq!(crate::report::mutation_score(&report), 50.0);
        assert!(crate::report::fail_under_score(&report) > 60.0);
        assert!(report.early_stop_reason.is_none(), "{report:?}");
        assert_eq!(
            std::fs::read_to_string(state.path().join("runs"))?.trim(),
            if use_schemata { "3" } else { "2" }
        );
        if use_schemata {
            assert_eq!(
                report
                    .schemata
                    .as_ref()
                    .expect("schemata summary")
                    .fast_path,
                report.tested_count()
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn reused_schemata_survivor_is_confirmed_before_early_stop() -> anyhow::Result<()> {
        for reuse_source in [ReuseSource::ExactCache, ReuseSource::IncrementalHistory] {
            for (early_stop, gate) in [
                (
                    EarlyStopConfig {
                        max_survivors: Some(1),
                        fail_under: None,
                    },
                    "max survivors",
                ),
                (
                    EarlyStopConfig {
                        max_survivors: None,
                        fail_under: Some(100.0),
                    },
                    "fail under",
                ),
            ] {
                let dir = tempfile::tempdir()?;
                let source = "\
package calc
func first(a, b int) bool { return a == b }
func second(a, b int) bool { return a == b }
func third(a, b int) bool { return a == b }
";
                std::fs::write(dir.path().join("calc.go"), source)?;
                let mutations = (0..3)
                    .map(|id| go_operator_mutation(id, "calc.go", source, id as usize))
                    .collect::<Vec<_>>();
                let commands = CommandConfig {
                    command: failing_command(),
                    force_default_command: false,
                    force_default_timeout: false,
                    project_commands: vec![],
                    language_commands: HashMap::new(),
                    build_command: vec![],
                    sandbox_command: vec![],
                    build_command_origin: BuildCommandOrigin::None,
                    timeout: Duration::from_secs(5),
                    language_timeouts: HashMap::new(),
                    test_selection: None,
                };
                seed_reused_survivor(dir.path(), &commands, &mutations[0], reuse_source)?;
                let runner = TestRunner {
                    commands,
                    parallelism: 1,
                    project_root: dir.path().to_path_buf(),
                    verbose: false,
                    show_output: false,
                    max_tested: None,
                    early_stop,
                    respect_workspace_ignores: true,
                    env: HashMap::new(),
                    incremental_history: true,
                    force_rerun: false,
                    learned_selection: false,
                    cancelled: Arc::new(AtomicBool::new(false)),
                };
                let report = runner.run_with_schemata(mutations).report;
                let schemata = report.schemata.as_ref().expect("schemata summary");

                assert_eq!(report.total, 3, "{gate} should not stop fresh mutations");
                assert_eq!(report.survived, 0);
                assert_eq!(report.killed, 3);
                assert_eq!(report.tested_count(), 3);
                assert_eq!(schemata.fast_path, report.tested_count());
                assert_eq!(
                    report.execution_for(0, MutationResult::Killed),
                    MutationExecution::Executed
                );
                assert!(report.early_stop_reason.is_none(), "{gate}: {report:?}");
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn exact_cached_kill_keeps_schemata_fail_under_reachable() -> anyhow::Result<()> {
        exact_cached_kill_keeps_fail_under_reachable(true)
    }

    #[cfg(unix)]
    #[test]
    fn late_restored_schemata_survivor_gets_a_fresh_verdict_before_the_gate() -> anyhow::Result<()>
    {
        for reuse_source in [ReuseSource::ExactCache, ReuseSource::IncrementalHistory] {
            let dir = tempfile::tempdir()?;
            let state = tempfile::tempdir()?;
            let source = "\
package calc
func first(a, b int) bool { return a == b }
func second(a, b int) bool { return a == b }
func third(a, b int) bool { return a == b }
";
            std::fs::write(dir.path().join("calc.go"), source)?;
            let mutations = (0..3)
                .map(|id| go_operator_mutation(id, "calc.go", source, id as usize))
                .collect::<Vec<_>>();
            let commands = CommandConfig {
                command: first_run_survives_second_kills_command(state.path()),
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            };
            seed_reused_survivor(dir.path(), &commands, &mutations[2], reuse_source)?;
            let runner = TestRunner {
                commands,
                parallelism: 1,
                project_root: dir.path().to_path_buf(),
                verbose: false,
                show_output: false,
                max_tested: None,
                early_stop: EarlyStopConfig {
                    max_survivors: None,
                    fail_under: Some(60.0),
                },
                respect_workspace_ignores: true,
                env: HashMap::new(),
                incremental_history: true,
                force_rerun: false,
                learned_selection: false,
                cancelled: Arc::new(AtomicBool::new(false)),
            };

            let report = runner.run_with_schemata(mutations).report;
            let runs = std::fs::read_to_string(state.path().join("runs"))?;
            let schemata = report.schemata.as_ref().expect("schemata summary");

            assert_eq!(report.planned_total, 3);
            assert_eq!(report.total, 3);
            assert_eq!(report.survived, 0);
            assert_eq!(report.killed, 3);
            assert_eq!(report.tested_count(), 3);
            assert_eq!(schemata.fast_path, report.tested_count());
            assert_eq!(
                report.execution_for(0, MutationResult::Killed),
                MutationExecution::Executed
            );
            assert_eq!(
                report.execution_for(2, MutationResult::Killed),
                MutationExecution::Executed
            );
            assert_eq!(runs.trim(), "4");
            assert!(report.early_stop_reason.is_none());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn schemata_fallback_preserves_cache_and_history_provenance_through_records()
    -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let go_source = "package calc\nfunc equal(a, b int) bool { return a == b }\n";
        let python_source = "\
def first(a, b):
    return a == b

def second(c, d):
    return c == d
";
        std::fs::write(dir.path().join("calc.go"), go_source)?;
        std::fs::write(dir.path().join("app.py"), python_source)?;
        let go = go_operator_mutation(0, "calc.go", go_source, 0);
        let python = |id, nth| {
            let offset = python_source
                .match_indices("==")
                .nth(nth)
                .expect("Python source should contain the operator")
                .0;
            Mutation {
                id,
                file: PathBuf::from("app.py"),
                language: "python".into(),
                line: 2 + nth * 3,
                column: 14,
                operator: "eq_to_neq".into(),
                description: "Replace == with !=".into(),
                original: "==".into(),
                replacement: "!=".into(),
                byte_range: offset..offset + 2,
            }
        };
        let python_cached = python(1, 0);
        let python_history = python(2, 1);
        let commands = CommandConfig {
            command: successful_command(),
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec![],
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::None,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: None,
        };
        let env = HashMap::new();
        let context_hash = cache_context_fingerprint(dir.path());
        let test_context_index = TestContextIndex::build(dir.path());
        let seed_cache = |mutation: &Mutation| -> anyhow::Result<()> {
            let selected = select_test_command(dir.path(), &commands, mutation);
            let context = selected.cache_context(
                &commands.build_command,
                commands.build_command_origin,
                &commands.sandbox_command,
                &env,
            );
            let context = format!("{context};context={context_hash:016x}");
            let source = std::fs::read(dir.path().join(&mutation.file))?;
            let key = CacheKey::new(
                &source,
                &cache_identity(dir.path(), mutation),
                &mutation.description,
                &context,
            );
            cache::store(dir.path(), &key, MutationResult::Survived);
            Ok(())
        };
        seed_cache(&go)?;
        seed_cache(&python_cached)?;

        let selected = select_test_command(dir.path(), &commands, &python_history);
        let command_context = selected.cache_context(
            &commands.build_command,
            commands.build_command_origin,
            &commands.sandbox_command,
            &env,
        );
        let source = std::fs::read(dir.path().join(&python_history.file))?;
        let query = incremental_history_query(
            dir.path(),
            &python_history,
            &source,
            &command_context,
            test_context_index.fingerprint_for_tests(&selected.selected_tests, context_hash),
        );
        cache::IncrementalHistoryStore::load(dir.path()).record(cache::IncrementalHistoryEntry {
            mutation_identity: query.mutation_identity,
            mutation_description: query.mutation_description,
            result: MutationResult::Survived,
            source_hash: query.source_hash,
            command_hash: query.command_hash,
            relevant_test_hash: query.relevant_test_hash,
            covering_tests: vec![],
            killer_test: None,
        });

        let runner = TestRunner {
            commands,
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env,
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let report = runner
            .run_with_schemata(vec![go, python_cached, python_history])
            .report;

        assert_eq!(report.tested_count(), 1);
        assert!(report.build_error_diagnostics.is_empty());
        assert_eq!(
            report.execution_for(0, MutationResult::Survived),
            MutationExecution::Executed
        );
        assert_eq!(
            report.execution_for(1, MutationResult::Survived),
            MutationExecution::ExactCache
        );
        assert_eq!(
            report.execution_for(2, MutationResult::Survived),
            MutationExecution::IncrementalHistory
        );
        let schemata = report.schemata.as_ref().expect("schemata summary");
        assert_eq!(schemata.fast_path, report.tested_count());
        assert_eq!(schemata.fallback, 2);

        let json: serde_json::Value =
            serde_json::from_str(&crate::report::json::to_json_string(&report)?)?;
        let mutations = json["mutations"].as_array().expect("mutation array");
        assert_eq!(mutations[0]["execution"]["state"], "executed");
        assert_eq!(mutations[1]["execution"]["state"], "exact_cache");
        assert_eq!(mutations[2]["execution"]["state"], "incremental_history");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn run_with_schemata_demotes_batch_on_schema_build_failure() {
        let dir = tempfile::tempdir().unwrap();
        let source = "\
package calc
func first(a, b int) bool { return a == b }
func second(c, d int) bool { return c == d }
";
        std::fs::write(dir.path().join("calc.go"), source).unwrap();
        let first = go_operator_mutation(0, "calc.go", source, 0);
        let second = go_operator_mutation(1, "calc.go", source, 1);
        // The build command fails only when the workspace contains schema wraps
        // (__togi_active): the shared schemata build breaks while regular
        // per-mutant runs (raw splice, no wraps) build fine (#412).
        let build = "if grep -rq __togi_active .; then exit 1; fi";
        let commands = CommandConfig {
            command: vec!["sh".into(), "-c".into(), "exit 1".into()],
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec!["sh".into(), "-c".into(), build.into()],
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::Configured,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: None,
        };
        let runner = TestRunner {
            commands,
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: false,
            force_rerun: true,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run_with_schemata(vec![first, second]).report;

        // No batch-wide build errors: both mutants were demoted to regular
        // runs, where the failing test command kills them.
        assert_eq!(report.total, 2);
        assert_eq!(report.killed, 2);
        assert_eq!(report.build_errors, 0);
        let schemata = report.schemata.expect("schemata summary");
        assert_eq!(schemata.fast_path, 0);
        assert!(
            schemata
                .fallback_reasons
                .iter()
                .any(|reason| reason.reason == "schema_build_failure" && reason.count == 2)
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_with_schemata_salvages_compatible_batches_after_schema_build_failure() {
        let dir = tempfile::tempdir().unwrap();
        let source = "\
package calc
func first(a, b int) bool { return a == b }
func second(c, d int) bool { return c == d }
";
        std::fs::write(dir.path().join("calc.go"), source).unwrap();
        let first = go_operator_mutation(0, "calc.go", source, 0);
        let second = go_operator_mutation(1, "calc.go", source, 1);
        // The wrapper for mutation 1 is incompatible with this build, but the
        // first mutation's schema batch compiles and should keep the fast path.
        let build = r#"if grep -rq '__togi_active("1")' .; then exit 1; fi"#;
        let commands = CommandConfig {
            command: vec!["sh".into(), "-c".into(), "exit 1".into()],
            force_default_command: false,
            force_default_timeout: false,
            project_commands: vec![],
            language_commands: HashMap::new(),
            build_command: vec!["sh".into(), "-c".into(), build.into()],
            sandbox_command: vec![],
            build_command_origin: BuildCommandOrigin::Configured,
            timeout: Duration::from_secs(5),
            language_timeouts: HashMap::new(),
            test_selection: None,
        };
        let runner = TestRunner {
            commands,
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: Some(1),
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: false,
            force_rerun: true,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run_with_schemata(vec![first, second]).report;

        // The salvaged schema run consumes the shared cap, so the demoted
        // mutation must not enter a separate regular-run budget.
        assert_eq!(report.total, 1);
        assert_eq!(report.killed, 1);
        assert_eq!(report.build_errors, 0);
        let schemata = report.schemata.expect("schemata summary");
        assert_eq!(schemata.fast_path, 1);
        assert!(
            schemata
                .fallback_reasons
                .iter()
                .any(|reason| reason.reason == "schema_build_failure" && reason.count == 1)
        );
    }

    #[cfg(unix)]
    #[test]
    fn run_with_schemata_executes_java_mutation_with_active_env() {
        if std::process::Command::new("javac")
            .arg("-version")
            .output()
            .is_err()
            || std::process::Command::new("java")
                .arg("-version")
                .output()
                .is_err()
        {
            eprintln!("skipping Java schemata runner test because javac/java is unavailable");
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        let source = "\
class Calc {
    static boolean same(int a, int b) {
        return a == b;
    }

    public static void main(String[] args) {
        if (!same(1, 1)) {
            throw new AssertionError(\"same values should match\");
        }
        if (same(1, 2)) {
            throw new AssertionError(\"different values should not match\");
        }
    }
}
";
        std::fs::write(dir.path().join("Calc.java"), source).unwrap();
        let mutation = java_operator_mutation(0, "Calc.java", source, 0);

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["java".into(), "Calc".into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec!["javac".into(), "Calc.java".into()],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::Configured,
                timeout: Duration::from_secs(30),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run_with_schemata(vec![mutation]).report;

        assert_eq!(report.total, 1);
        assert_eq!(report.results[0].1, MutationResult::Killed);
    }

    #[test]
    fn run_with_schemata_executes_rust_mutation_with_active_env() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"schemata_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )
        .unwrap();
        let source = "\
pub fn same(a: i32, b: i32) -> bool { a == b }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_values_match() {
        assert!(same(1, 1));
        assert!(!same(1, 2));
    }
}
";
        std::fs::write(dir.path().join("src/lib.rs"), source).unwrap();
        let mutation = rust_operator_mutation(0, "src/lib.rs", source, 0);

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["cargo".into(), "test".into(), "--quiet".into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(60),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: {
                let mut env = HashMap::new();
                env.insert(
                    "CARGO_TARGET_DIR".to_string(),
                    dir.path().join("target").display().to_string(),
                );
                env
            },
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run_with_schemata(vec![mutation]).report;

        assert_eq!(report.total, 1);
        assert_eq!(report.results[0].1, MutationResult::Killed);
    }

    #[cfg(unix)]
    #[test]
    fn schemata_survivors_get_fresh_direct_recipes_including_cache_and_early_stop()
    -> anyhow::Result<()> {
        for reuse in [
            None,
            Some(ReuseSource::ExactCache),
            Some(ReuseSource::IncrementalHistory),
        ] {
            for early_stop in [false, true] {
                for (status, expected) in
                    [(0, MutationResult::Survived), (1, MutationResult::Killed)]
                {
                    let dir = tempfile::tempdir()?;
                    let state = tempfile::tempdir()?;
                    std::fs::create_dir(dir.path().join("src"))?;
                    let source = "pub fn same(a: i32, b: i32) -> bool { a == b }\n";
                    std::fs::write(dir.path().join("src/lib.rs"), source)?;
                    let mutation = rust_operator_mutation(0, "src/lib.rs", source, 0);
                    let log = state.path().join("runs");
                    let env = HashMap::from([
                        ("TOGI_CONFIRMATION_LOG".into(), log.display().to_string()),
                        ("TOGI_DIRECT_STATUS".into(), status.to_string()),
                    ]);
                    let mut commands = test_command_config();
                    commands.command = vec![
                        "sh".into(),
                        "-c".into(),
                        r#"
if [ -n "${TOGI_MUTANT:-}" ]; then
    printf 'schema\n' >> "$TOGI_CONFIRMATION_LOG"
    touch side_effect
    exit 0
fi
printf 'direct:%s\n' "$(cat src/lib.rs)" >> "$TOGI_CONFIRMATION_LOG"
test ! -f side_effect || exit 2
exit "$TOGI_DIRECT_STATUS"
"#
                        .into(),
                    ];
                    if let Some(reuse) = reuse {
                        seed_reused_survivor_with_env(
                            dir.path(),
                            &commands,
                            &mutation,
                            reuse,
                            &env,
                        )?;
                    }
                    let mut runner = confirmation_runner(dir.path(), commands, env);
                    runner.force_rerun = false;
                    runner.incremental_history = true;
                    runner.max_tested = Some(1);
                    runner.early_stop.max_survivors = early_stop.then_some(1);
                    let outcome = runner.run_with_schemata(vec![mutation.clone()]);
                    assert_eq!(outcome.report.results[0].1, expected);
                    assert_eq!(
                        outcome.report.execution_for(0, expected),
                        MutationExecution::Executed
                    );
                    assert_eq!(outcome.report.tested_count(), 1);
                    let recipe = &outcome.replay_recipes[&0];
                    assert_eq!(recipe.origin, DirectRecipeOrigin::Executed);
                    assert_eq!(recipe.test_command, runner.commands.command);
                    assert!(!recipe.env.contains_key("TOGI_MUTANT"));
                    let expected_log = format!(
                        "{}direct:{}",
                        if reuse.is_some() { "" } else { "schema\n" },
                        source.replace("==", "!="),
                    );
                    assert_eq!(std::fs::read_to_string(log)?, expected_log);
                    assert_eq!(
                        std::fs::read_to_string(dir.path().join("src/lib.rs"))?,
                        source
                    );
                    assert!(!dir.path().join("side_effect").exists());
                    // A subsequent regular run must reuse the direct verdict,
                    // not a stale pre-confirmation schema survivor.
                    let repeated = runner.run(vec![mutation]).report;
                    assert_eq!(repeated.results[0].1, expected);
                    assert_eq!(
                        repeated.execution_for(0, expected),
                        MutationExecution::ExactCache
                    );
                }
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn confirmed_schema_survivor_triggers_the_fresh_survivor_limit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let source = "package calc\nfunc first(a,b int) bool { return a == b }\nfunc second(a,b int) bool { return a == b }\n";
        std::fs::write(dir.path().join("calc.go"), source)?;
        let first = go_operator_mutation(0, "calc.go", source, 0);
        let second = go_operator_mutation(1, "calc.go", source, 1);
        let mut commands = test_command_config();
        commands.command = successful_command();
        seed_reused_survivor(dir.path(), &commands, &first, ReuseSource::ExactCache)?;
        let mut runner = confirmation_runner(dir.path(), commands, HashMap::new());
        runner.force_rerun = false;
        runner.early_stop.max_survivors = Some(1);
        let outcome = runner.run_with_schemata(vec![first, second]);
        assert_eq!(outcome.report.planned_total, 2);
        assert_eq!(outcome.report.total, 1);
        assert_eq!(outcome.report.survived, 1);
        assert_eq!(outcome.report.tested_count(), 1);
        assert_eq!(
            outcome.replay_recipes[&0].origin,
            DirectRecipeOrigin::Executed
        );
        assert!(
            outcome
                .report
                .early_stop_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("--max-survivors 1"))
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn demoted_schema_survivors_do_not_restore_unconfirmed_verdicts() -> anyhow::Result<()> {
        for reuse in [ReuseSource::ExactCache, ReuseSource::IncrementalHistory] {
            for early_stop in [false, true] {
                let dir = tempfile::tempdir()?;
                let source = "package calc\nfunc first(a,b int) bool { return a == b }\nfunc second(a,b int) bool { return a == b }\n";
                std::fs::write(dir.path().join("calc.go"), source)?;
                let first = go_operator_mutation(0, "calc.go", source, 0);
                let second = go_operator_mutation(1, "calc.go", source, 1);
                let mut commands = test_command_config();
                commands.command = failing_command();
                commands.build_command = vec![
                    "sh".into(),
                    "-c".into(),
                    "case \"$(cat calc.go)\" in *__togi_active*) exit 1 ;; esac".into(),
                ];
                commands.build_command_origin = BuildCommandOrigin::Configured;
                seed_reused_survivor(dir.path(), &commands, &second, reuse)?;
                let mut runner = confirmation_runner(dir.path(), commands, HashMap::new());
                runner.force_rerun = false;
                runner.incremental_history = true;
                runner.max_tested = Some(2);
                runner.early_stop.max_survivors = early_stop.then_some(1);
                // The first mutant's schema build demotes the whole batch,
                // including the queued cache/history survivor.
                let outcome = runner.run_with_schemata(vec![first, second]);
                assert_eq!(outcome.report.killed, 2);
                assert_eq!(outcome.report.tested_count(), 2);
                assert_eq!(outcome.replay_recipes.len(), 2);
                assert!(
                    outcome
                        .replay_recipes
                        .values()
                        .all(|recipe| recipe.origin == DirectRecipeOrigin::Executed)
                );
                assert!(outcome.report.early_stop_reason.is_none());
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn failed_direct_confirmation_does_not_leave_a_cached_schema_survivor() -> anyhow::Result<()> {
        for reuse in [ReuseSource::ExactCache, ReuseSource::IncrementalHistory] {
            let dir = tempfile::tempdir()?;
            let source = "package calc\nfunc same(a,b int) bool { return a == b }\n";
            std::fs::write(dir.path().join("calc.go"), source)?;
            let mutation = go_operator_mutation(0, "calc.go", source, 0);
            let mut commands = test_command_config();
            commands.command = successful_command();
            commands.build_command = failing_command();
            commands.build_command_origin = BuildCommandOrigin::Configured;
            seed_reused_survivor(dir.path(), &commands, &mutation, reuse)?;
            let mut runner = confirmation_runner(dir.path(), commands, HashMap::new());
            runner.force_rerun = false;
            runner.incremental_history = true;
            let outcome = runner.run_with_schemata(vec![mutation.clone()]);
            assert_eq!(outcome.report.build_errors, 1);
            assert!(outcome.replay_recipes.is_empty());
            let repeated = runner.run(vec![mutation]);
            assert_eq!(repeated.report.build_errors, 1);
            assert_eq!(repeated.report.survived, 0);
            assert!(repeated.replay_recipes.is_empty());
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn schemata_confirmation_rechecks_narrowed_rust_survivors_on_the_full_route() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        let source = "pub fn same(a: i32, b: i32) -> bool { a == b }\n";
        std::fs::write(dir.path().join("src/lib.rs"), source).unwrap();
        let mutation = rust_operator_mutation(0, "src/lib.rs", source, 0);
        let mut selection = TestSelectionConfig::new();
        selection.insert(
            dir.path(),
            Path::new("src/lib.rs"),
            1,
            vec!["narrow_test".into()],
        );
        let (env, log) = fake_selection_command(dir.path(), "cargo", "narrow_test", 1);
        let mut commands = test_command_config();
        commands.command = vec!["cargo".into(), "test".into()];
        commands.test_selection = Some(selection);

        let outcome = confirmation_runner(dir.path(), commands, env)
            .run_with_schemata(vec![mutation.clone()]);
        let report = outcome.report;

        assert_eq!(report.results[0].1, MutationResult::Killed);
        assert_eq!(
            outcome.replay_recipes[&mutation.id].test_command,
            ["cargo", "test"]
        );
        assert!(
            !outcome.replay_recipes[&mutation.id]
                .env
                .contains_key("TOGI_MUTANT")
        );
        assert_eq!(
            report.selection_for(mutation.id),
            Some(TestSelectionProvenance::Narrowed {
                confirmation: SurvivorConfirmation::Killed,
            })
        );
        assert_eq!(
            std::fs::read_to_string(log)
                .unwrap()
                .lines()
                .collect::<Vec<_>>(),
            ["<test narrow_test>", "<test>"],
        );
    }
    #[test]
    fn max_tested_limits_mutations() {
        let (dir, file, _) = make_test_setup();

        let mutations: Vec<Mutation> = (0..5)
            .map(|i| {
                let mut m = make_test_mutation(&file);
                m.id = i;
                m.description = format!("max tested limit {i}");
                m
            })
            .collect();

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["true".into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: Some(2),
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(mutations).report;
        assert_eq!(report.total, 2, "should stop after max_tested");
    }

    #[test]
    fn max_survivors_stops_scheduling_new_mutations() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let mutations: Vec<Mutation> = (0..3)
            .map(|i| {
                let file = dir.path().join(format!("survivor{i}.txt"));
                std::fs::write(&file, b"hello world").expect("fixture file should be written");
                let mut mutation = make_test_mutation(&file);
                mutation.id = i;
                mutation.description = format!("survivor limit {i}");
                mutation
            })
            .collect();

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["true".into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: EarlyStopConfig {
                max_survivors: Some(1),
                fail_under: None,
            },
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(mutations).report;

        assert_eq!(report.planned_total, 3);
        assert_eq!(report.total, 1);
        assert_eq!(report.survived, 1);
        assert!(
            report
                .early_stop_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("--max-survivors 1"))
        );
    }

    #[test]
    fn fail_under_stops_when_threshold_cannot_be_reached() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let mutations: Vec<Mutation> = (0..3)
            .map(|i| {
                let file = dir.path().join(format!("threshold{i}.txt"));
                std::fs::write(&file, b"hello world").expect("fixture file should be written");
                let mut mutation = make_test_mutation(&file);
                mutation.id = i;
                mutation.description = format!("threshold gate {i}");
                mutation
            })
            .collect();

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["true".into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: EarlyStopConfig {
                max_survivors: None,
                fail_under: Some(80.0),
            },
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(mutations).report;

        assert_eq!(report.planned_total, 3);
        assert_eq!(report.total, 1);
        assert_eq!(report.survived, 1);
        assert!(
            report
                .early_stop_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("--fail-under 80.0"))
        );
    }

    #[test]
    fn late_exact_cache_restore_keeps_gate_and_survivor_evidence_separate() {
        let gate = EarlyStopState::new(
            EarlyStopConfig {
                max_survivors: None,
                fail_under: Some(60.0),
            },
            3,
        );
        gate.record_restored(MutationExecution::ExactCache, MutationResult::Killed);
        gate.record_fresh(MutationResult::Survived);
        assert!(!gate.should_stop());

        let survivor_limit = EarlyStopState::new(
            EarlyStopConfig {
                max_survivors: Some(1),
                fail_under: None,
            },
            4,
        );
        survivor_limit.record_restored(MutationExecution::ExactCache, MutationResult::Survived);
        assert!(!survivor_limit.should_stop());
        survivor_limit.record_fresh(MutationResult::Killed);
        assert!(!survivor_limit.should_stop());
        survivor_limit.record_fresh(MutationResult::Survived);
        assert!(
            survivor_limit
                .reason()
                .as_deref()
                .is_some_and(|reason| reason.contains("--max-survivors 1"))
        );
    }

    #[test]
    fn preclassified_exact_cache_evidence_is_aggregated_before_gating() {
        let mut cached_survivor = make_test_mutation(std::path::Path::new("cached-survivor"));
        cached_survivor.id = 0;
        let mut cached_kill = make_test_mutation(std::path::Path::new("cached-kill"));
        cached_kill.id = 1;
        let restored = vec![
            (
                0,
                MutationRunRecord::new(cached_survivor, MutationResult::Survived, None)
                    .with_execution(MutationExecution::ExactCache),
            ),
            (
                1,
                MutationRunRecord::new(cached_kill, MutationResult::Killed, None)
                    .with_execution(MutationExecution::ExactCache),
            ),
        ];
        let state = EarlyStopState::new(
            EarlyStopConfig {
                max_survivors: None,
                fail_under: Some(60.0),
            },
            1,
        );

        state.record_preclassified_exact_cache(&restored);

        assert!(!state.should_stop());
    }

    #[test]
    fn reused_regular_survivor_keeps_max_survivors_fresh_only() -> anyhow::Result<()> {
        for reuse_source in [ReuseSource::ExactCache, ReuseSource::IncrementalHistory] {
            for (early_stop, gate) in [
                (
                    EarlyStopConfig {
                        max_survivors: Some(1),
                        fail_under: None,
                    },
                    "max survivors",
                ),
                (
                    EarlyStopConfig {
                        max_survivors: None,
                        fail_under: Some(100.0),
                    },
                    "fail under",
                ),
            ] {
                let dir = tempfile::tempdir()?;
                let mutations = ["cached.txt", "fresh-one.txt", "fresh-two.txt"]
                    .into_iter()
                    .enumerate()
                    .map(|(id, name)| {
                        let file = dir.path().join(name);
                        std::fs::write(&file, b"hello world")?;
                        let mut mutation = make_test_mutation(&file);
                        mutation.id = u32::try_from(id)?;
                        mutation.description = format!("early-stop {name}");
                        Ok::<_, anyhow::Error>(mutation)
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?;
                let commands = CommandConfig {
                    command: failing_command(),
                    force_default_command: false,
                    force_default_timeout: false,
                    project_commands: vec![],
                    language_commands: HashMap::new(),
                    build_command: vec![],
                    sandbox_command: vec![],
                    build_command_origin: BuildCommandOrigin::None,
                    timeout: Duration::from_secs(5),
                    language_timeouts: HashMap::new(),
                    test_selection: None,
                };
                seed_reused_survivor(dir.path(), &commands, &mutations[0], reuse_source)?;
                let expected_execution = match reuse_source {
                    ReuseSource::ExactCache => MutationExecution::ExactCache,
                    ReuseSource::IncrementalHistory => MutationExecution::IncrementalHistory,
                };
                let runner = TestRunner {
                    commands,
                    parallelism: 1,
                    project_root: dir.path().to_path_buf(),
                    verbose: false,
                    show_output: false,
                    max_tested: None,
                    early_stop,
                    respect_workspace_ignores: true,
                    env: HashMap::new(),
                    incremental_history: true,
                    force_rerun: false,
                    learned_selection: false,
                    cancelled: Arc::new(AtomicBool::new(false)),
                };

                let report = runner.run(mutations).report;

                if matches!(reuse_source, ReuseSource::ExactCache) && gate == "fail under" {
                    assert_eq!(report.total, 1, "{gate}: {report:?}");
                    assert_eq!(report.survived, 1);
                    assert_eq!(report.killed, 0);
                    assert_eq!(report.tested_count(), 0);
                    assert_eq!(
                        report.execution_for(0, MutationResult::Survived),
                        MutationExecution::ExactCache
                    );
                    assert!(
                        report
                            .early_stop_reason
                            .as_deref()
                            .is_some_and(|reason| reason.contains("--fail-under 100.0")),
                        "{report:?}"
                    );
                } else {
                    assert_eq!(report.total, 3, "{gate} should not stop fresh mutations");
                    assert_eq!(report.survived, 1);
                    assert_eq!(report.killed, 2);
                    assert_eq!(report.tested_count(), 2);
                    assert_eq!(
                        report.execution_for(0, MutationResult::Survived),
                        expected_execution
                    );
                    assert!(report.early_stop_reason.is_none(), "{gate}: {report:?}");
                }
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn exact_cached_kill_keeps_regular_fail_under_reachable() -> anyhow::Result<()> {
        exact_cached_kill_keeps_fail_under_reachable(false)
    }

    #[cfg(unix)]
    #[test]
    fn late_restored_regular_survivor_reduces_fail_under_fresh_budget() -> anyhow::Result<()> {
        for reuse_source in [ReuseSource::ExactCache, ReuseSource::IncrementalHistory] {
            let dir = tempfile::tempdir()?;
            let state = tempfile::tempdir()?;
            let mutations = ["fresh-survivor.txt", "fresh-killer.txt", "cached.txt"]
                .into_iter()
                .enumerate()
                .map(|(id, name)| {
                    let file = dir.path().join(name);
                    std::fs::write(&file, b"hello world")?;
                    let mut mutation = make_test_mutation(&file);
                    mutation.id = u32::try_from(id)?;
                    mutation.description = format!("fresh-budget {name}");
                    Ok::<_, anyhow::Error>(mutation)
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            let commands = CommandConfig {
                command: first_run_survives_second_kills_command(state.path()),
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            };
            seed_reused_survivor(dir.path(), &commands, &mutations[2], reuse_source)?;
            let expected_execution = match reuse_source {
                ReuseSource::ExactCache => MutationExecution::ExactCache,
                ReuseSource::IncrementalHistory => MutationExecution::IncrementalHistory,
            };
            let runner = TestRunner {
                commands,
                parallelism: 1,
                project_root: dir.path().to_path_buf(),
                verbose: false,
                show_output: false,
                max_tested: None,
                early_stop: EarlyStopConfig {
                    max_survivors: None,
                    fail_under: Some(60.0),
                },
                respect_workspace_ignores: true,
                env: HashMap::new(),
                incremental_history: true,
                force_rerun: false,
                learned_selection: false,
                cancelled: Arc::new(AtomicBool::new(false)),
            };

            let report = runner.run(mutations).report;
            let runs = std::fs::read_to_string(state.path().join("runs"))?;

            assert_eq!(report.planned_total, 3);
            assert_eq!(report.total, 2);
            assert_eq!(report.survived, 2);
            assert_eq!(report.killed, 0);
            assert_eq!(report.tested_count(), 1);
            assert_eq!(
                report.execution_for(0, MutationResult::Survived),
                MutationExecution::Executed
            );
            assert_eq!(
                report.execution_for(2, MutationResult::Survived),
                expected_execution
            );
            assert!(report.results.iter().all(|(mutation, _)| mutation.id != 1));
            assert_eq!(runs.trim(), "1");
            assert!(
                report
                    .early_stop_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains("--fail-under 60.0"))
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn max_tested_reservation_caps_execution_before_commands_run() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();

        let mutations: Vec<Mutation> = (0..4)
            .map(|i| {
                let file = dir.path().join(format!("test{i}.txt"));
                std::fs::write(&file, b"hello world").unwrap();
                let mut mutation = make_test_mutation(&file);
                mutation.id = i;
                mutation.description = format!("max reservation {i}");
                mutation
            })
            .collect();

        let script = r#"
lock="$STATE_DIR/lock"
while ! mkdir "$lock" 2>/dev/null; do sleep 0.01; done
runs=0
if [ -f "$STATE_DIR/runs" ]; then runs=$(cat "$STATE_DIR/runs"); fi
runs=$((runs + 1))
printf '%s\n' "$runs" > "$STATE_DIR/runs"
rmdir "$lock"
sleep 0.2
"#;
        let mut env = HashMap::new();
        env.insert("STATE_DIR".to_string(), state.path().display().to_string());

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["sh".into(), "-c".into(), script.into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 4,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: Some(1),
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env,
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(mutations).report;
        let runs: usize = std::fs::read_to_string(state.path().join("runs"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        assert_eq!(report.total, 1);
        assert_eq!(report.survived, 1);
        assert_eq!(
            runs, 1,
            "max_tested should reserve before execution, not after commands finish"
        );
    }

    #[cfg(unix)]
    #[test]
    fn max_tested_does_not_read_sources_for_unscheduled_mutations() {
        struct ReadHookGuard;

        impl Drop for ReadHookGuard {
            fn drop(&mut self) {
                set_source_content_read_hook(None);
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let mutations: Vec<Mutation> = (0..25)
            .map(|i| {
                let file = dir.path().join(format!("mutation{i}.txt"));
                std::fs::write(&file, b"hello world").unwrap();
                let mut mutation = make_test_mutation(&file);
                mutation.id = i;
                mutation.description = format!("lazy setup {i}");
                mutation
            })
            .collect();

        let reads = Arc::new(AtomicUsize::new(0));
        let root = dir.path().to_path_buf();
        let reads_for_hook = reads.clone();
        set_source_content_read_hook(Some(Arc::new(move |path: &Path| {
            if path.starts_with(&root) {
                reads_for_hook.fetch_add(1, Ordering::SeqCst);
            }
        })));
        let _guard = ReadHookGuard;

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["true".into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 8,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: Some(1),
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(mutations).report;

        assert_eq!(report.total, 1);
        assert_eq!(report.survived, 1);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "source content should be read only after a mutation reserves execution budget"
        );
    }

    #[cfg(unix)]
    #[test]
    fn max_tested_cache_hits_release_reservation_for_uncached_execution() {
        fn run_case(cached_result: MutationResult) {
            let dir = tempfile::tempdir().unwrap();
            let state = tempfile::tempdir().unwrap();
            let cached_file = dir.path().join("cached.txt");
            let uncached_file = dir.path().join("uncached.txt");
            std::fs::write(&cached_file, b"hello world").unwrap();
            std::fs::write(&uncached_file, b"hello world").unwrap();

            let mut cached_mutation = make_test_mutation(&cached_file);
            cached_mutation.id = 1;
            cached_mutation.description = format!("cached {cached_result}");
            let mut uncached_mutation = make_test_mutation(&uncached_file);
            uncached_mutation.id = 2;
            uncached_mutation.description = "uncached execution".into();

            let script = r#"
lock="$STATE_DIR/lock"
while ! mkdir "$lock" 2>/dev/null; do sleep 0.01; done
runs=0
if [ -f "$STATE_DIR/runs" ]; then runs=$(cat "$STATE_DIR/runs"); fi
runs=$((runs + 1))
printf '%s\n' "$runs" > "$STATE_DIR/runs"
rmdir "$lock"
"#;
            let mut env = HashMap::new();
            env.insert("STATE_DIR".to_string(), state.path().display().to_string());
            let commands = CommandConfig {
                command: vec!["sh".into(), "-c".into(), script.into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            };

            let selected = select_test_command(dir.path(), &commands, &cached_mutation);
            let cache_ctx = selected.cache_context(
                &commands.build_command,
                commands.build_command_origin,
                &commands.sandbox_command,
                &env,
            );
            let cache_ctx = format!(
                "{cache_ctx};context={:016x}",
                cache_context_fingerprint(dir.path())
            );
            let cached_content = std::fs::read(&cached_file).unwrap();
            let key = CacheKey::new(
                &cached_content,
                &cache_identity(dir.path(), &cached_mutation),
                &cached_mutation.description,
                &cache_ctx,
            );
            cache::store(dir.path(), &key, cached_result);

            let runner = TestRunner {
                commands,
                parallelism: 2,
                project_root: dir.path().to_path_buf(),
                verbose: false,
                show_output: false,
                max_tested: Some(1),
                early_stop: Default::default(),
                respect_workspace_ignores: true,
                env,
                incremental_history: true,
                force_rerun: false,
                learned_selection: false,
                cancelled: Arc::new(AtomicBool::new(false)),
            };

            let report = runner.run(vec![cached_mutation, uncached_mutation]).report;
            let runs: usize = std::fs::read_to_string(state.path().join("runs"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();

            assert_eq!(report.total, 2);
            assert_eq!(report.results[0].1, cached_result);
            assert_eq!(report.results[1].1, MutationResult::Survived);
            assert_eq!(
                report.execution_for(report.results[0].0.id, cached_result),
                MutationExecution::ExactCache
            );
            assert_eq!(
                report.execution_for(report.results[1].0.id, MutationResult::Survived),
                MutationExecution::Executed
            );
            assert_eq!(report.tested_count(), 1);
            assert_eq!(report.execution_counts().exact_cache_reused, 1);
            assert_eq!(
                runs, 1,
                "cache hits should not consume the max_tested execution budget"
            );
        }

        run_case(MutationResult::Survived);
        run_case(MutationResult::Killed);
    }

    #[cfg(unix)]
    #[test]
    fn cache_identity_excludes_parallelism() {
        // Exact-cache identity must not include the runner's parallelism:
        // a mutation executed by a jobs=1 run must be an exact cache hit for
        // an otherwise identical jobs=4 run. This guards the real regression
        // (adding jobs to CacheKey::new or the cache context) that the
        // benchmark harness fakes cannot see.
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        let file = dir.path().join("target.txt");
        std::fs::write(&file, b"hello world").unwrap();
        let mut mutation = make_test_mutation(&file);
        mutation.id = 7;
        mutation.description = "jobs cache identity".into();

        let script = r#"
lock="$STATE_DIR/lock"
while ! mkdir "$lock" 2>/dev/null; do sleep 0.01; done
runs=0
if [ -f "$STATE_DIR/runs" ]; then runs=$(cat "$STATE_DIR/runs"); fi
runs=$((runs + 1))
printf '%s\n' "$runs" > "$STATE_DIR/runs"
rmdir "$lock"
"#;
        let mut env = HashMap::new();
        env.insert("STATE_DIR".to_string(), state.path().display().to_string());

        let make_runner = |parallelism: usize| TestRunner {
            commands: CommandConfig {
                command: vec!["sh".into(), "-c".into(), script.into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: env.clone(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        // Phase 1: a jobs=1 run executes the mutation and seeds the cache.
        let first = make_runner(1).run(vec![mutation.clone()]).report;
        assert_eq!(first.total, 1);
        assert_eq!(first.results[0].1, MutationResult::Survived);
        assert_eq!(
            first.execution_for(first.results[0].0.id, MutationResult::Survived),
            MutationExecution::Executed
        );

        // Phase 2: an otherwise identical jobs=4 run must hit the exact cache.
        let second = make_runner(4).run(vec![mutation]).report;
        assert_eq!(second.total, 1);
        assert_eq!(second.results[0].1, MutationResult::Survived);
        assert_eq!(
            second.execution_for(second.results[0].0.id, MutationResult::Survived),
            MutationExecution::ExactCache
        );
        assert_eq!(second.tested_count(), 0);
        assert_eq!(second.execution_counts().exact_cache_reused, 1);

        let runs: usize = std::fs::read_to_string(state.path().join("runs"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_eq!(
            runs, 1,
            "parallelism must not change exact-cache identity: the underlying test ran once"
        );
    }

    #[cfg(unix)]
    #[test]
    fn max_tested_does_not_count_build_errors() {
        let dir = tempfile::tempdir().unwrap();

        let mutations: Vec<Mutation> = ["first.txt", "second.txt", "third.txt"]
            .into_iter()
            .enumerate()
            .map(|(i, file_name)| {
                let file = dir.path().join(file_name);
                std::fs::write(&file, b"hello world").unwrap();
                let mut mutation = make_test_mutation(&file);
                mutation.id = i as u32;
                mutation.description = format!("build classification {i}");
                if i < 2 {
                    mutation.replacement = "build_error".into();
                } else {
                    mutation.replacement = "ok".into();
                }
                mutation
            })
            .collect();

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["true".into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![
                    "sh".into(),
                    "-c".into(),
                    "grep -q build_error *.txt && exit 1; exit 0".into(),
                ],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::Configured,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 4,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: Some(1),
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(mutations).report;

        assert_eq!(report.total, 3);
        assert_eq!(report.build_errors, 2);
        assert_eq!(report.survived, 1);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_queue_preserves_report_order_and_caps_concurrency() {
        let dir = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();

        let mutations: Vec<Mutation> = (0..5)
            .map(|i| {
                let file = dir.path().join(format!("test{i}.txt"));
                std::fs::write(&file, b"hello world").unwrap();
                let mut mutation = make_test_mutation(&file);
                mutation.id = (100 - i) as u32;
                mutation.description = format!("queue mutation {i}");
                mutation
            })
            .collect();
        let expected_ids: Vec<u32> = mutations.iter().map(|mutation| mutation.id).collect();

        let script = r#"
lock="$STATE_DIR/lock"
while ! mkdir "$lock" 2>/dev/null; do sleep 0.01; done
active=0
if [ -f "$STATE_DIR/active" ]; then active=$(cat "$STATE_DIR/active"); fi
active=$((active + 1))
printf '%s\n' "$active" > "$STATE_DIR/active"
max=0
if [ -f "$STATE_DIR/max" ]; then max=$(cat "$STATE_DIR/max"); fi
if [ "$active" -gt "$max" ]; then printf '%s\n' "$active" > "$STATE_DIR/max"; fi
rmdir "$lock"
sleep 0.15
while ! mkdir "$lock" 2>/dev/null; do sleep 0.01; done
active=$(cat "$STATE_DIR/active")
active=$((active - 1))
printf '%s\n' "$active" > "$STATE_DIR/active"
rmdir "$lock"
"#;
        let mut env = HashMap::new();
        env.insert("STATE_DIR".to_string(), state.path().display().to_string());

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["sh".into(), "-c".into(), script.into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 2,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env,
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(mutations).report;
        let actual_ids: Vec<u32> = report
            .results
            .iter()
            .map(|(mutation, _)| mutation.id)
            .collect();
        let max_active: usize = std::fs::read_to_string(state.path().join("max"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        assert_eq!(report.total, 5);
        assert_eq!(report.survived, 5);
        assert_eq!(actual_ids, expected_ids);
        assert!(
            max_active <= 2,
            "runner should not execute more commands concurrently than parallelism"
        );
    }

    #[test]
    fn report_aggregates_results_correctly() {
        let dir = tempfile::tempdir().unwrap();

        let survived_file = dir.path().join("survived.txt");
        std::fs::write(&survived_file, b"hello world").unwrap();
        let mut m_survived = make_test_mutation(&survived_file);
        m_survived.language = "survived_lang".into();

        let killed_file = dir.path().join("killed.txt");
        std::fs::write(&killed_file, b"hello world").unwrap();
        let mut m_killed = make_test_mutation(&killed_file);
        m_killed.id = 2;
        m_killed.language = "killed_lang".into();

        let mut lang_cmds = HashMap::new();
        lang_cmds.insert("survived_lang".into(), vec!["true".into()]);
        lang_cmds.insert("killed_lang".into(), vec!["false".into()]);

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["true".into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: lang_cmds,
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 2,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(vec![m_survived, m_killed]).report;
        assert_eq!(report.total, 2);
        assert_eq!(report.killed, 1);
        assert_eq!(report.survived, 1);
        assert_eq!(report.timeout, 0);
        assert_eq!(report.build_errors, 0);
    }

    #[test]
    fn language_commands_override_default() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();

        let mut mutation = make_test_mutation(&file);
        mutation.language = "go".into();

        let mut lang_cmds = HashMap::new();
        lang_cmds.insert("go".into(), vec!["false".into()]); // "false" = always fails = killed

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["true".into()], // default would survive
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: lang_cmds,
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(vec![mutation]).report;
        assert_eq!(report.killed, 1, "should use language-specific command");
        assert_eq!(
            report.test_command, None,
            "mixed language-specific commands should not report the default command as global context"
        );
    }

    #[cfg(unix)]
    #[test]
    fn mutations_on_same_file_run_in_isolated_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();

        // Each mutation has a unique description so cache can't short-circuit
        let mutations: Vec<Mutation> = ["alpha", "bravo", "charlie", "delta"]
            .into_iter()
            .enumerate()
            .map(|(i, replacement)| Mutation {
                id: i as u32,
                file: file.clone(),
                language: String::new(),
                line: 1,
                column: 1,
                operator: "test".into(),
                description: format!("unique mutation {i}"),
                original: "hello".into(),
                replacement: replacement.into(),
                byte_range: 0..5,
            })
            .collect();

        let barrier = tempfile::tempdir().unwrap();
        let script = format!(
            r#"value="$(cat test.txt)"
case "$value" in
  "alpha world"|"bravo world"|"charlie world"|"delta world") ;;
  *) exit 1 ;;
esac
printf '%s\n' "$PWD" > "{barrier}/$value"
while [ "$(find "{barrier}" -type f | wc -l)" -lt 4 ]; do sleep 0.1; done"#,
            barrier = barrier.path().display()
        );

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["sh".into(), "-c".into(), script],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 4,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(mutations).report;
        assert_eq!(report.total, 4);
        assert_eq!(report.survived, 4);
        assert_eq!(report.killed, 0);
        assert_eq!(report.timeout, 0);
        assert_eq!(report.build_errors, 0);
        let roots: std::collections::HashSet<String> = std::fs::read_dir(barrier.path())
            .unwrap()
            .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
            .collect();
        assert_eq!(
            roots.len(),
            4,
            "expected one distinct workspace root per same-file mutation"
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[cfg(unix)]
    #[test]
    fn mutations_on_different_files_run_in_isolated_workspaces() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        std::fs::write(&first, b"hello world").unwrap();
        std::fs::write(&second, b"hello world").unwrap();

        let mutations: Vec<Mutation> = [first.clone(), second.clone()]
            .into_iter()
            .enumerate()
            .map(|(i, file)| Mutation {
                id: i as u32,
                file,
                language: String::new(),
                line: 1,
                column: 1,
                operator: "test".into(),
                description: format!("different file mutation {i}"),
                original: "hello".into(),
                replacement: "world".into(),
                byte_range: 0..5,
            })
            .collect();

        let script = r#"first=$(cat first.txt); second=$(cat second.txt)
if { [ "$first" = "world world" ] && [ "$second" = "hello world" ]; } ||
   { [ "$first" = "hello world" ] && [ "$second" = "world world" ]; }; then
  exit 0
fi
echo "unexpected workspace contents: first=$first second=$second"
exit 1"#
            .to_string();

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["sh".into(), "-c".into(), script],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 4,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(mutations).report;
        assert_eq!(report.total, 2);
        assert_eq!(
            report.killed, 0,
            "each workspace should contain exactly one active mutation"
        );
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "hello world");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "hello world");
    }

    #[cfg(unix)]
    #[test]
    fn reused_workspace_is_reset_between_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.txt");
        let second = dir.path().join("second.txt");
        std::fs::write(&first, b"hello world").unwrap();
        std::fs::write(&second, b"hello world").unwrap();

        let mut first_mutation = make_test_mutation(&first);
        first_mutation.description = "first side-effect mutation".into();
        let mut second_mutation = make_test_mutation(&second);
        second_mutation.id = 2;
        second_mutation.description = "second side-effect mutation".into();

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    "test ! -f side_effect && touch side_effect".into(),
                ],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts: HashMap::new(),
                test_selection: None,
            },
            parallelism: 1,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(vec![first_mutation, second_mutation]).report;

        assert_eq!(report.total, 2);
        assert_eq!(report.survived, 2);
        assert_eq!(report.killed, 0);
    }

    #[test]
    fn per_language_timeout_overrides_default() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("test.txt");
        std::fs::write(&file, b"hello world").unwrap();

        // "slow_lang" gets a 100ms timeout → sleep 1s will timeout
        // "fast_lang" uses the default 5s → sleep 0.01s will survive
        let mut language_timeouts = HashMap::new();
        language_timeouts.insert("slow_lang".into(), Duration::from_millis(100));

        let mut m_slow = make_test_mutation(&file);
        m_slow.language = "slow_lang".into();
        m_slow.description = "slow".into();

        let mut m_fast = make_test_mutation(&file);
        m_fast.id = 2;
        m_fast.file = dir.path().join("test2.txt");
        std::fs::write(&m_fast.file, b"hello world").unwrap();
        m_fast.language = "fast_lang".into();
        m_fast.description = "fast".into();

        let runner = TestRunner {
            commands: CommandConfig {
                command: vec!["sleep".into(), "1".into()],
                force_default_command: false,
                force_default_timeout: false,
                project_commands: vec![],
                language_commands: HashMap::new(),
                build_command: vec![],
                sandbox_command: vec![],
                build_command_origin: BuildCommandOrigin::None,
                timeout: Duration::from_secs(5),
                language_timeouts,
                test_selection: None,
            },
            parallelism: 2,
            project_root: dir.path().to_path_buf(),
            verbose: false,
            show_output: false,
            max_tested: None,
            early_stop: Default::default(),
            respect_workspace_ignores: true,
            env: HashMap::new(),
            incremental_history: true,
            force_rerun: false,
            learned_selection: false,
            cancelled: Arc::new(AtomicBool::new(false)),
        };

        let report = runner.run(vec![m_slow, m_fast]).report;
        let slow_result = report
            .results
            .iter()
            .find(|(m, _)| m.description == "slow")
            .map(|(_, r)| *r);
        let fast_result = report
            .results
            .iter()
            .find(|(m, _)| m.description == "fast")
            .map(|(_, r)| *r);

        assert_eq!(slow_result, Some(MutationResult::Timeout));
        assert_eq!(fast_result, Some(MutationResult::Survived));
    }
}
