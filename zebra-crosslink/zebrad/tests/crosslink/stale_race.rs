//! How often a node that receives both branches of a fork ends on the stale one, by how many
//! blocks each branch has.
//!
//! Both branches leave a common prefix. The finalized branch is followed by BFT decisions, and
//! each of its blocks cites the newest decision it may, as live miners do. The stale branch's
//! blocks all repeat the fork tip's pointer, so they never need a decision. Both branches have
//! equal difficulty, so work is proportional to block count.
//!
//! A trial is a pair of branch lengths and an arrival order, and its outcome is the node's tip
//! once everything has arrived. Every prefix of a delivery is itself a whole trial, so one node
//! run checks the tip after each block and measures a row of trials: with every stale block
//! delivered first, the tip after `b` finalized blocks is the outcome of the trial with `b`
//! finalized blocks. Weighting the measured outcomes by the chance of each pair of lengths gives
//! the chance of ending on the stale branch for any hashrate split.
//!
//! Run `crosslink_stale_race`; replay one run with `crosslink_stale_race_one`. Results append to
//! `stale-race.csv` in the output directory, one line per trial.

use super::*;

/// One node run: an arrival order, and which branch's length it holds fixed at `k` while the
/// other grows one block at a time.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sweep {
    /// `k` stale blocks, then finalized blocks with their decisions: v15's sync, where a finalized
    /// block waits for its decision and the stale branch is admitted in one burst.
    StaleFirst,
    /// `k` finalized blocks with their decisions, then stale blocks.
    FinalizedFirst,
    /// Both branches by height, in a random order within each height; the stale branch stops at
    /// `k` and the finalized one runs on.
    TogetherRow,
    /// As `TogetherRow`, with the finalized branch stopping at `k` and the stale one running on.
    TogetherColumn,
}

const SWEEPS: [(Sweep, &str, &str); 4] = [
    (Sweep::StaleFirst, "stale_first", "stale_first"),
    (Sweep::FinalizedFirst, "finalized_first", "finalized_first"),
    (Sweep::TogetherRow, "together_row", "together"),
    (Sweep::TogetherColumn, "together_column", "together"),
];

fn sweep_name(sweep: Sweep) -> &'static str {
    SWEEPS.iter().find(|s| s.0 == sweep).expect("every sweep is named").1
}

fn sweep_arrival(sweep: Sweep) -> &'static str {
    SWEEPS.iter().find(|s| s.0 == sweep).expect("every sweep is named").2
}

fn sweep_from_name(name: &str) -> Sweep {
    SWEEPS.iter().find(|s| s.1 == name).unwrap_or_else(|| panic!("unknown sweep {name:?}")).0
}

/// Blocks both branches share past `bonded_start`'s, so the fork tip cites a decision.
const PREFIX_BLOCKS: usize = 8;

/// splitmix64.
struct SimRng(u64);

impl SimRng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// A chain followed by BFT: `blocks[0]` is `bonded_start`'s last block, and decision `k`
/// finalizes `blocks[k - 1]` (the bond block, for the genesis) with headers `blocks[k..k + 3]`.
/// Block `m` cites decision `m - 3`, the newest whose snapshot is σ + 1 below it.
struct CitedChain {
    gen: BlockGen,
    blocks: Vec<Arc<Block>>,
    pos_h: u32,
    fat_ptr: FatPointerToBftBlock,
}

impl CitedChain {
    /// The next block, and the decision its arrival makes possible.
    fn next(&mut self, miner: &Address) -> (Arc<Block>, Option<BftBlockAndFatPointerToItWrap>) {
        let m = self.blocks.len();
        self.gen.next_block(miner);
        let block = if m >= 3 { point_tip_at_bft(&mut self.gen, &self.fat_ptr) } else { self.gen.tip.clone() };
        self.blocks.push(block.clone());
        let decision = (m >= 2).then(|| next_signed_pos(&mut self.pos_h, &mut self.fat_ptr, &self.blocks[m - 2..m + 1]));
        (block, decision)
    }
}

fn push_finalized(tf: &mut TF, (block, decision): &(Arc<Block>, Option<BftBlockAndFatPointerToItWrap>)) {
    tf.push_instr_load_pow(block, ANY_OUTCOME);
    // A force-feed has no outcome-agnostic flag. Once the node has committed the stale branch, it
    // refuses these, and the refusal is only recorded.
    if let Some(decision) = decision {
        push_decided_pos(tf, decision, 0);
    }
}

/// A tip check: the trial with `stale_n` and `finalized_n` blocks ends on `expect` if the check
/// holds, and its failure message names the tip otherwise.
struct Check {
    stale_n: usize,
    finalized_n: usize,
    expect: BlockHash,
    expect_label: &'static str,
    other: BlockHash,
    other_label: &'static str,
}

/// Branch tips after `stale_n` and `finalized_n` blocks. With none on a branch its tip is the fork
/// tip, and a node still there with blocks on the other branch took neither, which is "other".
struct Tips<'a> {
    fork: BlockHash,
    stale: &'a [Arc<Block>],
    finalized: &'a [(Arc<Block>, Option<BftBlockAndFatPointerToItWrap>)],
}

fn push_check(tf: &mut TF, checks: &mut Vec<Check>, tips: &Tips, stale_n: usize, finalized_n: usize, expect_stale: bool) {
    if stale_n == 0 && finalized_n == 0 {
        return;
    }
    let stale = if stale_n == 0 { (tips.fork, "other") } else { (tips.stale[stale_n - 1].hash(), "stale") };
    let finalized = if finalized_n == 0 { (tips.fork, "finalized") } else { (tips.finalized[finalized_n - 1].0.hash(), "finalized") };
    let ((expect, expect_label), (other, other_label)) = if expect_stale { (stale, finalized) } else { (finalized, stale) };
    tf.push_instr_expect_pow_tip(&expect, 0);
    checks.push(Check { stale_n, finalized_n, expect, expect_label, other, other_label });
}

struct SweepRun {
    checks: Vec<Check>,
    bytes: Vec<u8>,
}

/// The run for `sweep` at `k`, out to `extent` blocks on the growing branch. `seed` picks the
/// stale branch's miner, and so its hashes and how ties break, and the order within each height.
fn sweep_run(sweep: Sweep, k: usize, extent: usize, seed: u64) -> SweepRun {
    let mut rng = SimRng(seed);
    let (stale_len, finalized_len) = match sweep {
        Sweep::StaleFirst | Sweep::TogetherRow => (k, extent),
        Sweep::FinalizedFirst | Sweep::TogetherColumn => (extent, k),
    };

    let mut tf = TF::new(&HARNESS_PARAMETERS);
    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut stale_key = [1u8; 20];
    stale_key[..8].copy_from_slice(&rng.next().to_le_bytes());
    let stale_miner = Address::Transparent(zcash_transparent::address::TransparentAddress::PublicKeyHash(stale_key));
    let gen = bonded_start(&mut tf, network);
    let mut chain = CitedChain { blocks: vec![gen.tip.clone()], gen, pos_h: 0, fat_ptr: FatPointerToBftBlock::null() };

    for _ in 0..PREFIX_BLOCKS {
        let next = chain.next(&miner);
        push_finalized(&mut tf, &next);
    }

    let fork = chain.gen.tip.hash();
    let mut stale_gen = chain.gen.clone();
    let fork_ptr = chain.gen.tip.header.fat_pointer_to_bft_block.clone();
    let stale: Vec<Arc<Block>> = (0..stale_len)
        .map(|_| {
            stale_gen.next_block(&stale_miner);
            point_tip_at_bft(&mut stale_gen, &fork_ptr)
        })
        .collect();
    let finalized: Vec<_> = (0..finalized_len).map(|_| chain.next(&miner)).collect();
    let tips = Tips { fork, stale: &stale, finalized: &finalized };
    let mut checks = Vec::new();

    match sweep {
        Sweep::StaleFirst => {
            stale.iter().for_each(|b| tf.push_instr_load_pow(b, ANY_OUTCOME));
            push_check(&mut tf, &mut checks, &tips, k, 0, false);
            for (j, f) in finalized.iter().enumerate() {
                push_finalized(&mut tf, f);
                push_check(&mut tf, &mut checks, &tips, k, j + 1, false);
            }
        }
        Sweep::FinalizedFirst => {
            finalized.iter().for_each(|f| push_finalized(&mut tf, f));
            push_check(&mut tf, &mut checks, &tips, 0, k, true);
            for (i, b) in stale.iter().enumerate() {
                tf.push_instr_load_pow(b, ANY_OUTCOME);
                push_check(&mut tf, &mut checks, &tips, i + 1, k, true);
            }
        }
        Sweep::TogetherRow | Sweep::TogetherColumn => {
            for h in 0..extent {
                let stale_before = rng.next() & 1 == 0;
                if stale_before && h < stale.len() {
                    tf.push_instr_load_pow(&stale[h], ANY_OUTCOME);
                }
                if h < finalized.len() {
                    push_finalized(&mut tf, &finalized[h]);
                }
                if !stale_before && h < stale.len() {
                    tf.push_instr_load_pow(&stale[h], ANY_OUTCOME);
                }
                if h + 1 >= k {
                    if sweep == Sweep::TogetherRow {
                        push_check(&mut tf, &mut checks, &tips, k, h + 1, false);
                    } else {
                        push_check(&mut tf, &mut checks, &tips, h + 1, k, true);
                    }
                }
            }
        }
    }

    SweepRun { checks, bytes: tf.write_to_bytes() }
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Ok(v) => v.parse().unwrap_or_else(|_| panic!("{name}={v:?} does not parse")),
        Err(_) => default,
    }
}

fn out_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("crosslink-stale-race");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// One run: STALE_RACE_SWEEP, STALE_RACE_K, STALE_RACE_EXTENT and STALE_RACE_SEED. Prints its
/// checks first, for the campaign to read the outcomes against.
#[test]
#[ignore]
fn crosslink_stale_race_one() {
    set_test_name(function_name!());
    let sweep = sweep_from_name(&env_or("STALE_RACE_SWEEP", "together_row".to_string()));
    let k: usize = env_or("STALE_RACE_K", 10);
    let extent: usize = env_or("STALE_RACE_EXTENT", 160);
    let seed: u64 = env_or("STALE_RACE_SEED", 0);
    let run = sweep_run(sweep, k, extent, seed);
    for c in &run.checks {
        println!(
            "stale-race-check: {} {} {} {} {} {}",
            c.stale_n, c.finalized_n, c.expect, c.expect_label, c.other, c.other_label
        );
    }
    test_bytes(run.bytes);
}

/// The 64 hex digits after the first occurrence of `marker` in `s`, past any quoting.
fn hash_after<'a>(s: &'a str, marker: &str) -> Option<&'a str> {
    let rest = &s[s.find(marker)? + marker.len()..];
    let start = rest.find(|c: char| c.is_ascii_hexdigit())?;
    rest.get(start..start + 64).filter(|h| h.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Each check's outcome: the expected tip's label if the check held, the other's if the node was
/// on the other tip, "other" if neither. None if the run did not finish, since then a check with
/// no failure message may never have run.
fn read_run(text: &str) -> Option<Vec<(usize, usize, &str)>> {
    if !text.contains("Test done, shutting down") && !text.contains("failed test ") {
        return None;
    }
    // Expected tip to actual tip, from every tip check's failure message.
    let mut actual = std::collections::HashMap::new();
    for piece in text.split("PoW tip: expected ").skip(1) {
        if let (Some(expect), Some(got)) = (hash_after(piece, "Hash("), hash_after(piece, "actually ")) {
            actual.insert(expect.to_string(), got.to_string());
        }
    }
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.split_once("stale-race-check: ").map(|(_, r)| r) else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        let &[stale_n, finalized_n, expect, expect_label, other, other_label] = f.as_slice() else { continue };
        let outcome = match actual.get(expect) {
            None => expect_label,
            Some(got) if got == other => other_label,
            Some(_) => "other",
        };
        out.push((stale_n.parse().ok()?, finalized_n.parse().ok()?, outcome));
    }
    Some(out)
}

/// Runs one sweep in its own process, since the harness ends every run with `exit` and binds a
/// fixed port. Returns its output, or None if it ran out of time.
fn run_sweep(envs: &[(&str, String)], log: &std::path::Path, limit_secs: u64) -> Option<String> {
    let exe = std::env::current_exe().expect("the test binary path");
    let log_file = std::fs::File::create(log).expect("run log creates");
    let mut child = std::process::Command::new(exe)
        .args(["stale_race::crosslink_stale_race_one", "--exact", "--ignored", "--nocapture"])
        .envs(envs.iter().map(|(k, v)| (*k, v.as_str())))
        .stdout(log_file.try_clone().expect("log handle clones"))
        .stderr(log_file)
        .spawn()
        .expect("run starts");
    let start = std::time::Instant::now();
    loop {
        if child.try_wait().expect("run status").is_some() {
            return Some(std::fs::read_to_string(log).unwrap_or_default());
        }
        if start.elapsed().as_secs() >= limit_secs {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// The campaign, one node at a time. STALE_RACE_KS is a comma list of the fixed branch's lengths
/// (each at most STALE_RACE_EXTENT, default 160, the growing branch's length). STALE_RACE_SWEEPS
/// is a comma list of sweep names. STALE_RACE_REPS (`a..b`, default 0..1) are repetition indices,
/// so a later run with `1..3` adds new repetitions rather than repeating these. STALE_RACE_SECS
/// (default 3600) bounds the whole run and STALE_RACE_RUN_SECS (default 600) each node. A run
/// that does not finish keeps its log.
#[test]
#[ignore]
fn crosslink_stale_race() {
    let reps: String = env_or("STALE_RACE_REPS", "0..1".to_string());
    let (a, b) = reps.split_once("..").expect("STALE_RACE_REPS is a..b");
    let (a, b): (u64, u64) = (a.parse().expect("repetition range start"), b.parse().expect("repetition range end"));
    let extent: usize = env_or("STALE_RACE_EXTENT", 160);
    let ks: Vec<usize> = env_or(
        "STALE_RACE_KS",
        "0,1,2,3,4,5,6,7,8,9,10,12,14,17,20,25,30,40,50,60,70,80,90,95,98,99,100,101,102,105,110,120,140,160".to_string(),
    )
    .split(',')
    .map(|s| s.trim().parse().expect("STALE_RACE_KS holds numbers"))
    .filter(|&k| k <= extent)
    .collect();
    let sweeps: Vec<Sweep> = env_or("STALE_RACE_SWEEPS", "stale_first,finalized_first,together_row,together_column".to_string())
        .split(',')
        .map(|s| sweep_from_name(s.trim()))
        .collect();
    let secs: u64 = env_or("STALE_RACE_SECS", 3600);
    let run_secs: u64 = env_or("STALE_RACE_RUN_SECS", 600);

    let dir = out_dir();
    let csv = dir.join("stale-race.csv");
    if !csv.exists() {
        std::fs::write(&csv, "arrival,sweep,extent,seed,stale_n,finalized_n,outcome\n").expect("results file writes");
    }
    let start = std::time::Instant::now();
    let mut ran = 0;
    'campaign: for rep in a..b {
        for &k in &ks {
            for (si, &sweep) in sweeps.iter().enumerate() {
                if start.elapsed().as_secs() >= secs {
                    break 'campaign;
                }
                let seed = rep * 1_000_000 + k as u64 * 10 + si as u64;
                let name = sweep_name(sweep);
                let log = dir.join(format!("{name}-k{k}-e{extent}-seed{seed}.log"));
                let envs = [
                    ("STALE_RACE_SWEEP", name.to_string()),
                    ("STALE_RACE_K", k.to_string()),
                    ("STALE_RACE_EXTENT", extent.to_string()),
                    ("STALE_RACE_SEED", seed.to_string()),
                ];
                let run_start = std::time::Instant::now();
                let text = run_sweep(&envs, &log, run_secs);
                let took = run_start.elapsed().as_secs_f64();
                let Some(trials) = text.as_deref().and_then(read_run) else {
                    eprintln!("stale race {name} k={k} seed {seed}: did not finish in {took:.0} s, log kept at {}", log.display());
                    continue;
                };
                let mut rows = String::new();
                let (mut stale, mut other) = (0, 0);
                for (stale_n, finalized_n, outcome) in &trials {
                    rows += &format!("{},{name},{extent},{seed},{stale_n},{finalized_n},{outcome}\n", sweep_arrival(sweep));
                    stale += (*outcome == "stale") as usize;
                    other += (*outcome == "other") as usize;
                }
                use std::io::Write;
                std::fs::OpenOptions::new().append(true).open(&csv).and_then(|mut f| f.write_all(rows.as_bytes())).expect("results append");
                if other == 0 {
                    let _ = std::fs::remove_file(&log);
                }
                eprintln!("stale race {name} k={k} seed {seed}: {} trials, {stale} stale, {other} other, {took:.1} s", trials.len());
                ran += 1;
            }
        }
    }
    eprintln!("stale race: {ran} runs in {:?}; results in {}", start.elapsed(), csv.display());
}
