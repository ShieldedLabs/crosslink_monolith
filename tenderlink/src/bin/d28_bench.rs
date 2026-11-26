//! Mock-chain component benchmark or one private distributed benchmark validator.
use std::{io::Write, path::PathBuf};
use tenderlink::d28::bench::{self, BenchConfig, distributed};

const HELP: &str = "d28_bench --nodes N --offline N --heights N --warmup N --headers N
  --timeout-secs N --workers N --host-delay-ms N --store-delay-ms N
  [--max-body BYTES] [--chatter-secs N] [--json PATH]
OR d28_bench --distributed CONFIG.json [--json REPORT.json]

Real D28/STP/crypto/durable-image mock-chain benchmark.
Default: all validators on localhost. --distributed: one validator per process,
exact private IPv6 peer roster and strict JSONL all-online ACK controller;
configuration comes only from CONFIG.json, not mixed component CLI flags.
Distributed body_bytes defaults to 0 (legacy headers + solution_bytes 0..1344).
Positive body_bytes targets the EXACT serialized body including its parent certificate:
use headers=32, solution_bytes=0, max_body=10485760 for a size sweep through 10 MiB.
Distributed nodes <=32, byte-mode headers 1..2048; full-roster parent must fit.
Mock solution padding uses unchanged parsing/serialization, NOT valid Equihash work.
Distributed timing_profile defaults to production; explicit wan changes benchmark
base timers to 10000/5000/7000ms, request timeout10000ms and status expiry15000ms.
Rho, round increments, production limits and defaults remain unchanged.
No PoW, ledger, physical persistence, transactions, or TPS claims.
Component defaults: 4 nodes, 0 offline, 30 measured heights + 3 warmup, 32 headers,
180 second whole-run deadline, 4 executor workers, zero artificial delays,
production 1 MiB max body and production timers, zero chatter hold.
--headers controls exact serialized synthetic Zcash header payload (187B unpadded).
--host-delay-ms delays proposal acquisition and validation asynchronously.
--store-delay-ms blocks each in-memory full-image replacement (not fsync).
--chatter-secs holds final-height tip unpublished; separate traffic window.
JSON status complete exits 0; partial (including deadline) exits 1; bad input exits 2.
Component kernel counters require a wrapper-marked isolated netns;
distributed reports sample non-lo VM interfaces, including management traffic.";

struct Options {
    config: BenchConfig,
    json: Option<PathBuf>,
    distributed: Option<PathBuf>,
}
fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Options>, String> {
    let mut config = BenchConfig::default();
    let mut json = None;
    let mut distributed = None;
    let mut component_flags = false;
    let mut args = args.into_iter();
    while let Some(option) = args.next() {
        if option == "--help" || option == "-h" {
            return Ok(None);
        }
        let value = args
            .next()
            .ok_or_else(|| format!("{option}: missing value"))?;
        macro_rules! number {
            ($field:ident) => {{
                component_flags = true;
                config.$field = value
                    .parse()
                    .map_err(|_| format!("{option}: expected an unsigned integer"))?;
            }};
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
            "--json" => {
                if json.replace(PathBuf::from(value)).is_some() {
                    return Err("duplicate --json".into());
                }
            }
            "--distributed" => {
                if distributed.replace(PathBuf::from(value)).is_some() {
                    return Err("duplicate --distributed".into());
                }
            }
            _ => return Err(format!("unknown option {option}")),
        }
    }
    if distributed.is_some() && component_flags {
        return Err("--distributed cannot be mixed with component configuration flags".into());
    }
    if distributed.is_none() {
        config.validate().map_err(|e| e.to_string())?;
    }
    Ok(Some(Options {
        config,
        json,
        distributed,
    }))
}
fn write_report(path: Option<&PathBuf>, report: &impl serde::Serialize) -> Result<(), String> {
    let Some(path) = path else {
        return Ok(());
    };
    let write = || -> Result<(), Box<dyn std::error::Error>> {
        // A killed writer must not leave a truncated final report. Publish a
        // complete sibling with an atomic no-replace hard link; plain rename
        // could clobber an earlier trial's report after a check/create race.
        let mut name = path.file_name().ok_or("report path has no filename")?.to_os_string();
        name.push(format!(".partial-{}", std::process::id()));
        let temporary = path.with_file_name(name);
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        let written = (|| -> Result<(), Box<dyn std::error::Error>> {
            serde_json::to_writer_pretty(&mut output, report)?;
            output.write_all(b"\n")?;
            output.flush()?;
            Ok(())
        })();
        drop(output);
        let published = written.and_then(|()| {
            std::fs::hard_link(&temporary, path)?;
            Ok(())
        });
        // Only our create_new-owned temporary is eligible for cleanup. An
        // interrupted process may leave a .partial file, never a partial report.
        let cleanup = std::fs::remove_file(&temporary);
        published?;
        cleanup?;
        Ok(())
    };
    write().map_err(|e| format!("JSON {}: {e}", path.display()))
}
fn main() -> std::process::ExitCode {
    let Options {
        config,
        json,
        distributed,
    } = match parse(std::env::args().skip(1)) {
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
    let distributed = match distributed
        .map(|p| distributed::DistributedConfig::load(&p))
        .transpose()
    {
        Ok(c) => c,
        Err(error) => {
            eprintln!("distributed config: {error}");
            return 2.into();
        }
    };
    let workers = distributed.as_ref().map_or(config.workers, |c| c.workers);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("runtime: {error}");
            return 2.into();
        }
    };
    if let Some(config) = distributed {
        let report = match runtime.block_on(distributed::run(config)) {
            Ok(report) => report,
            Err(error) => {
                eprintln!("distributed setup: {error}");
                return 2.into();
            }
        };
        if let Err(error) = write_report(json.as_ref(), &report) {
            eprintln!("{error}");
            return 2.into();
        }
        println!(
            "D28 distributed node {}: {} ({} measured ACKed heights)",
            report.config.node, report.status, report.summary.measured_heights
        );
        if let Some(failure) = &report.failure {
            println!("Failure: {failure}");
        }
        if let Some(p) = report.summary.latency_ms {
            println!(
                "Local START/ACK-to-push latency (n={}): p50 {:.2} ms, p95 {:.2} ms, p99 {:.2} ms",
                report.summary.latency_samples, p.p50, p.p95, p.p99
            );
        }
        println!(
            "All-online ACK barrier overhead is included in elapsed time. No PoW/ledger/TPS measurement."
        );
        return if report.status == "complete" {
            std::process::ExitCode::SUCCESS
        } else {
            1.into()
        };
    }
    let report = match runtime.block_on(bench::run(config)) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("benchmark setup: {error}");
            return 2.into();
        }
    };
    if let Err(error) = write_report(json.as_ref(), &report) {
        eprintln!("{error}");
        return 2.into();
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
    fn report_publication_is_complete_and_never_clobbers() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target")
            .join(format!("atomic-report-test-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("report.json");
        let expected = serde_json::json!({"status":"complete", "heights":[1, 2, 3]});
        write_report(Some(&path), &expected).unwrap();
        let original = std::fs::read(&path).unwrap();
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&original).unwrap(), expected);
        assert!(write_report(Some(&path), &serde_json::json!({"replaced":true})).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);

        struct Fails;
        impl serde::Serialize for Fails {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("injected serialization failure"))
            }
        }
        let failed = dir.join("failed.json");
        assert!(write_report(Some(&failed), &Fails).is_err());
        assert!(!failed.exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let stale = dir.join(format!("failed.json.partial-{}", std::process::id()));
        std::fs::write(&stale, b"preexisting temporary").unwrap();
        assert!(write_report(Some(&failed), &expected).is_err());
        assert!(!failed.exists());
        assert_eq!(std::fs::read(&stale).unwrap(), b"preexisting temporary");
        std::fs::remove_dir_all(dir).unwrap();
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
        let parsed = parse(args("--distributed node.json --json report.json"))
            .unwrap()
            .unwrap();
        assert_eq!(parsed.distributed.unwrap(), PathBuf::from("node.json"));
        assert_eq!(parsed.json.unwrap(), PathBuf::from("report.json"));
        assert!(parse(args("--distributed n.json --nodes 4")).is_err());
        assert!(parse(args("--distributed n.json --distributed m.json")).is_err());
    }
}
