//! Timing of test-format runs, one JSON object per line, appended as it happens.
//!
//! A test's life has phases the harness can see: setup (from `set_test_name` to `test_start`,
//! which is the scenario building its blocks), boot (from `test_start` to the first instruction:
//! node start-up and waiting for the tip), then each instruction. Rows are written as they
//! happen, so a test that hangs still leaves its partial timeline, and a missing `test` row is
//! itself the sign that it never finished.
//!
//! Rows go to `$CROSSLINK_TEST_TIMINGS` when set (a file path, or `off`), otherwise to
//! `target/crosslink-timings/<$CROSSLINK_TEST_RUN_ID, or "adhoc">.jsonl` in the workspace.

use std::{
    io::Write,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

use serde_json::json;
use zcash_primitives::bft::ZcashCrosslinkParameters;

// Its own lock, taken only briefly and never across an await or the shutdown call, so timing
// can't join the TEST_* locks in a deadlock like the one the end-of-test assert once caused.
struct Clock {
    setup_begin: Option<Instant>,
    boot_begin: Option<Instant>,
    first_instr: Option<Instant>,
    instrs_us: u64,
    instr_count: u64,
    failed_count: u64,
}

static CLOCK: Mutex<Clock> = Mutex::new(Clock {
    setup_begin: None,
    boot_begin: None,
    first_instr: None,
    instrs_us: 0,
    instr_count: 0,
    failed_count: 0,
});

/// Called as a test body starts, before it builds any blocks.
pub fn mark_setup_begin() {
    CLOCK.lock().unwrap().setup_begin = Some(Instant::now());
}

/// Called as the harness starts booting the node.
pub fn mark_boot_begin() {
    CLOCK.lock().unwrap().boot_begin = Some(Instant::now());
}

/// One handled instruction. `outcome` is the last check it made, as (condition, message); an
/// instruction that checks nothing (a roster force-include) has none. `failed` is whether it
/// recorded a test failure.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record_instr(
    test: &str,
    index: usize,
    kind: &str,
    should_fail: bool,
    start: Instant,
    outcome: Option<(bool, String)>,
    failed: bool,
    height: Option<u32>,
    data_bytes: usize,
) {
    let duration = start.elapsed();
    let boot_begin = {
        let mut clock = CLOCK.lock().unwrap();
        clock.first_instr.get_or_insert(start);
        clock.instrs_us += micros(duration);
        clock.instr_count += 1;
        clock.failed_count += u64::from(failed);
        clock.boot_begin
    };
    let (accepted, message) = match outcome {
        Some((accepted, message)) => (Some(accepted), Some(message)),
        None => (None, None),
    };
    append(json!({
        "type": "instr",
        "run": run_id(),
        "test": test,
        "index": index,
        "kind": kind,
        "should_fail": should_fail,
        "accepted": accepted,
        "failed": failed,
        "message": message,
        "start_ms": boot_begin.map(|boot| start.saturating_duration_since(boot).as_secs_f64() * 1000.0),
        "us": micros(duration),
        "height": height,
        "data_bytes": data_bytes,
    }));
}

/// The whole test, written once its instructions are done and before the pass/fail assert, so
/// a failing test still gets its row.
pub(crate) fn record_test_end(test: &str, params: &ZcashCrosslinkParameters, passed: bool) {
    let now = Instant::now();
    let (setup_us, boot_us, total_us, instrs_us, instr_count, failed_count) = {
        let clock = CLOCK.lock().unwrap();
        let between = |from: Option<Instant>, to: Option<Instant>| match (from, to) {
            (Some(from), Some(to)) => Some(micros(to.saturating_duration_since(from))),
            _ => None,
        };
        (
            between(clock.setup_begin, clock.boot_begin),
            between(clock.boot_begin, clock.first_instr),
            between(clock.setup_begin.or(clock.boot_begin), Some(now)),
            clock.instrs_us,
            clock.instr_count,
            clock.failed_count,
        )
    };
    let (commit, dirty) = git_state();
    append(json!({
        "type": "test",
        "run": run_id(),
        "commit": commit,
        "dirty": dirty,
        "test": test,
        "passed": passed,
        "setup_us": setup_us,
        "boot_us": boot_us,
        "instrs_us": instrs_us,
        "instr_count": instr_count,
        "failed_count": failed_count,
        "total_us": total_us,
        "sigma": params.bc_confirmation_depth_sigma,
        "bootstrap": format!("{:?}", params.bootstrap),
        "staking_period": params.staking.period,
        "staking_day_window": params.staking.day_window,
        "staking_action_delay": params.staking.action_delay,
    }));
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn run_id() -> String {
    std::env::var("CROSSLINK_TEST_RUN_ID").unwrap_or_else(|_| "adhoc".to_string())
}

/// Whether rows are written at all. A node loading a chain prefix file runs the test format's
/// instruction reader but is not a test run, so it turns this off.
pub(crate) static RECORD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

fn path() -> Option<PathBuf> {
    if !RECORD.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    match std::env::var("CROSSLINK_TEST_TIMINGS") {
        Ok(path) if path == "off" => None,
        Ok(path) => Some(PathBuf::from(path)),
        Err(_) => {
            // Tests run from their package directory, one level below the workspace target.
            let package = std::env::var_os("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .or_else(|| std::env::current_dir().ok())?;
            Some(package.join("../target/crosslink-timings").join(format!("{}.jsonl", run_id())))
        }
    }
}

fn append(row: serde_json::Value) {
    let Some(path) = path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // One write per row: rows from successive test processes append whole lines.
    let line = format!("{row}\n");
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = file.write_all(line.as_bytes());
    }
}

/// The checked-out commit and whether tracked files differ from it, so rows from different
/// builds can be told apart. `None` when git isn't available.
fn git_state() -> (Option<String>, Option<bool>) {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let commit = git(&["rev-parse", "--short", "HEAD"]);
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).map(|status| !status.is_empty());
    (commit, dirty)
}
