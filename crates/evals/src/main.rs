//! Native suite entry point — see `evals` (the lib crate, `lib.rs`) for how
//! this relates to the real `harness-evals` integration in `server.rs` /
//! `bin/harness_evals_run.rs`.

use anyhow::Result;
use evals::{metrics, report, suite};

#[tokio::main]
async fn main() -> Result<()> {
    let out_dir = std::env::var("EVALS_OUT_DIR").unwrap_or_else(|_| "evals-out".to_string());
    std::fs::create_dir_all(&out_dir)?;

    let goldens = suite::suite();
    let metrics = metrics::all_metrics();

    println!("running {} eval case(s) against condesate...", goldens.len());
    let mut cases = Vec::with_capacity(goldens.len());
    for golden in &goldens {
        let case = report::run_golden(golden, &metrics).await;
        println!("  [{}] {} ({:.2})", if case.passed { "PASS" } else { "FAIL" }, case.id, case.value);
        cases.push(case);
    }

    let run_report = report::RunReport::build(cases);
    report::print_table(&run_report);

    let json_path = format!("{out_dir}/report.json");
    let csv_path = format!("{out_dir}/report.csv");
    let dashboard_path = format!("{out_dir}/dashboard.html");
    report::write_json(&run_report, &json_path)?;
    report::write_csv(&run_report, &csv_path)?;
    report::write_dashboard(&run_report, &dashboard_path)?;

    println!("wrote {json_path}, {csv_path}, {dashboard_path}");

    if run_report.failed > 0 {
        std::process::exit(1);
    }
    Ok(())
}
