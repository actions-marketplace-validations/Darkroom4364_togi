pub mod coverage;
pub mod github;
pub mod html;
pub mod json;
pub mod sarif;
pub mod terminal;

use crate::{BuildErrorDiagnostic, Mutation, MutationExecution, MutationReport, MutationResult};
use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;

/// Compute mutation score from tests executed during this invocation.
///
/// Build errors, cache/history reuse, coverage-suppressed (uncovered), and
/// learned-selection (subsumed) mutants do not contribute to the denominator.
pub fn mutation_score(report: &MutationReport) -> f64 {
    let executions = report.execution_counts();
    if executions.executed > 0 {
        (executions.executed_killed as f64 / executions.executed as f64) * 100.0
    } else if report.total == report.uncovered_count() + report.subsumed_count() {
        // Nothing was eligible to execute: either the report is empty or every
        // mutant sat on a zero-coverage line or was subsumed by a cluster sibling.
        // Vacuous pass, same as an empty report.
        100.0
    } else {
        0.0
    }
}

/// Compute the score used only to enforce `--fail-under`.
///
/// Unlike [`mutation_score`], exact-cache verdicts preserve their final
/// killed/survived/timeout evidence so an exact-warm gate agrees with its cold
/// run. Incremental history remains intentionally excluded.
pub fn fail_under_score(report: &MutationReport) -> f64 {
    let mut eligible = 0usize;
    let mut killed = 0usize;
    for (mutation, result) in &report.results {
        if !matches!(
            report.execution_for(mutation.id, *result),
            MutationExecution::Executed | MutationExecution::ExactCache
        ) {
            continue;
        }
        match result {
            MutationResult::Killed => {
                eligible += 1;
                killed += 1;
            }
            MutationResult::Survived | MutationResult::Timeout => eligible += 1,
            MutationResult::BuildError | MutationResult::Uncovered | MutationResult::Subsumed => {}
        }
    }

    if eligible > 0 {
        (killed as f64 / eligible as f64) * 100.0
    } else if report
        .results
        .iter()
        .all(|(_, result)| matches!(result, MutationResult::Uncovered | MutationResult::Subsumed))
    {
        100.0
    } else {
        0.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildErrorGroup {
    pub count: usize,
    pub language: String,
    pub operator: String,
    pub runner: String,
    pub phase: String,
    pub fingerprint: String,
    pub command: Vec<String>,
    pub message: String,
    pub files: Vec<BuildErrorFileCount>,
    pub examples: Vec<BuildErrorExample>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildErrorFileCount {
    pub file: String,
    pub count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildErrorExample {
    pub mutation_id: u32,
    pub file: String,
    pub line: usize,
}

struct BuildErrorGroupAccumulator {
    count: usize,
    command: Vec<String>,
    message: String,
    files: BTreeMap<String, usize>,
    examples: Vec<BuildErrorExample>,
}

/// Group build errors into actionable buckets for report output.
pub fn build_error_groups(report: &MutationReport) -> Vec<BuildErrorGroup> {
    let diagnostics: BTreeMap<u32, &BuildErrorDiagnostic> = report
        .build_error_diagnostics
        .iter()
        .map(|diagnostic| (diagnostic.mutation_id, diagnostic))
        .collect();
    let mut groups =
        BTreeMap::<(String, String, String, String, String), BuildErrorGroupAccumulator>::new();

    for (mutation, result) in &report.results {
        if *result != MutationResult::BuildError
            || report.execution_for(mutation.id, *result).is_reused()
        {
            continue;
        }

        let diagnostic = diagnostics.get(&mutation.id).copied();
        let language = label_or_unknown(&mutation.language);
        let operator = label_or_unknown(&mutation.operator);
        let runner = diagnostic
            .map(|diagnostic| diagnostic.runner.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let phase = diagnostic
            .map(|diagnostic| diagnostic.phase.clone())
            .unwrap_or_else(|| "unknown".to_string());
        let message = diagnostic
            .map(|diagnostic| diagnostic.message.clone())
            .unwrap_or_else(|| "build error diagnostic unavailable".to_string());
        let fingerprint = diagnostic
            .map(|diagnostic| diagnostic.fingerprint.clone())
            .unwrap_or_else(|| BuildErrorDiagnostic::fingerprint_for(&message));
        let command = diagnostic
            .map(|diagnostic| diagnostic.command.clone())
            .unwrap_or_default();
        let key = (
            language.clone(),
            operator.clone(),
            runner.clone(),
            phase.clone(),
            fingerprint.clone(),
        );
        let accumulator = groups
            .entry(key)
            .or_insert_with(|| BuildErrorGroupAccumulator {
                count: 0,
                command,
                message,
                files: BTreeMap::new(),
                examples: Vec::new(),
            });

        accumulator.count += 1;
        *accumulator
            .files
            .entry(mutation.file.display().to_string())
            .or_default() += 1;
        if accumulator.examples.len() < 3 {
            accumulator.examples.push(BuildErrorExample {
                mutation_id: mutation.id + 1,
                file: mutation.file.display().to_string(),
                line: mutation.line,
            });
        }
    }

    let mut groups: Vec<_> = groups
        .into_iter()
        .map(
            |((language, operator, runner, phase, fingerprint), accumulator)| BuildErrorGroup {
                count: accumulator.count,
                language,
                operator,
                runner,
                phase,
                fingerprint,
                command: accumulator.command,
                message: accumulator.message,
                files: accumulator
                    .files
                    .into_iter()
                    .map(|(file, count)| BuildErrorFileCount { file, count })
                    .collect(),
                examples: accumulator.examples,
            },
        )
        .collect();
    groups.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| left.language.cmp(&right.language))
            .then_with(|| left.operator.cmp(&right.operator))
            .then_with(|| left.runner.cmp(&right.runner))
            .then_with(|| left.phase.cmp(&right.phase))
            .then_with(|| left.fingerprint.cmp(&right.fingerprint))
    });
    groups
}

fn label_or_unknown(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed.to_string()
    }
}

/// serde `skip_serializing_if` helper: omit zero counts so reports without
/// coverage-suppressed or subsumed mutants keep their previous shape
/// byte-for-byte.
pub(crate) fn is_zero(value: &usize) -> bool {
    *value == 0
}

pub fn print_report(
    report: &crate::MutationReport,
    format: crate::cli::OutputFormat,
) -> anyhow::Result<()> {
    print_report_with_baseline(report, format, None)
}

/// Render a report with optional per-survivor baseline comparison data.
pub fn print_report_with_baseline(
    report: &crate::MutationReport,
    format: crate::cli::OutputFormat,
    comparison: Option<&crate::baseline::SurvivorBaselineComparison>,
) -> anyhow::Result<()> {
    print_report_with_baseline_and_replay(report, format, comparison, None)
}

/// Render a report, adding replay metadata only to JSON when source evidence
/// and regular direct recipes were captured.
pub fn print_report_with_baseline_and_replay(
    report: &crate::MutationReport,
    format: crate::cli::OutputFormat,
    comparison: Option<&crate::baseline::SurvivorBaselineComparison>,
    replay: Option<(
        &crate::replay::ReplayReportCapture,
        &std::collections::BTreeMap<u32, crate::replay::RegularDirectRecipe>,
    )>,
) -> anyhow::Result<()> {
    use crate::cli::OutputFormat;
    match format {
        OutputFormat::Json => {
            if let Some((capture, direct_recipes)) = replay {
                json::print_report_with_baseline_and_replay(
                    report,
                    comparison,
                    capture,
                    direct_recipes,
                )?;
            } else {
                json::print_report_with_baseline(report, comparison)?;
            }
        }
        OutputFormat::Github => github::print_report_with_baseline(report, comparison),
        OutputFormat::Html => {
            let path = std::path::Path::new("togi-report.html");
            html::write_report_with_baseline(report, path, comparison)?;
            eprintln!("HTML report written to {}", path.display());
        }
        OutputFormat::Sarif => sarif::print_report_with_baseline(report, comparison)?,
        OutputFormat::Terminal => terminal::print_report_with_baseline(report, comparison),
    }
    Ok(())
}

/// Render a baseline suite failure without fabricating a mutation report.
pub fn print_run_suite_failure(
    failure: &crate::runner::RunSuiteFailure,
    format: crate::cli::OutputFormat,
) -> anyhow::Result<()> {
    match format {
        crate::cli::OutputFormat::Json => json::print_run_suite_failure(failure)?,
        _ => terminal::print_run_suite_failure(failure),
    }
    Ok(())
}

pub fn print_coverage_gate_report(
    report: &crate::coverage::CoverageGateReport,
    format: crate::cli::OutputFormat,
) -> anyhow::Result<()> {
    use crate::cli::OutputFormat;
    match format {
        OutputFormat::Json => coverage::print_json(report)?,
        OutputFormat::Github => coverage::print_github(report),
        OutputFormat::Html => {
            let path = std::path::Path::new("togi-coverage-report.html");
            coverage::write_html(report, path)?;
            eprintln!("HTML coverage report written to {}", path.display());
        }
        OutputFormat::Sarif => sarif::print_coverage_gate_report(report)?,
        OutputFormat::Terminal => coverage::print_terminal(report),
    }
    Ok(())
}

/// Generate a markdown PR comment summarizing mutation results.
///
/// Includes a hidden marker comment so CI pipelines can find/replace
/// existing togi comments on subsequent runs.
pub fn format_pr_comment(report: &MutationReport, baseline_score: Option<f64>) -> String {
    format_pr_comment_with_baseline(report, baseline_score, None)
}

/// Generate a markdown PR comment with optional survivor baseline annotations.
pub fn format_pr_comment_with_baseline(
    report: &MutationReport,
    baseline_score: Option<f64>,
    comparison: Option<&crate::baseline::SurvivorBaselineComparison>,
) -> String {
    use crate::{MutationExecution, MutationResult};
    use std::fmt::Write;

    let execution_counts = report.execution_counts();
    let score = mutation_score(report);
    let tested = execution_counts.executed;
    let uncovered = report.uncovered_count();
    let subsumed = report.subsumed_count();
    let emoji = if report.survived == 0 && report.timeout == 0 && report.build_errors == 0 {
        "✓"
    } else if score >= 80.0 {
        "⚠"
    } else {
        "✗"
    };

    let mut md = String::new();
    writeln!(md, "<!-- togi-mutation-report -->").unwrap();
    writeln!(md, "## {emoji} togi mutation report").unwrap();
    writeln!(md).unwrap();
    let delta_str = if let Some(base) = baseline_score {
        let delta = score - base;
        let sign = if delta >= 0.0 { "+" } else { "" };
        format!(" ({sign}{delta:.1}% vs baseline)")
    } else {
        String::new()
    };
    let uncovered_str = if uncovered > 0 {
        format!(", {uncovered} uncovered")
    } else {
        String::new()
    };
    let subsumed_str = if subsumed > 0 {
        format!(", {subsumed} subsumed")
    } else {
        String::new()
    };
    let reused_str = if execution_counts.reused() > 0 {
        format!(
            ", {} exact-cache reused, {} incremental-history reused",
            execution_counts.exact_cache_reused, execution_counts.incremental_history_reused
        )
    } else {
        String::new()
    };
    writeln!(
        md,
        "**{score:.1}%** mutation score{delta_str} — {}/{} freshly executed killed, {} survived, {} timeout, {} build errors{uncovered_str}{subsumed_str}{reused_str} — {:.2}s",
        execution_counts.executed_killed,
        tested,
        report.survived,
        report.timeout,
        report.build_errors,
        report.duration.as_secs_f64()
    )
    .unwrap();
    if report.total < report.planned_total {
        writeln!(
            md,
            "\nPartial report: stopped after {}/{} scheduled mutations.",
            report.total, report.planned_total
        )
        .expect("writing to String should not fail");
    }
    if let Some(reason) = &report.early_stop_reason {
        writeln!(md, "\nEarly stop: {reason}").expect("writing to String should not fail");
    }
    writeln!(md).unwrap();

    let equivalent_advisories = crate::equivalent::advisories_for(report);
    let survived: Vec<_> = report
        .results
        .iter()
        .filter(|(_, r)| *r == MutationResult::Survived)
        .collect();

    if !survived.is_empty() {
        writeln!(md, "<details>").unwrap();
        writeln!(
            md,
            "<summary>{} survived mutation{}</summary>",
            survived.len(),
            if survived.len() == 1 { "" } else { "s" }
        )
        .unwrap();
        writeln!(md).unwrap();
        if comparison.is_some() {
            let _ = writeln!(md, "| File | Line | Operator | Description | Baseline |");
            let _ = writeln!(md, "|------|------|----------|-------------|----------|");
        } else {
            let _ = writeln!(md, "| File | Line | Operator | Description |");
            let _ = writeln!(md, "|------|------|----------|-------------|");
        }
        for (mutation, result) in survived {
            let provenance = match report.execution_for(mutation.id, *result) {
                MutationExecution::Executed => String::new(),
                MutationExecution::ExactCache => " (reused: exact cache)".to_string(),
                MutationExecution::IncrementalHistory => {
                    " (reused: incremental history)".to_string()
                }
                MutationExecution::NotExecuted(reason) => format!(" (not executed: {reason})"),
            };
            let selection = report
                .selection_for(mutation.id)
                .map(|selection| format!(" (selection: {selection})"))
                .unwrap_or_default();
            let provenance = format!("{provenance}{selection}");
            let advisory = equivalent_advisories
                .get(&mutation.id)
                .map(|reason| format!(" Likely equivalent (advisory): {}", reason.message()))
                .unwrap_or_default();
            if let Some(comparison) = comparison {
                let baseline_status = comparison
                    .status_for(mutation.id)
                    .map(|status| escape_md_cell(status.as_str()))
                    .unwrap_or_default();
                let _ = writeln!(
                    md,
                    "| `{}` | {} | `{}` | {}{}{} | {} |",
                    escape_md_cell(&mutation.file.display().to_string()),
                    mutation.line,
                    escape_md_cell(&mutation.operator),
                    escape_md_cell(&mutation.description),
                    provenance,
                    advisory,
                    baseline_status,
                );
            } else {
                let _ = writeln!(
                    md,
                    "| `{}` | {} | `{}` | {}{}{} |",
                    escape_md_cell(&mutation.file.display().to_string()),
                    mutation.line,
                    escape_md_cell(&mutation.operator),
                    escape_md_cell(&mutation.description),
                    provenance,
                    advisory,
                );
            }
        }
        writeln!(md).unwrap();
        writeln!(md, "</details>").unwrap();
    }

    md
}

/// Escape characters that break markdown table cells.
fn escape_md_cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ").replace('\r', "")
}

/// Write a PR comment markdown file.
pub fn write_pr_comment(
    report: &MutationReport,
    path: &std::path::Path,
    baseline_score: Option<f64>,
) -> anyhow::Result<()> {
    write_pr_comment_with_baseline(report, path, baseline_score, None)
}

/// Write a PR comment with optional survivor baseline annotations.
pub fn write_pr_comment_with_baseline(
    report: &MutationReport,
    path: &std::path::Path,
    baseline_score: Option<f64>,
    comparison: Option<&crate::baseline::SurvivorBaselineComparison>,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let md = format_pr_comment_with_baseline(report, baseline_score, comparison);
    fs::write(path, md)?;
    Ok(())
}

/// Generate a unified diff snippet for a survived mutation.
///
/// Reads the source file, extracts context around the mutated line,
/// and returns a unified diff string showing original vs mutated code.
pub fn mutation_diff(mutation: &Mutation) -> Option<String> {
    let content = fs::read_to_string(&mutation.file).ok()?;
    let lines: Vec<&str> = content.lines().collect();
    let line_idx = mutation.line.checked_sub(1)?;
    if line_idx >= lines.len() {
        return None;
    }

    let original_line = lines[line_idx];
    let col_idx = mutation.column.saturating_sub(1);
    let byte_start = col_idx.min(original_line.len());
    let byte_end = byte_start + mutation.original.len();
    if byte_end > original_line.len() {
        return None;
    }
    if !original_line.is_char_boundary(byte_start) || !original_line.is_char_boundary(byte_end) {
        return None;
    }
    if &original_line[byte_start..byte_end] != mutation.original.as_str() {
        return None;
    }
    let mutated_line = format!(
        "{}{}{}",
        &original_line[..byte_start],
        mutation.replacement,
        &original_line[byte_end..]
    );

    let ctx = 1usize;
    let start = line_idx.saturating_sub(ctx);
    let end = (line_idx + ctx + 1).min(lines.len());

    let file_display = mutation.file.display().to_string();
    let hunk_start = start + 1;
    let hunk_len = end - start;
    let mut diff = String::new();
    writeln!(diff, "--- a/{file_display}").ok()?;
    writeln!(diff, "+++ b/{file_display}").ok()?;
    writeln!(
        diff,
        "@@ -{hunk_start},{hunk_len} +{hunk_start},{hunk_len} @@"
    )
    .ok()?;
    for (i, line) in lines.iter().enumerate().take(end).skip(start) {
        if i == line_idx {
            writeln!(diff, "-{original_line}").ok()?;
            writeln!(diff, "+{mutated_line}").ok()?;
        } else {
            writeln!(diff, " {line}").ok()?;
        }
    }
    Some(diff)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use tempfile::TempDir;

    fn make_mutation(
        dir: &std::path::Path,
        filename: &str,
        content: &str,
        line: usize,
        column: usize,
        original: &str,
        replacement: &str,
    ) -> Mutation {
        let path = dir.join(filename);
        std::fs::write(&path, content).unwrap();
        Mutation {
            id: 0,
            file: path,
            language: "go".into(),
            line,
            column,
            operator: "test_op".into(),
            description: "test".into(),
            original: original.into(),
            replacement: replacement.into(),
            byte_range: 0..0,
        }
    }

    fn report_mutation(id: u32, file: &str, result: MutationResult) -> (Mutation, MutationResult) {
        (
            Mutation {
                id,
                file: std::path::PathBuf::from(file),
                language: "rust".into(),
                line: usize::try_from(id + 1).unwrap(),
                column: 1,
                operator: "eq_to_neq".into(),
                description: "Replace == with !=".into(),
                original: "==".into(),
                replacement: "!=".into(),
                byte_range: 0..2,
            },
            result,
        )
    }

    #[test]
    fn likely_equivalent_advisory_is_rendered_without_changing_survivor_semantics() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("fixture.rs");
        std::fs::write(&path, crate::test_helpers::BOOLEAN_LITERAL_SOURCE).unwrap();
        let mut mutation = crate::test_helpers::boolean_literal_mutation(path);
        mutation.id = 1;
        let mut report = crate::test_helpers::sample_report();
        report.results[1] = (mutation, MutationResult::Survived);

        let reason = "both operands are the same boolean literal, so either logical operator produces the same value";
        let terminal = crate::report::terminal::format_report_plain(&report);
        let json = crate::report::json::to_json_string(&report).unwrap();
        let html = crate::report::html::generate_report(&report).unwrap();
        let sarif = crate::report::sarif::to_sarif_string(&report).unwrap();
        let comment = format_pr_comment(&report, None);

        assert!(terminal.contains(&format!("Likely equivalent (advisory): {reason}")));
        assert!(terminal.contains("SURVIVED"));
        assert!(json.contains(&format!("\"likely_equivalent\": \"{reason}\"")));
        assert!(html.contains(&format!("Likely equivalent (advisory): {reason}")));
        assert!(sarif.contains(&format!("\"likely_equivalent\": \"{reason}\"")));
        assert!(comment.contains(&format!("Likely equivalent (advisory): {reason}")));
        assert_eq!(report.survived, 1);
    }

    #[test]
    fn build_error_groups_deduplicate_by_diagnostic_fingerprint() {
        let report = MutationReport {
            selection_provenance: std::collections::BTreeMap::new(),
            results: vec![
                report_mutation(0, "src/a.rs", MutationResult::BuildError),
                report_mutation(1, "src/b.rs", MutationResult::BuildError),
                report_mutation(2, "src/c.rs", MutationResult::Killed),
            ],
            execution_provenance: BTreeMap::new(),
            build_error_diagnostics: vec![
                BuildErrorDiagnostic::new(
                    0,
                    "regular",
                    "build_command",
                    vec!["cargo".into(), "check".into()],
                    "error[E0308]: mismatched types at line 10",
                ),
                BuildErrorDiagnostic::new(
                    1,
                    "regular",
                    "build_command",
                    vec!["cargo".into(), "check".into()],
                    "error[E0308]: mismatched types at line 20",
                ),
            ],
            schemata: None,
            baseline_timing: None,
            duration: std::time::Duration::from_millis(10),
            test_command: None,
            build_command: vec![],
            planned_total: 3,
            early_stop_reason: None,
            total: 3,
            killed: 1,
            survived: 0,
            timeout: 0,
            build_errors: 2,
        };

        let groups = build_error_groups(&report);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[0].language, "rust");
        assert_eq!(groups[0].operator, "eq_to_neq");
        assert_eq!(groups[0].runner, "regular");
        assert_eq!(groups[0].phase, "build_command");
        assert_eq!(groups[0].files.len(), 2);
        assert_eq!(groups[0].examples.len(), 2);
    }

    #[test]
    fn build_error_groups_exclude_reused_build_error_verdicts() {
        let mut report = crate::test_helpers::sample_report();
        report.results[0].1 = MutationResult::BuildError;
        report.killed = 0;
        report.build_errors = 1;
        report
            .execution_provenance
            .insert(0, crate::MutationExecution::ExactCache);

        assert!(build_error_groups(&report).is_empty());
    }

    #[test]
    fn mutation_diff_basic() {
        let tmp = TempDir::new().unwrap();
        let content = "package main\n\nfunc f() bool {\n\treturn true\n}\n";
        let m = make_mutation(tmp.path(), "main.go", content, 4, 9, "true", "false");
        let diff = mutation_diff(&m).unwrap();
        assert!(
            diff.contains("-\treturn true"),
            "diff should show original line: {diff}"
        );
        assert!(
            diff.contains("+\treturn false"),
            "diff should show mutated line: {diff}"
        );
    }

    #[test]
    fn mutation_diff_empty_replacement() {
        let tmp = TempDir::new().unwrap();
        let content = "x = 1 + 2\ny = 3\n";
        let m = make_mutation(tmp.path(), "test.py", content, 1, 7, "+ 2", "");
        let diff = mutation_diff(&m).unwrap();
        assert!(
            diff.contains("-x = 1 + 2"),
            "diff should show original: {diff}"
        );
        assert!(diff.contains("+x = 1"), "diff should show removal: {diff}");
    }

    #[test]
    fn mutation_diff_replacement_at_start() {
        let tmp = TempDir::new().unwrap();
        let content = "true\n";
        let m = make_mutation(tmp.path(), "t.go", content, 1, 1, "true", "false");
        let diff = mutation_diff(&m).unwrap();
        assert!(diff.contains("-true"), "{diff}");
        assert!(diff.contains("+false"), "{diff}");
    }

    #[test]
    fn mutation_diff_replacement_at_end() {
        let tmp = TempDir::new().unwrap();
        let content = "a = true\n";
        let m = make_mutation(tmp.path(), "t.py", content, 1, 5, "true", "false");
        let diff = mutation_diff(&m).unwrap();
        assert!(diff.contains("-a = true"), "{diff}");
        assert!(diff.contains("+a = false"), "{diff}");
    }

    #[test]
    fn mutation_diff_invalid_line_returns_none() {
        let tmp = TempDir::new().unwrap();
        let content = "one line\n";
        let m = make_mutation(tmp.path(), "t.go", content, 99, 1, "x", "y");
        assert!(mutation_diff(&m).is_none());
    }

    #[test]
    fn mutation_diff_line_zero_returns_none() {
        let tmp = TempDir::new().unwrap();
        let content = "hello\n";
        let m = make_mutation(tmp.path(), "t.go", content, 0, 1, "h", "H");
        assert!(mutation_diff(&m).is_none());
    }

    #[test]
    fn mutation_diff_multibyte_chars() {
        let tmp = TempDir::new().unwrap();
        // "日本語 = true\n" — CJK chars are 3 bytes each
        let content = "日本語 = true\n";
        // "true" starts at byte offset 12 (9 bytes for CJK + 3 for " = ")
        // column is 1-indexed byte offset: 13
        let m = make_mutation(tmp.path(), "t.rs", content, 1, 13, "true", "false");
        let diff = mutation_diff(&m).unwrap();
        assert!(diff.contains("-日本語 = true"), "{diff}");
        assert!(diff.contains("+日本語 = false"), "{diff}");
    }

    #[test]
    fn mutation_diff_invalid_char_boundary_returns_none() {
        let tmp = TempDir::new().unwrap();
        let content = "日本語\n";
        // column 2 (1-indexed) → byte_start 1, which is mid-character
        let m = make_mutation(tmp.path(), "t.rs", content, 1, 2, "x", "y");
        assert!(mutation_diff(&m).is_none());
    }

    #[test]
    fn mutation_diff_original_mismatch_returns_none() {
        let tmp = TempDir::new().unwrap();
        let content = "a = true\n";
        let m = make_mutation(tmp.path(), "t.rs", content, 1, 5, "false", "true");
        assert!(mutation_diff(&m).is_none());
    }

    #[test]
    fn pr_comment_contains_marker_and_score() {
        let report = crate::test_helpers::sample_report();
        let md = format_pr_comment(&report, None);
        assert!(md.contains("<!-- togi-mutation-report -->"));
        assert!(md.contains("50.0%"));
        assert!(md.contains("togi mutation report"));
    }

    #[test]
    fn pr_comment_lists_survived_mutations() {
        let report = crate::test_helpers::sample_report();
        let md = format_pr_comment(&report, None);
        assert!(md.contains("survived mutation"));
        assert!(md.contains("src/handler.rs"));
    }

    #[test]
    fn pr_comment_adds_baseline_column_for_active_survivor_comparison_only() {
        let report = crate::test_helpers::sample_report();
        let comparison =
            crate::baseline::SurvivorBaselineComparison::from_statuses(BTreeMap::from([
                (0, crate::baseline::SurvivorBaselineStatus::New),
                (1, crate::baseline::SurvivorBaselineStatus::Historic),
            ]));

        let inactive = format_pr_comment(&report, None);
        assert_eq!(
            inactive,
            format_pr_comment_with_baseline(&report, None, None)
        );
        assert!(!inactive.contains("| Baseline |"));

        let active = format_pr_comment_with_baseline(&report, None, Some(&comparison));
        assert!(active.contains("| File | Line | Operator | Description | Baseline |"));
        assert!(active.contains(" | historic |"));
        assert!(!active.contains(" | new |"));
    }

    #[test]
    fn pr_comment_labels_mixed_survivor_provenance() {
        let mut report = uncovered_report(vec![
            report_mutation(0, "src/fresh.rs", MutationResult::Survived),
            report_mutation(1, "src/exact.rs", MutationResult::Survived),
            report_mutation(2, "src/history.rs", MutationResult::Survived),
        ]);
        report.survived = 3;
        report
            .execution_provenance
            .insert(1, crate::MutationExecution::ExactCache);
        report
            .execution_provenance
            .insert(2, crate::MutationExecution::IncrementalHistory);

        let md = format_pr_comment(&report, None);

        assert!(md.contains("| `src/fresh.rs` | 1 | `eq_to_neq` | Replace == with != |"));
        assert!(md.contains("Replace == with != (reused: exact cache)"));
        assert!(md.contains("Replace == with != (reused: incremental history)"));
    }

    #[test]
    fn pr_comment_no_details_when_all_killed() {
        use crate::{MutationReport, MutationResult};
        use std::time::Duration;
        let report = MutationReport {
            selection_provenance: std::collections::BTreeMap::new(),
            results: vec![(
                Mutation {
                    id: 0,
                    file: std::path::PathBuf::from("test.rs"),
                    language: "rust".into(),
                    line: 1,
                    column: 1,
                    operator: "op".into(),
                    description: "d".into(),
                    original: "x".into(),
                    replacement: "y".into(),
                    byte_range: 0..1,
                },
                MutationResult::Killed,
            )],
            execution_provenance: BTreeMap::new(),
            build_error_diagnostics: vec![],
            schemata: None,
            baseline_timing: None,
            duration: Duration::from_secs(1),
            test_command: None,
            build_command: vec![],
            planned_total: 1,
            early_stop_reason: None,
            total: 1,
            killed: 1,
            survived: 0,
            timeout: 0,
            build_errors: 0,
        };
        let md = format_pr_comment(&report, None);
        assert!(md.contains("✓"));
        assert!(!md.contains("<details>"));
    }

    #[test]
    fn pr_comment_no_checkmark_when_timeouts() {
        use crate::{MutationReport, MutationResult};
        use std::time::Duration;
        let report = MutationReport {
            selection_provenance: std::collections::BTreeMap::new(),
            results: vec![(
                Mutation {
                    id: 0,
                    file: std::path::PathBuf::from("test.rs"),
                    language: "rust".into(),
                    line: 1,
                    column: 1,
                    operator: "op".into(),
                    description: "d".into(),
                    original: "x".into(),
                    replacement: "y".into(),
                    byte_range: 0..1,
                },
                MutationResult::Timeout,
            )],
            execution_provenance: BTreeMap::new(),
            build_error_diagnostics: vec![],
            schemata: None,
            baseline_timing: None,
            duration: Duration::from_secs(1),
            test_command: None,
            build_command: vec![],
            planned_total: 1,
            early_stop_reason: None,
            total: 1,
            killed: 0,
            survived: 0,
            timeout: 1,
            build_errors: 0,
        };
        let md = format_pr_comment(&report, None);
        assert!(!md.contains("✓"), "should not show checkmark with timeouts");
        assert!(md.contains("1 timeout"));
        assert!(!md.contains("<details>"));
    }

    #[test]
    fn pr_comment_no_checkmark_when_all_build_errors() {
        use crate::{MutationReport, MutationResult};
        use std::time::Duration;
        let report = MutationReport {
            selection_provenance: std::collections::BTreeMap::new(),
            results: vec![(
                Mutation {
                    id: 0,
                    file: std::path::PathBuf::from("test.rs"),
                    language: "rust".into(),
                    line: 1,
                    column: 1,
                    operator: "op".into(),
                    description: "d".into(),
                    original: "x".into(),
                    replacement: "y".into(),
                    byte_range: 0..1,
                },
                MutationResult::BuildError,
            )],
            execution_provenance: BTreeMap::new(),
            build_error_diagnostics: vec![],
            schemata: None,
            baseline_timing: None,
            duration: Duration::from_secs(1),
            test_command: None,
            build_command: vec![],
            planned_total: 1,
            early_stop_reason: None,
            total: 1,
            killed: 0,
            survived: 0,
            timeout: 0,
            build_errors: 1,
        };
        let md = format_pr_comment(&report, None);
        assert!(
            !md.contains("✓"),
            "should not show checkmark when every mutation is a build error"
        );
        assert!(md.contains("## ✗ togi mutation report"));
        assert!(md.contains("1 build errors"));
    }

    #[test]
    fn pr_comment_no_checkmark_when_mixed_build_errors() {
        use crate::{MutationReport, MutationResult};
        use std::time::Duration;
        let report = MutationReport {
            selection_provenance: std::collections::BTreeMap::new(),
            results: vec![
                (
                    Mutation {
                        id: 0,
                        file: std::path::PathBuf::from("test.rs"),
                        language: "rust".into(),
                        line: 1,
                        column: 1,
                        operator: "op".into(),
                        description: "d".into(),
                        original: "x".into(),
                        replacement: "y".into(),
                        byte_range: 0..1,
                    },
                    MutationResult::Killed,
                ),
                (
                    Mutation {
                        id: 1,
                        file: std::path::PathBuf::from("test.rs"),
                        language: "rust".into(),
                        line: 2,
                        column: 1,
                        operator: "op".into(),
                        description: "d".into(),
                        original: "x".into(),
                        replacement: "y".into(),
                        byte_range: 0..1,
                    },
                    MutationResult::BuildError,
                ),
            ],
            execution_provenance: BTreeMap::new(),
            build_error_diagnostics: vec![],
            schemata: None,
            baseline_timing: None,
            duration: Duration::from_secs(1),
            test_command: None,
            build_command: vec![],
            planned_total: 2,
            early_stop_reason: None,
            total: 2,
            killed: 1,
            survived: 0,
            timeout: 0,
            build_errors: 1,
        };
        let md = format_pr_comment(&report, None);
        assert!(
            !md.contains("✓"),
            "should not show checkmark when build errors remain"
        );
        assert!(md.contains("## ⚠ togi mutation report"));
        assert!(md.contains("100.0%"));
        assert!(md.contains("1 build errors"));
    }

    #[test]
    fn pr_comment_includes_baseline_delta() {
        let report = crate::test_helpers::sample_report();
        let md = format_pr_comment(&report, Some(40.0));
        assert!(md.contains("vs baseline"), "should include baseline delta");
        assert!(md.contains("+10.0%"), "50% - 40% = +10%");
    }

    fn uncovered_report(results: Vec<(Mutation, MutationResult)>) -> MutationReport {
        let total = results.len();
        let killed = results
            .iter()
            .filter(|(_, r)| *r == MutationResult::Killed)
            .count();
        MutationReport {
            selection_provenance: std::collections::BTreeMap::new(),
            results,
            execution_provenance: BTreeMap::new(),
            build_error_diagnostics: vec![],
            schemata: None,
            baseline_timing: None,
            duration: std::time::Duration::from_secs(1),
            test_command: None,
            build_command: vec![],
            planned_total: total,
            early_stop_reason: None,
            total,
            killed,
            survived: 0,
            timeout: 0,
            build_errors: 0,
        }
    }

    #[test]
    fn mutation_score_excludes_uncovered_mutants() {
        // 1 of 1 executed mutants killed + 2 uncovered → 100%, not 33%.
        let report = uncovered_report(vec![
            report_mutation(0, "src/a.rs", MutationResult::Killed),
            report_mutation(1, "src/b.rs", MutationResult::Uncovered),
            report_mutation(2, "src/c.rs", MutationResult::Uncovered),
        ]);
        assert_eq!(report.uncovered_count(), 2);
        assert_eq!(report.tested_count(), 1);
        assert_eq!(mutation_score(&report), 100.0);
    }

    #[test]
    fn mutation_score_excludes_exact_cache_verdicts() {
        let mut report = crate::test_helpers::sample_report();
        report
            .execution_provenance
            .insert(0, crate::MutationExecution::ExactCache);

        let counts = report.execution_counts();
        assert_eq!(counts.executed, 1);
        assert_eq!(counts.executed_killed, 0);
        assert_eq!(counts.exact_cache_reused, 1);
        assert_eq!(report.tested_count(), 1);
        assert_eq!(mutation_score(&report), 0.0);
    }

    #[test]
    fn fail_under_score_keeps_exact_cache_evidence_out_of_public_score() {
        struct Case {
            name: &'static str,
            result: MutationResult,
            execution: Option<MutationExecution>,
            public_score: f64,
            gate_score: f64,
        }

        let cases = [
            Case {
                name: "fresh killed",
                result: MutationResult::Killed,
                execution: None,
                public_score: 100.0,
                gate_score: 100.0,
            },
            Case {
                name: "exact cached killed",
                result: MutationResult::Killed,
                execution: Some(MutationExecution::ExactCache),
                public_score: 0.0,
                gate_score: 100.0,
            },
            Case {
                name: "exact cached survived",
                result: MutationResult::Survived,
                execution: Some(MutationExecution::ExactCache),
                public_score: 0.0,
                gate_score: 0.0,
            },
            Case {
                name: "exact cached timeout",
                result: MutationResult::Timeout,
                execution: Some(MutationExecution::ExactCache),
                public_score: 0.0,
                gate_score: 0.0,
            },
            Case {
                name: "exact cached build error",
                result: MutationResult::BuildError,
                execution: Some(MutationExecution::ExactCache),
                public_score: 0.0,
                gate_score: 0.0,
            },
            Case {
                name: "incremental history killed",
                result: MutationResult::Killed,
                execution: Some(MutationExecution::IncrementalHistory),
                public_score: 0.0,
                gate_score: 0.0,
            },
            Case {
                name: "uncovered",
                result: MutationResult::Uncovered,
                execution: None,
                public_score: 100.0,
                gate_score: 100.0,
            },
            Case {
                name: "subsumed",
                result: MutationResult::Subsumed,
                execution: None,
                public_score: 100.0,
                gate_score: 100.0,
            },
        ];

        for case in cases {
            let mut report = uncovered_report(vec![report_mutation(0, "src/a.rs", case.result)]);
            if let Some(execution) = case.execution {
                report.execution_provenance.insert(0, execution);
            }
            assert_eq!(
                mutation_score(&report),
                case.public_score,
                "{} public score",
                case.name
            );
            assert_eq!(
                fail_under_score(&report),
                case.gate_score,
                "{} gate score",
                case.name
            );
        }
    }

    #[test]
    fn mutation_score_is_100_when_all_mutants_uncovered() {
        let report = uncovered_report(vec![
            report_mutation(0, "src/a.rs", MutationResult::Uncovered),
            report_mutation(1, "src/b.rs", MutationResult::Uncovered),
        ]);
        assert_eq!(report.tested_count(), 0);
        assert_eq!(mutation_score(&report), 100.0);
    }

    #[test]
    fn pr_comment_includes_uncovered_count_when_present() {
        let report = uncovered_report(vec![
            report_mutation(0, "src/a.rs", MutationResult::Killed),
            report_mutation(1, "src/b.rs", MutationResult::Uncovered),
        ]);
        let md = format_pr_comment(&report, None);
        assert!(md.contains("1 uncovered"), "got: {md}");
        // Uncovered mutants are not failures and do not block the checkmark.
        assert!(md.contains("✓"), "got: {md}");
    }

    #[test]
    fn pr_comment_omits_uncovered_count_when_zero() {
        let report = crate::test_helpers::sample_report();
        let md = format_pr_comment(&report, None);
        assert!(!md.contains("uncovered"), "got: {md}");
    }

    #[test]
    fn mutation_score_excludes_subsumed_mutants() {
        // 1 of 1 executed mutants killed + 2 subsumed → 100%, not 33%.
        let report = uncovered_report(vec![
            report_mutation(0, "src/a.rs", MutationResult::Killed),
            report_mutation(1, "src/b.rs", MutationResult::Subsumed),
            report_mutation(2, "src/b.rs", MutationResult::Subsumed),
        ]);
        assert_eq!(report.subsumed_count(), 2);
        assert_eq!(report.tested_count(), 1);
        assert_eq!(mutation_score(&report), 100.0);
    }

    #[test]
    fn pr_comment_includes_subsumed_count_when_present() {
        let report = uncovered_report(vec![
            report_mutation(0, "src/a.rs", MutationResult::Killed),
            report_mutation(1, "src/b.rs", MutationResult::Subsumed),
        ]);
        let md = format_pr_comment(&report, None);
        assert!(md.contains("1 subsumed"), "got: {md}");
        // Subsumed mutants are not failures and do not block the checkmark.
        assert!(md.contains("✓"), "got: {md}");
    }

    #[test]
    fn pr_comment_omits_subsumed_count_when_zero() {
        let report = crate::test_helpers::sample_report();
        let md = format_pr_comment(&report, None);
        assert!(!md.contains("subsumed"), "got: {md}");
    }
}
