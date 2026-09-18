use crate::{
    Mutation, MutationExecution, MutationReport, MutationResult, SurvivorConfirmation,
    TestSelectionProvenance,
};
use anyhow::Result;
use serde::Serialize;
use std::collections::BTreeMap;

const SARIF_SCHEMA: &str = "https://json.schemastore.org/sarif-2.1.0.json";
const SARIF_VERSION: &str = "2.1.0";
const INFORMATION_URI: &str = "https://github.com/Darkroom4364/togi";

#[derive(Serialize)]
struct SarifReport {
    #[serde(rename = "$schema")]
    schema: &'static str,
    version: &'static str,
    runs: Vec<SarifRun>,
}

#[derive(Serialize)]
struct SarifRun {
    tool: SarifTool,
    invocations: Vec<SarifInvocation>,
    results: Vec<SarifResult>,
}

#[derive(Serialize)]
struct SarifTool {
    driver: SarifDriver,
}

#[derive(Serialize)]
struct SarifDriver {
    name: &'static str,
    version: &'static str,
    #[serde(rename = "informationUri")]
    information_uri: &'static str,
    rules: Vec<SarifRule>,
}

#[derive(Serialize)]
struct SarifRule {
    id: String,
    #[serde(rename = "shortDescription")]
    short_description: SarifMessage,
}

#[derive(Serialize)]
struct SarifMessage {
    text: String,
}

#[derive(Serialize)]
struct SarifInvocation {
    #[serde(rename = "executionSuccessful")]
    execution_successful: bool,
    properties: SarifInvocationProperties,
}

#[derive(Serialize)]
struct SarifInvocationProperties {
    killed: usize,
    survived: usize,
    timeout: usize,
    build_errors: usize,
    tested: usize,
    executed_killed: usize,
    #[serde(skip_serializing_if = "super::is_zero")]
    exact_cache_reused: usize,
    #[serde(skip_serializing_if = "super::is_zero")]
    incremental_history_reused: usize,
    #[serde(skip_serializing_if = "super::is_zero")]
    uncovered: usize,
    #[serde(skip_serializing_if = "super::is_zero")]
    subsumed: usize,
    mutation_score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    selection_confirmation: Option<SarifSelectionConfirmationSummary>,
}

#[derive(Serialize)]
struct SarifSelectionConfirmationSummary {
    full: usize,
    narrowed: usize,
    confirmation_not_requested: usize,
    confirmation_not_needed: usize,
    confirmation_confirmed_survived: usize,
    confirmation_killed: usize,
    confirmation_timed_out: usize,
    confirmation_build_error: usize,
}

impl SarifSelectionConfirmationSummary {
    fn from_report(report: &MutationReport) -> Option<Self> {
        if report.selection_provenance.is_empty() {
            return None;
        }

        let mut summary = Self {
            full: 0,
            narrowed: 0,
            confirmation_not_requested: 0,
            confirmation_not_needed: 0,
            confirmation_confirmed_survived: 0,
            confirmation_killed: 0,
            confirmation_timed_out: 0,
            confirmation_build_error: 0,
        };
        for selection in report.selection_provenance.values() {
            match selection {
                TestSelectionProvenance::Full => summary.full += 1,
                TestSelectionProvenance::Narrowed { confirmation } => {
                    summary.narrowed += 1;
                    match confirmation {
                        SurvivorConfirmation::NotRequested => {
                            summary.confirmation_not_requested += 1
                        }
                        SurvivorConfirmation::NotNeeded => summary.confirmation_not_needed += 1,
                        SurvivorConfirmation::ConfirmedSurvived => {
                            summary.confirmation_confirmed_survived += 1
                        }
                        SurvivorConfirmation::Killed => summary.confirmation_killed += 1,
                        SurvivorConfirmation::TimedOut => summary.confirmation_timed_out += 1,
                        SurvivorConfirmation::BuildError => summary.confirmation_build_error += 1,
                    }
                }
            }
        }
        Some(summary)
    }
}

#[derive(Serialize)]
struct SarifResult {
    #[serde(rename = "ruleId")]
    rule_id: String,
    level: &'static str,
    message: SarifMessage,
    locations: Vec<SarifLocation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    properties: Option<SarifResultProperties>,
}

#[derive(Serialize)]
struct SarifResultProperties {
    execution: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    nonexecution_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline_status: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    test_selection_mode: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    selection_confirmation: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    likely_equivalent: Option<&'static str>,
}

#[derive(Serialize)]
struct SarifLocation {
    #[serde(rename = "physicalLocation")]
    physical_location: SarifPhysicalLocation,
}

#[derive(Serialize)]
struct SarifPhysicalLocation {
    #[serde(rename = "artifactLocation")]
    artifact_location: SarifArtifactLocation,
    region: SarifRegion,
}

#[derive(Serialize)]
struct SarifArtifactLocation {
    uri: String,
}

#[derive(Serialize)]
struct SarifRegion {
    #[serde(rename = "startLine")]
    start_line: usize,
    #[serde(rename = "startColumn")]
    start_column: usize,
    #[serde(rename = "byteOffset")]
    byte_offset: usize,
    #[serde(rename = "byteLength")]
    byte_length: usize,
}

/// SARIF artifact URIs always use forward slashes, even on Windows.
fn artifact_uri(mutation: &Mutation) -> String {
    mutation.file.display().to_string().replace('\\', "/")
}

fn result_for_with_baseline(
    mutation: &Mutation,
    execution: MutationExecution,
    status: Option<crate::baseline::SurvivorBaselineStatus>,
    selection: Option<TestSelectionProvenance>,
    advisory: Option<&'static str>,
) -> SarifResult {
    let advisory_suffix = advisory
        .map(|reason| format!(" Likely equivalent (advisory): {reason}."))
        .unwrap_or_default();
    SarifResult {
        rule_id: mutation.operator.clone(),
        level: "warning",
        message: SarifMessage {
            text: format!(
                "Survived mutation: {} ({} → {}){advisory_suffix}",
                mutation.description, mutation.original, mutation.replacement
            ),
        },
        locations: vec![SarifLocation {
            physical_location: SarifPhysicalLocation {
                artifact_location: SarifArtifactLocation {
                    uri: artifact_uri(mutation),
                },
                region: SarifRegion {
                    start_line: mutation.line,
                    start_column: mutation.column,
                    byte_offset: mutation.byte_range.start,
                    byte_length: mutation.byte_range.end - mutation.byte_range.start,
                },
            },
        }],
        properties: (!execution.is_tested()
            || status.is_some()
            || selection.is_some()
            || advisory.is_some())
        .then(|| SarifResultProperties {
            execution: execution.state_name(),
            nonexecution_reason: execution.reason().map(|reason| reason.name()),
            baseline_status: status.map(|status| status.as_str()),
            test_selection_mode: selection.map(|selection| selection.mode_name()),
            selection_confirmation: selection
                .and_then(|selection| selection.confirmation())
                .map(|confirmation| confirmation.name()),
            likely_equivalent: advisory,
        }),
    }
}

/// Rule descriptions from the operator registry, keyed by operator id.
fn registry_descriptions() -> BTreeMap<String, String> {
    crate::operators::all_operators()
        .iter()
        .map(|op| (op.id().to_string(), op.description().to_string()))
        .collect()
}

pub fn print_report(report: &MutationReport) -> Result<()> {
    print_report_with_baseline(report, None)
}

pub(super) fn print_coverage_gate_report(
    report: &crate::coverage::CoverageGateReport,
) -> Result<()> {
    println!("{}", coverage_gate_to_sarif_string(report)?);
    Ok(())
}

/// Coverage gates run before mutation generation; do not represent their
/// failures as surviving mutants or invent mutation execution totals.
fn coverage_gate_to_sarif_string(report: &crate::coverage::CoverageGateReport) -> Result<String> {
    use serde_json::json;

    let mut rules = Vec::new();
    let mut results = Vec::new();
    for (id, label, metric) in [
        (
            "togi.coverage.line-threshold",
            "Overall line coverage",
            &report.line_coverage,
        ),
        (
            "togi.coverage.diff-threshold",
            "Changed-line coverage",
            &report.diff_coverage,
        ),
    ] {
        if metric.meets_threshold() {
            continue;
        }
        rules.push(SarifRule {
            id: id.into(),
            short_description: SarifMessage {
                text: format!("{label} below threshold"),
            },
        });
        results.push(json!({
            "ruleId": id,
            "level": "error",
            "message": { "text": format!(
                "{label}: {:.1}% ({}/{}) is below the {:.1}% threshold.",
                metric.percent(), metric.covered, metric.total,
                metric.threshold.expect("a failed metric has a threshold"),
            ) },
            "properties": metric,
        }));
    }
    if report.fail_on_uncovered_diff {
        let id = "togi.coverage.uncovered-changed-line";
        for file in &report.uncovered_changed_lines {
            for line in &file.lines {
                results.push(json!({
                    "ruleId": id,
                    "level": "error",
                    "message": { "text": "Changed line is not covered by tests." },
                    "properties": { "uncovered_count": 1 },
                    "locations": [{ "physicalLocation": {
                        "artifactLocation": { "uri": coverage_artifact_uri(&file.file) },
                        "region": { "startLine": line },
                    } }],
                }));
            }
        }
        if report
            .uncovered_changed_lines
            .iter()
            .any(|file| !file.lines.is_empty())
        {
            rules.push(SarifRule {
                id: id.into(),
                short_description: SarifMessage {
                    text: "Uncovered changed line".into(),
                },
            });
        }
    }
    let tool = SarifTool {
        driver: SarifDriver {
            name: "togi",
            version: env!("CARGO_PKG_VERSION"),
            information_uri: INFORMATION_URI,
            rules,
        },
    };
    Ok(serde_json::to_string_pretty(&json!({
        "$schema": SARIF_SCHEMA,
        "version": SARIF_VERSION,
        "runs": [{ "tool": tool, "results": results }],
    }))?)
}

fn coverage_artifact_uri(file: &std::path::Path) -> String {
    use std::fmt::Write;

    #[cfg(unix)]
    let path = {
        use std::os::unix::ffi::OsStrExt;
        file.as_os_str().as_bytes()
    };
    #[cfg(not(unix))]
    let normalized = file
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/");
    #[cfg(not(unix))]
    let path = normalized.as_bytes();

    let mut uri = String::new();
    for &byte in path {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char);
            }
            _ => write!(uri, "%{byte:02X}").unwrap(),
        }
    }
    uri
}

pub fn print_report_with_baseline(
    report: &MutationReport,
    comparison: Option<&crate::baseline::SurvivorBaselineComparison>,
) -> Result<()> {
    println!("{}", to_sarif_string_with_baseline(report, comparison)?);
    Ok(())
}

/// Serialize report to a SARIF 2.1.0 string (for testing and programmatic use).
///
/// Emits one result per surviving mutant; killed, timeout, build-error,
/// uncovered, and subsumed mutants are not findings. Uncovered mutants are
/// deliberately not code-scanning results (a zero-coverage line is not an
/// actionable surviving mutant), and subsumed mutants were never executed
/// (their cluster canonical carries the signal); both only appear in the
/// invocation totals.
pub fn to_sarif_string(report: &MutationReport) -> Result<String> {
    to_sarif_string_with_baseline(report, None)
}

/// Serialize report with optional per-survivor baseline comparison data.
pub fn to_sarif_string_with_baseline(
    report: &MutationReport,
    comparison: Option<&crate::baseline::SurvivorBaselineComparison>,
) -> Result<String> {
    let registry = registry_descriptions();
    let equivalent_advisories = crate::equivalent::advisories_for(report);
    let surviving: Vec<(&Mutation, MutationExecution)> = report
        .results
        .iter()
        .filter(|(_, result)| *result == MutationResult::Survived)
        .map(|(mutation, result)| (mutation, report.execution_for(mutation.id, *result)))
        .collect();

    let mut rules: Vec<SarifRule> = Vec::new();
    for (mutation, _) in &surviving {
        if rules.iter().any(|rule| rule.id == mutation.operator) {
            continue;
        }
        let description = registry
            .get(&mutation.operator)
            .cloned()
            .unwrap_or_else(|| mutation.description.clone());
        rules.push(SarifRule {
            id: mutation.operator.clone(),
            short_description: SarifMessage { text: description },
        });
    }

    let execution_counts = report.execution_counts();
    let sarif_report = SarifReport {
        schema: SARIF_SCHEMA,
        version: SARIF_VERSION,
        runs: vec![SarifRun {
            tool: SarifTool {
                driver: SarifDriver {
                    name: "togi",
                    version: env!("CARGO_PKG_VERSION"),
                    information_uri: INFORMATION_URI,
                    rules,
                },
            },
            invocations: vec![SarifInvocation {
                execution_successful: true,
                properties: SarifInvocationProperties {
                    killed: report.killed,
                    survived: report.survived,
                    timeout: report.timeout,
                    build_errors: report.build_errors,
                    tested: execution_counts.executed,
                    executed_killed: execution_counts.executed_killed,
                    exact_cache_reused: execution_counts.exact_cache_reused,
                    incremental_history_reused: execution_counts.incremental_history_reused,
                    uncovered: report.uncovered_count(),
                    subsumed: report.subsumed_count(),
                    mutation_score: super::mutation_score(report),
                    selection_confirmation: SarifSelectionConfirmationSummary::from_report(report),
                },
            }],
            results: surviving
                .iter()
                .map(|(mutation, execution)| {
                    result_for_with_baseline(
                        mutation,
                        *execution,
                        comparison.and_then(|comparison| comparison.status_for(mutation.id)),
                        report.selection_for(mutation.id),
                        equivalent_advisories
                            .get(&mutation.id)
                            .map(|reason| reason.message()),
                    )
                })
                .collect(),
        }],
    };

    Ok(serde_json::to_string_pretty(&sarif_report)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::sample_report;
    use crate::{SurvivorConfirmation, TestSelectionProvenance};
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::time::Duration;

    fn coverage_report() -> crate::coverage::CoverageGateReport {
        crate::coverage::CoverageGateReport {
            line_coverage: crate::coverage::CoverageMetric {
                covered: 1,
                total: 2,
                threshold: Some(80.0),
            },
            diff_coverage: crate::coverage::CoverageMetric {
                covered: 1,
                total: 2,
                threshold: Some(90.0),
            },
            uncovered_changed_lines: vec![crate::coverage::CoverageUncoveredFile {
                file: PathBuf::from("src").join("gap #é%.go"),
                lines: vec![7, 9],
            }],
            fail_on_uncovered_diff: true,
        }
    }

    #[test]
    fn coverage_gate_sarif_combines_failures_and_encodes_locations() {
        let value: Value =
            serde_json::from_str(&coverage_gate_to_sarif_string(&coverage_report()).unwrap())
                .unwrap();
        let run = &value["runs"][0];
        assert_eq!(run["tool"]["driver"]["rules"].as_array().unwrap().len(), 3);
        let results = run["results"].as_array().unwrap();
        assert_eq!(results.len(), 4);
        assert_eq!(results[0]["ruleId"], "togi.coverage.line-threshold");
        assert_eq!(results[1]["ruleId"], "togi.coverage.diff-threshold");
        for (result, line) in results[2..].iter().zip([7, 9]) {
            assert_eq!(result["ruleId"], "togi.coverage.uncovered-changed-line");
            assert_eq!(result["properties"]["uncovered_count"], 1);
            let location = &result["locations"][0]["physicalLocation"];
            assert_eq!(
                location["artifactLocation"]["uri"],
                "src/gap%20%23%C3%A9%25.go"
            );
            assert_eq!(location["region"]["startLine"], line);
        }
        assert!(
            run.get("invocations").is_none(),
            "no mutation totals before a campaign"
        );
    }

    #[test]
    fn coverage_gate_sarif_omits_disabled_or_met_thresholds() {
        let mut report = coverage_report();
        report.fail_on_uncovered_diff = false;
        for threshold in [None, Some(50.0)] {
            report.line_coverage.threshold = threshold;
            report.diff_coverage.threshold = threshold;
            let value: Value =
                serde_json::from_str(&coverage_gate_to_sarif_string(&report).unwrap()).unwrap();
            assert!(value["runs"][0]["results"].as_array().unwrap().is_empty());
            assert!(
                value["runs"][0]["tool"]["driver"]["rules"]
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn coverage_gate_sarif_preserves_non_utf8_path_bytes() {
        use std::os::unix::ffi::OsStrExt;

        let mut report = coverage_report();
        report.uncovered_changed_lines[0].file =
            PathBuf::from(std::ffi::OsStr::from_bytes(b"src/gap-\xff.go"));
        let value: Value =
            serde_json::from_str(&coverage_gate_to_sarif_string(&report).unwrap()).unwrap();
        assert_eq!(
            value["runs"][0]["results"][2]["locations"][0]["physicalLocation"]["artifactLocation"]
                ["uri"],
            "src/gap-%FF.go"
        );
    }

    fn mutation(id: u32, file: &str, line: usize, operator: &str, description: &str) -> Mutation {
        Mutation {
            id,
            file: PathBuf::from(file),
            language: String::new(),
            line,
            column: 3,
            operator: operator.into(),
            description: description.into(),
            original: "==".into(),
            replacement: "!=".into(),
            byte_range: 10..12,
        }
    }

    fn report_with(results: Vec<(Mutation, MutationResult)>) -> MutationReport {
        let killed = results
            .iter()
            .filter(|(_, r)| *r == MutationResult::Killed)
            .count();
        let survived = results
            .iter()
            .filter(|(_, r)| *r == MutationResult::Survived)
            .count();
        let timeout = results
            .iter()
            .filter(|(_, r)| *r == MutationResult::Timeout)
            .count();
        let build_errors = results
            .iter()
            .filter(|(_, r)| *r == MutationResult::BuildError)
            .count();
        MutationReport {
            selection_provenance: std::collections::BTreeMap::new(),
            planned_total: results.len(),
            early_stop_reason: None,
            total: results.len(),
            killed,
            survived,
            timeout,
            build_errors,
            duration: Duration::from_secs(0),
            test_command: None,
            build_command: vec![],
            results,
            execution_provenance: BTreeMap::new(),
            build_error_diagnostics: vec![],
            schemata: None,
            baseline_timing: None,
        }
    }

    fn parse(report: &MutationReport) -> Value {
        let sarif_str = to_sarif_string(report).expect("report should serialize");
        serde_json::from_str(&sarif_str).expect("output should be valid JSON")
    }

    #[test]
    fn sarif_output_is_valid_json_with_envelope() {
        let value = parse(&sample_report());
        assert!(value.is_object());
        assert_eq!(
            value["$schema"],
            "https://json.schemastore.org/sarif-2.1.0.json"
        );
        assert_eq!(value["version"], "2.1.0");
        assert_eq!(value["runs"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn sarif_driver_has_name_version_and_rules() {
        let value = parse(&sample_report());
        let driver = &value["runs"][0]["tool"]["driver"];
        assert_eq!(driver["name"], "togi");
        assert_eq!(driver["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            driver["informationUri"],
            "https://github.com/Darkroom4364/togi"
        );
        // Only the surviving mutant's operator produces a rule.
        let rules = driver["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["id"], "eq_to_neq");
        // Description comes from the operator registry, not the mutation.
        assert_eq!(rules[0]["shortDescription"]["text"], "Replace == with !=");
    }

    #[test]
    fn sarif_only_surviving_mutants_produce_results() {
        let report = report_with(vec![
            (
                mutation(0, "src/a.rs", 1, "op_a", "killed one"),
                MutationResult::Killed,
            ),
            (
                mutation(1, "src/b.rs", 2, "op_b", "survived one"),
                MutationResult::Survived,
            ),
            (
                mutation(2, "src/c.rs", 3, "op_c", "timed out"),
                MutationResult::Timeout,
            ),
            (
                mutation(3, "src/d.rs", 4, "op_d", "build error"),
                MutationResult::BuildError,
            ),
        ]);
        let value = parse(&report);
        let results = value["runs"][0]["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["ruleId"], "op_b");
    }

    #[test]
    fn sarif_keeps_confirmation_outcomes_in_invocation_properties() {
        let mut report = report_with(vec![
            (
                mutation(0, "src/killed.rs", 1, "op", "killed by confirmation"),
                MutationResult::Killed,
            ),
            (
                mutation(1, "src/survived.rs", 2, "op", "confirmed survivor"),
                MutationResult::Survived,
            ),
        ]);
        report.selection_provenance.insert(
            0,
            TestSelectionProvenance::Narrowed {
                confirmation: SurvivorConfirmation::Killed,
            },
        );
        report.selection_provenance.insert(
            1,
            TestSelectionProvenance::Narrowed {
                confirmation: SurvivorConfirmation::ConfirmedSurvived,
            },
        );

        let value = parse(&report);

        assert_eq!(value["runs"][0]["results"].as_array().unwrap().len(), 1);
        assert_eq!(
            value["runs"][0]["results"][0]["properties"]["test_selection_mode"],
            "narrowed"
        );
        assert_eq!(
            value["runs"][0]["results"][0]["properties"]["selection_confirmation"],
            "confirmed_survived"
        );
        assert_eq!(
            value["runs"][0]["invocations"][0]["properties"]["selection_confirmation"],
            serde_json::json!({
                "full": 0,
                "narrowed": 2,
                "confirmation_not_requested": 0,
                "confirmation_not_needed": 0,
                "confirmation_confirmed_survived": 1,
                "confirmation_killed": 1,
                "confirmation_timed_out": 0,
                "confirmation_build_error": 0
            })
        );
        assert!(
            parse(&sample_report())["runs"][0]["invocations"][0]["properties"]
                .get("selection_confirmation")
                .is_none()
        );
    }

    #[test]
    fn sarif_adds_active_baseline_statuses_only_to_survivor_results() {
        let mut report = report_with(vec![
            (
                mutation(0, "src/killed.rs", 1, "op", "killed"),
                MutationResult::Killed,
            ),
            (
                mutation(1, "src/fresh.rs", 2, "op", "fresh"),
                MutationResult::Survived,
            ),
            (
                mutation(2, "src/exact.rs", 3, "op", "exact"),
                MutationResult::Survived,
            ),
        ]);
        report
            .execution_provenance
            .insert(2, MutationExecution::ExactCache);
        let comparison =
            crate::baseline::SurvivorBaselineComparison::from_statuses(BTreeMap::from([
                (0, crate::baseline::SurvivorBaselineStatus::New),
                (1, crate::baseline::SurvivorBaselineStatus::Historic),
                (2, crate::baseline::SurvivorBaselineStatus::NonComparable),
            ]));

        let inactive = to_sarif_string(&report).unwrap();
        assert_eq!(
            inactive,
            to_sarif_string_with_baseline(&report, None).unwrap()
        );
        let inactive: Value = serde_json::from_str(&inactive).unwrap();
        assert!(
            inactive["runs"][0]["results"][0]
                .get("properties")
                .is_none()
        );

        let active: Value = serde_json::from_str(
            &to_sarif_string_with_baseline(&report, Some(&comparison)).unwrap(),
        )
        .unwrap();
        let results = active["runs"][0]["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["properties"]["baseline_status"], "historic");
        assert_eq!(
            results[1]["properties"],
            serde_json::json!({
                "execution": "exact_cache",
                "baseline_status": "non_comparable"
            })
        );
    }

    #[test]
    fn sarif_survivor_results_include_nonfresh_provenance() {
        let mut report = report_with(vec![
            (
                mutation(0, "src/fresh.rs", 1, "op", "fresh"),
                MutationResult::Survived,
            ),
            (
                mutation(1, "src/exact.rs", 2, "op", "exact"),
                MutationResult::Survived,
            ),
            (
                mutation(2, "src/history.rs", 3, "op", "history"),
                MutationResult::Survived,
            ),
            (
                mutation(3, "src/skipped.rs", 4, "op", "skipped"),
                MutationResult::Survived,
            ),
        ]);
        report
            .execution_provenance
            .insert(1, MutationExecution::ExactCache);
        report
            .execution_provenance
            .insert(2, MutationExecution::IncrementalHistory);
        report.execution_provenance.insert(
            3,
            MutationExecution::NotExecuted(crate::MutationNonExecutionReason::Subsumed),
        );

        let results = parse(&report)["runs"][0]["results"].clone();

        assert!(results[0].get("properties").is_none());
        assert_eq!(
            results[1]["properties"],
            serde_json::json!({"execution": "exact_cache"})
        );
        assert_eq!(
            results[2]["properties"],
            serde_json::json!({"execution": "incremental_history"})
        );
        assert_eq!(
            results[3]["properties"],
            serde_json::json!({
                "execution": "not_executed",
                "nonexecution_reason": "subsumed"
            })
        );
    }
    #[test]
    fn sarif_result_has_level_message_and_location() {
        let value = parse(&sample_report());
        let result = &value["runs"][0]["results"][0];
        assert_eq!(result["level"], "warning");
        assert_eq!(
            result["message"]["text"],
            "Survived mutation: changed == to != (== → !=)"
        );
        let location = &result["locations"][0]["physicalLocation"];
        assert_eq!(location["artifactLocation"]["uri"], "src/handler.rs");
        assert_eq!(location["region"]["startLine"], 15);
        assert_eq!(location["region"]["startColumn"], 5);
        assert_eq!(location["region"]["byteOffset"], 0);
        assert_eq!(location["region"]["byteLength"], 2);
    }

    #[test]
    fn sarif_invocation_properties_carry_totals_and_score() {
        let value = parse(&sample_report());
        let invocation = &value["runs"][0]["invocations"][0];
        assert_eq!(invocation["executionSuccessful"], true);
        let properties = &invocation["properties"];
        assert_eq!(properties["killed"], 1);
        assert_eq!(properties["survived"], 1);
        assert_eq!(properties["timeout"], 0);
        assert_eq!(properties["build_errors"], 0);
        assert_eq!(properties["tested"], 2);
        assert_eq!(properties["executed_killed"], 1);
        assert_eq!(properties["mutation_score"], 50.0);
    }

    #[test]
    fn sarif_rules_deduplicate_and_fall_back_to_mutation_description() {
        let report = report_with(vec![
            (
                mutation(0, "src/a.rs", 1, "custom_op", "Custom operator description"),
                MutationResult::Survived,
            ),
            (
                mutation(1, "src/b.rs", 2, "custom_op", "Custom operator description"),
                MutationResult::Survived,
            ),
        ]);
        let value = parse(&report);
        assert_eq!(value["runs"][0]["results"].as_array().unwrap().len(), 2);
        let rules = value["runs"][0]["tool"]["driver"]["rules"]
            .as_array()
            .unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0]["id"], "custom_op");
        // Unknown operators keep the mutation's own description.
        assert_eq!(
            rules[0]["shortDescription"]["text"],
            "Custom operator description"
        );
    }

    #[test]
    fn sarif_empty_report_has_no_results_or_rules() {
        let value = parse(&report_with(vec![]));
        let run = &value["runs"][0];
        assert_eq!(run["results"], serde_json::json!([]));
        assert_eq!(run["tool"]["driver"]["rules"], serde_json::json!([]));
        assert_eq!(run["invocations"][0]["properties"]["mutation_score"], 100.0);
    }

    #[test]
    fn sarif_uncovered_mutants_produce_no_results_but_appear_in_totals() {
        let report = report_with(vec![
            (
                mutation(0, "src/a.rs", 1, "op_a", "covered and killed"),
                MutationResult::Killed,
            ),
            (
                mutation(1, "src/dead.rs", 9, "op_b", "zero coverage"),
                MutationResult::Uncovered,
            ),
        ]);
        let value = parse(&report);
        // Uncovered mutants are not code-scanning findings.
        assert_eq!(value["runs"][0]["results"], serde_json::json!([]));
        let properties = &value["runs"][0]["invocations"][0]["properties"];
        assert_eq!(properties["uncovered"], 1);
        // Score excludes uncovered mutants: 1 killed of 1 tested → 100%.
        assert_eq!(properties["mutation_score"], 100.0);
    }

    #[test]
    fn sarif_omits_uncovered_property_when_zero() {
        let value = parse(&sample_report());
        let properties = &value["runs"][0]["invocations"][0]["properties"];
        assert!(properties.get("uncovered").is_none());
    }

    #[test]
    fn sarif_subsumed_mutants_produce_no_results_but_appear_in_totals() {
        let report = report_with(vec![
            (
                mutation(0, "src/a.rs", 1, "op_a", "canonical kill"),
                MutationResult::Killed,
            ),
            (
                mutation(1, "src/a.rs", 5, "op_b", "same killer test"),
                MutationResult::Subsumed,
            ),
        ]);
        let value = parse(&report);
        // Subsumed mutants are not code-scanning findings.
        assert_eq!(value["runs"][0]["results"], serde_json::json!([]));
        let properties = &value["runs"][0]["invocations"][0]["properties"];
        assert_eq!(properties["subsumed"], 1);
        // Score excludes subsumed mutants: 1 killed of 1 tested → 100%.
        assert_eq!(properties["mutation_score"], 100.0);
    }

    #[test]
    fn sarif_omits_subsumed_property_when_zero() {
        let value = parse(&sample_report());
        let properties = &value["runs"][0]["invocations"][0]["properties"];
        assert!(properties.get("subsumed").is_none());
    }

    #[test]
    fn sarif_artifact_uris_use_forward_slashes() {
        let report = report_with(vec![(
            mutation(0, "src\\win\\a.rs", 7, "op", "desc"),
            MutationResult::Survived,
        )]);
        let value = parse(&report);
        assert_eq!(
            value["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["artifactLocation"]
                ["uri"],
            "src/win/a.rs"
        );
    }

    #[test]
    fn sarif_small_report_matches_expected_structure() {
        let report = report_with(vec![
            (
                mutation(0, "src/a.rs", 3, "lt_to_lte", "changed < to <="),
                MutationResult::Killed,
            ),
            (
                mutation(1, "src/b.rs", 9, "eq_to_neq", "changed == to !="),
                MutationResult::Survived,
            ),
        ]);
        let value = parse(&report);
        let expected = serde_json::json!({
            "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
            "version": "2.1.0",
            "runs": [{
                "tool": {
                    "driver": {
                        "name": "togi",
                        "version": env!("CARGO_PKG_VERSION"),
                        "informationUri": "https://github.com/Darkroom4364/togi",
                        "rules": [{
                            "id": "eq_to_neq",
                            "shortDescription": { "text": "Replace == with !=" }
                        }]
                    }
                },
                "invocations": [{
                    "executionSuccessful": true,
                    "properties": {
                        "killed": 1,
                        "survived": 1,
                        "timeout": 0,
                        "build_errors": 0,
                        "tested": 2,
                        "executed_killed": 1,
                        "mutation_score": 50.0
                    }
                }],
                "results": [{
                    "ruleId": "eq_to_neq",
                    "level": "warning",
                    "message": {
                        "text": "Survived mutation: changed == to != (== → !=)"
                    },
                    "locations": [{
                        "physicalLocation": {
                            "artifactLocation": { "uri": "src/b.rs" },
                            "region": {
                                "startLine": 9,
                                "startColumn": 3,
                                "byteOffset": 10,
                                "byteLength": 2
                            }
                        }
                    }]
                }]
            }]
        });
        assert_eq!(value, expected);
    }
}
