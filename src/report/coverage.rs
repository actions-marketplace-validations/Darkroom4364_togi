use crate::cli::OutputFormat;
use crate::coverage::CoverageGateReport;
use anyhow::Result;
use std::fmt::Write;
use std::path::Path;

pub fn print_terminal(report: &CoverageGateReport) {
    print!("{}", format_terminal(report));
}

pub fn print_github(report: &CoverageGateReport) {
    print!("{}", format_github(report));
}

pub fn print_json(report: &CoverageGateReport) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(report)?);
    Ok(())
}

pub fn write_html(report: &CoverageGateReport, path: &Path) -> Result<()> {
    std::fs::write(path, format_html(report))?;
    Ok(())
}

#[allow(dead_code)]
pub fn print(report: &CoverageGateReport, format: OutputFormat) -> Result<()> {
    match format {
        OutputFormat::Json => print_json(report)?,
        OutputFormat::Github => print_github(report),
        OutputFormat::Html => write_html(report, Path::new("togi-coverage-report.html"))?,
        OutputFormat::Sarif => super::sarif::print_coverage_gate_report(report)?,
        OutputFormat::Terminal => print_terminal(report),
    }
    Ok(())
}

fn format_terminal(report: &CoverageGateReport) -> String {
    let mut out = String::new();
    writeln!(out, "Coverage gate failed").unwrap();
    write_metric(&mut out, "Overall line coverage", &report.line_coverage).unwrap();
    write_metric(&mut out, "Changed-line coverage", &report.diff_coverage).unwrap();
    if report.fail_on_uncovered_diff || !report.uncovered_changed_lines.is_empty() {
        writeln!(out, "Uncovered changed lines:").unwrap();
        for file in &report.uncovered_changed_lines {
            writeln!(
                out,
                "  {}: {}",
                file.file.display(),
                join_lines(&file.lines)
            )
            .unwrap();
        }
    }
    out
}

fn format_github(report: &CoverageGateReport) -> String {
    let mut out = String::new();
    writeln!(out, "## Coverage gate failed").unwrap();
    write_metric_md(&mut out, "Overall line coverage", &report.line_coverage).unwrap();
    write_metric_md(&mut out, "Changed-line coverage", &report.diff_coverage).unwrap();
    if report.fail_on_uncovered_diff || !report.uncovered_changed_lines.is_empty() {
        writeln!(out, "\n### Uncovered changed lines").unwrap();
        for file in &report.uncovered_changed_lines {
            writeln!(
                out,
                "- `{}`: {}",
                file.file.display(),
                join_lines(&file.lines)
            )
            .unwrap();
        }
    }
    out
}

fn format_html(report: &CoverageGateReport) -> String {
    let mut out = String::new();
    write!(
        out,
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"UTF-8\">"
    )
    .unwrap();
    write!(out, "<title>togi coverage gate</title>").unwrap();
    write!(
        out,
        "<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">"
    )
    .unwrap();
    write!(
        out,
        "<style>body{{font-family:system-ui,sans-serif;margin:2rem;line-height:1.5}}\
         h1{{margin-bottom:1rem}}.metric{{margin:.5rem 0}}\
         table{{border-collapse:collapse;margin-top:1rem}}td,th{{padding:.4rem .6rem;border-bottom:1px solid #ccc;text-align:left}}\
         code{{font-family:SFMono-Regular,Menlo,monospace}}</style></head><body>"
    )
    .unwrap();
    write!(out, "<h1>Coverage gate failed</h1>").unwrap();
    write_metric_html(&mut out, "Overall line coverage", &report.line_coverage).unwrap();
    write_metric_html(&mut out, "Changed-line coverage", &report.diff_coverage).unwrap();
    if report.fail_on_uncovered_diff || !report.uncovered_changed_lines.is_empty() {
        write!(out, "<h2>Uncovered changed lines</h2><table><thead><tr><th>File</th><th>Lines</th></tr></thead><tbody>").unwrap();
        for file in &report.uncovered_changed_lines {
            write!(
                out,
                "<tr><td><code>{}</code></td><td>{}</td></tr>",
                html_escape(&file.file.display().to_string()),
                html_escape(&join_lines(&file.lines))
            )
            .unwrap();
        }
        write!(out, "</tbody></table>").unwrap();
    }
    write!(out, "</body></html>").unwrap();
    out
}

fn write_metric(
    out: &mut String,
    label: &str,
    metric: &crate::coverage::CoverageMetric,
) -> std::fmt::Result {
    let threshold = metric
        .threshold
        .map(|value| format!(" (threshold {value:.1}%)"))
        .unwrap_or_default();
    writeln!(
        out,
        "{label}: {:.1}% ({}/{}){threshold}",
        metric.percent(),
        metric.covered,
        metric.total
    )
}

fn write_metric_md(
    out: &mut String,
    label: &str,
    metric: &crate::coverage::CoverageMetric,
) -> std::fmt::Result {
    let threshold = metric
        .threshold
        .map(|value| format!(" threshold {value:.1}%"))
        .unwrap_or_default();
    writeln!(
        out,
        "- **{label}**: {:.1}% ({}/{}){threshold}",
        metric.percent(),
        metric.covered,
        metric.total
    )
}

fn write_metric_html(
    out: &mut String,
    label: &str,
    metric: &crate::coverage::CoverageMetric,
) -> std::fmt::Result {
    let threshold = metric
        .threshold
        .map(|value| format!(" (threshold {value:.1}%)"))
        .unwrap_or_default();
    writeln!(
        out,
        "<div class=\"metric\"><strong>{label}</strong>: {:.1}% ({}/{}){threshold}</div>",
        metric.percent(),
        metric.covered,
        metric.total
    )
}

fn join_lines(lines: &[usize]) -> String {
    lines
        .iter()
        .map(usize::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
