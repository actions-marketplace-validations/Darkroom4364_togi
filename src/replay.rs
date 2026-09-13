use crate::config::BuildCommandOrigin;
use crate::source_identity::{
    is_normalized_project_relative_path, normalized_project_relative_path, range_matches,
    resolve_normalized_project_relative_path, source_fingerprint,
};
use crate::{Mutation, MutationExecution, MutationResult};
use anyhow::Context;
#[cfg(not(windows))]
use cap_fs_ext::MetadataExt as CapMetadataExt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

pub const REPORT_KIND: &str = "mutation_report";
pub const REPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectRecipeOrigin {
    Executed,
    ExactCache,
    IncrementalHistory,
}

impl DirectRecipeOrigin {
    pub(crate) fn from_execution(execution: MutationExecution) -> Option<Self> {
        match execution {
            MutationExecution::Executed => Some(Self::Executed),
            MutationExecution::ExactCache => Some(Self::ExactCache),
            MutationExecution::IncrementalHistory => Some(Self::IncrementalHistory),
            MutationExecution::NotExecuted(_) => None,
        }
    }

    fn matches_execution(&self, execution: MutationExecution) -> bool {
        matches!(
            (self, execution),
            (Self::Executed, MutationExecution::Executed)
                | (Self::ExactCache, MutationExecution::ExactCache)
                | (
                    Self::IncrementalHistory,
                    MutationExecution::IncrementalHistory
                )
        )
    }
}

/// The final, direct one-mutation invocation captured before cache/history
/// lookup. Commands are already sandbox-prefixed and must be run as-is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegularDirectRecipe {
    pub test_command: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_command: Option<Vec<String>>,
    /// The source of `build_command`; legacy reports default to configured.
    pub build_command_origin: BuildCommandOrigin,
    pub timeout_ms: u64,
    pub env: BTreeMap<String, String>,
    pub respect_workspace_ignores: bool,
    pub origin: DirectRecipeOrigin,
}

#[derive(Debug, Clone)]
pub struct CapturedMutationSource {
    pub path: String,
    pub source_fingerprint: String,
    pub byte_start: usize,
    pub byte_end: usize,
    pub language: String,
    pub original: String,
    pub replacement: String,
}

#[derive(Debug, Clone)]
pub struct ReplayReportCapture {
    source_revision: Option<String>,
    sources: BTreeMap<u32, CapturedMutationSource>,
    capture_failed: BTreeSet<u32>,
}

impl ReplayReportCapture {
    /// Capture all replay source evidence before any baseline or mutation
    /// command can execute.
    pub fn capture(project_root: &Path, mutations: &[Mutation]) -> Self {
        let source_revision = git_head(project_root).ok();
        let mut sources = BTreeMap::new();
        let mut capture_failed = BTreeSet::new();
        for mutation in mutations {
            let Some(path) = normalized_project_relative_path(project_root, &mutation.file) else {
                capture_failed.insert(mutation.id);
                continue;
            };
            let Some(resolved) = resolve_normalized_project_relative_path(project_root, &path)
            else {
                capture_failed.insert(mutation.id);
                continue;
            };
            let Ok(source) = std::fs::read(resolved) else {
                capture_failed.insert(mutation.id);
                continue;
            };
            if !range_matches(
                &source,
                mutation.byte_range.start,
                mutation.byte_range.end,
                &mutation.original,
            ) {
                capture_failed.insert(mutation.id);
                continue;
            }
            sources.insert(
                mutation.id,
                CapturedMutationSource {
                    path,
                    source_fingerprint: source_fingerprint(&source),
                    byte_start: mutation.byte_range.start,
                    byte_end: mutation.byte_range.end,
                    language: mutation.language.clone(),
                    original: mutation.original.clone(),
                    replacement: mutation.replacement.clone(),
                },
            );
        }
        Self {
            source_revision,
            sources,
            capture_failed,
        }
    }

    /// Revalidate initial evidence immediately before JSON serialization. A
    /// changed revision or target file makes the captured direct invocation
    /// unavailable instead of publishing a misleading replay recipe.
    pub fn revalidate(&mut self, project_root: &Path) {
        let revision_matches = self
            .source_revision
            .as_deref()
            .zip(git_head(project_root).ok().as_deref())
            .is_some_and(|(expected, current)| expected == current);
        if !revision_matches {
            self.capture_failed.extend(self.sources.keys().copied());
            return;
        }
        for (id, source) in &self.sources {
            let valid = resolve_normalized_project_relative_path(project_root, &source.path)
                .and_then(|path| std::fs::read(path).ok())
                .is_some_and(|bytes| {
                    source_fingerprint(&bytes) == source.source_fingerprint
                        && range_matches(
                            &bytes,
                            source.byte_start,
                            source.byte_end,
                            &source.original,
                        )
                });
            if !valid {
                self.capture_failed.insert(*id);
            }
        }
    }

    pub fn source_revision(&self) -> Option<&str> {
        self.source_revision.as_deref()
    }

    pub fn source_for(&self, mutation_id: u32) -> Option<&CapturedMutationSource> {
        self.sources.get(&mutation_id)
    }

    pub fn capture_failed(&self, mutation_id: u32) -> bool {
        self.capture_failed.contains(&mutation_id)
            || self.source_revision.is_none()
            || !self.sources.contains_key(&mutation_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayUnavailableReason {
    CaptureFailed,
    Schemata,
    NotExecuted,
    MissingDirectRecipe,
}

impl ReplayUnavailableReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CaptureFailed => "capture_failed",
            Self::Schemata => "schemata",
            Self::NotExecuted => "not_executed",
            Self::MissingDirectRecipe => "missing_direct_recipe",
        }
    }
}

pub fn replay_unavailable_reason(
    capture: &ReplayReportCapture,
    mutation_id: u32,
    result: MutationResult,
    direct_recipe: Option<&RegularDirectRecipe>,
    execution: MutationExecution,
    schemata_enabled: bool,
) -> Option<ReplayUnavailableReason> {
    if capture.capture_failed(mutation_id) {
        return Some(ReplayUnavailableReason::CaptureFailed);
    }
    if !matches!(
        result,
        MutationResult::Killed | MutationResult::Survived | MutationResult::Timeout
    ) {
        return Some(ReplayUnavailableReason::NotExecuted);
    }
    let Some(recipe) = direct_recipe else {
        return Some(if schemata_enabled {
            ReplayUnavailableReason::Schemata
        } else {
            ReplayUnavailableReason::MissingDirectRecipe
        });
    };
    if !recipe.origin.matches_execution(execution) {
        return Some(ReplayUnavailableReason::CaptureFailed);
    }
    None
}

pub fn git_head(project_root: &Path) -> anyhow::Result<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .current_dir(project_root)
        .output()
        .context("could not read Git HEAD")?;
    if !output.status.success() {
        anyhow::bail!(
            "could not read Git HEAD: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let revision = String::from_utf8(output.stdout)
        .context("Git HEAD was not UTF-8")?
        .trim()
        .to_owned();
    if !is_valid_git_revision(&revision) {
        anyhow::bail!("Git returned an invalid HEAD revision");
    }
    Ok(revision)
}

fn is_valid_git_revision(revision: &str) -> bool {
    matches!(revision.len(), 40 | 64)
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Debug, Deserialize)]
struct ReplayReportV1 {
    kind: String,
    schema_version: u32,
    generator: String,
    source_revision: Option<String>,
    mutations: Vec<ReplayMutationV1>,
}

#[derive(Debug, Deserialize)]
struct ReplayMutationV1 {
    id: u32,
    line: usize,
    column: usize,
    language: String,
    operator: String,
    description: String,
    result: String,
    source_path: String,
    byte_start: usize,
    byte_end: usize,
    source_fingerprint: String,
    original: String,
    replacement: String,
    replay: StoredReplayRecipe,
    execution: StoredMutationExecution,
}

#[derive(Debug, Deserialize)]
struct StoredMutationExecution {
    state: StoredExecutionState,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StoredExecutionState {
    Executed,
    ExactCache,
    IncrementalHistory,
    NotExecuted,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StoredReplayRecipe {
    RegularDirect {
        test_command: Vec<String>,
        #[serde(default)]
        build_command: Option<Vec<String>>,
        #[serde(default = "legacy_build_command_origin")]
        build_command_origin: BuildCommandOrigin,
        timeout_ms: u64,
        #[serde(default)]
        env: BTreeMap<String, String>,
        respect_workspace_ignores: bool,
        origin: DirectRecipeOrigin,
    },
    Unavailable {
        reason: String,
    },
}

struct ValidatedReplay {
    source_revision: String,
    source_fingerprint: String,
    expected: MutationResult,
    mutation: Mutation,
    recipe: RegularDirectRecipe,
}

/// Parse, validate, and force one fresh direct replay from the current Git
/// project. The report is the complete command/config input; no TOML, cache,
/// history, learned selection, or schemata configuration is consulted.
pub fn replay_mutation(
    mutant_id: u32,
    report_path: &Path,
    show_output: bool,
    verify_killed: bool,
    cancelled: &AtomicBool,
) -> anyhow::Result<()> {
    let validated = validate_report_mutation(read_v1_report(report_path)?, mutant_id)?;
    if verify_killed && validated.expected != MutationResult::Survived {
        anyhow::bail!("--verify-killed requires a recorded survivor");
    }
    let project_root = current_project_root()?;
    let current_revision = validate_project_and_source(&project_root, &validated, verify_killed)?;

    let build_command =
        replay_build_command(&validated.recipe, crate::config::AUTO_GO_COMPILE_OUTPUT);
    let build_command_json = build_command
        .as_ref()
        .map(serde_json::to_string)
        .transpose()?;

    let run = if verify_killed {
        crate::runner::run_repair_verification
    } else {
        crate::runner::run_replay_mutation
    };
    let fresh = run(
        &project_root,
        &validated.mutation,
        crate::runner::ReplayRunConfig {
            test_command: validated.recipe.test_command.clone(),
            build_command,
            timeout: Duration::from_millis(validated.recipe.timeout_ms),
            env: validated
                .recipe
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            respect_workspace_ignores: validated.recipe.respect_workspace_ignores,
            source_revision: &current_revision,
            source_fingerprint: &validated.source_fingerprint,
            show_output,
            cancelled,
        },
    )?;
    if fresh.cancelled {
        anyhow::bail!("replay cancelled");
    }

    println!("Replay mutation #{mutant_id}");
    if verify_killed {
        println!(
            "Verification: current unmutated suite passed; checking the recorded survivor is killed."
        );
        println!("Report Git HEAD: {}", validated.source_revision);
        println!("Current Git HEAD: {current_revision} (including working-tree changes)");
    }
    println!(
        "Expected historical result: {}",
        result_name(validated.expected)
    );
    println!("Fresh result: {}", result_name(fresh.result));
    println!(
        "Execution: forced fresh direct execution; cache and history were not read or written."
    );
    println!(
        "Effective command: {}",
        serde_json::to_string(&validated.recipe.test_command)?
    );
    if let Some(build_command) = build_command_json {
        println!("Effective build command: {build_command}");
    }
    if show_output {
        if let Some(output) = fresh
            .test_output
            .as_deref()
            .filter(|output| !output.is_empty())
        {
            println!("Output:\n{output}");
        }
    }

    if verify_killed {
        if fresh.result != MutationResult::Killed {
            anyhow::bail!(
                "repair not verified: expected killed, fresh execution returned {}",
                result_name(fresh.result)
            );
        }
        println!("Verified: the current suite kills mutation #{mutant_id}.");
    } else if fresh.result != validated.expected {
        anyhow::bail!(
            "replay divergence: report expected {}, fresh execution returned {}",
            result_name(validated.expected),
            result_name(fresh.result)
        );
    }
    Ok(())
}

fn read_v1_report(report_path: &Path) -> anyhow::Result<ReplayReportV1> {
    let content = std::fs::read_to_string(report_path)
        .map_err(|error| anyhow::anyhow!("could not read {}: {error}", report_path.display()))?;
    let report: ReplayReportV1 = serde_json::from_str(&content).map_err(|error| {
        anyhow::anyhow!(
            "could not parse {} as a replayable v1 JSON mutation report: {error}",
            report_path.display()
        )
    })?;
    if report.kind != REPORT_KIND {
        anyhow::bail!(
            "report kind {:?} is not a mutation report; regenerate a v1 JSON report",
            report.kind
        );
    }
    if report.schema_version != REPORT_SCHEMA_VERSION {
        anyhow::bail!(
            "report schema version {} is unsupported; regenerate a v1 JSON report",
            report.schema_version
        );
    }
    if report.generator.trim().is_empty() {
        anyhow::bail!("report generator is empty; regenerate a v1 JSON report");
    }
    let Some(source_revision) = report.source_revision.as_deref() else {
        anyhow::bail!(
            "report was generated without a Git source revision and cannot be replayed; rerun `togi check` from a Git worktree"
        );
    };
    if !is_valid_git_revision(source_revision) {
        anyhow::bail!("report source revision is invalid; regenerate a v1 JSON report");
    }
    Ok(report)
}

fn validate_report_mutation(
    report: ReplayReportV1,
    mutant_id: u32,
) -> anyhow::Result<ValidatedReplay> {
    if mutant_id == 0 {
        anyhow::bail!("mutation id must be a 1-based report-local id");
    }
    let source_revision = report.source_revision.ok_or_else(|| {
        anyhow::anyhow!(
            "report was generated without a Git source revision and cannot be replayed; rerun `togi check` from a Git worktree"
        )
    })?;
    let mut matching = report
        .mutations
        .into_iter()
        .filter(|mutation| mutation.id == mutant_id);
    let mutation = matching
        .next()
        .ok_or_else(|| anyhow::anyhow!("mutation id {mutant_id} not found in report"))?;
    if matching.next().is_some() {
        anyhow::bail!("report contains duplicate mutation id {mutant_id}");
    }
    let execution = mutation.execution;
    if execution.state == StoredExecutionState::NotExecuted {
        anyhow::bail!("mutation id {mutant_id} was not executed and is not replayable");
    }
    let recipe = match mutation.replay {
        StoredReplayRecipe::RegularDirect {
            test_command,
            build_command,
            build_command_origin,
            timeout_ms,
            env,
            respect_workspace_ignores,
            origin,
        } => RegularDirectRecipe {
            test_command,
            build_command,
            build_command_origin,
            timeout_ms,
            env,
            respect_workspace_ignores,
            origin,
        },
        StoredReplayRecipe::Unavailable { reason } => {
            anyhow::bail!("mutation id {mutant_id} is not replayable: {reason}");
        }
    };
    validate_execution_provenance(&execution, &recipe, mutant_id)?;
    let expected = parse_replayable_result(&mutation.result)?;
    let validated = ValidatedReplay {
        source_revision,
        source_fingerprint: mutation.source_fingerprint,
        expected,
        mutation: Mutation {
            id: mutation.id - 1,
            file: PathBuf::from(mutation.source_path),
            language: mutation.language,
            line: mutation.line,
            column: mutation.column,
            operator: mutation.operator,
            description: mutation.description,
            original: mutation.original,
            replacement: mutation.replacement,
            byte_range: mutation.byte_start..mutation.byte_end,
        },
        recipe,
    };
    validate_static_replay(&validated)?;
    Ok(validated)
}

fn validate_execution_provenance(
    execution: &StoredMutationExecution,
    recipe: &RegularDirectRecipe,
    mutant_id: u32,
) -> anyhow::Result<()> {
    if execution.reason.is_some() {
        anyhow::bail!("mutation id {mutant_id} has an invalid execution provenance reason");
    }
    let matches_origin = matches!(
        (execution.state, &recipe.origin),
        (StoredExecutionState::Executed, DirectRecipeOrigin::Executed)
            | (
                StoredExecutionState::ExactCache,
                DirectRecipeOrigin::ExactCache
            )
            | (
                StoredExecutionState::IncrementalHistory,
                DirectRecipeOrigin::IncrementalHistory
            )
    );
    if !matches_origin {
        anyhow::bail!(
            "mutation id {mutant_id} execution provenance does not match replay recipe origin"
        );
    }
    Ok(())
}

fn validate_static_replay(replay: &ValidatedReplay) -> anyhow::Result<()> {
    validate_replay_source_path(&replay.mutation.file)?;
    if !is_valid_source_fingerprint(&replay.source_fingerprint) {
        anyhow::bail!("report source fingerprint is invalid");
    }
    if replay.mutation.byte_range.start > replay.mutation.byte_range.end {
        anyhow::bail!("report mutation byte range is invalid");
    }
    validate_recipe(&replay.recipe)
}

fn validate_replay_source_path(path: &Path) -> anyhow::Result<()> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("report source path is not valid UTF-8"))?;
    if !is_normalized_project_relative_path(path) {
        anyhow::bail!("report source path is not a normalized safe project-relative path");
    }
    if path.split('/').any(is_replay_control_path_component) {
        anyhow::bail!("report source path targets a Togi or Git control path");
    }
    Ok(())
}

fn is_replay_control_path_component(component: &str) -> bool {
    let component = component.to_ascii_lowercase();
    matches!(
        component.as_str(),
        ".git" | ".togi" | ".togi-cache" | ".togi.lock" | ".togi-baseline"
    ) || component.starts_with(".togi-")
}

fn parse_replayable_result(result: &str) -> anyhow::Result<MutationResult> {
    match result {
        "killed" => Ok(MutationResult::Killed),
        "survived" => Ok(MutationResult::Survived),
        "timeout" => Ok(MutationResult::Timeout),
        _ => anyhow::bail!("report contains a non-replayable historical result {result:?}"),
    }
}

fn current_project_root() -> anyhow::Result<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .context("could not locate current Git project")?;
    if !output.status.success() {
        anyhow::bail!("not a git repository; run replay from the report's project root");
    }
    let path = String::from_utf8(output.stdout)
        .context("Git project root was not UTF-8")?
        .trim()
        .to_owned();
    // Keep Git's own path form: canonicalize() would produce a verbatim
    // `\\?\`-prefixed path on Windows that `git clone` misclassifies as a
    // remote. Validation canonicalizes internally where identity matters.
    Ok(PathBuf::from(path))
}

fn validate_project_and_source(
    project_root: &Path,
    replay: &ValidatedReplay,
    allow_changed_revision: bool,
) -> anyhow::Result<String> {
    let current_revision = git_head(project_root)?;
    if !allow_changed_revision && current_revision != replay.source_revision {
        anyhow::bail!(
            "report Git HEAD {} does not match current Git HEAD {}",
            replay.source_revision,
            current_revision
        );
    }
    validate_replay_source_path(&replay.mutation.file)?;
    let source_path = replay.mutation.file.to_string_lossy();
    let resolved = resolve_normalized_project_relative_path(project_root, &source_path)
        .ok_or_else(|| {
            anyhow::anyhow!("could not safely resolve report source path {source_path:?}")
        })?;
    let canonical_root = project_root
        .canonicalize()
        .context("could not canonicalize current Git project root")?;
    let resolved_relative = normalized_project_relative_path(&canonical_root, &resolved)
        .ok_or_else(|| anyhow::anyhow!("could not normalize resolved replay source path"))?;
    if resolved_relative
        .split('/')
        .any(is_replay_control_path_component)
    {
        anyhow::bail!("resolved replay source path targets a Togi or Git control path");
    }
    let metadata = std::fs::metadata(&resolved)
        .with_context(|| format!("could not inspect replay source {}", resolved.display()))?;
    if !metadata.is_file() {
        anyhow::bail!("replay source {} is not a regular file", resolved.display());
    }
    #[cfg(not(windows))]
    if CapMetadataExt::nlink(&metadata) > 1 {
        // A safe lexical path can otherwise hard-link to Togi control state.
        anyhow::bail!(
            "replay source {} has multiple hard links and cannot be isolated safely",
            resolved.display()
        );
    }
    let source = read_replay_source(&resolved)
        .with_context(|| format!("could not read replay source {}", resolved.display()))?;
    if source_fingerprint(&source) != replay.source_fingerprint {
        anyhow::bail!("report source fingerprint does not match current target source");
    }
    if !range_matches(
        &source,
        replay.mutation.byte_range.start,
        replay.mutation.byte_range.end,
        &replay.mutation.original,
    ) {
        anyhow::bail!("report mutation byte range and original bytes do not match target source");
    }
    Ok(current_revision)
}

fn read_replay_source(path: &Path) -> std::io::Result<Vec<u8>> {
    #[cfg(test)]
    REPLAY_SOURCE_READS.with(|reads| reads.set(reads.get() + 1));
    std::fs::read(path)
}

#[cfg(test)]
thread_local! {
    static REPLAY_SOURCE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn is_valid_source_fingerprint(fingerprint: &str) -> bool {
    fingerprint.strip_prefix("sha256:").is_some_and(|hash| {
        hash.len() == 64
            && hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn validate_recipe(recipe: &RegularDirectRecipe) -> anyhow::Result<()> {
    validate_command(&recipe.test_command, "test")?;
    if let Some(build_command) = &recipe.build_command {
        validate_command(build_command, "build")?;
    }
    if recipe.timeout_ms == 0 {
        anyhow::bail!("report replay timeout must be greater than zero");
    }
    for (key, value) in &recipe.env {
        if !is_valid_env_key(key) || value.contains('\0') {
            anyhow::bail!("report replay environment override is invalid");
        }
    }
    Ok(())
}

fn validate_command(command: &[String], label: &str) -> anyhow::Result<()> {
    let Some(program) = command.first() else {
        anyhow::bail!("report replay {label} command is empty");
    };
    if program.is_empty() || command.iter().any(|arg| arg.contains('\0')) {
        anyhow::bail!("report replay {label} command is invalid");
    }
    Ok(())
}

fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(ch) if ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

/// Clone a serialized build command for replay, adapting only the known
/// auto-detected Go null-device argument to this host.
fn replay_build_command(
    recipe: &RegularDirectRecipe,
    target_null_device: &str,
) -> Option<Vec<String>> {
    let mut build_command = recipe.build_command.clone();
    if recipe.build_command_origin == BuildCommandOrigin::AutoDetected {
        if let Some(command) = &mut build_command {
            normalize_auto_go_compile_output(command, target_null_device);
        }
    }
    build_command
}

/// Normalize only the terminal exact auto-detected Go compile command.
///
/// The command may have a sandbox wrapper prefix. Replay recipes retain their
/// serialized command unchanged; this operates on the execution copy after
/// validation, so cache identity remains host-specific.
fn normalize_auto_go_compile_output(command: &mut [String], target_null_device: &str) {
    const AUTO_GO_COMPILE_ARG_COUNT: usize = 7;
    const OUTPUT_INDEX: usize = 5;
    if command.len() < AUTO_GO_COMPILE_ARG_COUNT {
        return;
    }
    let suffix_start = command.len() - AUTO_GO_COMPILE_ARG_COUNT;
    let suffix = &mut command[suffix_start..];
    let is_auto_go_compile = suffix[0] == "go"
        && suffix[1] == "test"
        && suffix[2] == "-c"
        && suffix[3] == "-vet=off"
        && suffix[4] == "-o"
        && (suffix[OUTPUT_INDEX] == crate::config::AUTO_GO_COMPILE_UNIX_OUTPUT
            || suffix[OUTPUT_INDEX] == crate::config::AUTO_GO_COMPILE_WINDOWS_OUTPUT)
        && suffix[6] == "./...";
    if is_auto_go_compile && suffix[OUTPUT_INDEX] != target_null_device {
        suffix[OUTPUT_INDEX] = target_null_device.to_owned();
    }
}

fn legacy_build_command_origin() -> BuildCommandOrigin {
    BuildCommandOrigin::Configured
}

fn result_name(result: MutationResult) -> &'static str {
    match result {
        MutationResult::Killed => "killed",
        MutationResult::Survived => "survived",
        MutationResult::Timeout => "timeout",
        MutationResult::BuildError => "build_error",
        MutationResult::Uncovered => "uncovered",
        MutationResult::Subsumed => "subsumed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn static_test_report(source_path: &str, test_command: Vec<String>) -> ReplayReportV1 {
        ReplayReportV1 {
            kind: REPORT_KIND.into(),
            schema_version: REPORT_SCHEMA_VERSION,
            generator: "togi/test".into(),
            source_revision: Some("a".repeat(40)),
            mutations: vec![ReplayMutationV1 {
                id: 1,
                line: 1,
                column: 1,
                language: "rust".into(),
                operator: "op".into(),
                description: "d".into(),
                result: "survived".into(),
                source_path: source_path.into(),
                byte_start: 0,
                byte_end: 1,
                source_fingerprint: format!("sha256:{}", "b".repeat(64)),
                original: "x".into(),
                replacement: "y".into(),
                replay: StoredReplayRecipe::RegularDirect {
                    test_command,
                    build_command: None,
                    build_command_origin: BuildCommandOrigin::None,
                    timeout_ms: 1,
                    env: BTreeMap::new(),
                    respect_workspace_ignores: true,
                    origin: DirectRecipeOrigin::Executed,
                },
                execution: StoredMutationExecution {
                    state: StoredExecutionState::Executed,
                    reason: None,
                },
            }],
        }
    }

    fn replay_source_read_count() -> usize {
        REPLAY_SOURCE_READS.with(|reads| reads.get())
    }

    fn reset_replay_source_read_count() {
        REPLAY_SOURCE_READS.with(|reads| reads.set(0));
    }

    #[test]
    fn static_rejections_do_not_reach_the_replay_source_reader() {
        reset_replay_source_read_count();
        assert!(
            validate_report_mutation(static_test_report(".git/config", vec!["true".into()]), 1)
                .is_err()
        );
        assert_eq!(replay_source_read_count(), 0);

        reset_replay_source_read_count();
        assert!(validate_report_mutation(static_test_report("src/lib.rs", vec![]), 1).is_err());
        assert_eq!(replay_source_read_count(), 0);
    }

    #[test]
    fn missing_source_revision_is_not_a_panic() {
        let mut report = static_test_report("src/lib.rs", vec!["true".into()]);
        report.source_revision = None;
        let error = match validate_report_mutation(report, 1) {
            Ok(_) => panic!("report without a source revision must fail"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("without a Git source revision"));
    }

    #[cfg(unix)]
    #[test]
    fn resolved_control_aliases_are_rejected_before_reading_source() -> anyhow::Result<()> {
        if std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_err()
        {
            return Ok(());
        }

        let repo = tempfile::tempdir()?;
        let root = repo.path();
        for args in [
            &["init"][..],
            &["config", "user.email", "test@example.com"],
            &["config", "user.name", "Togi Test"],
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()?;
            assert!(output.status.success());
        }
        std::fs::write(root.join("README"), b"fixture")?;
        for args in [&["add", "."][..], &["commit", "-m", "initial"]] {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()?;
            assert!(output.status.success());
        }

        let cached_source = root.join(".togi-cache/alias-target");
        std::fs::create_dir_all(cached_source.parent().unwrap())?;
        std::fs::write(&cached_source, b"x")?;
        std::os::unix::fs::symlink(&cached_source, root.join("alias"))?;
        let mut replay =
            validate_report_mutation(static_test_report("alias", vec!["true".into()]), 1)?;
        replay.source_revision = git_head(root)?;
        replay.source_fingerprint = source_fingerprint(b"x");

        reset_replay_source_read_count();
        let error = validate_project_and_source(root, &replay, false).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("resolved replay source path targets a Togi or Git control path"),
            "{error:#}"
        );
        assert_eq!(replay_source_read_count(), 0);
        std::fs::remove_file(root.join("alias"))?;
        std::fs::hard_link(&cached_source, root.join("alias"))?;
        reset_replay_source_read_count();
        let error = validate_project_and_source(root, &replay, false).unwrap_err();
        assert!(
            error.to_string().contains("multiple hard links"),
            "{error:#}"
        );
        assert_eq!(replay_source_read_count(), 0);
        Ok(())
    }
    #[test]
    fn direct_recipe_origin_requires_matching_execution() {
        assert!(DirectRecipeOrigin::Executed.matches_execution(MutationExecution::Executed));
        assert!(!DirectRecipeOrigin::Executed.matches_execution(MutationExecution::ExactCache));
    }

    #[test]
    fn command_validation_rejects_empty_commands() {
        assert!(validate_command(&[], "test").is_err());
        assert!(validate_command(&[String::new()], "test").is_err());
    }

    fn auto_go_compile_command(output: &str) -> Vec<String> {
        vec![
            "go".into(),
            "test".into(),
            "-c".into(),
            "-vet=off".into(),
            "-o".into(),
            output.into(),
            "./...".into(),
        ]
    }

    fn direct_recipe(
        build_command: Option<Vec<String>>,
        build_command_origin: BuildCommandOrigin,
    ) -> RegularDirectRecipe {
        RegularDirectRecipe {
            test_command: vec!["test".into()],
            build_command,
            build_command_origin,
            timeout_ms: 1_000,
            env: BTreeMap::new(),
            respect_workspace_ignores: true,
            origin: DirectRecipeOrigin::Executed,
        }
    }

    #[test]
    fn normalizes_auto_go_compile_output_for_either_host() {
        let unix_recipe = direct_recipe(
            Some(auto_go_compile_command("/dev/null")),
            BuildCommandOrigin::AutoDetected,
        );
        assert_eq!(
            replay_build_command(&unix_recipe, "NUL"),
            Some(auto_go_compile_command("NUL"))
        );

        let windows_recipe = direct_recipe(
            Some(auto_go_compile_command("NUL")),
            BuildCommandOrigin::AutoDetected,
        );
        assert_eq!(
            replay_build_command(&windows_recipe, "/dev/null"),
            Some(auto_go_compile_command("/dev/null"))
        );
    }

    #[test]
    fn normalizes_only_the_terminal_auto_go_compile_suffix_after_a_wrapper() {
        let wrapper = vec!["sandbox".into(), "--".into()];
        let mut wrapped_unix = wrapper.clone();
        wrapped_unix.extend(auto_go_compile_command("/dev/null"));
        let mut wrapped_windows = wrapper.clone();
        wrapped_windows.extend(auto_go_compile_command("NUL"));

        let mut expected_windows = wrapper.clone();
        expected_windows.extend(auto_go_compile_command("NUL"));
        assert_eq!(
            replay_build_command(
                &direct_recipe(Some(wrapped_unix), BuildCommandOrigin::AutoDetected),
                "NUL"
            ),
            Some(expected_windows)
        );

        let mut expected_unix = wrapper;
        expected_unix.extend(auto_go_compile_command("/dev/null"));
        assert_eq!(
            replay_build_command(
                &direct_recipe(Some(wrapped_windows), BuildCommandOrigin::AutoDetected),
                "/dev/null"
            ),
            Some(expected_unix)
        );
    }

    #[test]
    fn configured_auto_go_compile_lookalike_is_replayed_byte_for_byte() {
        let build_command = auto_go_compile_command("/dev/null");
        let recipe = direct_recipe(Some(build_command.clone()), BuildCommandOrigin::Configured);

        assert_eq!(
            replay_build_command(&recipe, "NUL"),
            Some(build_command.clone())
        );
        assert_eq!(recipe.build_command, Some(build_command));
    }

    #[test]
    fn auto_go_compile_normalization_leaves_other_commands_unchanged() {
        let mut configured = auto_go_compile_command("/dev/null");
        configured.push("-tags=ci".into());
        let original = configured.clone();

        normalize_auto_go_compile_output(&mut configured, "NUL");

        assert_eq!(configured, original);
    }
}
