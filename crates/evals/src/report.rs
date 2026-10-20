//! Running a suite and reporting the result as a table, JSON, CSV, and a
//! static HTML dashboard.

use crate::golden::Golden;
use crate::metrics::Metric;
use anyhow::Result;
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, serde::Serialize)]
pub struct CaseReport {
    pub id: String,
    pub description: String,
    pub dimension: String,
    pub metric: String,
    pub value: f64,
    pub threshold: f64,
    pub passed: bool,
    pub elapsed_ms: u64,
    pub steps: usize,
    pub detail: String,
}

impl CaseReport {
    fn error(golden: &Golden, err: &anyhow::Error) -> Self {
        Self {
            id: golden.id.to_string(),
            description: golden.description.to_string(),
            dimension: "error".into(),
            metric: "case_error".into(),
            value: 0.0,
            threshold: 1.0,
            passed: false,
            elapsed_ms: 0,
            steps: 0,
            detail: format!("case failed to run: {err}"),
        }
    }
}

/// Run one golden case against the real `condesate` loop, then grade every
/// metric that applies to its `Expectation`. A case that errors (rather than
/// producing an `Expectation` mismatch) is reported as a failed case, not a
/// crash — same "never raises" contract `harness-evals`' `evaluate()` uses.
pub async fn run_golden(golden: &Golden, metrics: &[Box<dyn Metric>]) -> CaseReport {
    match (golden.run)().await {
        Ok(outcome) => {
            let scores: Vec<_> =
                metrics.iter().filter(|m| m.applies(&golden.expectation)).map(|m| m.measure(&golden.expectation, &outcome)).collect();
            match scores.first() {
                Some(first) => {
                    let passed = scores.iter().all(|s| s.passed());
                    let value = scores.iter().map(|s| s.value).sum::<f64>() / scores.len() as f64;
                    let detail = scores.iter().map(|s| s.detail.clone()).collect::<Vec<_>>().join("; ");
                    CaseReport {
                        id: golden.id.to_string(),
                        description: golden.description.to_string(),
                        dimension: first.dimension.clone(),
                        metric: scores.iter().map(|s| s.metric.clone()).collect::<Vec<_>>().join("+"),
                        value,
                        threshold: first.threshold,
                        passed,
                        elapsed_ms: outcome.elapsed_ms,
                        steps: outcome.steps,
                        detail,
                    }
                }
                None => CaseReport {
                    id: golden.id.to_string(),
                    description: golden.description.to_string(),
                    dimension: "unscored".into(),
                    metric: "none".into(),
                    value: 0.0,
                    threshold: 1.0,
                    passed: false,
                    elapsed_ms: outcome.elapsed_ms,
                    steps: outcome.steps,
                    detail: "no registered metric applies to this case's expectation".into(),
                },
            }
        }
        Err(e) => CaseReport::error(golden, &e),
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct DimensionSummary {
    pub dimension: String,
    pub total: usize,
    pub passed: usize,
    pub pass_rate: f64,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RunReport {
    pub generated_at_ms: u64,
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub pass_rate: f64,
    pub by_dimension: Vec<DimensionSummary>,
    pub cases: Vec<CaseReport>,
}

impl RunReport {
    pub fn build(cases: Vec<CaseReport>) -> Self {
        let total = cases.len();
        let passed = cases.iter().filter(|c| c.passed).count();
        let failed = total - passed;
        let pass_rate = if total == 0 { 0.0 } else { passed as f64 / total as f64 };

        let mut by_dim: BTreeMap<String, (usize, usize)> = BTreeMap::new();
        for c in &cases {
            let entry = by_dim.entry(c.dimension.clone()).or_insert((0, 0));
            entry.0 += 1;
            if c.passed {
                entry.1 += 1;
            }
        }
        let by_dimension = by_dim
            .into_iter()
            .map(|(dimension, (total, passed))| DimensionSummary {
                dimension,
                total,
                passed,
                pass_rate: if total == 0 { 0.0 } else { passed as f64 / total as f64 },
            })
            .collect();

        let generated_at_ms =
            SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);

        Self { generated_at_ms, total, passed, failed, pass_rate, by_dimension, cases }
    }
}

// ---------------------------------------------------------------------------
// Table (stdout)
// ---------------------------------------------------------------------------

pub fn print_table(report: &RunReport) {
    let headers = ["STATUS", "ID", "DIMENSION", "SCORE", "MS", "STEPS", "DETAIL"];
    let rows: Vec<[String; 7]> = report
        .cases
        .iter()
        .map(|c| {
            [
                if c.passed { "PASS".to_string() } else { "FAIL".to_string() },
                c.id.clone(),
                c.dimension.clone(),
                format!("{:.2}", c.value),
                c.elapsed_ms.to_string(),
                c.steps.to_string(),
                c.detail.clone(),
            ]
        })
        .collect();

    let mut widths = headers.map(|h| h.len());
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.len());
        }
    }

    let print_row = |cells: &[String; 7]| {
        let line: Vec<String> =
            cells.iter().enumerate().map(|(i, c)| format!("{:<width$}", c, width = widths[i])).collect();
        println!("{}", line.join("  "));
    };

    println!();
    print_row(&headers.map(|h| h.to_string()));
    println!("{}", widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("  "));
    for row in &rows {
        print_row(row);
    }
    println!();
    println!(
        "{}/{} passed ({:.0}%)",
        report.passed,
        report.total,
        report.pass_rate * 100.0
    );
    for d in &report.by_dimension {
        println!("  {:<14} {}/{} ({:.0}%)", d.dimension, d.passed, d.total, d.pass_rate * 100.0);
    }
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

pub fn write_json(report: &RunReport, path: &str) -> Result<()> {
    let json = serde_json::to_string_pretty(report)?;
    std::fs::write(path, json)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// CSV
// ---------------------------------------------------------------------------

pub fn write_csv(report: &RunReport, path: &str) -> Result<()> {
    let mut w = csv::Writer::from_path(path)?;
    w.write_record([
        "id", "description", "dimension", "metric", "value", "threshold", "passed", "elapsed_ms", "steps", "detail",
    ])?;
    for c in &report.cases {
        w.write_record(&[
            c.id.clone(),
            c.description.clone(),
            c.dimension.clone(),
            c.metric.clone(),
            format!("{:.4}", c.value),
            format!("{:.4}", c.threshold),
            c.passed.to_string(),
            c.elapsed_ms.to_string(),
            c.steps.to_string(),
            c.detail.clone(),
        ])?;
    }
    w.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Dashboard (static HTML, no external assets)
// ---------------------------------------------------------------------------

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn write_dashboard(report: &RunReport, path: &str) -> Result<()> {
    let mut rows = String::new();
    for c in &report.cases {
        rows.push_str(&format!(
            "<tr class=\"{cls}\"><td>{status}</td><td><code>{id}</code></td><td>{dim}</td><td>{metric}</td><td>{value:.2}</td><td>{ms}ms</td><td>{steps}</td><td>{detail}</td></tr>\n",
            cls = if c.passed { "pass" } else { "fail" },
            status = if c.passed { "PASS" } else { "FAIL" },
            id = escape_html(&c.id),
            dim = escape_html(&c.dimension),
            metric = escape_html(&c.metric),
            value = c.value,
            ms = c.elapsed_ms,
            steps = c.steps,
            detail = escape_html(&c.detail),
        ));
    }

    let mut dim_bars = String::new();
    for d in &report.by_dimension {
        dim_bars.push_str(&format!(
            "<div class=\"dim-row\"><span class=\"dim-label\">{dim}</span><div class=\"bar\"><div class=\"bar-fill\" style=\"width:{pct:.0}%\"></div></div><span class=\"dim-count\">{passed}/{total}</span></div>\n",
            dim = escape_html(&d.dimension),
            pct = d.pass_rate * 100.0,
            passed = d.passed,
            total = d.total,
        ));
    }

    let html = format!(
        r#"<title>condesate evals dashboard</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{ font-family: -apple-system, system-ui, sans-serif; max-width: 960px; margin: 2rem auto; padding: 0 1rem; }}
  h1 {{ font-size: 1.3rem; }}
  .summary {{ display: flex; gap: 2rem; align-items: baseline; margin-bottom: 1.5rem; }}
  .summary .rate {{ font-size: 2rem; font-weight: 700; }}
  .summary .rate.good {{ color: #2e8b57; }}
  .summary .rate.bad {{ color: #c0392b; }}
  .dim-row {{ display: flex; align-items: center; gap: 0.75rem; margin: 0.35rem 0; }}
  .dim-label {{ width: 8rem; font-size: 0.85rem; opacity: 0.8; }}
  .bar {{ flex: 1; height: 0.6rem; background: rgba(127,127,127,0.2); border-radius: 999px; overflow: hidden; }}
  .bar-fill {{ height: 100%; background: #2e8b57; }}
  .dim-count {{ width: 3.5rem; text-align: right; font-size: 0.85rem; opacity: 0.8; }}
  table {{ width: 100%; border-collapse: collapse; margin-top: 1.5rem; font-size: 0.85rem; }}
  th, td {{ text-align: left; padding: 0.4rem 0.6rem; border-bottom: 1px solid rgba(127,127,127,0.25); vertical-align: top; }}
  tr.fail td:first-child {{ color: #c0392b; font-weight: 700; }}
  tr.pass td:first-child {{ color: #2e8b57; font-weight: 700; }}
  code {{ font-family: ui-monospace, Menlo, monospace; }}
  footer {{ margin-top: 2rem; font-size: 0.75rem; opacity: 0.6; }}
</style>
<h1>condesate evals dashboard</h1>
<div class="summary">
  <div class="rate {rate_class}">{pass_rate:.0}%</div>
  <div>{passed}/{total} cases passed<br>generated at {generated_at_ms}ms (unix epoch)</div>
</div>
{dim_bars}
<table>
  <thead><tr><th>Status</th><th>ID</th><th>Dimension</th><th>Metric</th><th>Score</th><th>Latency</th><th>Steps</th><th>Detail</th></tr></thead>
  <tbody>
{rows}  </tbody>
</table>
<footer>Generated by <code>evals</code> against the real <code>condesate</code> agent loop — see docs/references.md for the harness-evals / Awesome-AI-Evaluations-Tools inspiration.</footer>
"#,
        rate_class = if report.pass_rate >= 0.8 { "good" } else { "bad" },
        pass_rate = report.pass_rate * 100.0,
        passed = report.passed,
        total = report.total,
        generated_at_ms = report.generated_at_ms,
        dim_bars = dim_bars,
        rows = rows,
    );

    std::fs::write(path, html)?;
    Ok(())
}
