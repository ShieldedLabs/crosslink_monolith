//! Scenario fuzzing: a generator turns a seed into an ordinary test file, which runs through the
//! same harness as every scenario, one node process per input.
//!
//! A run is a finding when something that must always hold does not: the node panics, stops
//! answering, or contradicts an expectation the generator could derive. Actions whose outcome
//! the generator cannot know carry ANY_OUTCOME, and whatever depends on them is no longer
//! expected of the node (the model marks it unknown) until the generator re-establishes it.
//!
//! Every decision is read from a byte stream (`Choices`), from a seeded generator here and, later,
//! from libFuzzer on a platform that has it. Past the end of the stream every choice is 0, and 0
//! is always the most ordinary option, so any prefix of an input is itself an input.
//!
//! Run a campaign with `crosslink_fuzz` (CROSSLINK_FUZZ_SEEDS=a..b, CROSSLINK_FUZZ_SECS); replay
//! one with `crosslink_fuzz_one` and CROSSLINK_FUZZ_SEED or CROSSLINK_FUZZ_FILE.

use super::*;

/// The generator's decisions, as bytes.
pub(crate) struct Choices {
    bytes: Vec<u8>,
    o: usize,
}

impl Choices {
    /// `n` bytes from `seed`, by splitmix64: deterministic across platforms and builds.
    pub(crate) fn from_seed(seed: u64, n: usize) -> Choices {
        let mut state = seed;
        let mut bytes = Vec::with_capacity(n + 8);
        while bytes.len() < n {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            bytes.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
        }
        bytes.truncate(n);
        Choices { bytes, o: 0 }
    }

    fn byte(&mut self) -> u8 {
        let b = self.bytes.get(self.o).copied().unwrap_or(0);
        self.o += 1;
        b
    }

    /// A number in `0..n`; 0 when the stream has run out.
    pub(crate) fn below(&mut self, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        let v = u16::from_le_bytes([self.byte(), self.byte()]) as usize;
        v % n
    }

    /// True `per_256` times in 256; false when the stream has run out.
    pub(crate) fn unusual(&mut self, per_256: u8) -> bool {
        let b = self.byte();
        per_256 != 0 && b > 255 - per_256
    }
}

/// A peer index RECV_POW has not used yet. A peer that served a bad block may have been
/// disconnected, so each such delivery gets its own.
const FRESH_PEERS_FROM: u64 = 100;

/// What the generator builds and what it believes the node holds.
pub(crate) struct FuzzGen {
    pub tf: TF,
    pub network: Network,
    pub key: TestKey,
    pub miner: Address,
    /// The best chain the generator built, one generator state per height: `main[i]` has the
    /// block at height `i + 1` as its tip.
    pub main: Vec<BlockGen>,
    /// Whether the node's best tip is known to be `main`'s. False after an action that could
    /// have moved it unpredictably; true again once `main` is strictly longer than anything
    /// else the node could hold.
    pub tip_known: bool,
    /// The longest chain the node might hold that is not `main`, by height, when the tip is not
    /// known.
    pub rival_height: u32,
    /// Next fresh peer index for deliveries that may get the peer disconnected.
    pub next_fresh_peer: u64,
    /// For distinct forks: each one mines to its own address.
    pub forks_made: u64,
}

impl FuzzGen {
    pub(crate) fn new(params: &zcash_primitives::bft::ZcashCrosslinkParameters) -> FuzzGen {
        let network = regtest_network(params);
        let key = TestKey::new(b"crosslink test miner");
        let miner = key.address();
        let gen = BlockGen::init_at_genesis_plus_1(network.clone(), BlockGen::REGTEST_GENESIS_HASH, &miner);
        let mut tf = TF::new(params);
        tf.push_instr_load_pow(&gen.tip, 0);
        FuzzGen { tf, network, key, miner, main: vec![gen], tip_known: true, rival_height: 0, next_fresh_peer: FRESH_PEERS_FROM, forks_made: 0 }
    }

    pub(crate) fn tip(&self) -> &BlockGen {
        self.main.last().expect("main always holds genesis + 1")
    }

    fn height(&self) -> u32 {
        self.main.len() as u32
    }

    /// Delivers a block the node must take, through LOAD_POW or a peer.
    fn deliver(&mut self, c: &mut Choices, block: &Block, flags: u32) {
        if c.unusual(64) {
            let peer = 1 + c.below(3) as u64;
            self.tf.push_instr_recv_pow(block, peer, flags);
        } else {
            self.tf.push_instr_load_pow(block, flags);
        }
    }

    fn expect_tip(&mut self) {
        if self.tip_known {
            let hash = self.tip().tip.hash();
            self.tf.push_instr_expect_pow_tip(&hash, 0);
        }
    }

    /// After `main` grew: the tip is known again once nothing else can match it.
    fn after_main_grew(&mut self) {
        if !self.tip_known && self.height() > self.rival_height {
            self.tip_known = true;
        }
    }
}

/// Extends the best chain by one valid block.
fn move_extend(g: &mut FuzzGen, c: &mut Choices) {
    let mut next = g.tip().clone();
    let block = next.next_block(&g.miner.clone());
    g.main.push(next);
    // A valid extension of the tip the node holds is accepted; when the tip is not known, the
    // node may hold a rival of equal height and treat this block as a side chain, so only the
    // verdict's arrival is checked.
    let flags = if g.tip_known { 0 } else { ANY_OUTCOME };
    g.deliver(c, &block, flags);
    g.after_main_grew();
    g.expect_tip();
}

/// A branch from `depth` blocks below the tip, `len` blocks long, mined to its own address.
fn move_fork(g: &mut FuzzGen, c: &mut Choices) {
    let max_depth = (g.height() as usize - 1).min(6);
    if max_depth == 0 {
        return move_extend(g, c);
    }
    let depth = 1 + c.below(max_depth);
    let len = 1 + c.below(depth + 2);
    g.forks_made += 1;
    let fork_miner = TestKey::new(format!("crosslink fuzz fork {}", g.forks_made).as_bytes()).address();

    let fork_at = g.main.len() - 1 - depth;
    let mut branch: Vec<BlockGen> = g.main[..=fork_at].to_vec();
    for _ in 0..len {
        let mut next = branch.last().unwrap().clone();
        let block = next.next_block(&fork_miner);
        branch.push(next);
        // A valid block on a known parent is taken, as a side chain or as the new best.
        g.deliver(c, &block, if g.tip_known { 0 } else { ANY_OUTCOME });
    }

    let (main_h, branch_h) = (g.main.len(), branch.len());
    if branch_h > main_h {
        g.main = branch;
        g.after_main_grew();
    } else if branch_h == main_h {
        // Equal work: which tip wins is a tie-break the model does not assume.
        g.tip_known = false;
        g.rival_height = g.rival_height.max(branch_h as u32);
    }
    g.expect_tip();
}

/// The next block, changed so that it must be refused, delivered, then the node checked to be
/// alive with its tip unmoved.
fn move_invalid_block(g: &mut FuzzGen, c: &mut Choices) {
    let mut scratch = g.tip().clone();
    let valid = scratch.next_block(&g.miner.clone());
    let mut block = valid.as_ref().clone();
    let what = c.below(3);
    match what {
        // A merkle root that does not cover the transactions.
        0 => {
            let mut header = block.header.as_ref().clone();
            header.merkle_root.0[0] ^= 1;
            block.header = Arc::new(header);
        }
        // No transactions at all, so no coinbase.
        1 => block.transactions.clear(),
        // The coinbase twice.
        _ => {
            let coinbase = block.transactions[0].clone();
            block.transactions.push(coinbase);
        }
    }
    let peer = g.next_fresh_peer;
    g.next_fresh_peer += 1;
    // A synthetic peer advertises a block by its coinbase height, so one with no coinbase can
    // only be loaded.
    if what != 1 && c.unusual(128) {
        g.tf.push_instr_recv_pow(&block, peer, SHOULD_FAIL);
    } else {
        g.tf.push_instr_load_pow(&block, SHOULD_FAIL);
    }
    g.tf.push_instr_expect_node_alive(0);
    g.expect_tip();
}

/// The next block with random bytes flipped: the node may take it or not, but must keep running.
fn move_mangled_block(g: &mut FuzzGen, c: &mut Choices) {
    let mut scratch = g.tip().clone();
    let valid = scratch.next_block(&g.miner.clone());
    let mut bytes = valid.zcash_serialize_to_vec().expect("a generated block serializes");
    let flips = 1 + c.below(4);
    for _ in 0..flips {
        let i = c.below(bytes.len());
        bytes[i] ^= 1 << c.below(8);
    }
    // A synthetic peer advertises a block by its coinbase height and LOAD_POW hands the node a
    // parsed block, so bytes that no longer parse to a block with one have no doorway.
    // TODO: a raw wire doorway (RECV_STP_PACKET with BLOCK_CHUNKs) for bytes that don't parse.
    // Only loaded, not offered by a peer: a node need not fetch an advertised block (one at a
    // height it holds adds no work), and an offer it ignores costs the instruction's whole wait.
    let parses = Block::zcash_deserialize(&bytes[..]).is_ok_and(|b| b.coinbase_height().is_some());
    if !parses {
        return;
    }
    g.tf.push_instr_load_pow_bytes(&bytes, ANY_OUTCOME);
    g.tf.push_instr_expect_node_alive(0);
    // Taken, it is a block at the tip's next height that `main` does not have.
    g.tip_known = false;
    g.rival_height = g.rival_height.max(g.height() + 1);
}

type Move = fn(&mut FuzzGen, &mut Choices);

/// The moves, most ordinary first: choice 0 picks extend.
const MOVES: &[Move] = &[move_extend, move_fork, move_invalid_block, move_mangled_block];

/// The test file for `c`. Each input enables a subset of the moves, so rare combinations are not
/// drowned out by common ones.
pub(crate) fn generate(c: &mut Choices) -> Vec<u8> {
    let mut g = FuzzGen::new(&HARNESS_PARAMETERS);
    let enabled: Vec<Move> = MOVES.iter().enumerate().filter(|(i, _)| *i == 0 || !c.unusual(64)).map(|(_, m)| *m).collect();
    let moves_n = 4 + c.below(28);
    for _ in 0..moves_n {
        let m = enabled[c.below(enabled.len())];
        m(&mut g, c);
    }
    g.tf.push_instr_expect_node_alive(0);
    g.tf.push_instr_expect_staking_replay_matches(0);
    g.tf.write_to_bytes()
}

fn fuzz_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("crosslink-fuzz");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// One input: CROSSLINK_FUZZ_FILE, or the input for CROSSLINK_FUZZ_SEED. Writes the file it ran
/// to the fuzz directory first, so a crash still leaves it behind.
#[test]
#[ignore]
fn crosslink_fuzz_one() {
    set_test_name(function_name!());
    let bytes = match (std::env::var("CROSSLINK_FUZZ_FILE"), std::env::var("CROSSLINK_FUZZ_SEED")) {
        (Ok(path), _) => std::fs::read(&path).expect("CROSSLINK_FUZZ_FILE reads"),
        (_, Ok(seed)) => {
            let seed: u64 = seed.parse().expect("CROSSLINK_FUZZ_SEED is a number");
            let bytes = generate(&mut Choices::from_seed(seed, 4096));
            std::fs::write(fuzz_dir().join(format!("seed-{seed}.zeccltf")), &bytes).expect("fuzz input writes");
            bytes
        }
        _ => panic!("set CROSSLINK_FUZZ_SEED or CROSSLINK_FUZZ_FILE"),
    };
    test_bytes(bytes);
}

/// How one child run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Pass,
    /// What failed, with numbers and hashes removed, so the same failure after minimizing (which
    /// renumbers instructions) still compares equal.
    Fail(String),
    Hang,
}

/// Seconds a child may run before it is a hang. Every harness wait is bounded at 30 s, so a run
/// far past its longest normal time is not still working.
const CHILD_LIMIT_SECS: u64 = 150;

fn run_child(env: (&str, String), log: &std::path::Path) -> Verdict {
    let exe = std::env::current_exe().expect("the test binary path");
    let log_file = std::fs::File::create(log).expect("child log creates");
    let mut child = std::process::Command::new(exe)
        .args(["fuzz::crosslink_fuzz_one", "--exact", "--ignored", "--nocapture"])
        .env(env.0, env.1)
        .stdout(log_file.try_clone().expect("log handle clones"))
        .stderr(log_file)
        .spawn()
        .expect("child starts");
    let start = std::time::Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("child status") {
            let text = std::fs::read_to_string(log).unwrap_or_default();
            return if status.success() && text.contains("Test done, shutting down") { Verdict::Pass } else { Verdict::Fail(signature(&text)) };
        }
        if start.elapsed().as_secs() >= CHILD_LIMIT_SECS {
            let _ = child.kill();
            let _ = child.wait();
            return Verdict::Hang;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// The first failure in a child's output, without the numbers that change from run to run.
fn signature(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| l.contains("failed test "))
        .map(|l| match l.find(": [(") {
            // The first (index, "message") pair's message.
            Some(i) => l[i..].splitn(2, ", \"").nth(1).unwrap_or(l).to_string(),
            None => l.to_string(),
        })
        .or_else(|| text.lines().skip_while(|l| !l.contains("panicked at")).nth(1).map(str::to_string))
        .unwrap_or_else(|| "exited without a failure message".to_string());
    // Each run of hex characters holding a digit (a number, a hash) becomes '#'.
    let line: Vec<char> = line.chars().take(200).collect();
    let mut out = String::new();
    let mut i = 0;
    while i < line.len() {
        let mut j = i;
        while j < line.len() && line[j].is_ascii_hexdigit() {
            j += 1;
        }
        if j > i && line[i..j].iter().any(char::is_ascii_digit) {
            out.push('#');
            i = j;
        } else {
            out.push(line[i]);
            i += 1;
        }
    }
    out
}

/// The file with instructions `drop` removed, its data compacted.
fn without(bytes: &[u8], drop: std::ops::Range<usize>) -> Vec<u8> {
    let tf = TF::read_from_bytes(bytes).expect("a fuzz input reads back");
    let mut out = TF { instrs: Vec::new(), data: Vec::new() };
    for (i, instr) in tf.instrs.iter().enumerate() {
        if !drop.contains(&i) {
            out.push_instr_ex(instr.kind, instr.flags, instr.data_slice(bytes), instr.val);
        }
    }
    out.write_to_bytes()
}

/// Deletes instructions while `keep` still holds of the run: halves, then smaller pieces, never
/// the SET_PARAMS that opens the file. Stops after `attempts` runs.
fn minimize(bytes: Vec<u8>, keep: &dyn Fn(&Verdict, &std::path::Path) -> bool, name: &str, attempts: usize) -> Vec<u8> {
    let mut best = bytes;
    let mut tried = 0;
    let instrs_n = |b: &[u8]| TF::read_from_bytes(b).map(|tf| tf.instrs.len()).unwrap_or(0);
    let mut chunk = (instrs_n(&best) / 2).max(1);
    let dir = fuzz_dir();
    while chunk >= 1 && tried < attempts {
        let mut i = 1;
        let mut progressed = false;
        while i < instrs_n(&best) && tried < attempts {
            let candidate = without(&best, i..(i + chunk).min(instrs_n(&best)));
            let path = dir.join(format!("{name}.try.zeccltf"));
            std::fs::write(&path, &candidate).expect("candidate writes");
            tried += 1;
            let log = dir.join(format!("{name}.try.log"));
            let got = run_child(("CROSSLINK_FUZZ_FILE", path.display().to_string()), &log);
            if keep(&got, &log) {
                best = candidate;
                progressed = true;
            } else {
                i += chunk;
            }
        }
        if !progressed {
            if chunk == 1 {
                break;
            }
            chunk /= 2;
        }
    }
    best
}

/// A campaign: CROSSLINK_FUZZ_SEEDS (`a..b`, default 0..20) within CROSSLINK_FUZZ_SECS (default
/// 300, inside nextest's per-test bound; run the test binary directly to go longer). Passing
/// inputs are deleted; a failing one is kept with its log, minimized (CROSSLINK_FUZZ_MINIMIZE
/// attempts, default 40, 0 for none), and the campaign fails listing what it found.
#[test]
#[ignore]
fn crosslink_fuzz() {
    let seeds = std::env::var("CROSSLINK_FUZZ_SEEDS").unwrap_or_else(|_| "0..20".to_string());
    let (a, b) = seeds.split_once("..").expect("CROSSLINK_FUZZ_SEEDS is a..b");
    let (a, b): (u64, u64) = (a.parse().expect("seed range start"), b.parse().expect("seed range end"));
    let secs: u64 = std::env::var("CROSSLINK_FUZZ_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(300);
    let attempts: usize = std::env::var("CROSSLINK_FUZZ_MINIMIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let dir = fuzz_dir();
    let start = std::time::Instant::now();
    let mut findings = Vec::new();
    let mut ran = 0;
    for seed in a..b {
        if start.elapsed().as_secs() >= secs {
            break;
        }
        let log = dir.join(format!("seed-{seed}.log"));
        let verdict = run_child(("CROSSLINK_FUZZ_SEED", seed.to_string()), &log);
        ran += 1;
        let input = dir.join(format!("seed-{seed}.zeccltf"));
        eprintln!("fuzz seed {seed}: {verdict:?}");
        if verdict == Verdict::Pass {
            let _ = std::fs::remove_file(&input);
            let _ = std::fs::remove_file(&log);
            continue;
        }
        let bytes = std::fs::read(&input).expect("the failing input was written");
        if attempts > 0 {
            let same = |got: &Verdict, _: &std::path::Path| *got == verdict;
            let min = minimize(bytes.clone(), &same, &format!("seed-{seed}"), attempts);
            std::fs::write(dir.join(format!("seed-{seed}.min.zeccltf")), &min).expect("minimized input writes");
            eprintln!("fuzz seed {seed}: minimized {} -> {} bytes", bytes.len(), min.len());
        }
        findings.push((seed, verdict));
    }
    eprintln!("fuzz: {ran} inputs, {} findings, in {:?}; files in {}", findings.len(), start.elapsed(), dir.display());
    assert!(findings.is_empty(), "fuzz findings: {findings:?}");
}

/// Minimizes the file CROSSLINK_FUZZ_FILE, keeping every deletion after which the run still fails
/// and its output still contains CROSSLINK_FUZZ_KEEP: for a finding whose first failure is not the
/// one of interest. Writes `<file>.min.zeccltf`. CROSSLINK_FUZZ_MINIMIZE caps the runs (default 60).
#[test]
#[ignore]
fn crosslink_fuzz_minimize() {
    let path = PathBuf::from(std::env::var("CROSSLINK_FUZZ_FILE").expect("set CROSSLINK_FUZZ_FILE"));
    let keep_text = std::env::var("CROSSLINK_FUZZ_KEEP").expect("set CROSSLINK_FUZZ_KEEP");
    // How many times it must appear, for a failure told apart by repeating (default 1).
    let keep_n: usize = std::env::var("CROSSLINK_FUZZ_KEEP_N").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let attempts: usize = std::env::var("CROSSLINK_FUZZ_MINIMIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(60);
    let bytes = std::fs::read(&path).expect("CROSSLINK_FUZZ_FILE reads");
    let keep = |got: &Verdict, log: &std::path::Path| {
        *got != Verdict::Pass && std::fs::read_to_string(log).is_ok_and(|text| text.matches(keep_text.as_str()).count() >= keep_n)
    };
    let name = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let min = minimize(bytes.clone(), &keep, &name, attempts);
    let out = path.with_extension("min.zeccltf");
    std::fs::write(&out, &min).expect("minimized input writes");
    let instrs_n = TF::read_from_bytes(&min).map(|tf| tf.instrs.len()).unwrap_or(0);
    eprintln!("minimized {} -> {} bytes, {instrs_n} instructions: {}", bytes.len(), min.len(), out.display());
}

// # Findings
//
// Each is a minimized fuzz input under crosslink-test-data/fuzz/. One still open is #[ignore]d
// with its reason, so the suite stays green until it is fixed; its control shows the input means
// what the finding says.

fn finding(name: &str) -> Vec<u8> {
    let path = format!("{}/../crosslink-test-data/fuzz/{name}.zeccltf", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|err| panic!("{path}: {err}"))
}

/// Instruction 4 of the input: block 4 with the same header, and so the same hash, as the real
/// one, but a body the node refuses (its auth data no longer matches the header's commitment).
const POISON_INSTR: usize = 4;

/// Found by the fuzzer, 2026-10-16 (seed 3). Blocks 1 to 3, then a copy of block 4 whose body was
/// changed without changing its hash, which the node refuses; then the real block 4, which it
/// commits, and the real block 5, which it refuses with the copy's error, word for word, though
/// block 5 is valid (see the control). Everything built on block 5 then waits for a parent that
/// never commits: a refused body leaves something behind, keyed by the hash it shares with the
/// real block, that decides the next block's verdict.
///
/// Cause (confirmed 2026-10-16): `WriteBlockWorkerTask::handle_commit` (zebra-state
/// service/write.rs, from upstream Zebra) stores a failed block's error in `parent_error_map`
/// under its hash, so that its children are refused without being validated. The copy's error is
/// stored under block 4's hash. The real block 4 still commits, since only its parent's hash is
/// looked up, but block 5's parent is block 4's hash, so block 5 gets the copy's error. A hash
/// commits only to the header, and the copy failed for its body, so the error says nothing about
/// block 4's children. Removing the hash from the map when a block with it commits makes this
/// pass with the suite otherwise unchanged; not storing errors that only show a body does not
/// match its header would also work.
#[test]
#[ignore = "open finding: a refused block body poisons the next block's commit; ingest code, needs a human look before a fix"]
fn crosslink_fuzz_finding_refused_body_poisons_the_next_block() {
    set_test_name(function_name!());
    test_bytes(finding("refused_body_poisons_the_next_block"));
}

/// The same input without the refused copy: blocks 1 to 5 commit and 5 is the tip, so block 5 is
/// valid, and in the finding it is the copy that makes the node refuse it.
#[test]
fn crosslink_fuzz_finding_refused_body_poisons_the_next_block_control() {
    set_test_name(function_name!());
    let bytes = finding("refused_body_poisons_the_next_block");
    let tf = TF::read_from_bytes(&bytes).expect("the finding reads");
    let (poison, real) = (tf.instrs[POISON_INSTR], tf.instrs[POISON_INSTR + 1]);
    let (poison, real) = (poison.data_slice(&bytes), real.data_slice(&bytes));
    let (poison_block, real_block) = (Block::zcash_deserialize(poison).unwrap(), Block::zcash_deserialize(real).unwrap());
    assert_eq!(poison_block.hash(), real_block.hash(), "the copy shares the real block 4's hash");
    assert_ne!(poison, real, "but not its bytes");
    test_bytes(without(&bytes, POISON_INSTR..POISON_INSTR + 1));
}

// # Directed: a refused body must not condemn its header
//
// A block is named by its header's hash, which commits to the transactions only through the
// merkle root and the auth commitment. A copy of a real block with a different body under the
// same header has the real block's hash, and is refused for its body. If the node then condemns
// the hash, anyone who relays such a copy first keeps the real block out (the mutated-block
// class, CVE-2012-2459 in Bitcoin). Each scenario: three blocks, the copy (refused), the real
// block (must be taken), and one more on top.

fn refused_body_then_real(mutate: fn(&mut Block)) -> Vec<u8> {
    let mut c = Choices::from_seed(0, 0);
    let mut g = FuzzGen::new(&HARNESS_PARAMETERS);
    for _ in 0..3 {
        move_extend(&mut g, &mut c);
    }
    let mut next = g.tip().clone();
    let real = next.next_block(&g.miner.clone());
    let mut copy = real.as_ref().clone();
    mutate(&mut copy);
    assert_eq!(copy.hash(), real.hash(), "the copy keeps the real block's header");
    g.tf.push_instr_load_pow(&copy, SHOULD_FAIL);
    g.tf.push_instr_expect_node_alive(0);
    g.tf.push_instr_load_pow(&real, 0);
    g.main.push(next);
    g.expect_tip();
    move_extend(&mut g, &mut c);
    g.tf.write_to_bytes()
}

#[test]
fn crosslink_fuzz_directed_empty_body_does_not_condemn_the_real_block() {
    set_test_name(function_name!());
    test_bytes(refused_body_then_real(|b| b.transactions.clear()));
}

#[test]
fn crosslink_fuzz_directed_repeated_coinbase_does_not_condemn_the_real_block() {
    set_test_name(function_name!());
    test_bytes(refused_body_then_real(|b| {
        let coinbase = b.transactions[0].clone();
        b.transactions.push(coinbase);
    }));
}
