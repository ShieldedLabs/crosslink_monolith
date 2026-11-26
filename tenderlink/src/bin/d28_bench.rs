//! Small CLI for the reusable component benchmark; not a node or chain exporter.
use std::{io::Write, path::PathBuf};
use tenderlink::d28::bench::{self, BenchConfig};

const HELP: &str = "d28_bench --nodes N --offline N --heights N --warmup N --headers N
  --timeout-secs N --workers N --host-delay-ms N --store-delay-ms N
  [--max-body BYTES] [--chatter-secs N] [--json PATH]

Real D28/STP/crypto/durable-image component benchmark on localhost only.
No PoW, ledger, physical persistence, transactions, or TPS claims.
Defaults: 4 nodes, 0 offline, 30 measured heights + 3 warmup, 32 headers,
180 second whole-run deadline, 4 executor workers, zero artificial delays,
production 1 MiB max body and production timers, zero chatter hold.
--headers controls exact serialized synthetic Zcash header payload (187B each).
--host-delay-ms delays proposal acquisition and validation asynchronously.
--store-delay-ms blocks each in-memory full-image replacement (not fsync).
--chatter-secs holds final-height ACK/tip unpublished; separate traffic window.
JSON status complete exits 0; partial (including deadline) exits 1; bad input exits 2.
Kernel traffic counters are null except in a wrapper-marked isolated netns.";

fn parse(
    args: impl IntoIterator<Item = String>,
) -> Result<Option<(BenchConfig, Option<PathBuf>)>, String> {
    let mut config = BenchConfig::default();
    let mut json = None;
    let mut args = args.into_iter();
    while let Some(option) = args.next() {
        if option == "--help" || option == "-h" {
            return Ok(None);
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{option}: missing value"))?;
        macro_rules! number {
            ($field:ident) => {
                config.$field = value
                    .parse()
                    .map_err(|_| format!("{option}: expected an unsigned integer"))?
            };
        }
        match option.as_str() {
            "--nodes" => number!(nodes),
            "--offline" => number!(offline),
            "--heights" => number!(heights),
            "--warmup" => number!(warmup),
            "--headers" => number!(headers),
            "--timeout-secs" => number!(timeout_secs),
            "--workers" => number!(workers),
            "--host-delay-ms" => number!(host_delay_ms),
            "--store-delay-ms" => number!(store_delay_ms),
            "--max-body" => number!(max_body),
            "--chatter-secs" => number!(chatter_secs),
            "--json" => json = Some(PathBuf::from(value)),
            _ => return Err(format!("unknown option {option}")),
        }
    }
    config.validate().map_err(|e| e.to_string())?;
    Ok(Some((config, json)))
}
fn main() -> std::process::ExitCode {
    let (config, json) = match parse(std::env::args().skip(1)) {
        Ok(Some(parsed)) => parsed,
        Ok(None) => {
            println!("{HELP}");
            return std::process::ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("d28_bench: {error}\nUse --help for options.");
            return 2.into();
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.workers)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("runtime: {error}");
            return 2.into();
        }
    };
    let report = match runtime.block_on(bench::run(config)) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("benchmark setup: {error}");
            return 2.into();
        }
    };
    if let Some(path) = json {
        let write = || -> Result<(), Box<dyn std::error::Error>> {
            // Refuse to overwrite an earlier result, especially a matrix trial.
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)?;
            serde_json::to_writer_pretty(&mut output, &report)?;
            output.write_all(b"\n")?;
            output.flush()?;
            Ok(())
        };
        if let Err(error) = write() {
            eprintln!("JSON {}: {error}", path.display());
            return 2.into();
        }
    }
    println!(
        "D28 component benchmark: {} ({} measured cohort heights)",
        report.status, report.summary.measured_heights
    );
    if let Some(error) = &report.failure {
        println!("Failure: {error}");
    }
    for (label, latency) in [
        ("first", report.summary.latency_ms.first),
        ("quorum", report.summary.latency_ms.quorum),
        ("all", report.summary.latency_ms.all),
    ] {
        if let Some(latency) = latency {
            println!(
                "{label} node callback latency (n={}): p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms",
                report.summary.latency_samples[label], latency.p50, latency.p95, latency.p99
            );
        }
    }
    if let Some(rate) = report.summary.committed_heights_per_second {
        println!(
            "All-online ACK-barrier chain rate: {:.3} heights/s, {:.0} serialized body B/s, {:.0} synthetic payload B/s",
            rate,
            report
                .summary
                .committed_body_bytes_per_second
                .unwrap_or(0.0),
            report
                .summary
                .committed_payload_bytes_per_second
                .unwrap_or(0.0)
        );
    }
    println!(
        "In-memory image {} bytes/node; no fsync/PoW/ledger/TPS measurement. Final in-flight tail excluded from measured window.",
        report.durable_image_bytes_per_node
    );
    if report.status == "complete" {
        std::process::ExitCode::SUCCESS
    } else {
        1.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }
    #[test]
    fn parser_and_safety_bounds() {
        assert!(
            parse(args("--nodes 4 --offline 1 --heights 2 --warmup 1"))
                .unwrap()
                .is_some()
        );
        assert!(parse(args("--nodes 4 --offline 2")).is_err());
        assert!(parse(args("--workers 0")).is_err());
        assert!(parse(args("--headers -1")).is_err());
        assert!(parse(args("--listen 0.0.0.0")).is_err());
        assert!(parse(args("--heights")).is_err());
        assert!(parse(args("--help")).unwrap().is_none());
    }
}
