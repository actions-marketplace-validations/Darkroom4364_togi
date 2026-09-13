use anyhow::Context;
use clap::Parser;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::Deserialize;
use togi::{BaselineTiming, ChangedFile, Mutation};

struct ExecuteOptions {
    verbose: bool,
    show_output: bool,
    early_stop: togi::runner::EarlyStopConfig,
    env: HashMap<String, String>,
    force_rerun: bool,
    learned_selection: bool,
    cancelled: Arc<AtomicBool>,
}

#[derive(Debug)]
struct ResolvedCheckConfig {
    config: togi::config::Config,
    fail_fast: bool,
    has_custom_test_cmd: bool,
    has_cli_timeout: bool,
    profile: Option<togi::config::ResourceProfile>,
}

fn main() {
    let cancelled = Arc::new(AtomicBool::new(false));

    // First Ctrl+C sets the flag so the runner can stop gracefully.
    // Second Ctrl+C force-exits for impatient users.
    #[cfg(unix)]
    {
        use signal_hook::consts::SIGINT;
        // Conditional shutdown MUST precede flag registration: if the flag
        // were registered first, the first SIGINT would both set the flag
        // AND immediately terminate.
        signal_hook::flag::register_conditional_shutdown(SIGINT, 130, cancelled.clone())
            .expect("failed to register SIGINT conditional shutdown");
        signal_hook::flag::register(SIGINT, cancelled.clone())
            .expect("failed to register SIGINT flag handler");
    }
    #[cfg(windows)]
    {
        let cancelled_handler = cancelled.clone();
        ctrlc::set_handler(move || {
            if cancelled_handler.swap(true, Ordering::SeqCst) {
                eprintln!("\nForce exit — files may need manual restoration (check git status)");
                process::exit(130);
            }
            eprintln!("\nInterrupted — finishing current mutation and cleaning up...");
        })
        .expect("failed to set Ctrl+C handler");
    }

    let cli = togi::cli::Cli::parse();

    match cli.command {
        togi::cli::Commands::Check(cfg) => {
            if let Err(e) = run_check(cfg, cancelled) {
                eprintln!("Error: {e:#}");
                process::exit(2);
            }
        }
        togi::cli::Commands::TestMap { path, output } => {
            if let Err(e) = run_test_map(path, output, &cancelled) {
                eprintln!("Error: {e:#}");
                process::exit(2);
            }
        }
        togi::cli::Commands::Clean => {
            let project_root = get_project_root().unwrap_or_else(|e| {
                eprintln!("Error: {e:#}");
                process::exit(2);
            });
            match togi::cache::clear(&project_root) {
                Ok(()) => println!("Cache cleared."),
                Err(e) => {
                    eprintln!("Error clearing cache: {e}");
                    process::exit(2);
                }
            }
        }
        togi::cli::Commands::Explain { mutant_id, report } => {
            if let Err(e) = explain_mutation(mutant_id, &report) {
                eprintln!("Error: {e:#}");
                process::exit(2);
            }
        }
        togi::cli::Commands::Replay {
            mutant_id,
            report,
            show_output,
            verify_killed,
        } => {
            if let Err(e) = togi::replay::replay_mutation(
                mutant_id,
                &report,
                show_output,
                verify_killed,
                cancelled.as_ref(),
            ) {
                eprintln!("Error: {e:#}");
                process::exit(2);
            }
        }
        togi::cli::Commands::ListOperators => {
            print_operators();
        }
        togi::cli::Commands::Init => {
            let path = std::path::Path::new("togi.toml");
            if path.exists() {
                eprintln!("togi.toml already exists");
                process::exit(2);
            }
            if let Err(e) = togi::config::Config::write_template(path) {
                eprintln!("Error: {e}");
                process::exit(2);
            }
            println!("Created togi.toml (auto-detected from project)");
        }
    }
}

#[derive(Deserialize)]
struct ExplainReport {
    test_command: Option<Vec<String>>,
    build_command: Option<Vec<String>>,
    mutations: Vec<ExplainMutation>,
}

#[derive(Deserialize)]
struct ExplainMutation {
    id: u32,
    file: String,
    line: usize,
    operator: String,
    description: String,
    result: String,
    execution: Option<ExplainMutationExecution>,
    test_selection: Option<ExplainTestSelection>,
    original: Option<String>,
    replacement: Option<String>,
    diff: Option<String>,
}

#[derive(Deserialize)]
struct ExplainMutationExecution {
    state: String,
    reason: Option<String>,
}

#[derive(Deserialize)]
struct ExplainTestSelection {
    mode: String,
    confirmation: Option<String>,
}

fn explain_mutation(mutant_id: u32, report_path: &Path) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(report_path)
        .map_err(|e| anyhow::anyhow!("could not read {}: {e}", report_path.display()))?;
    let report: ExplainReport = serde_json::from_str(&content)
        .map_err(|e| anyhow::anyhow!("could not parse {} as JSON: {e}", report_path.display()))?;
    let mutation = report
        .mutations
        .iter()
        .find(|m| m.id == mutant_id)
        .ok_or_else(|| anyhow::anyhow!("mutation id {mutant_id} not found in report"))?;

    println!("Mutation #{}", mutation.id);
    println!(
        "{}:{} — {} ({})",
        mutation.file, mutation.line, mutation.operator, mutation.result
    );
    println!("{}", mutation.description);

    if let (Some(original), Some(replacement)) = (&mutation.original, &mutation.replacement) {
        println!();
        println!("Change: {original} -> {replacement}");
    }

    if let Some(diff) = &mutation.diff {
        println!();
        println!("{diff}");
    }

    let (execution_state, execution_reason) = mutation
        .execution
        .as_ref()
        .map(|execution| (execution.state.as_str(), execution.reason.as_deref()))
        .unwrap_or_else(|| match mutation.result.as_str() {
            "killed" | "survived" | "timeout" => ("executed", None),
            "build_error" => ("not_executed", Some("build_error")),
            "uncovered" => ("not_executed", Some("uncovered")),
            "subsumed" => ("not_executed", Some("subsumed")),
            _ => ("not_executed", None),
        });
    let test_executed = execution_state == "executed";
    let reused_verdict = matches!(execution_state, "exact_cache" | "incremental_history");
    let execution_detail = match execution_state {
        "executed" => "executed".to_string(),
        "exact_cache" => "reused from exact cache".to_string(),
        "incremental_history" => "reused from incremental history".to_string(),
        "not_executed" => match execution_reason {
            Some(reason) => format!("not executed ({reason})"),
            None => "not executed".to_string(),
        },
        other => format!("not executed ({other})"),
    };

    println!();
    println!("Execution: {execution_detail}");
    if let Some(selection) = &mutation.test_selection {
        println!("Test selection: {}", selection.mode);
        if let Some(confirmation) = &selection.confirmation {
            println!("Full-suite confirmation: {confirmation}");
        }
    }
    if test_executed {
        if let Some(command) = report.test_command.as_ref().filter(|cmd| !cmd.is_empty()) {
            println!("Test command: {}", serde_json::to_string(command)?);
        }
    } else {
        println!("Test command: not run.");
    }
    if let Some(command) = report.build_command.as_ref().filter(|cmd| !cmd.is_empty()) {
        println!("Build check: {}", serde_json::to_string(command)?);
    }

    match mutation.result.as_str() {
        "survived" => {
            println!("Why it survived:");
            if test_executed {
                println!(
                    "  The configured test command completed successfully with this mutation."
                );
            } else {
                println!("  This verdict was not freshly tested in this invocation.");
            }
            println!(
                "  Add an assertion that distinguishes the original behavior from the mutated one."
            );
        }
        "killed" => {
            println!("Why it was killed:");
            if test_executed {
                println!(
                    "  The configured test command failed with this mutation, so existing tests caught it."
                );
            } else {
                println!("  This verdict was not freshly tested in this invocation.");
            }
        }
        "timeout" => {
            println!("Why it timed out:");
            if test_executed {
                println!("  The configured test command exceeded the mutation timeout.");
            } else {
                println!("  This verdict was not freshly tested in this invocation.");
            }
        }
        "build_error" => {
            println!("Why it was not testable:");
            if reused_verdict {
                println!("  This build-error verdict was not produced in this invocation.");
            } else {
                println!("  The mutation made the project fail its build check.");
            }
        }
        "uncovered" => {
            println!("Why it was not executed:");
            println!("  Coverage data shows this line is never executed by the test suite.");
            println!("  Add a test that reaches this line to make the mutant meaningful.");
        }
        "subsumed" => {
            println!("Why it was not executed:");
            println!(
                "  Learned selection clustered it with an earlier mutant that shares its recorded killer test."
            );
            println!("  Re-run without --learned-selection to execute every mutant.");
        }
        other => {
            println!("Result: {other}");
        }
    }

    Ok(())
}

fn print_operators() {
    let ops = togi::operators::all_operators();
    let mut by_category: std::collections::BTreeMap<&str, Vec<(&str, &str)>> =
        std::collections::BTreeMap::new();
    for op in &ops {
        let cat = togi::operators::operator_category(op.id());
        by_category
            .entry(cat)
            .or_default()
            .push((op.id(), op.description()));
    }
    for (category, ops) in &by_category {
        println!("{category}:");
        for (id, desc) in ops {
            println!("  {id:<30} {desc}");
        }
        println!();
    }
}

fn run_check(cfg: togi::cli::CheckArgs, cancelled: Arc<AtomicBool>) -> anyhow::Result<()> {
    let all = cfg.all;
    let paths = cfg.path.clone();
    let dry_run = cfg.dry_run;
    let verbose = cfg.verbose;
    let show_output = cfg.show_output;
    let output_format = cfg.format;
    let json_report_path = cfg.json_report.clone();
    let fail_under = cfg.fail_under;
    let max_survivors = match (cfg.first_survivor, cfg.max_survivors) {
        (true, _) => Some(1),
        (false, Some(0)) => anyhow::bail!("--max-survivors must be greater than 0"),
        (false, value) => value,
    };
    let early_stop = togi::runner::EarlyStopConfig {
        max_survivors,
        fail_under,
    };
    let shard = cfg.shard.as_deref().map(parse_shard).transpose()?;
    let save_baseline = cfg.save_baseline;
    let check_baseline = cfg.check_baseline;
    let pr_comment = cfg.pr_comment.clone();
    let force_rerun = cfg.force_rerun;
    let learned_selection = cfg.learned_selection;

    let resolved = resolve_config(cfg)?;
    let ResolvedCheckConfig {
        mut config,
        fail_fast,
        has_custom_test_cmd,
        has_cli_timeout,
        profile,
    } = resolved;
    let project_root = match get_project_root() {
        Ok(project_root) => project_root,
        Err(_) if all => {
            std::env::current_dir().context("could not determine project directory")?
        }
        Err(error) => return Err(error),
    };
    validate_json_report_destinations(
        json_report_path.as_deref(),
        output_format,
        save_baseline,
        pr_comment.as_deref(),
        &project_root,
    )?;
    let ambiguous_test_command = match config.resolve_test_command(&project_root) {
        togi::config::TestCommandResolution::Resolved => None,
        togi::config::TestCommandResolution::Ambiguous(ambiguity) => Some(ambiguity),
    };
    if let Some(ambiguity) = &ambiguous_test_command {
        if has_custom_test_cmd || !config.has_configured_test_command_routes() {
            return Err(ambiguity.error());
        }
    }
    let _lock = togi::lock::acquire(&project_root)?;
    let build_command_origin = config.resolve_build_command(&project_root);
    warn_if_resource_oversubscribed(config.test.jobs);
    let profile_env = if has_custom_test_cmd {
        HashMap::new()
    } else {
        profile
            .map(|profile| resource_profile_env(profile, &config))
            .unwrap_or_default()
    };

    if fail_fast {
        let args = togi::config::failfast_args(&config.test.command);
        config.test.command.extend(args);
        for lang_config in config.test.languages.values_mut() {
            let args = togi::config::failfast_args(&lang_config.command);
            lang_config.command.extend(args);
        }
    }

    let all_langs = togi::languages::all();
    let known: Vec<&str> = all_langs.iter().map(|l| l.name()).collect();
    config.warn_unknown_languages(&known);

    let changed_files = collect_files(
        &config,
        all,
        &paths,
        output_format == togi::cli::OutputFormat::Json,
        &project_root,
    )?;
    if changed_files.is_empty() {
        if output_format == togi::cli::OutputFormat::Json || json_report_path.is_some() {
            emit_empty_json_check_output(
                &config,
                build_command_origin,
                dry_run,
                &project_root,
                output_format == togi::cli::OutputFormat::Json,
                json_report_path.as_deref(),
            )?;
        }
        return Ok(());
    }

    // Ambiguity can be deferred only when runner precedence selects a configured
    // project or language command for every collected source file.
    if let Some(ambiguity) = &ambiguous_test_command {
        for changed_file in &changed_files {
            if !project_root.join(&changed_file.path).exists() {
                continue;
            }
            let Some(extension) = changed_file
                .path
                .extension()
                .and_then(|extension| extension.to_str())
            else {
                continue;
            };
            let Some(language) = all_langs
                .iter()
                .find(|language| language.extensions().contains(&extension))
            else {
                continue;
            };
            if !config.has_configured_test_command_for_path(
                &project_root,
                &changed_file.path,
                language.name(),
            ) {
                return Err(ambiguity.error_for_path(&changed_file.path));
            }
        }
    }

    let coverage_gate_active = config.mutations.min_line_coverage.is_some()
        || config.mutations.min_diff_coverage.is_some()
        || config.mutations.fail_on_uncovered_diff;
    let coverage_stats = resolve_coverage_stats(&config, &project_root, coverage_gate_active)?;

    if coverage_gate_active {
        let stats = coverage_stats
            .as_ref()
            .expect("coverage stats should exist when coverage gates are enabled");
        let mut coverage_report =
            togi::coverage::diff_coverage_report(stats, &changed_files, &project_root);
        coverage_report.line_coverage.threshold = config.mutations.min_line_coverage;
        coverage_report.diff_coverage.threshold = config.mutations.min_diff_coverage;
        coverage_report.fail_on_uncovered_diff = config.mutations.fail_on_uncovered_diff;
        if !coverage_report.passes() {
            togi::report::print_coverage_gate_report(&coverage_report, output_format)?;
            exit_with(_lock, 1);
        }
    }

    let mutations = generate_mutations(&changed_files, &config, &project_root)?;
    let mut classified =
        filter_mutations(mutations, &config, &project_root, coverage_stats.as_ref())?;

    if let Some((k, n)) = shard {
        let total = classified.to_run.len();
        classified.to_run.retain(|m| m.id as usize % n == k - 1);
        eprintln!(
            "Shard {k}/{n}: {} of {total} mutations",
            classified.to_run.len()
        );
    }

    if classified.to_run.is_empty() && classified.uncovered.is_empty() {
        if output_format == togi::cli::OutputFormat::Json {
            eprintln!("No mutations generated. Possible causes:");
            eprintln!("  - Changed files are in an unsupported language");
            eprintln!("  - All mutable nodes were filtered out (test files, noisy patterns)");
            eprintln!("  - max_per_run or max_per_file is set to 0 in togi.toml");
        } else {
            println!("No mutations generated. Possible causes:");
            println!("  - Changed files are in an unsupported language");
            println!("  - All mutable nodes were filtered out (test files, noisy patterns)");
            println!("  - max_per_run or max_per_file is set to 0 in togi.toml");
        }
        if output_format == togi::cli::OutputFormat::Json || json_report_path.is_some() {
            emit_empty_json_check_output(
                &config,
                build_command_origin,
                dry_run,
                &project_root,
                output_format == togi::cli::OutputFormat::Json,
                json_report_path.as_deref(),
            )?;
        }
        return Ok(());
    }

    if dry_run {
        if output_format == togi::cli::OutputFormat::Json {
            togi::report::json::print_dry_run(&classified.to_run)?;
        } else {
            print_dry_run(&classified.to_run);
        }
        return Ok(());
    }

    let baseline_mutations = classified
        .to_run
        .iter()
        .chain(&classified.uncovered)
        .cloned()
        .collect::<Vec<_>>();
    // Snapshot exact source evidence before baseline or mutation commands can
    // run; JSON serialization revalidates it after the run.
    let mut replay_capture =
        togi::replay::ReplayReportCapture::capture(&project_root, &baseline_mutations);
    let (baseline_timing, commands) = if baseline_mutations.is_empty() {
        (None, None)
    } else {
        let mut commands = command_config(
            &config,
            &project_root,
            build_command_origin,
            has_custom_test_cmd,
            has_cli_timeout,
        );
        if config.test.calibrate_timeout {
            eprintln!("Measuring baseline test runtime...");
        } else {
            eprintln!("Checking baseline test suites...");
        }
        let measurement = match togi::runner::check_baseline_health(
            &project_root,
            &baseline_mutations,
            togi::runner::BaselineHealthConfig {
                commands: &commands,
                default_measurement_timeout: config
                    .test
                    .calibrate_timeout
                    .then(|| baseline_measurement_timeout(config.test.timeout)),
                schemata_enabled: config.mutations.schemata,
                env: &profile_env,
                cancelled: &cancelled,
                respect_workspace_ignores: config.mutations.respect_workspace_ignores,
            },
        ) {
            Ok(measurement) => measurement,
            Err(_) if cancelled.load(Ordering::Acquire) => exit_with(_lock, 130),
            Err(error) => {
                if let Some(failure) = togi::runner::run_suite_failure(&error) {
                    togi::report::print_run_suite_failure(failure, output_format)?;
                }
                return Err(error);
            }
        };
        let baseline_timing = if config.test.calibrate_timeout {
            if let Some(measurement) = calibration_measurement(&measurement) {
                let timeout_secs = calibrated_timeout_seconds(
                    measurement.build_duration,
                    measurement.test_duration,
                    config.test.timeout_multiplier,
                    config.test.timeout_slack,
                );
                config.test.timeout = timeout_secs;
                commands.timeout = Duration::from_secs(timeout_secs);
                let timing = BaselineTiming {
                    build_command: if build_command_origin.runs_before_tests() {
                        config.test.build_command.clone()
                    } else {
                        vec![]
                    },
                    build_duration: measurement.build_duration,
                    test_command: measurement.test_command.clone(),
                    test_duration: measurement.test_duration,
                    calibrated_timeout: Duration::from_secs(timeout_secs),
                };
                eprintln!("{}", baseline_timing_summary(&timing));
                Some(timing)
            } else {
                None
            }
        } else {
            None
        };
        (baseline_timing, Some(commands))
    };

    if cancelled.load(Ordering::Acquire) {
        eprintln!("Interrupted; skipping mutation report and side effects.");
        exit_with(_lock, 130);
    }

    let project_root_ref = project_root.clone();
    let (mut report, run_cancelled, direct_recipes) = if classified.to_run.is_empty() {
        // Every generated mutant sits on a zero-coverage line: nothing to run.
        (
            coverage_only_report(&config, build_command_origin),
            false,
            BTreeMap::new(),
        )
    } else {
        eprintln!("Running {} mutations...", classified.to_run.len());
        let outcome = execute(
            classified.to_run,
            config,
            commands.expect("baseline commands should exist for scheduled mutations"),
            project_root,
            ExecuteOptions {
                verbose,
                show_output,
                early_stop,
                env: profile_env,
                force_rerun,
                learned_selection,
                cancelled: cancelled.clone(),
            },
        );
        (outcome.report, outcome.cancelled, outcome.replay_recipes)
    };
    if run_cancelled || cancelled.load(Ordering::Acquire) {
        eprintln!("Interrupted; skipping mutation report and side effects.");
        exit_with(_lock, 130);
    }

    report.baseline_timing = baseline_timing;
    merge_uncovered(&mut report, classified.uncovered);

    let baseline_eligible = togi::baseline::is_baseline_eligible(&report);
    let mut should_fail = false;
    let partial_report = report.total < report.planned_total;
    let baseline_actions_allowed = baseline_eligible;
    let mut baseline_score: Option<f64> = None;
    let mut loaded_baseline = false;
    let mut survivor_comparison = None;
    let mut baseline_regressions = None;
    let mut baseline_load_error = None;

    if check_baseline && baseline_actions_allowed {
        match togi::baseline::load_baseline(&project_root_ref) {
            Ok(Some(baseline)) => {
                loaded_baseline = true;
                baseline_score =
                    Some(baseline.killed as f64 / baseline.total.max(1) as f64 * 100.0);
                survivor_comparison = Some(togi::baseline::compare_survivors(
                    &report,
                    &baseline,
                    &project_root_ref,
                ));
                let current = togi::baseline::from_report(&report, &project_root_ref);
                if togi::baseline::check_regression(&current, &baseline) {
                    should_fail = true;
                    baseline_regressions =
                        Some(togi::baseline::per_file_regressions(&current, &baseline));
                }
            }
            Ok(None) => {}
            Err(error) => baseline_load_error = Some(error),
        }
    }

    // Test commands can change symlink topology. Re-resolve all publication
    // paths after the campaign, before any report side effect.
    validate_json_report_destinations(
        json_report_path.as_deref(),
        output_format,
        save_baseline,
        pr_comment.as_deref(),
        &project_root_ref,
    )?;

    let json_stdout = output_format == togi::cli::OutputFormat::Json;
    if json_stdout || json_report_path.is_some() {
        replay_capture.revalidate(&project_root_ref);
        let json = togi::report::json::to_json_string_with_baseline_and_replay(
            &report,
            survivor_comparison.as_ref(),
            &replay_capture,
            &direct_recipes,
        )?;
        if let Some(path) = &json_report_path {
            write_json_report(path, &json)?;
        }
        if json_stdout {
            println!("{json}");
        }
    }
    if !json_stdout {
        togi::report::print_report_with_baseline(
            &report,
            output_format,
            survivor_comparison.as_ref(),
        )?;
    }

    if let Some(error) = baseline_load_error {
        return Err(error);
    }

    if let Some(regressions) = baseline_regressions {
        eprintln!("Mutation score regression detected!");
        for r in regressions {
            eprintln!(
                "  {} — {:.1}% → {:.1}%",
                r.file, r.baseline_pct, r.current_pct
            );
        }
    }

    if (save_baseline || check_baseline) && !baseline_actions_allowed {
        if partial_report {
            eprintln!("Partial early-stop report; skipping baseline save/check.");
        } else {
            eprintln!(
                "Report has no complete fresh execution evidence; skipping baseline save/check."
            );
        }
    } else if save_baseline {
        let saved = togi::baseline::save_baseline_from_report(&report, &project_root_ref)?;
        debug_assert!(saved.is_some(), "eligible report must save a baseline");
        eprintln!("Baseline saved to .togi-baseline");
    } else if check_baseline && !loaded_baseline {
        eprintln!("warning: no baseline found — use --save-baseline first");
    }
    if let Some(path) = &pr_comment {
        togi::report::write_pr_comment_with_baseline(
            &report,
            path,
            baseline_score,
            survivor_comparison.as_ref(),
        )?;
        eprintln!("PR comment written to {}", path.display());
    }

    if should_fail {
        exit_with(_lock, 1);
    } else if let Some(threshold) = fail_under {
        let gate_score = togi::report::fail_under_score(&report);
        if gate_score < threshold {
            eprintln!(
                "Fail-under gate score {gate_score:.1}% is below --fail-under threshold {threshold:.1}% (displayed mutation score is fresh-only)."
            );
            exit_with(_lock, 1);
        }
    } else if has_fresh_timeout_or_build_error(&report) || (report.survived > 0 && !loaded_baseline)
    {
        exit_with(_lock, 1);
    }

    Ok(())
}

fn has_fresh_timeout_or_build_error(report: &togi::MutationReport) -> bool {
    report.results.iter().any(|(mutation, result)| {
        matches!(
            *result,
            togi::MutationResult::Timeout | togi::MutationResult::BuildError
        ) && !report.execution_for(mutation.id, *result).is_reused()
    })
}

fn exit_with(lock: togi::lock::LockGuard, code: i32) -> ! {
    drop(lock);
    process::exit(code);
}

fn resolve_config(cfg: togi::cli::CheckArgs) -> anyhow::Result<ResolvedCheckConfig> {
    let mut config = togi::config::Config::load(cfg.config.as_deref())?;
    let has_custom_test_cmd = cfg.test_cmd.is_some();
    let has_cli_timeout = cfg.timeout.is_some();
    let profile = cfg.profile.or(config.test.profile);

    if let Some(b) = cfg.base {
        config.diff.base = b;
    }
    if let Some(profile) = profile {
        if cfg.jobs.is_none() && !config.test.jobs_was_explicit() {
            config.test.jobs = profile.default_jobs();
        }
    }
    if let Some(j) = cfg.jobs {
        config.test.jobs = j;
    }
    if let Some(t) = cfg.timeout {
        config.test.timeout = t;
    }
    if cfg.calibrate_timeout {
        config.test.calibrate_timeout = true;
    }
    if cfg.skip_baseline_timing {
        config.test.calibrate_timeout = false;
    }
    if let Some(multiplier) = cfg.timeout_multiplier {
        config.test.timeout_multiplier = multiplier;
    }
    if let Some(slack) = cfg.timeout_slack {
        config.test.timeout_slack = slack;
    }
    validate_timeout_calibration(config.test.timeout_multiplier)?;
    if has_cli_timeout && config.test.calibrate_timeout {
        eprintln!("warning: --timeout overrides baseline timing calibration for this run");
        config.test.calibrate_timeout = false;
    }
    if let Some(max) = cfg.max_per_run {
        config.mutations.max_per_run = max;
    }
    if cfg.schemata {
        config.mutations.schemata = true;
    }
    if cfg.no_schemata {
        config.mutations.schemata = false;
    }
    if let Some(cmd) = cfg.test_cmd {
        config.test.command =
            shell_words::split(&cmd).map_err(|e| anyhow::anyhow!("bad --test-cmd: {e}"))?;
    }
    if let Some(mode) = cfg.coverage {
        config.mutations.coverage = Some(mode);
    }
    if let Some(path) = cfg.coverage_file {
        config.mutations.coverage_file = Some(path);
    }
    if let Some(cmd) = cfg.coverage_cmd {
        config.mutations.coverage_command =
            shell_words::split(&cmd).map_err(|e| anyhow::anyhow!("bad --coverage-cmd: {e}"))?;
    }
    if let Some(value) = cfg.min_line_coverage {
        validate_coverage_percentage(value, "--min-line-coverage")?;
        config.mutations.min_line_coverage = Some(value);
    }
    if let Some(value) = cfg.min_diff_coverage {
        validate_coverage_percentage(value, "--min-diff-coverage")?;
        config.mutations.min_diff_coverage = Some(value);
    }
    if cfg.fail_on_uncovered_diff {
        config.mutations.fail_on_uncovered_diff = true;
    }
    if let Some(path) = cfg.test_selection_file {
        config.mutations.test_selection_file = Some(path);
    }
    if cfg.no_incremental_history {
        config.mutations.incremental_history = false;
    }
    if let Some(cmd) = cfg.build_cmd {
        config.test.set_build_command(
            shell_words::split(&cmd).map_err(|e| anyhow::anyhow!("bad --build-cmd: {e}"))?,
        );
    }

    if cfg.no_skip_defaults {
        config.mutations.skip_noisy_files = false;
    }
    if let Some(ops) = cfg.operators {
        config.mutations.operators = ops;
    }

    if config.mutations.coverage.is_some() && !config.mutations.coverage_command.is_empty() {
        anyhow::bail!("choose either built-in coverage collection or coverage_command, not both");
    }
    if !config.mutations.coverage_command.is_empty() && config.mutations.coverage_file.is_none() {
        anyhow::bail!(
            "coverage collection command requires an LCOV output path; set [mutations] coverage_file or --coverage-file"
        );
    }
    if (config.mutations.min_line_coverage.is_some()
        || config.mutations.min_diff_coverage.is_some()
        || config.mutations.fail_on_uncovered_diff)
        && config.mutations.coverage_file.is_none()
        && config.mutations.coverage.is_none()
        && config.mutations.coverage_command.is_empty()
    {
        anyhow::bail!(
            "coverage gates require a coverage source; set [mutations] coverage_file, coverage_command, coverage = \"auto\", or use the corresponding CLI flags"
        );
    }

    let profile_fail_fast = profile.is_some_and(|profile| profile.default_fail_fast());
    let requested_fail_fast = cfg.fail_fast || profile_fail_fast;
    let fail_fast = requested_fail_fast && !has_custom_test_cmd;
    if cfg.fail_fast && has_custom_test_cmd {
        eprintln!(
            "warning: --fail-fast is ignored when --test-cmd is set; include fail-fast flags in the custom command"
        );
    } else if profile_fail_fast && has_custom_test_cmd {
        eprintln!(
            "warning: --profile cool fail-fast default is ignored when --test-cmd is set; include fail-fast flags in the custom command"
        );
    }
    Ok(ResolvedCheckConfig {
        config,
        fail_fast,
        has_custom_test_cmd,
        has_cli_timeout,
        profile,
    })
}

fn validate_coverage_percentage(value: f64, flag: &str) -> anyhow::Result<()> {
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        anyhow::bail!("{flag} must be a finite percentage between 0 and 100");
    }
    Ok(())
}

fn resolve_coverage_stats(
    config: &togi::config::Config,
    project_root: &Path,
    coverage_gate_active: bool,
) -> anyhow::Result<Option<togi::coverage::CoverageStats>> {
    if let Some(mode) = config.mutations.coverage {
        let output_path = config
            .mutations
            .coverage_file
            .as_ref()
            .map(|path| resolve_coverage_path(path, project_root));
        return collect_builtin_coverage_stats(mode, project_root, output_path.as_deref())
            .map(Some);
    }

    if !config.mutations.coverage_command.is_empty() {
        let coverage_file = config
            .mutations
            .coverage_file
            .as_ref()
            .expect("coverage command should be validated to require coverage_file");
        let resolved_cov_path = resolve_coverage_path(coverage_file, project_root);
        run_coverage_command(
            &config.mutations.coverage_command,
            &resolved_cov_path,
            project_root,
        )?;
    }

    let Some(cov_path) = config.mutations.coverage_file.as_ref() else {
        return Ok(None);
    };
    let resolved_cov_path = resolve_coverage_path(cov_path, project_root);
    let coverage_required = coverage_gate_active || !config.mutations.coverage_command.is_empty();

    match std::fs::read_to_string(&resolved_cov_path) {
        Ok(cov_content) => Ok(Some(togi::coverage::parse_lcov_stats(
            &cov_content,
            project_root,
        ))),
        Err(e) => {
            if coverage_required {
                return Err(anyhow::anyhow!(
                    "could not read coverage file {}: {e}",
                    resolved_cov_path.display()
                ));
            }
            eprintln!(
                "warning: could not read coverage file {}: {e} — running all mutations",
                resolved_cov_path.display()
            );
            Ok(None)
        }
    }
}

fn resolve_coverage_path(path: &Path, project_root: &Path) -> PathBuf {
    if path.is_relative() {
        project_root.join(path)
    } else {
        path.to_path_buf()
    }
}

fn run_coverage_command(
    command: &[String],
    coverage_file: &Path,
    project_root: &Path,
) -> anyhow::Result<()> {
    let Some(program) = command.first() else {
        anyhow::bail!("coverage command is empty");
    };
    if let Some(parent) = coverage_file.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "could not create parent directory for coverage file {}",
                coverage_file.display()
            )
        })?;
    }

    // foxguard: ignore[rs/no-command-injection]
    // The coverage command is explicit user configuration and is executed
    // directly as argv without a shell.
    let output = std::process::Command::new(program)
        .args(&command[1..])
        .current_dir(project_root)
        .env("TOGI_COVERAGE_FILE", coverage_file)
        .output()
        .with_context(|| format!("failed to run coverage command `{}`", command.join(" ")))?;

    if !output.status.success() {
        anyhow::bail!(
            "coverage command `{}` failed with status {}.\nstdout:\n{}\nstderr:\n{}",
            command.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    if !coverage_file.is_file() {
        anyhow::bail!(
            "coverage command `{}` completed but did not produce {}",
            command.join(" "),
            coverage_file.display()
        );
    }

    Ok(())
}

fn collect_builtin_coverage_stats(
    mode: togi::config::CoverageMode,
    project_root: &Path,
    output_path: Option<&Path>,
) -> anyhow::Result<togi::coverage::CoverageStats> {
    match mode {
        togi::config::CoverageMode::Auto => collect_auto_coverage_stats(project_root, output_path),
    }
}

fn collect_auto_coverage_stats(
    project_root: &Path,
    output_path: Option<&Path>,
) -> anyhow::Result<togi::coverage::CoverageStats> {
    match togi::config::detect_builtin_coverage_adapter(project_root) {
        Some(togi::config::BuiltinCoverageAdapter::Go) => {
            collect_go_builtin_coverage_stats(project_root, output_path)
        }
        None => anyhow::bail!(
            "built-in coverage auto is not supported for this project yet. Supported ecosystems: Go. Use --coverage-cmd ... plus --coverage-file ... for other projects."
        ),
    }
}

fn collect_go_builtin_coverage_stats(
    project_root: &Path,
    output_path: Option<&Path>,
) -> anyhow::Result<togi::coverage::CoverageStats> {
    let module_path = go_module_path(project_root)?;
    let tempdir = tempfile::tempdir()?;
    let profile_path = tempdir.path().join("coverage.out");
    let output = std::process::Command::new("go")
        .arg("test")
        .arg("./...")
        .arg("-coverpkg")
        .arg("./...")
        .arg("-coverprofile")
        .arg(&profile_path)
        .current_dir(project_root)
        .output()
        .context("failed to run built-in Go coverage collection")?;
    if !output.status.success() {
        anyhow::bail!(
            "built-in Go coverage collection failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let profile = std::fs::read_to_string(&profile_path)
        .context("could not read Go coverage profile produced by built-in coverage collection")?;
    let stats = parse_go_coverprofile_stats(project_root, project_root, &module_path, &profile)?;

    if let Some(output_path) = output_path {
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "could not create parent directory for coverage file {}",
                    output_path.display()
                )
            })?;
        }
        std::fs::write(output_path, togi::coverage::stats_to_lcov(&stats)).with_context(|| {
            format!(
                "could not write generated coverage file {}",
                output_path.display()
            )
        })?;
    }

    Ok(stats)
}

fn parse_go_coverprofile_stats(
    repo_root: &Path,
    module_root: &Path,
    module_path: &str,
    profile: &str,
) -> anyhow::Result<togi::coverage::CoverageStats> {
    let mut covered_lines = togi::coverage::CoverageMap::new();
    let mut total_lines = togi::coverage::CoverageMap::new();

    for line in profile.lines().skip(1) {
        let Some((location, fields)) = line.split_once(' ') else {
            continue;
        };
        let fields: Vec<&str> = fields.split_whitespace().collect();
        if fields.len() < 2 {
            continue;
        }

        let execution_count = fields[1]
            .parse::<u64>()
            .with_context(|| format!("invalid Go coverage execution count in `{line}`"))?;
        let Some((file, range)) = location.rsplit_once(':') else {
            continue;
        };
        let Some((start, end)) = range.split_once(',') else {
            continue;
        };
        let start_line = parse_go_cover_line(start)?;
        let end_line = parse_go_cover_line(end)?;
        let file = PathBuf::from(normalize_go_cover_file(
            repo_root,
            module_root,
            module_path,
            file,
        ));

        for line_no in start_line..=end_line {
            total_lines.entry(file.clone()).or_default().insert(line_no);
            if execution_count > 0 {
                covered_lines
                    .entry(file.clone())
                    .or_default()
                    .insert(line_no);
            }
        }
    }

    Ok(togi::coverage::CoverageStats {
        covered_lines,
        total_lines,
    })
}

fn warn_if_resource_oversubscribed(jobs: usize) {
    if let Ok(available) = std::thread::available_parallelism() {
        if jobs > available.get() {
            eprintln!(
                "warning: {jobs} togi jobs exceed available parallelism ({}); test runners may oversubscribe CPUs",
                available.get()
            );
        }
    }
}

fn validate_timeout_calibration(multiplier: f64) -> anyhow::Result<()> {
    if !multiplier.is_finite() || multiplier <= 0.0 {
        anyhow::bail!("timeout_multiplier must be a positive finite number");
    }
    Ok(())
}

fn calibration_measurement(
    measurement: &togi::runner::BaselineHealthMeasurement,
) -> Option<&togi::runner::BaselineSuiteMeasurement> {
    measurement
        .suites
        .iter()
        .filter(|suite| suite.uses_default_timeout)
        .max_by_key(|suite| {
            suite
                .build_duration
                .filter(|duration| *duration > suite.test_duration)
                .unwrap_or(suite.test_duration)
        })
}

fn calibrated_timeout_seconds(
    build_duration: Option<Duration>,
    test_duration: Duration,
    multiplier: f64,
    slack: u64,
) -> u64 {
    let baseline = build_duration
        .filter(|duration| *duration > test_duration)
        .unwrap_or(test_duration);
    let seconds = baseline.as_secs_f64() * multiplier + slack as f64;
    seconds.ceil().max(1.0).min(u64::MAX as f64) as u64
}

fn baseline_timing_summary(timing: &BaselineTiming) -> String {
    let build = timing
        .build_duration
        .map(|duration| format!(", build {:.2}s", duration.as_secs_f64()))
        .unwrap_or_default();
    format!(
        "Baseline timing: test {:.2}s{build}; mutation timeout {:.2}s",
        timing.test_duration.as_secs_f64(),
        timing.calibrated_timeout.as_secs_f64()
    )
}

fn baseline_measurement_timeout(configured_timeout_seconds: u64) -> Duration {
    Duration::from_secs(configured_timeout_seconds.saturating_mul(10).max(60))
}

fn resource_profile_env(
    profile: togi::config::ResourceProfile,
    config: &togi::config::Config,
) -> HashMap<String, String> {
    let mut commands: Vec<&[String]> = Vec::new();
    commands.push(&config.test.command);
    commands.extend(
        config
            .test
            .languages
            .values()
            .map(|lang_config| lang_config.command.as_slice()),
    );
    commands.extend(config.projects.values().filter_map(|project| {
        project
            .test
            .as_ref()
            .and_then(|test| test.command.as_deref())
    }));
    resource_profile_env_for_commands(profile, &commands, |name| std::env::var_os(name).is_some())
}

fn resource_profile_env_for_commands(
    profile: togi::config::ResourceProfile,
    commands: &[&[String]],
    env_exists: impl Fn(&str) -> bool,
) -> HashMap<String, String> {
    let mut env = HashMap::new();
    if profile != togi::config::ResourceProfile::Cool {
        return env;
    }

    let has_runner = |runner: &str| {
        commands
            .iter()
            .filter_map(|command| command.first())
            .any(|program| program == runner)
    };
    let mut set_if_missing = |key: &str, value: &str| {
        if !env_exists(key) {
            env.insert(key.to_string(), value.to_string());
        }
    };

    if has_runner("cargo") {
        set_if_missing("CARGO_BUILD_JOBS", "1");
        set_if_missing("RUST_TEST_THREADS", "1");
    }
    if has_runner("go") {
        set_if_missing("GOMAXPROCS", "1");
    }
    if has_runner("pytest") {
        set_if_missing("PYTEST_XDIST_AUTO_NUM_WORKERS", "1");
    }

    env
}

/// Parse a shard spec like "1/4" into (k, n) where k is 1-indexed.
fn parse_shard(s: &str) -> anyhow::Result<(usize, usize)> {
    let parts: Vec<&str> = s.split('/').collect();
    if parts.len() != 2 {
        anyhow::bail!("invalid --shard format '{s}', expected k/n (e.g. 1/4)");
    }
    let k: usize = parts[0]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid shard index '{}'", parts[0]))?;
    let n: usize = parts[1]
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid shard count '{}'", parts[1]))?;
    if n == 0 {
        anyhow::bail!("invalid shard count: n must be >= 1 for --shard {s}");
    }
    if k == 0 || k > n {
        anyhow::bail!("--shard {s}: k must be 1..={n}");
    }
    Ok((k, n))
}

/// Collects files to mutate. Returns an empty vec with user-facing messages
/// when there's nothing to do.
fn collect_files(
    config: &togi::config::Config,
    all: bool,
    paths: &[PathBuf],
    json_output: bool,
    project_root: &Path,
) -> anyhow::Result<Vec<ChangedFile>> {
    let skip_noisy = config.mutations.skip_noisy_files;
    let exclude_globs = &config.mutations.exclude_paths;

    if all {
        let mut files =
            togi::diff::collect_all_supported_files(project_root, skip_noisy, exclude_globs)?;
        if !paths.is_empty() {
            files.retain(|f| paths.iter().any(|p| f.path.starts_with(p)));
        }
        if files.is_empty() {
            if json_output {
                eprintln!("No supported source files found. Nothing to mutate.");
            } else {
                println!("No supported source files found. Nothing to mutate.");
            }
            return Ok(vec![]);
        }
        if json_output {
            eprintln!("Scanning all {} supported files...", files.len());
        } else {
            println!("Scanning all {} supported files...", files.len());
        }
        return Ok(files);
    }

    let diff_output = get_git_diff(&config.diff.base)?;
    if diff_output.is_empty() {
        if json_output {
            eprintln!(
                "No changes found in diff against `{}`. The selected diff is empty; that is normal for a clean clone, not an error.\n\
Preview all supported files without tests (bounded):\n\
  togi check --all --dry-run --max-per-run 10\n\
After a supported source change, run `togi check`, or `togi check --base <ref>` for another intended base.\n\
`--all` and `--base` are alternatives; do not combine them.\n\
If command detection is ambiguous, run `togi init` from the repository root.",
                config.diff.base
            );
        } else {
            println!(
                "No changes found in diff against `{}`. The selected diff is empty; that is normal for a clean clone, not an error.\n\
Preview all supported files without tests (bounded):\n\
  togi check --all --dry-run --max-per-run 10\n\
After a supported source change, run `togi check`, or `togi check --base <ref>` for another intended base.\n\
`--all` and `--base` are alternatives; do not combine them.\n\
If command detection is ambiguous, run `togi init` from the repository root.",
                config.diff.base
            );
        }
        return Ok(vec![]);
    }

    let mut files = togi::diff::parse_diff_bytes(&diff_output);
    if skip_noisy {
        files.retain(|f| !togi::diff::is_noisy_file(&f.path));
    }
    files.retain(|f| !togi::diff::matches_user_excludes(&f.path, exclude_globs));
    if files.is_empty() {
        if json_output {
            eprintln!("No added/modified lines found. Nothing to mutate.");
        } else {
            println!("No added/modified lines found. Nothing to mutate.");
        }
        return Ok(vec![]);
    }
    Ok(files)
}

fn generate_mutations(
    changed_files: &[ChangedFile],
    config: &togi::config::Config,
    project_root: &Path,
) -> anyhow::Result<Vec<Mutation>> {
    let max = if config.mutations.max_per_run == 0 {
        usize::MAX
    } else {
        config.mutations.max_per_run
    };
    let generation_limit = if config.mutations.coverage_file.is_some() {
        usize::MAX
    } else if !config.test.build_command.is_empty() {
        max.saturating_mul(2)
    } else {
        max
    };
    togi::mutator::generate_mutations(
        changed_files,
        project_root,
        generation_limit,
        config.mutations.max_per_file,
        &config.mutations.operators,
    )
}

/// Mutations partitioned for the run: `to_run` executes against the test
/// suite; `uncovered` sits on lines with known zero coverage and is reported
/// as `uncovered` without being executed.
struct ClassifiedMutations {
    to_run: Vec<Mutation>,
    uncovered: Vec<Mutation>,
}

fn filter_mutations(
    mutations: Vec<Mutation>,
    config: &togi::config::Config,
    project_root: &Path,
    coverage_stats: Option<&togi::coverage::CoverageStats>,
) -> anyhow::Result<ClassifiedMutations> {
    let mut classified = if let Some(coverage) = coverage_stats {
        split_by_coverage(mutations, coverage, project_root)
    } else if let Some(ref cov_path) = config.mutations.coverage_file {
        let resolved_cov_path = if std::path::Path::new(cov_path).is_relative() {
            project_root.join(cov_path)
        } else {
            PathBuf::from(cov_path)
        };
        match std::fs::read_to_string(&resolved_cov_path) {
            Ok(cov_content) => {
                let coverage = togi::coverage::parse_lcov_stats(&cov_content, project_root);
                split_by_coverage(mutations, &coverage, project_root)
            }
            Err(e) => {
                eprintln!(
                    "warning: could not read coverage file {}: {e} — running all mutations",
                    resolved_cov_path.display()
                );
                ClassifiedMutations {
                    to_run: mutations,
                    uncovered: Vec::new(),
                }
            }
        }
    } else {
        ClassifiedMutations {
            to_run: mutations,
            uncovered: Vec::new(),
        }
    };
    if config.mutations.max_per_run > 0 && classified.to_run.len() > config.mutations.max_per_run {
        eprintln!(
            "warning: mutation count capped at max_per_run ({}). Increase in togi.toml or use --dry-run to preview.",
            config.mutations.max_per_run
        );
        classified.to_run.truncate(config.mutations.max_per_run);
    }

    Ok(classified)
}

fn split_by_coverage(
    mutations: Vec<Mutation>,
    coverage: &togi::coverage::CoverageStats,
    project_root: &Path,
) -> ClassifiedMutations {
    let before = mutations.len();
    let classified = togi::coverage::classify_by_coverage(mutations, coverage, project_root);
    let covered = classified.covered.len();
    if covered < before {
        eprintln!("Coverage: {covered} of {before} mutations on covered lines");
        let uncovered = classified.uncovered.len();
        if uncovered > 0 {
            eprintln!(
                "Coverage: {uncovered} mutant{} on zero-coverage lines will be reported as uncovered (not executed)",
                if uncovered == 1 { "" } else { "s" }
            );
        }
    }
    ClassifiedMutations {
        to_run: classified.covered,
        uncovered: classified.uncovered,
    }
}

/// Merge coverage-suppressed mutants into the report as `Uncovered`.
///
/// They count toward `total`/`planned_total` (they were part of the scheduled
/// work) but stay out of the tested denominator; see
/// `MutationReport::tested_count`.
fn merge_uncovered(report: &mut togi::MutationReport, uncovered: Vec<Mutation>) {
    let n = uncovered.len();
    if n == 0 {
        return;
    }
    report.results.extend(
        uncovered
            .into_iter()
            .map(|m| (m, togi::MutationResult::Uncovered)),
    );
    report.total += n;
    report.planned_total += n;
}

/// Build an empty execution report for runs where every generated mutant was
/// coverage-suppressed and nothing had to be executed.
fn coverage_only_report(
    config: &togi::config::Config,
    build_command_origin: togi::config::BuildCommandOrigin,
) -> togi::MutationReport {
    togi::MutationReport {
        results: Vec::new(),
        execution_provenance: BTreeMap::new(),
        selection_provenance: BTreeMap::new(),
        build_error_diagnostics: Vec::new(),
        schemata: None,
        baseline_timing: None,
        duration: Duration::ZERO,
        test_command: Some(config.test.command.clone()),
        build_command: if build_command_origin.runs_before_tests() {
            config.test.build_command.clone()
        } else {
            Vec::new()
        },
        planned_total: 0,
        early_stop_reason: None,
        total: 0,
        killed: 0,
        survived: 0,
        timeout: 0,
        build_errors: 0,
    }
}

fn emit_empty_json_check_output(
    config: &togi::config::Config,
    build_command_origin: togi::config::BuildCommandOrigin,
    dry_run: bool,
    project_root: &Path,
    print_stdout: bool,
    json_report_path: Option<&Path>,
) -> anyhow::Result<()> {
    if dry_run {
        if print_stdout {
            togi::report::json::print_dry_run(&[])?;
        }
    } else {
        let report = coverage_only_report(config, build_command_origin);
        let mut capture = togi::replay::ReplayReportCapture::capture(project_root, &[]);
        capture.revalidate(project_root);
        let json = togi::report::json::to_json_string_with_baseline_and_replay(
            &report,
            None,
            &capture,
            &BTreeMap::new(),
        )?;
        if let Some(path) = json_report_path {
            write_json_report(path, &json)?;
        }
        if print_stdout {
            println!("{json}");
        }
    }
    Ok(())
}

fn validate_json_report_destinations(
    json_report_path: Option<&Path>,
    output_format: togi::cli::OutputFormat,
    save_baseline: bool,
    pr_comment: Option<&Path>,
    project_root: &Path,
) -> anyhow::Result<()> {
    let Some(json_report_path) = json_report_path else {
        return Ok(());
    };
    let current_dir = std::env::current_dir().context("could not determine current directory")?;
    let json_report_identity = destination_identity(json_report_path, &current_dir)?;
    let mut destinations = Vec::with_capacity(3);

    if output_format == togi::cli::OutputFormat::Html {
        destinations.push(("HTML report", PathBuf::from("togi-report.html")));
    }
    if save_baseline {
        destinations.push(("baseline", project_root.join(".togi-baseline")));
    }
    if let Some(path) = pr_comment {
        destinations.push(("PR comment", path.to_path_buf()));
    }

    for (name, path) in destinations {
        if same_output_destination(
            &json_report_identity,
            &destination_identity(&path, &current_dir)?,
        ) {
            anyhow::bail!(
                "--json-report {} conflicts with the {name} destination {}",
                json_report_path.display(),
                path.display()
            );
        }
    }
    Ok(())
}

fn destination_identity(path: &Path, current_dir: &Path) -> anyhow::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    };
    let (root, components) = absolute_output_destination_components(&absolute)?;
    resolve_output_destination(root, components)
}

fn absolute_output_destination_components(
    path: &Path,
) -> anyhow::Result<(PathBuf, VecDeque<OsString>)> {
    let mut root = PathBuf::new();
    let mut components = VecDeque::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => root.push(prefix.as_os_str()),
            Component::RootDir => root.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir | Component::Normal(_) => {
                components.push_back(component.as_os_str().to_os_string());
            }
        }
    }
    if !root.has_root() {
        anyhow::bail!("output destination must be absolute: {}", path.display());
    }
    Ok((root, components))
}

fn relative_output_destination_components(path: &Path) -> anyhow::Result<VecDeque<OsString>> {
    let mut components = VecDeque::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir | Component::Normal(_) => {
                components.push_back(component.as_os_str().to_os_string());
            }
            Component::Prefix(_) | Component::RootDir => {
                anyhow::bail!(
                    "could not safely resolve output destination symlink target {}",
                    path.display()
                );
            }
        }
    }
    Ok(components)
}

fn resolve_output_destination(
    mut resolved: PathBuf,
    mut components: VecDeque<OsString>,
) -> anyhow::Result<PathBuf> {
    let mut symlink_depth = 0;
    while let Some(component) = components.pop_front() {
        if component == OsStr::new("..") {
            resolved.pop();
            continue;
        }

        let candidate = resolved.join(&component);
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                symlink_depth += 1;
                if symlink_depth > 40 {
                    anyhow::bail!(
                        "could not safely resolve output destination {}: too many symlinks",
                        candidate.display()
                    );
                }

                let target = std::fs::read_link(&candidate).with_context(|| {
                    format!(
                        "could not read output destination symlink {}",
                        candidate.display()
                    )
                })?;
                let mut target_components = if target.is_absolute() {
                    let (target_root, components) =
                        absolute_output_destination_components(&target)?;
                    resolved = target_root;
                    components
                } else {
                    relative_output_destination_components(&target)?
                };
                target_components.append(&mut components);
                components = target_components;
            }
            Ok(_) => resolved.push(component),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Queues retain only normal names and `..`; after a missing
                // component, `..` would be ambiguous if the campaign creates
                // that component before publishing an output.
                if components
                    .iter()
                    .any(|remaining| remaining == OsStr::new(".."))
                {
                    anyhow::bail!(
                        "could not safely resolve output destination {}: missing intermediate component",
                        candidate.display()
                    );
                }
                resolved.push(component);
                resolved.extend(components);
                return Ok(resolved);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "could not resolve output destination component {}",
                        candidate.display()
                    )
                });
            }
        }
    }
    Ok(resolved)
}

/// Deliberately reject case-only aliases even on case-sensitive volumes so an
/// Action cannot overwrite a sidecar when a checkout runs on case-insensitive
/// APFS or Windows. Valid UTF-8 paths use UAX canonical caseless matching;
/// non-UTF-8 paths retain exact PathBuf equality and are never lossy-normalized.
fn same_output_destination(left: &Path, right: &Path) -> bool {
    left == right
        || left
            .to_str()
            .zip(right.to_str())
            .is_some_and(|(left, right)| caseless::canonical_caseless_match_str(left, right))
}

fn write_json_report(path: &Path, json: &str) -> anyhow::Result<()> {
    publish_json_report(path, |staged| {
        staged.write_all(json.as_bytes())?;
        staged.write_all(b"\n")
    })
}

fn publish_json_report(
    path: &Path,
    write_contents: impl FnOnce(&mut tempfile::NamedTempFile) -> std::io::Result<()>,
) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut staged = tempfile::NamedTempFile::new_in(parent).with_context(|| {
        format!(
            "could not create JSON report staging file in {}",
            parent.display()
        )
    })?;
    write_contents(&mut staged)
        .with_context(|| format!("could not write JSON report {}", path.display()))?;
    staged
        .flush()
        .with_context(|| format!("could not flush JSON report {}", path.display()))?;
    staged
        .as_file()
        .sync_all()
        .with_context(|| format!("could not sync JSON report {}", path.display()))?;
    staged
        .persist(path)
        .map(|_| ())
        .map_err(|error| error.error)
        .with_context(|| format!("could not publish JSON report {}", path.display()))
}

fn print_dry_run(mutations: &[Mutation]) {
    println!(
        "Dry run — {} mutations would be generated:",
        mutations.len()
    );
    for m in mutations {
        println!(
            "  [{}] {}:{} — {}: {} → {}",
            m.id + 1,
            m.file.display(),
            m.line,
            m.operator,
            m.original,
            m.replacement
        );
    }
}

fn command_config(
    config: &togi::config::Config,
    project_root: &Path,
    build_command_origin: togi::config::BuildCommandOrigin,
    force_default_command: bool,
    force_default_timeout: bool,
) -> togi::runner::CommandConfig {
    let language_commands = config
        .test
        .languages
        .iter()
        .map(|(language, language_config)| (language.clone(), language_config.command.clone()))
        .collect();
    let language_timeouts = config
        .test
        .languages
        .iter()
        .filter_map(|(language, language_config)| {
            language_config
                .timeout
                .map(|timeout| (language.clone(), Duration::from_secs(timeout)))
        })
        .collect();
    let project_commands = config
        .projects
        .values()
        .map(|project| {
            let (command, timeout) = project
                .test
                .as_ref()
                .map(|test| (test.command.clone(), test.timeout))
                .unwrap_or((None, None));
            togi::runner::ProjectCommandConfig {
                path: project.path.clone(),
                command,
                timeout: timeout.map(Duration::from_secs),
            }
        })
        .collect();

    togi::runner::CommandConfig {
        command: config.test.command.clone(),
        sandbox_command: config.test.sandbox_command.clone(),
        force_default_command,
        force_default_timeout,
        project_commands,
        language_commands,
        build_command: config.test.build_command.clone(),
        build_command_origin,
        timeout: Duration::from_secs(config.test.timeout),
        language_timeouts,
        test_selection: load_test_selection(
            config.mutations.test_selection_file.as_deref(),
            project_root,
        ),
    }
}

fn execute(
    mutations: Vec<Mutation>,
    config: togi::config::Config,
    commands: togi::runner::CommandConfig,
    project_root: PathBuf,
    options: ExecuteOptions,
) -> togi::runner::RunOutcome {
    let use_schemata = config.mutations.schemata;

    let runner = togi::runner::TestRunner {
        commands,
        parallelism: config.test.jobs,
        project_root,
        verbose: options.verbose,
        show_output: options.show_output,
        max_tested: if config.mutations.max_per_run == 0 {
            None
        } else {
            Some(config.mutations.max_per_run)
        },
        early_stop: options.early_stop,
        respect_workspace_ignores: config.mutations.respect_workspace_ignores,
        env: options.env,
        incremental_history: config.mutations.incremental_history,
        force_rerun: options.force_rerun,
        learned_selection: options.learned_selection,
        cancelled: options.cancelled,
    };

    if use_schemata {
        runner.run_with_schemata(mutations)
    } else {
        runner.run(mutations)
    }
}

fn load_test_selection(
    path: Option<&Path>,
    project_root: &Path,
) -> Option<togi::runner::TestSelectionConfig> {
    let path = path?;
    let resolved_path = if path.is_relative() {
        project_root.join(path)
    } else {
        path.to_path_buf()
    };

    match std::fs::read_to_string(&resolved_path)
        .with_context(|| {
            format!(
                "could not read test selection file {}",
                resolved_path.display()
            )
        })
        .and_then(|content| parse_test_selection_json(&content, project_root))
    {
        Ok(selection) => Some(selection),
        Err(e) => {
            eprintln!("warning: {e:#} — running full test commands");
            None
        }
    }
}

fn parse_test_selection_json(
    content: &str,
    project_root: &Path,
) -> anyhow::Result<togi::runner::TestSelectionConfig> {
    let raw: std::collections::HashMap<
        String,
        std::collections::HashMap<String, Vec<RawSelectedTest>>,
    > = serde_json::from_str(content).context("could not parse test selection JSON")?;
    let mut selection = togi::runner::TestSelectionConfig::new();

    for (file, lines) in raw {
        for (line, raw_tests) in lines {
            let line = line
                .parse::<usize>()
                .with_context(|| format!("invalid line number '{line}' for {file}"))?;
            if line == 0 {
                anyhow::bail!("invalid line number '0' for {file}");
            }
            let tests = raw_tests
                .into_iter()
                .map(RawSelectedTest::into_selected)
                .collect::<anyhow::Result<Vec<_>>>()
                .with_context(|| format!("invalid test selection entry for {file}:{line}"))?;
            selection.insert_tests(project_root, Path::new(&file), line, tests);
        }
    }

    Ok(selection)
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawSelectedTest {
    Name(String),
    Timed {
        name: String,
        #[serde(default)]
        duration_ms: Option<u64>,
    },
}

impl RawSelectedTest {
    fn into_selected(self) -> anyhow::Result<togi::runner::SelectedTest> {
        match self {
            Self::Name(name) => selected_test(name, None),
            Self::Timed { name, duration_ms } => selected_test(name, duration_ms),
        }
    }
}

fn selected_test(
    name: String,
    duration_ms: Option<u64>,
) -> anyhow::Result<togi::runner::SelectedTest> {
    if name.trim().is_empty() {
        anyhow::bail!("test name cannot be empty");
    }
    Ok(togi::runner::SelectedTest::new(name, duration_ms))
}

type TestSelectionJson = BTreeMap<String, BTreeMap<String, Vec<String>>>;

fn run_test_map(
    path: Option<PathBuf>,
    output: PathBuf,
    cancelled: &AtomicBool,
) -> anyhow::Result<()> {
    let module_root = match path {
        Some(path) => path,
        None => get_project_root()?,
    }
    .canonicalize()
    .context("could not resolve test-map path")?;
    let repo_root = git_root_for_path(&module_root).unwrap_or_else(|_| module_root.clone());
    let map = generate_go_test_selection_map(&module_root, &repo_root, cancelled)?;
    let output_path = if output.is_relative() {
        module_root.join(output)
    } else {
        output
    };

    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create {}", parent.display()))?;
    }
    let json = serde_json::to_string_pretty(&map)?;
    std::fs::write(&output_path, format!("{json}\n"))
        .with_context(|| format!("could not write {}", output_path.display()))?;
    println!(
        "Wrote test selection map for {} source files to {}",
        map.len(),
        output_path.display()
    );
    Ok(())
}

fn generate_go_test_selection_map(
    module_root: &Path,
    repo_root: &Path,
    cancelled: &AtomicBool,
) -> anyhow::Result<TestSelectionJson> {
    let module_path = go_module_path(module_root)?;
    let tests = go_test_names(module_root)?;
    let mut map = TestSelectionJson::new();

    for test in tests {
        if cancelled.load(Ordering::SeqCst) {
            anyhow::bail!("interrupted");
        }
        let profile = run_go_test_coverage(module_root, &test, cancelled)?;
        add_go_coverage_to_selection_map(
            &mut map,
            repo_root,
            module_root,
            &module_path,
            &profile,
            &test,
        )?;
    }

    Ok(map)
}

fn git_root_for_path(path: &Path) -> anyhow::Result<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| format!("failed to find git root for {}", path.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "could not find git root for {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(PathBuf::from(String::from_utf8(output.stdout)?.trim()))
}

fn go_module_path(project_root: &Path) -> anyhow::Result<String> {
    let output = std::process::Command::new("go")
        .args(["list", "-m"])
        .current_dir(project_root)
        .output()
        .context("failed to run go list -m")?;
    if !output.status.success() {
        anyhow::bail!(
            "go list -m failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_string())
}

fn go_test_names(project_root: &Path) -> anyhow::Result<Vec<String>> {
    let output = std::process::Command::new("go")
        .args(["test", "-list", ".", "./..."])
        .current_dir(project_root)
        .output()
        .context("failed to list Go tests")?;
    if !output.status.success() {
        anyhow::bail!(
            "go test -list failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    Ok(unique_go_test_names(
        String::from_utf8(output.stdout)?
            .lines()
            .filter(|line| line.starts_with("Test"))
            .map(str::to_string),
    ))
}

fn unique_go_test_names(names: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut unique = Vec::new();
    for name in names {
        if seen.insert(name.clone()) {
            unique.push(name);
        }
    }
    unique
}

fn run_go_test_coverage(
    project_root: &Path,
    test: &str,
    cancelled: &AtomicBool,
) -> anyhow::Result<String> {
    if cancelled.load(Ordering::SeqCst) {
        anyhow::bail!("interrupted");
    }
    let tempdir = tempfile::tempdir()?;
    let profile_path = tempdir.path().join("coverage.out");
    let output = std::process::Command::new("go")
        .arg("test")
        .arg("./...")
        .arg("-run")
        .arg(format!("^{}$", escape_go_regex(test)))
        .arg("-coverpkg")
        .arg("./...")
        .arg("-coverprofile")
        .arg(&profile_path)
        .current_dir(project_root)
        .output()
        .with_context(|| format!("failed to run Go test {test}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "go test -run {test} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    std::fs::read_to_string(&profile_path)
        .with_context(|| format!("could not read coverage profile for {test}"))
}

fn add_go_coverage_to_selection_map(
    map: &mut TestSelectionJson,
    repo_root: &Path,
    module_root: &Path,
    module_path: &str,
    profile: &str,
    test: &str,
) -> anyhow::Result<()> {
    for line in profile.lines().skip(1) {
        let Some((location, fields)) = line.split_once(' ') else {
            continue;
        };
        let fields: Vec<&str> = fields.split_whitespace().collect();
        if fields.len() < 2 || fields[1] == "0" {
            continue;
        }

        let Some((file, range)) = location.rsplit_once(':') else {
            continue;
        };
        let Some((start, end)) = range.split_once(',') else {
            continue;
        };
        let start_line = parse_go_cover_line(start)?;
        let end_line = parse_go_cover_line(end)?;
        let file = normalize_go_cover_file(repo_root, module_root, module_path, file);

        for line in start_line..=end_line {
            let tests = map
                .entry(file.clone())
                .or_default()
                .entry(line.to_string())
                .or_default();
            if !tests.iter().any(|existing| existing == test) {
                tests.push(test.to_string());
            }
        }
    }

    Ok(())
}

fn parse_go_cover_line(position: &str) -> anyhow::Result<usize> {
    position
        .split_once('.')
        .map(|(line, _)| line)
        .unwrap_or(position)
        .parse::<usize>()
        .with_context(|| format!("invalid Go coverage position '{position}'"))
}

fn normalize_go_cover_file(
    repo_root: &Path,
    module_root: &Path,
    module_path: &str,
    file: &str,
) -> String {
    let module_relative = file
        .strip_prefix(module_path)
        .and_then(|path| path.strip_prefix('/'))
        .unwrap_or(file);
    let repo_relative = module_root.join(module_relative);

    repo_relative
        .strip_prefix(repo_root)
        .unwrap_or(Path::new(module_relative))
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part.to_string_lossy()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn escape_go_regex(test: &str) -> String {
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

fn get_project_root() -> anyhow::Result<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .output()?;
    if !output.status.success() {
        anyhow::bail!("Not a git repository. Run togi from inside a git project.");
    }
    let path = String::from_utf8(output.stdout)?.trim().to_string();
    Ok(PathBuf::from(path))
}

fn get_git_diff(base: &str) -> anyhow::Result<Vec<u8>> {
    validate_diff_base(base)?;
    let output = std::process::Command::new("git")
        .args(["-c", "core.quotePath=false", "diff", "--no-ext-diff", base])
        .output()?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "Could not diff against '{base}'. Is the branch up to date? Try running 'git fetch' first.\n\nDetails: {stderr}"
        );
    }
    Ok(output.stdout)
}

fn validate_diff_base(base: &str) -> anyhow::Result<()> {
    if base.trim().is_empty() {
        anyhow::bail!("diff base cannot be empty");
    }
    if base.starts_with('-') {
        anyhow::bail!("diff base must be a ref, commit, or tag, not an option: {base}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check_config() -> togi::cli::CheckArgs {
        togi::cli::CheckArgs {
            all: false,
            path: vec![],
            base: None,
            config: None,
            format: togi::cli::OutputFormat::Terminal,
            json_report: None,
            profile: None,
            jobs: None,
            timeout: None,
            calibrate_timeout: false,
            skip_baseline_timing: false,
            timeout_multiplier: None,
            timeout_slack: None,
            max_per_run: None,
            first_survivor: false,
            max_survivors: None,
            schemata: false,
            no_schemata: false,
            dry_run: false,
            verbose: false,
            show_output: false,
            test_cmd: None,
            coverage: None,
            coverage_file: None,
            coverage_cmd: None,
            min_line_coverage: None,
            min_diff_coverage: None,
            fail_on_uncovered_diff: false,
            test_selection_file: None,
            no_incremental_history: false,
            learned_selection: false,
            force_rerun: false,
            build_cmd: None,
            fail_fast: false,
            no_skip_defaults: false,
            operators: None,
            fail_under: None,
            shard: None,
            save_baseline: false,
            check_baseline: false,
            pr_comment: None,
        }
    }

    fn report_with_outcome(
        result: togi::MutationResult,
        execution: Option<togi::MutationExecution>,
    ) -> togi::MutationReport {
        let mutation = togi::Mutation {
            id: 0,
            file: std::path::PathBuf::from("src/lib.rs"),
            language: "rust".to_owned(),
            line: 1,
            column: 1,
            operator: "op".to_owned(),
            description: "description".to_owned(),
            original: "a".to_owned(),
            replacement: "b".to_owned(),
            byte_range: 0..1,
        };
        let mut execution_provenance = std::collections::BTreeMap::new();
        if let Some(execution) = execution {
            execution_provenance.insert(mutation.id, execution);
        }
        togi::MutationReport {
            results: vec![(mutation, result)],
            execution_provenance,
            selection_provenance: std::collections::BTreeMap::new(),
            build_error_diagnostics: vec![],
            schemata: None,
            baseline_timing: None,
            duration: std::time::Duration::ZERO,
            test_command: None,
            build_command: vec![],
            planned_total: 1,
            early_stop_reason: None,
            total: 1,
            killed: 0,
            survived: 0,
            timeout: if result == togi::MutationResult::Timeout {
                1
            } else {
                0
            },
            build_errors: if result == togi::MutationResult::BuildError {
                1
            } else {
                0
            },
        }
    }

    #[test]
    fn fresh_timeout_and_build_error_trigger_default_outcome_gate() {
        for result in [
            togi::MutationResult::Timeout,
            togi::MutationResult::BuildError,
        ] {
            assert!(
                has_fresh_timeout_or_build_error(&report_with_outcome(result, None)),
                "{result:?} should trigger the default outcome gate"
            );
        }
    }

    #[test]
    fn reused_timeout_and_build_error_do_not_trigger_default_outcome_gate() {
        for execution in [
            togi::MutationExecution::ExactCache,
            togi::MutationExecution::IncrementalHistory,
        ] {
            for result in [
                togi::MutationResult::Timeout,
                togi::MutationResult::BuildError,
            ] {
                assert!(
                    !has_fresh_timeout_or_build_error(&report_with_outcome(
                        result,
                        Some(execution),
                    )),
                    "{execution:?} {result:?} should not trigger the default outcome gate"
                );
            }
        }
    }

    #[test]
    fn resolve_config_applies_profile_jobs_when_implicit() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let config_path = dir.path().join("togi.toml");
        std::fs::write(&config_path, "").expect("empty config should be written");
        let mut cfg = check_config();
        cfg.config = Some(config_path);
        cfg.profile = Some(togi::config::ResourceProfile::Cool);

        let resolved = resolve_config(cfg).expect("config should resolve");

        assert_eq!(resolved.config.test.jobs, 1);
        assert!(resolved.fail_fast);
    }

    #[test]
    fn resolve_config_rejects_removed_confirm_survivors_setting() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let config_path = dir.path().join("togi.toml");
        std::fs::write(&config_path, "[mutations]\nconfirm_survivors = true\n")
            .expect("config should be written");
        let mut cfg = check_config();
        cfg.config = Some(config_path);

        let error = resolve_config(cfg).expect_err("stale setting should fail");

        let message = format!("{error:#}");
        assert!(message.contains("confirm_survivors"), "{message}");
        assert!(message.contains("removed"), "{message}");
    }

    #[test]
    fn resolve_config_keeps_explicit_jobs_over_profile() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let config_path = dir.path().join("togi.toml");
        std::fs::write(
            &config_path,
            r#"
[test]
profile = "cool"
jobs = 4
"#,
        )
        .expect("config should be written");
        let mut cfg = check_config();
        cfg.config = Some(config_path);

        let resolved = resolve_config(cfg).expect("config should resolve");

        assert_eq!(resolved.profile, Some(togi::config::ResourceProfile::Cool));
        assert_eq!(resolved.config.test.jobs, 4);
        assert!(resolved.fail_fast);
    }

    #[test]
    fn resolve_config_cli_jobs_override_profile() {
        let dir = tempfile::tempdir().expect("tempdir should be created");
        let config_path = dir.path().join("togi.toml");
        std::fs::write(&config_path, "").expect("empty config should be written");
        let mut cfg = check_config();
        cfg.config = Some(config_path);
        cfg.profile = Some(togi::config::ResourceProfile::Ci);
        cfg.jobs = Some(3);

        let resolved = resolve_config(cfg).expect("config should resolve");

        assert_eq!(resolved.config.test.jobs, 3);
    }

    #[test]
    fn resolve_config_applies_timeout_calibration_options() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("togi.toml");
        std::fs::write(&config_path, "")?;
        let mut cfg = check_config();
        cfg.config = Some(config_path);
        cfg.calibrate_timeout = true;
        cfg.timeout_multiplier = Some(2.5);
        cfg.timeout_slack = Some(6);

        let resolved = resolve_config(cfg)?;

        assert!(resolved.config.test.calibrate_timeout);
        assert_eq!(resolved.config.test.timeout_multiplier, 2.5);
        assert_eq!(resolved.config.test.timeout_slack, 6);
        Ok(())
    }

    #[test]
    fn resolve_config_skip_baseline_timing_disables_configured_calibration() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("togi.toml");
        std::fs::write(&config_path, "[test]\ncalibrate_timeout = true\n")?;
        let mut cfg = check_config();
        cfg.config = Some(config_path);
        cfg.skip_baseline_timing = true;

        let resolved = resolve_config(cfg)?;

        assert!(!resolved.config.test.calibrate_timeout);
        Ok(())
    }

    #[test]
    fn resolve_config_cli_timeout_disables_calibration() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("togi.toml");
        std::fs::write(&config_path, "[test]\ncalibrate_timeout = true\n")?;
        let mut cfg = check_config();
        cfg.config = Some(config_path);
        cfg.timeout = Some(12);

        let resolved = resolve_config(cfg)?;

        assert_eq!(resolved.config.test.timeout, 12);
        assert!(!resolved.config.test.calibrate_timeout);
        Ok(())
    }

    #[test]
    fn resolve_config_accepts_builtin_coverage_auto_without_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("togi.toml");
        std::fs::write(&config_path, "")?;
        let mut cfg = check_config();
        cfg.config = Some(config_path);
        cfg.coverage = Some(togi::config::CoverageMode::Auto);

        let resolved = resolve_config(cfg)?;

        assert_eq!(
            resolved.config.mutations.coverage,
            Some(togi::config::CoverageMode::Auto)
        );
        assert!(resolved.config.mutations.coverage_file.is_none());
        Ok(())
    }

    #[test]
    fn resolve_config_rejects_coverage_command_without_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("togi.toml");
        std::fs::write(&config_path, "")?;
        let mut cfg = check_config();
        cfg.config = Some(config_path);
        cfg.coverage_cmd = Some("go test ./...".into());

        let err = resolve_config(cfg).unwrap_err();

        assert!(
            err.to_string()
                .contains("coverage collection command requires")
        );
        Ok(())
    }

    #[test]
    fn resolve_config_rejects_invalid_timeout_multiplier() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config_path = dir.path().join("togi.toml");
        std::fs::write(&config_path, "")?;
        let mut cfg = check_config();
        cfg.config = Some(config_path);
        cfg.timeout_multiplier = Some(0.0);

        let err = match resolve_config(cfg) {
            Ok(_) => panic!("invalid multiplier should fail"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("timeout_multiplier"));
        Ok(())
    }
    #[test]
    fn calibrated_timeout_uses_slowest_baseline_duration() {
        let timeout = calibrated_timeout_seconds(
            Some(Duration::from_millis(1_900)),
            Duration::from_millis(400),
            2.0,
            1,
        );

        assert_eq!(timeout, 5);
    }

    #[test]
    fn calibration_uses_the_slowest_default_timeout_route() {
        let measurement = togi::runner::BaselineHealthMeasurement {
            suites: vec![
                togi::runner::BaselineSuiteMeasurement {
                    build_duration: None,
                    test_command: vec!["fast".into()],
                    test_duration: Duration::from_secs(1),
                    uses_default_timeout: true,
                },
                togi::runner::BaselineSuiteMeasurement {
                    build_duration: None,
                    test_command: vec!["slow".into()],
                    test_duration: Duration::from_secs(4),
                    uses_default_timeout: true,
                },
                togi::runner::BaselineSuiteMeasurement {
                    build_duration: Some(Duration::from_secs(10)),
                    test_command: vec!["explicit".into()],
                    test_duration: Duration::from_secs(10),
                    uses_default_timeout: false,
                },
            ],
        };

        let selected = calibration_measurement(&measurement).unwrap();

        assert_eq!(selected.test_command, vec!["slow"]);
    }

    #[test]
    fn baseline_measurement_timeout_is_more_generous_than_mutation_timeout() {
        assert_eq!(baseline_measurement_timeout(1), Duration::from_secs(60));
        assert_eq!(baseline_measurement_timeout(30), Duration::from_secs(300));
    }

    #[test]
    fn cool_profile_sets_safe_runner_env_without_overriding_user_env() {
        let cargo = vec!["cargo".to_string(), "test".to_string()];
        let go = vec!["go".to_string(), "test".to_string(), "./...".to_string()];
        let pytest = vec!["pytest".to_string()];
        let commands: [&[String]; 3] = [&cargo, &go, &pytest];

        let env = resource_profile_env_for_commands(
            togi::config::ResourceProfile::Cool,
            &commands,
            |name| name == "GOMAXPROCS",
        );

        assert_eq!(env.get("CARGO_BUILD_JOBS").map(String::as_str), Some("1"));
        assert_eq!(env.get("RUST_TEST_THREADS").map(String::as_str), Some("1"));
        assert_eq!(
            env.get("PYTEST_XDIST_AUTO_NUM_WORKERS").map(String::as_str),
            Some("1")
        );
        assert!(!env.contains_key("GOMAXPROCS"));
    }

    #[test]
    fn parse_test_selection_json_accepts_file_line_test_map() {
        let root = Path::new("/repo");
        let json = r#"{
            "src/calc.go": {
                "12": ["TestAdd", "TestMax"]
            }
        }"#;

        assert!(parse_test_selection_json(json, root).is_ok());
    }

    #[test]
    fn parse_test_selection_json_accepts_timed_test_entries() {
        let root = Path::new("/repo");
        let json = r#"{
            "src/lib.rs": {
                "9": [
                    {"name": "math::fast_add", "duration_ms": 3},
                    {"name": "math::slow_add", "duration_ms": 30}
                ]
            }
        }"#;

        assert!(parse_test_selection_json(json, root).is_ok());
    }

    #[test]
    fn parse_test_selection_json_rejects_empty_test_name() {
        let root = Path::new("/repo");
        let json = r#"{
            "src/lib.rs": {
                "9": [{"name": ""}]
            }
        }"#;

        let err = match parse_test_selection_json(json, root) {
            Ok(_) => panic!("empty test name should be rejected"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("invalid test selection entry"));
    }

    #[test]
    fn validate_diff_base_rejects_option_like_values() {
        let err = validate_diff_base("--output=/tmp/togi.diff").unwrap_err();

        assert!(err.to_string().contains("not an option"));
    }

    #[test]
    fn parse_test_selection_json_rejects_non_numeric_line() {
        let root = Path::new("/repo");
        let json = r#"{
            "src/calc.go": {
                "line": ["TestAdd"]
            }
        }"#;

        let err = parse_test_selection_json(json, root).unwrap_err();

        assert!(err.to_string().contains("invalid line number"));
    }

    #[test]
    fn parse_test_selection_json_rejects_zero_line() {
        let root = Path::new("/repo");
        let json = r#"{
            "src/calc.go": {
                "0": ["TestAdd"]
            }
        }"#;

        let err = parse_test_selection_json(json, root).unwrap_err();

        assert_eq!(err.to_string(), "invalid line number '0' for src/calc.go");
    }

    #[test]
    fn normalize_go_cover_file_strips_module_path() {
        assert_eq!(
            normalize_go_cover_file(
                Path::new("/repo/module"),
                Path::new("/repo/module"),
                "example.com/calc",
                "example.com/calc/sub/calc.go"
            ),
            "sub/calc.go"
        );
    }

    #[test]
    fn normalize_go_cover_file_returns_repo_relative_path_for_nested_module() {
        assert_eq!(
            normalize_go_cover_file(
                Path::new("/repo"),
                Path::new("/repo/services/api"),
                "example.com/api",
                "example.com/api/pkg/file.go"
            ),
            "services/api/pkg/file.go"
        );
    }

    #[test]
    fn unique_go_test_names_preserves_first_seen_order() {
        let names = vec![
            "TestAdd".to_string(),
            "TestMax".to_string(),
            "TestAdd".to_string(),
            "TestIsPositive".to_string(),
        ];

        assert_eq!(
            unique_go_test_names(names),
            vec!["TestAdd", "TestMax", "TestIsPositive"]
        );
    }

    #[test]
    fn go_coverage_selection_map_includes_only_covered_lines() {
        let profile = r#"mode: set
example.com/calc/calc.go:4.24,6.2 1 1
example.com/calc/calc.go:9.29,10.11 1 0
"#;
        let mut map = TestSelectionJson::new();

        add_go_coverage_to_selection_map(
            &mut map,
            Path::new("/repo/module"),
            Path::new("/repo/module"),
            "example.com/calc",
            profile,
            "TestAdd",
        )
        .unwrap();

        let file = map.get("calc.go").unwrap();
        assert_eq!(file.get("4").unwrap(), &vec!["TestAdd".to_string()]);
        assert_eq!(file.get("6").unwrap(), &vec!["TestAdd".to_string()]);
        assert!(!file.contains_key("9"));
    }
    #[test]
    fn parse_go_coverprofile_stats_tracks_total_and_covered_lines() {
        let profile = r#"mode: set
example.com/calc/calc.go:4.24,6.2 1 1
example.com/calc/calc.go:9.29,10.11 1 0
"#;

        let stats = parse_go_coverprofile_stats(
            Path::new("/repo/module"),
            Path::new("/repo/module"),
            "example.com/calc",
            profile,
        )
        .unwrap();

        let file = stats.covered_lines.get(Path::new("calc.go")).unwrap();
        assert!(file.contains(&4));
        assert!(file.contains(&6));
        assert!(!file.contains(&9));

        let total = stats.total_lines.get(Path::new("calc.go")).unwrap();
        assert!(total.contains(&4));
        assert!(total.contains(&10));
    }
    #[test]
    fn destination_identity_resolves_equivalent_and_nested_missing_paths() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let current_dir = dir.path().join("current");
        std::fs::create_dir_all(current_dir.join("existing/child"))?;

        let absolute = current_dir.join("existing/report.json");
        let relative = Path::new("existing/child/../report.json");
        assert_eq!(
            destination_identity(relative, &current_dir)?,
            destination_identity(&absolute, &current_dir)?
        );

        let missing_leaf = current_dir.join("missing-leaf.json");
        assert_eq!(
            destination_identity(&missing_leaf, &current_dir)?,
            destination_identity(&current_dir, &current_dir)?.join("missing-leaf.json")
        );
        let missing_nested = current_dir.join("missing/parent/report.json");
        assert_eq!(
            destination_identity(&missing_nested, &current_dir)?,
            destination_identity(&current_dir, &current_dir)?.join("missing/parent/report.json")
        );
        assert!(
            destination_identity(&current_dir.join("foo/../report.json"), &current_dir).is_err(),
            "a parent traversal after a missing component is unsafe to preflight"
        );
        assert!(
            same_output_destination(&absolute, &current_dir.join("EXISTING/REPORT.JSON")),
            "case-only aliases are deliberately rejected for case-insensitive volumes"
        );
        assert!(
            same_output_destination(
                Path::new("/tmp/über-report.html"),
                Path::new("/tmp/ÜBER-report.html")
            ),
            "valid UTF-8 aliases use Unicode-aware lowercase comparison"
        );
        assert!(
            same_output_destination(Path::new("/tmp/café.md"), Path::new("/tmp/cafe\u{301}.md")),
            "valid UTF-8 aliases normalize NFC and NFD before comparison"
        );
        assert!(
            same_output_destination(Path::new("/tmp/straße.md"), Path::new("/tmp/STRASSE.md")),
            "canonical caseless matching folds sharp s"
        );
        assert!(
            same_output_destination(Path::new("/tmp/σigma.md"), Path::new("/tmp/ςIGMA.md")),
            "canonical caseless matching folds Greek sigma variants"
        );
        assert!(
            same_output_destination(Path::new("/tmp/σigma.md"), Path::new("/tmp/ΣIGMA.md")),
            "canonical caseless matching folds Greek sigma case variants"
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn destination_identity_does_not_conflate_non_utf8_paths() {
        use std::os::unix::ffi::OsStrExt;

        let mut left = PathBuf::new();
        left.push(OsStr::from_bytes(&[b'/', b't', 0xff]));
        let mut right = PathBuf::new();
        right.push(OsStr::from_bytes(&[b'/', b't', 0xfe]));
        assert!(!same_output_destination(&left, &right));
        assert!(same_output_destination(&left, &left));
    }

    #[cfg(unix)]
    #[test]
    fn destination_identity_follows_symlinks_before_parent_components() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let current_dir = dir.path().join("current");
        std::fs::create_dir_all(current_dir.join("target/nested"))?;
        std::os::unix::fs::symlink("target/nested", current_dir.join("link"))?;
        std::os::unix::fs::symlink("target/report.json", current_dir.join("togi-report.html"))?;

        assert_eq!(
            destination_identity(Path::new("link/../report.json"), &current_dir)?,
            destination_identity(&current_dir.join("target/report.json"), &current_dir)?
        );
        assert_eq!(
            destination_identity(Path::new("togi-report.html"), &current_dir)?,
            destination_identity(&current_dir.join("target/report.json"), &current_dir)?
        );
        Ok(())
    }

    #[test]
    fn json_report_publish_replaces_existing_report_after_staging() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let report_path = dir.path().join("report.json");
        std::fs::write(
            &report_path,
            "{\"kind\":\"mutation_report\",\"old\":true}\n",
        )?;

        write_json_report(&report_path, "{\"kind\":\"mutation_report\",\"new\":true}")?;

        assert_eq!(
            std::fs::read_to_string(&report_path)?,
            "{\"kind\":\"mutation_report\",\"new\":true}\n"
        );
        Ok(())
    }

    #[test]
    fn json_report_publish_preserves_existing_report_on_write_failure() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let report_path = dir.path().join("report.json");
        let existing = "{\"kind\":\"mutation_report\",\"old\":true}\n";
        std::fs::write(&report_path, existing)?;

        let error = publish_json_report(&report_path, |staged| {
            staged.write_all(b"{\"kind\":\"mutation_report\",\"partial\":")?;
            Err(std::io::Error::other("simulated write failure"))
        });

        assert!(error.is_err());
        assert_eq!(std::fs::read_to_string(&report_path)?, existing);
        Ok(())
    }
}
