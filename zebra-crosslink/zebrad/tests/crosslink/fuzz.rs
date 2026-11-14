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
//! one with `crosslink_fuzz_one` and CROSSLINK_FUZZ_SEED or CROSSLINK_FUZZ_FILE. The directed
//! scenarios draw their choices from CROSSLINK_FUZZ_SEED too, and from a fresh random seed each
//! run when it is unset.

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

/// A peer index LOAD_POW via STP has not used yet. A peer that served a bad block may have been
/// disconnected, so each such delivery gets its own.
const FRESH_PEERS_FROM: u64 = 100;

/// The deepest a fork reaches below `main`'s tip.
const FORK_DEPTH_MAX: usize = 6;

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

    /// The parameters the node runs, for the staking calendar.
    pub params: zcash_primitives::bft::ZcashCrosslinkParameters,
    /// Spends submitted to the mempool and not yet mined.
    pub pending: Vec<FuzzPending>,
    /// Spends mined on `main`, for double spends of them.
    pub mined_spends: Vec<FuzzMinedSpend>,
    /// Coins whose spend was mined on a block a reorg dropped: the node may have put that spend
    /// back in its mempool, so what it does with another spend of the coin is not known.
    pub unsure_coins: std::collections::HashSet<zebra_chain::transparent::OutPoint>,
    /// Every bond the generator has tried to create, whether or not the node took it.
    pub bonds: Vec<FuzzBond>,
    /// Staking actions mined on `main`, for replays of them.
    pub mined_staking: Vec<FuzzMinedStaking>,
    /// For distinct transactions: conflicting spends and bond seeds are numbered by this.
    pub txs_made: u64,
    /// `rival_height` and `main`'s tip when the transaction model was last brought up to date: a
    /// change in either since means the node may have switched chains, which resets its mempool.
    pub model_rival_height: u32,
    pub model_tip: (u32, BlockHash),
}

/// A spend the node took into its mempool.
#[derive(Clone)]
pub(crate) struct FuzzPending {
    pub tx: Arc<Transaction>,
    pub coin: (zebra_chain::transparent::OutPoint, zebra_chain::transparent::Utxo),
    /// Whether the node is known to hold it still.
    pub known: bool,
}

/// A spend mined on `main` at `height`, in the block with hash `block`.
#[derive(Clone)]
pub(crate) struct FuzzMinedSpend {
    pub coin: (zebra_chain::transparent::OutPoint, zebra_chain::transparent::Utxo),
    pub height: u32,
    pub block: BlockHash,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FuzzBondState {
    Absent,
    Active,
    Unbonding,
    Withdrawn,
}

/// A bond and what the generator believes the node holds for it: `state`, entered at `at`
/// (height and block hash on `main`), and the index of the `fuzz_finalizer` it targets, which
/// its last retarget, if any, set at `target_at`. A retarget does not move `at`, since the
/// staking action delay counts from the bond's creation and its unbonding only. `version` is
/// the bond version its next action spends under FeaturenetUpgrade3: the txid of the create or
/// of the last action that spent it, which also moved `at` or `target_at`.
#[derive(Clone)]
pub(crate) struct FuzzBond {
    pub seed: Vec<u8>,
    pub key: [u8; 32],
    pub state: FuzzBondState,
    pub at: (u32, BlockHash),
    pub known: bool,
    pub target: usize,
    pub target_at: (u32, BlockHash),
    pub version: [u8; 32],
}

/// A staking action on bond `bond_i`, mined on `main` at `height` in the block with hash
/// `block`. `retarget` holds the finalizers a retarget moves the bond from and to.
#[derive(Clone)]
pub(crate) struct FuzzMinedStaking {
    pub tx: Arc<Transaction>,
    pub bond_i: usize,
    pub height: u32,
    pub block: BlockHash,
    pub retarget: Option<(usize, usize)>,
}

impl FuzzGen {
    pub(crate) fn new(params: &zcash_primitives::bft::ZcashCrosslinkParameters) -> FuzzGen {
        FuzzGen::new_with_node_config(params, "")
    }

    /// A generator for a node that also takes `node_config`, as `TF::new_with_node_config`.
    pub(crate) fn new_with_node_config(params: &zcash_primitives::bft::ZcashCrosslinkParameters, node_config: &str) -> FuzzGen {
        let network = test_network(params, node_config);
        let key = TestKey::new(b"crosslink test miner");
        let miner = key.address();
        let gen = BlockGen::init_at_genesis_plus_1(network.clone(), BlockGen::REGTEST_GENESIS_HASH, &miner);
        let mut tf = TF::new_with_node_config(params, node_config);
        tf.push_instr_load_pow(&gen.tip, 0);
        let model_tip = (1, gen.tip.hash());
        FuzzGen {
            tf, network, key, miner, main: vec![gen], tip_known: true, rival_height: 0, next_fresh_peer: FRESH_PEERS_FROM, forks_made: 0,
            params: params.clone(), pending: Vec::new(), mined_spends: Vec::new(), unsure_coins: Default::default(), bonds: Vec::new(),
            mined_staking: Vec::new(), txs_made: 0, model_rival_height: 0, model_tip,
        }
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
    let max_depth = (g.height() as usize - 1).min(FORK_DEPTH_MAX);
    if max_depth == 0 {
        return move_extend(g, c);
    }
    let depth = 1 + c.below(max_depth);
    let len = 1 + c.below(depth + 2);
    fork(g, c, depth, len);
}

/// Delivers a branch from `depth` blocks below the tip, `len` blocks long, mined to its own
/// address, and makes it `main` if it is longer.
fn fork(g: &mut FuzzGen, c: &mut Choices, depth: usize, len: usize) {
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
/// Bytes that still parse to a block with a coinbase height are loaded as they are, or sometimes
/// served by a peer that claims they are the valid block; bytes that do not are always served
/// that way, which is the only way they reach a node. Either way the valid block comes later
/// through the ordinary moves, so a lie told about it must not keep it out.
fn move_mangled_block(g: &mut FuzzGen, c: &mut Choices) {
    let mut scratch = g.tip().clone();
    let valid = scratch.next_block(&g.miner.clone());
    let mut bytes = valid.zcash_serialize_to_vec().expect("a generated block serializes");
    let flips = 1 + c.below(4);
    for _ in 0..flips {
        let i = c.below(bytes.len());
        bytes[i] ^= 1 << c.below(8);
    }
    if c.unusual(64) {
        // At least one byte: a peer claiming a block and holding none of it has nothing to
        // serve, so the node's request would go unanswered whatever the node does.
        let cut = 1 + c.below(bytes.len() - 1);
        bytes.truncate(cut);
    }
    let parses = Block::zcash_deserialize(&bytes[..]).is_ok_and(|b| b.coinbase_height().is_some());
    if parses && !c.unusual(64) {
        // Only loaded, not offered as itself by a peer: a node need not fetch an advertised block
        // (one at a height it holds adds no work), and an offer it ignores costs the
        // instruction's whole wait.
        g.tf.push_instr_load_pow_bytes(&bytes, ANY_OUTCOME);
        g.tf.push_instr_expect_node_alive(0);
        // Taken, it is a block at the tip's next height that `main` does not have.
        g.tip_known = false;
        g.rival_height = g.rival_height.max(g.height() + 1);
        return;
    }
    // Claimed as the valid block, which is at the next height, so the node asks for it. Bytes
    // that do not hash to the claim, or do not parse, are not that block.
    let claim = StpClaim::of(&valid).expect("a generated block has a coinbase height");
    let peer = g.next_fresh_peer;
    g.next_fresh_peer += 1;
    let same = bytes == valid.zcash_serialize_to_vec().expect("a generated block serializes");
    g.tf.push_instr_recv_pow_claimed(claim, &bytes, peer, if same { ANY_OUTCOME } else { SHOULD_FAIL });
    g.tf.push_instr_expect_node_alive(0);
}

// ---- Transaction and staking moves ----
// Spends use only coinbase outputs deeper than any fork the moves make, so a reorg never removes
// the coin itself, only (perhaps) a spend of it.

/// How deep a coinbase must be before a move spends it: mature, and below any fork.
const SPENDABLE_DEPTH: u32 = {
    let below_forks = FORK_DEPTH_MAX as u32 + 1;
    let maturity = zebra_chain::transparent::MIN_TRANSPARENT_COINBASE_MATURITY;
    if maturity > below_forks { maturity } else { below_forks }
};

/// Whether `main` holds the block `hash` at `height`.
fn on_main(g: &FuzzGen, (height, hash): (u32, BlockHash)) -> bool {
    height >= 1 && g.main.get(height as usize - 1).is_some_and(|gen| gen.tip.hash() == hash)
}

/// Marks unknown whatever a chain switch since the last transaction move may have changed. A
/// switch resets the node's mempool, and a reorg below a mined action undoes it.
fn refresh_model(g: &mut FuzzGen) {
    let switched = !g.tip_known || g.rival_height != g.model_rival_height || !on_main(g, g.model_tip);
    if switched {
        for p in &mut g.pending {
            p.known = false;
        }
    }
    // A spend made under an earlier network upgrade can no longer be mined, and the mempool
    // clears it at the activation (ZIP-200).
    let upgrade = g.tip().next_upgrade();
    let (current, stale): (Vec<FuzzPending>, Vec<FuzzPending>) =
        std::mem::take(&mut g.pending).into_iter().partition(|p| p.tx.network_upgrade() == Some(upgrade));
    g.pending = current;
    g.unsure_coins.extend(stale.into_iter().map(|p| p.coin.0));
    let (kept, dropped): (Vec<FuzzMinedSpend>, Vec<FuzzMinedSpend>) =
        std::mem::take(&mut g.mined_spends).into_iter().partition(|s| on_main(g, (s.height, s.block)));
    g.mined_spends = kept;
    g.unsure_coins.extend(dropped.into_iter().map(|s| s.coin.0));
    let mined_staking: Vec<FuzzMinedStaking> =
        std::mem::take(&mut g.mined_staking).into_iter().filter(|m| on_main(g, (m.height, m.block))).collect();
    g.mined_staking = mined_staking;
    for i in 0..g.bonds.len() {
        let bond = &g.bonds[i];
        if (bond.at.0 != 0 && !on_main(g, bond.at)) || (bond.target_at.0 != 0 && !on_main(g, bond.target_at)) {
            g.bonds[i].known = false;
        }
    }
    g.model_rival_height = g.rival_height;
    g.model_tip = (g.height(), g.tip().tip.hash());
}

/// Coinbase outputs at SPENDABLE_DEPTH paying the generator's key that nothing it made spends, oldest first.
/// For the mempool, also none whose earlier spend a reorg may have put back there.
fn free_coins(g: &FuzzGen, for_mempool: bool) -> Vec<(zebra_chain::transparent::OutPoint, zebra_chain::transparent::Utxo)> {
    let next_height = g.height() + 1;
    let lock_script = g.key.lock_script();
    let mut coins: Vec<_> = g
        .tip()
        .utxos
        .iter()
        .filter(|(outpoint, utxo)| {
            utxo.from_coinbase
                && utxo.output.lock_script == lock_script
                && next_height >= utxo.height.0 + SPENDABLE_DEPTH
                && !g.pending.iter().any(|p| p.coin.0 == **outpoint)
                && !(for_mempool && g.unsure_coins.contains(*outpoint))
        })
        .map(|(outpoint, utxo)| (*outpoint, utxo.clone()))
        .collect();
    coins.sort_by_key(|(outpoint, utxo)| (utxo.height, outpoint.hash.0, outpoint.index));
    coins
}

/// Extends `main` until a coinbase is at SPENDABLE_DEPTH.
fn fund(g: &mut FuzzGen) {
    while free_coins(g, false).is_empty() {
        let mut next = g.tip().clone();
        let block = next.next_block(&g.miner.clone());
        g.main.push(next);
        g.tf.push_instr_load_pow(&block, if g.tip_known { 0 } else { ANY_OUTCOME });
        g.after_main_grew();
    }
    g.expect_tip();
}

/// Mines `txs` in the next block of `main` and delivers it. Returns whether the node is known
/// to have taken it as its tip.
fn mine(g: &mut FuzzGen, c: &mut Choices, txs: &[Arc<Transaction>]) -> bool {
    let known = g.tip_known;
    let mut next = g.tip().clone();
    let block = next.next_block_with_txs(&g.miner.clone(), txs);
    g.main.push(next);
    g.deliver(c, &block, if known { 0 } else { ANY_OUTCOME });
    g.after_main_grew();
    g.expect_tip();
    known && g.tip_known
}

/// Delivers a block on `main`'s tip that the node must refuse when the tip is known. When it
/// is not, the node may hold a chain on which the block is valid, and then holds it as a rival.
fn refuse(g: &mut FuzzGen, c: &mut Choices, block: &Block) {
    if g.tip_known {
        if c.unusual(64) {
            let peer = g.next_fresh_peer;
            g.next_fresh_peer += 1;
            g.tf.push_instr_recv_pow(block, peer, SHOULD_FAIL);
        } else {
            g.tf.push_instr_load_pow(block, SHOULD_FAIL);
        }
    } else {
        g.tf.push_instr_load_pow(block, ANY_OUTCOME);
        g.rival_height = g.rival_height.max(g.height() + 1);
    }
    g.tf.push_instr_expect_node_alive(0);
    g.expect_tip();
}

/// Hands the node a transaction it must take, through SUBMIT_TX or a peer's relay.
fn offer_tx(g: &mut FuzzGen, c: &mut Choices, tx: &Arc<Transaction>) {
    if c.unusual(96) {
        g.tf.push_instr_recv_tx_wire(tx, 0, 0);
    } else {
        g.tf.push_instr_submit_tx(tx, 0);
    }
}

/// A spend of `coin` paying another key, so it conflicts with `transparent_spend` of the same
/// coin. `salt` picks the key, so each conflict is a distinct transaction.
fn conflicting_spend(
    upgrade: NetworkUpgrade,
    key: &TestKey,
    coin: &(zebra_chain::transparent::OutPoint, zebra_chain::transparent::Utxo),
    salt: u64,
) -> Arc<Transaction> {
    let other = TestKey::new(format!("crosslink fuzz conflict {salt}").as_bytes());
    let input = (coin.0, coin.1.output.clone());
    let input_value = input.1.value;
    let pay = |fee: zebra_chain::amount::Amount<zebra_chain::amount::NonNegative>| {
        vec![zebra_chain::transparent::Output::new((input_value - fee).expect("the input covers the fee"), other.lock_script())]
    };
    let draft = signed_tx(upgrade, Some(key), &[input.clone()], pay(zebra_chain::amount::Amount::zero()), None, 0);
    let fee = mempool_fee(&draft);
    signed_tx(upgrade, Some(key), &[input], pay(fee), None, 0)
}

/// A spend into the mempool, through SUBMIT_TX or a peer's relay.
fn move_spend(g: &mut FuzzGen, c: &mut Choices) {
    refresh_model(g);
    let coins = free_coins(g, true);
    if coins.is_empty() {
        return fund(g);
    }
    let coin = coins[c.below(coins.len())].clone();
    let tx = transparent_spend(g.tip().next_upgrade(), &g.key, (coin.0, coin.1.output.clone()));
    offer_tx(g, c, &tx);
    g.tf.push_instr_expect_mempool_contains(&tx, 0);
    g.pending.push(FuzzPending { tx, coin, known: true });
}

/// Mines every pending spend, and sometimes a spend straight into the block too. A mined spend
/// leaves the mempool.
fn move_mine_pending(g: &mut FuzzGen, c: &mut Choices) {
    refresh_model(g);
    let mut spends: Vec<(Arc<Transaction>, (zebra_chain::transparent::OutPoint, zebra_chain::transparent::Utxo))> =
        g.pending.iter().map(|p| (p.tx.clone(), p.coin.clone())).collect();
    if spends.is_empty() || c.unusual(64) {
        if let Some(coin) = free_coins(g, false).first().cloned() {
            spends.push((transparent_spend(g.tip().next_upgrade(), &g.key, (coin.0, coin.1.output.clone())), coin));
        }
    }
    if spends.is_empty() {
        return fund(g);
    }
    let txs: Vec<Arc<Transaction>> = spends.iter().map(|(tx, _)| tx.clone()).collect();
    let known = mine(g, c, &txs);
    if known {
        for tx in &txs {
            g.tf.push_instr_expect_mempool_absent(tx, 0);
        }
    }
    let (height, block) = (g.height(), g.tip().tip.hash());
    g.mined_spends.extend(spends.into_iter().map(|(_, coin)| FuzzMinedSpend { coin, height, block }));
    g.pending.clear();
    if c.unusual(64) {
        g.tf.push_instr_expect_staking_replay_matches(0);
    }
}

/// A second spend of a coin already spent, in the mempool or on `main`, offered to the mempool
/// or mined; or two spends of one coin in one block.
fn move_double_spend(g: &mut FuzzGen, c: &mut Choices) {
    refresh_model(g);
    let (pending_n, mined_n) = (g.pending.len(), g.mined_spends.len());
    if pending_n + mined_n == 0 || c.unusual(32) {
        // Both spends in one block, which must be refused.
        let Some(coin) = free_coins(g, false).first().cloned() else { return fund(g) };
        g.txs_made += 1;
        let upgrade = g.tip().next_upgrade();
        let first = transparent_spend(upgrade, &g.key, (coin.0, coin.1.output.clone()));
        let second = conflicting_spend(upgrade, &g.key, &coin, g.txs_made);
        let block = g.tip().clone().next_block_with_txs(&g.miner.clone(), &[first, second]);
        return refuse(g, c, &block);
    }
    g.txs_made += 1;
    let upgrade = g.tip().next_upgrade();
    let i = c.below(pending_n + mined_n);
    let into_block = c.unusual(128);
    if i < pending_n {
        let original = g.pending[i].clone();
        let conflict = conflicting_spend(upgrade, &g.key, &original.coin, g.txs_made);
        if into_block {
            // The block takes the conflict, and the mempool drops the spend it held.
            g.pending.remove(i);
            let known = mine(g, c, &[conflict]);
            if known && original.known {
                g.tf.push_instr_expect_mempool_absent(&original.tx, 0);
            }
            let (height, block) = (g.height(), g.tip().tip.hash());
            g.mined_spends.push(FuzzMinedSpend { coin: original.coin, height, block });
        } else if original.known {
            g.tf.push_instr_submit_tx(&conflict, SHOULD_FAIL);
            g.tf.push_instr_expect_mempool_contains(&original.tx, 0);
        } else {
            // Without the original, the node may take the conflict instead; whatever it holds
            // for this coin is then unknown.
            g.tf.push_instr_submit_tx(&conflict, ANY_OUTCOME);
            g.unsure_coins.insert(original.coin.0);
            g.pending.remove(i);
        }
    } else {
        let spent = g.mined_spends[i - pending_n].clone();
        let conflict = conflicting_spend(upgrade, &g.key, &spent.coin, g.txs_made);
        // Offered to the mempool, a new transaction spending an output the chain no longer has is
        // held while the verifier waits for that output to appear, and refused when the wait
        // (17 s) runs out: slow, so rarely chosen. Only a transaction the mempool held when a
        // block spent its inputs is refused at once ("inputs were spent").
        if into_block || !c.unusual(64) {
            // The generator is told the coin is unspent, so the block is well formed (its
            // coinbase claims the fee) and wrong only in spending it twice.
            let mut scratch = g.tip().clone();
            scratch.utxos.insert(spent.coin.0, spent.coin.1.clone());
            let block = scratch.next_block_with_txs(&g.miner.clone(), &[conflict]);
            refuse(g, c, &block);
        } else if g.tip_known {
            g.tf.push_instr_submit_tx(&conflict, SHOULD_FAIL);
            g.tf.push_instr_expect_mempool_absent(&conflict, 0);
        } else {
            g.tf.push_instr_submit_tx(&conflict, ANY_OUTCOME);
            g.unsure_coins.insert(spent.coin.0);
        }
    }
}

/// The finalizers bonds are made to: a few, so retargets and shared targets happen.
fn fuzz_finalizer(i: usize) -> zcash_primitives::bft::FinalizerAddress {
    zcash_primitives::bft::FinalizerAddress::create(
        &zebra_crosslink::rng_private_public_key_from_address(format!("crosslink fuzz finalizer {i}").as_bytes()).0,
    )
}

/// Expiry heights of retargets start here. A retarget has no spend to make it unique, so two
/// retargets between the same finalizers would be one transaction, the second a replay; each
/// gets its own expiry height instead, far above any height a run reaches.
const RETARGET_EXPIRY_FROM: u32 = 1_000_000;

/// A retarget of the bond `bond_seed` names, from `fuzz_finalizer(from)` to `fuzz_finalizer(to)`,
/// spending `spends`, for the next block of `main`. Each call makes a distinct transaction.
fn fuzz_retarget(g: &mut FuzzGen, bond_seed: &[u8], from: usize, to: usize, spends: Option<[u8; 32]>) -> Arc<Transaction> {
    use zcash_primitives::transaction::StakingAction;

    g.txs_made += 1;
    let expiry_height = RETARGET_EXPIRY_FROM + g.txs_made as u32;
    let (from_finalizer, to_finalizer) = (fuzz_finalizer(from), fuzz_finalizer(to));
    signed_staking_tx(g.tip().next_upgrade(), bond_seed, expiry_height, |unique_pubkey, signature| {
        StakingAction::RetargetDelegationBond { unique_pubkey, signature, from_finalizer, to_finalizer, spends }
    })
}

/// Mines `tx`, a valid staking action on bond `bond_i`, in the next block of `main`, and records
/// what it did: the bond is in `state` and targets `target` after it.
fn mine_staking(
    g: &mut FuzzGen,
    c: &mut Choices,
    bond_i: usize,
    tx: Arc<Transaction>,
    state: FuzzBondState,
    target: usize,
    retarget: Option<(usize, usize)>,
) {
    let h = g.height() + 1;
    let known = mine(g, c, &[tx.clone()]);
    let at = (h, g.main[h as usize - 1].tip.hash());
    let bond = &mut g.bonds[bond_i];
    bond.state = state;
    bond.target = target;
    if retarget.is_some() {
        bond.target_at = at;
    } else {
        bond.at = at;
    }
    bond.known = known;
    let action = tx.staking_action().expect("a staking transaction");
    if !action.acts_on_existing_bond() || action.spends().is_some() {
        bond.version = tx.hash().0;
    }
    g.mined_staking.push(FuzzMinedStaking { tx, bond_i, height: at.0, block: at.1, retarget });
}

/// Whether the next block of `main` is under FeaturenetUpgrade3, where an action on a bond must
/// spend the bond's version.
fn spends_versions(g: &FuzzGen) -> bool {
    NetworkUpgrade::FeaturenetUpgrade3
        .activation_height(&g.network)
        .is_some_and(|activation_height| BlockHeight(g.height() + 1) >= activation_height)
}

/// Whether the next block of `main` still allows the fuzzer's unfunded zero-zat create. From
/// FeaturenetUpgrade1 a create must bond a power of 10 zats, and an upgrade left unscheduled
/// activates with the next one, so scheduling FeaturenetUpgrade3 alone also activates it.
fn zero_create_allowed(g: &FuzzGen) -> bool {
    NetworkUpgrade::FeaturenetUpgrade1
        .activation_height(&g.network)
        .map_or(true, |activation_height| BlockHeight(g.height() + 1) < activation_height)
}

/// The version an action on `bond` in the next block spends, and whether that is the one the
/// node requires: mostly the bond's version under FeaturenetUpgrade3 and none before it, and
/// sometimes the wrong one, for the refusal: none, the bond's key, which is never a version, a
/// made-up version, or the right one too early.
fn fuzz_spends(g: &FuzzGen, c: &mut Choices, bond: &FuzzBond) -> (Option<[u8; 32]>, bool) {
    let required = spends_versions(g).then_some(bond.version);
    if !c.unusual(32) {
        return (required, true);
    }
    let wrong = match (required, c.below(3)) {
        (Some(_), 0) => None,
        (Some(_), 1) => Some(bond.key),
        (None, 0) => Some(bond.version),
        _ => Some([0xee; 32]),
    };
    (wrong, wrong == required)
}

/// An unbond of the bond `bond_seed` names, spending `spends`.
fn fuzz_unbond(g: &FuzzGen, bond_seed: &[u8], spends: Option<[u8; 32]>) -> Arc<Transaction> {
    use zcash_primitives::transaction::StakingAction;

    signed_staking_tx(g.tip().next_upgrade(), bond_seed, 0, |unique_pubkey, signature| {
        StakingAction::BeginDelegationUnbonding { unique_pubkey, signature, spends }
    })
}

/// A withdrawal of `amount_zats` from the bond `bond_seed` names, spending `spends`.
fn fuzz_withdraw(g: &FuzzGen, bond_seed: &[u8], amount_zats: u64, spends: Option<[u8; 32]>) -> Arc<Transaction> {
    use zcash_primitives::transaction::StakingAction;

    signed_staking_tx(g.tip().next_upgrade(), bond_seed, 0, |unique_pubkey, signature| {
        StakingAction::WithdrawDelegationBond { amount_zats, unique_pubkey, signature, spends }
    })
}

/// A bond not yet created, added to `g.bonds`. Returns its index.
fn new_fuzz_bond(g: &mut FuzzGen) -> usize {
    g.txs_made += 1;
    let seed = format!("crosslink fuzz bond {}", g.txs_made).into_bytes();
    let key = staking_tx_create_bond(g.tip().next_upgrade(), &seed, fuzz_finalizer(0), 0)
        .staking_action()
        .expect("a staking transaction")
        .bond_key();
    let none = (0, BlockHash([0; 32]));
    g.bonds.push(FuzzBond { seed, key, state: FuzzBondState::Absent, at: none, known: true, target: 0, target_at: none, version: [0; 32] });
    g.bonds.len() - 1
}

/// A bond action in the next block: create, unbond, withdraw or retarget, on a new bond or one
/// made before, valid or not. Whether it is valid follows from the bond's state and the staking
/// calendar, the rules the node applies: create, unbond and withdraw only on staking days, each at
/// least `action_delay` after the bond's last one, and a retarget only from the bond's current
/// target. Sometimes first mines up to the next height where that action could be valid, so
/// valid actions are common rather than rare.
fn move_staking(g: &mut FuzzGen, c: &mut Choices) {
    refresh_model(g);
    let staking = g.params.staking;
    // Mostly an action on a bond in the state it needs, so bonds move through their whole life;
    // sometimes a bond in any state, or one never created, for the refusals.
    let needs = |action: usize| [FuzzBondState::Absent, FuzzBondState::Active, FuzzBondState::Unbonding, FuzzBondState::Active][action];
    let fitting = |g: &FuzzGen, action: usize| -> Vec<usize> {
        (0..g.bonds.len()).filter(|&i| g.bonds[i].known && g.bonds[i].state == needs(action)).collect()
    };
    // Advancing a bond that exists is the ordinary choice: a whole life takes three actions, and
    // staking is one move in nine. An active bond is retargeted some of the time instead.
    let mut action = if !fitting(g, 2).is_empty() && !c.unusual(128) {
        2
    } else if !fitting(g, 1).is_empty() && !c.unusual(128) {
        if c.unusual(96) { 3 } else { 1 }
    } else {
        c.below(4)
    };
    if action != 0 && fitting(g, action).is_empty() && !c.unusual(64) {
        action = 0;
    }
    let fits = fitting(g, action);
    let bond_i = if !fits.is_empty() && (action != 0 || c.unusual(32)) && !c.unusual(48) {
        fits[c.below(fits.len())]
    } else if !g.bonds.is_empty() && action != 0 && !c.unusual(64) {
        c.below(g.bonds.len())
    } else {
        new_fuzz_bond(g)
    };

    let is_staking_day = |h: u32| h % staking.period < staking.day_window;
    let bond = g.bonds[bond_i].clone();
    // A retarget is exempt from the calendar, so it has nothing to wait for.
    if action != 3 && !c.unusual(96) {
        let earliest = if bond.at.0 == 0 { 0 } else { bond.at.0 + staking.action_delay };
        while g.height() + 1 < earliest || !is_staking_day(g.height() + 1) {
            mine(g, c, &[]);
        }
    }

    let h = g.height() + 1;
    let upgrade = g.tip().next_upgrade();
    let wrong_amount = action == 2 && c.unusual(32);
    let (spends, spends_right) = fuzz_spends(g, c, &bond);
    let mut retarget = None;
    let (tx, valid, after, target) = match action {
        0 => {
            let target = c.below(3);
            (
                staking_tx_create_bond(upgrade, &bond.seed, fuzz_finalizer(target), 0),
                bond.state == FuzzBondState::Absent && zero_create_allowed(g),
                FuzzBondState::Active,
                target,
            )
        }
        1 => (fuzz_unbond(g, &bond.seed, spends), bond.state == FuzzBondState::Active && spends_right, FuzzBondState::Unbonding, bond.target),
        2 => (
            fuzz_withdraw(g, &bond.seed, u64::from(wrong_amount), spends),
            bond.state == FuzzBondState::Unbonding && !wrong_amount && spends_right,
            FuzzBondState::Withdrawn,
            bond.target,
        ),
        _ => {
            // To another finalizer; sometimes from one the bond does not target, for the refusal.
            let to = (bond.target + 1 + c.below(2)) % 3;
            let from = if c.unusual(32) { (bond.target + 1 + c.below(2)) % 3 } else { bond.target };
            retarget = Some((from, to));
            (
                fuzz_retarget(g, &bond.seed, from, to, spends),
                bond.state == FuzzBondState::Active && from == bond.target && spends_right,
                FuzzBondState::Active,
                to,
            )
        }
    };
    let delay_met = bond.at.0 == 0 || h >= bond.at.0 + staking.action_delay;
    let valid = valid && (action == 3 || (delay_met && is_staking_day(h)));

    if !bond.known {
        // The node may hold this bond in another state: the action may or may not apply. If it
        // does, the node holds a block `main` does not have, a rival at the next height.
        let block = g.tip().clone().next_block_with_txs(&g.miner.clone(), &[tx]);
        g.tf.push_instr_load_pow(&block, ANY_OUTCOME);
        g.tf.push_instr_expect_node_alive(0);
        g.tip_known = false;
        g.rival_height = g.rival_height.max(h);
        return;
    }
    if valid {
        mine_staking(g, c, bond_i, tx, after, target, retarget);
    } else {
        let block = g.tip().clone().next_block_with_txs(&g.miner.clone(), &[tx]);
        refuse(g, c, &block);
        if !g.tip_known {
            g.bonds[bond_i].known = false;
        }
    }

    let bond = &g.bonds[bond_i];
    if bond.known && g.tip_known {
        let status = match bond.state {
            FuzzBondState::Absent => TF_BOND_ABSENT,
            FuzzBondState::Active => TF_BOND_ACTIVE,
            FuzzBondState::Unbonding => TF_BOND_UNBONDING,
            FuzzBondState::Withdrawn => TF_BOND_WITHDRAWN,
        };
        let key = bond.key;
        g.tf.push_instr_expect_bond(key, status, TEST_STAKE_IGNORED, 0);
    }
    if c.unusual(64) {
        g.tf.push_instr_expect_staking_replay_matches(0);
    }
}

/// The deepest a replayed transaction's first copy sits below the tip while it stays in the
/// non-finalized chain, where a replay is refused under any network upgrade.
const REPLAY_DEPTH_MAX: u32 = zebra_state::MAX_BLOCK_REORG_HEIGHT / 2;

/// Blocks enough for what was mined before them to be finalized.
const FINALIZED_BETWEEN: u32 = zebra_state::MAX_BLOCK_REORG_HEIGHT + 10;

/// Extends `main` by `n` empty blocks, loaded without drawing choices, which a long run would
/// use up the input on.
fn load_empty_blocks(g: &mut FuzzGen, n: u32) {
    for _ in 0..n {
        let mut next = g.tip().clone();
        let block = next.next_block(&g.miner.clone());
        g.main.push(next);
        g.tf.push_instr_load_pow(&block, if g.tip_known { 0 } else { ANY_OUTCOME });
        g.after_main_grew();
    }
    // A mempool that misses tips while blocks arrive this fast resets, dropping what it held.
    for p in &mut g.pending {
        p.known = false;
    }
    g.expect_tip();
}

/// A staking action, empty blocks until it is finalized, then a replay. A bond is made first if
/// none is active, while the network upgrade may still allow a zero-zat create, and is sometimes
/// finalized first; then any activation the run would cross is passed before the action, since a
/// replay made under another upgrade is refused for that alone. Many actions are refused on
/// purpose, so a few are tried until one is mined.
fn move_finalize(g: &mut FuzzGen, c: &mut Choices) {
    refresh_model(g);
    if !g.bonds.iter().any(|bond| bond.known && bond.state == FuzzBondState::Active) {
        move_staking(g, c);
    }
    // Half the time the bonds are finalized before the action too, so under FeaturenetUpgrade3
    // the node finds a bond's first version in the finalized state.
    if c.unusual(128) {
        load_empty_blocks(g, FINALIZED_BETWEEN + c.below(8) as u32);
    }
    let n = FINALIZED_BETWEEN + c.below(8) as u32;
    let upgrade_after_run = |g: &FuzzGen| NetworkUpgrade::current(&g.network, BlockHeight(g.height() + 2 + n));
    while g.tip().next_upgrade() != upgrade_after_run(g) {
        load_empty_blocks(g, 1);
    }
    let before = g.height();
    for _ in 0..4 {
        move_staking(g, c);
        if g.mined_staking.last().is_some_and(|s| s.height > before) {
            break;
        }
    }
    load_empty_blocks(g, n);
    move_replay(g, c);
}

/// Bonds made before FeaturenetUpgrade3 and acted on after it, so a bond's first action spends
/// its create's txid. Ordinary moves seldom reach that before a run ends, since the fuzzer's
/// zero-zat create is refused from the activation on. A bond is made on a staking day before
/// the activation if none is active, then `main` grows past it, sometimes until the bond is
/// finalized. Sometimes a retarget naming the bond's key comes first, which must be refused, and
/// sometimes a heavier branch from below the first spend undoes it.
fn move_cross_featurenet_upgrade_3(g: &mut FuzzGen, c: &mut Choices) {
    refresh_model(g);
    let ahead = NetworkUpgrade::FeaturenetUpgrade3.activation_height(&g.network).filter(|&h| BlockHeight(g.height() + 1) < h);
    let Some(activation) = ahead else {
        return move_staking(g, c);
    };
    let staking = g.params.staking;
    let active_bonds = |g: &FuzzGen| -> Vec<usize> {
        (0..g.bonds.len()).filter(|&i| g.bonds[i].known && g.bonds[i].state == FuzzBondState::Active).collect()
    };
    if active_bonds(g).is_empty() {
        while (g.height() + 1) % staking.period >= staking.day_window {
            mine(g, c, &[]);
        }
        if zero_create_allowed(g) && g.tip_known {
            let bond_i = new_fuzz_bond(g);
            let target = c.below(3);
            let create = staking_tx_create_bond(g.tip().next_upgrade(), &g.bonds[bond_i].seed, fuzz_finalizer(target), 0);
            mine_staking(g, c, bond_i, create, FuzzBondState::Active, target, None);
        }
    }
    let mut past = c.below(8) as u32;
    if c.unusual(128) {
        past += FINALIZED_BETWEEN;
    }
    load_empty_blocks(g, (activation.0 - 1).saturating_sub(g.height()) + past);
    refresh_model(g);

    let active = active_bonds(g);
    if g.tip_known && !active.is_empty() && c.unusual(128) {
        let bond = g.bonds[active[c.below(active.len())]].clone();
        let wrong = fuzz_retarget(g, &bond.seed, bond.target, (bond.target + 1 + c.below(2)) % 3, Some(bond.key));
        let block = g.tip().clone().next_block_with_txs(&g.miner.clone(), &[wrong]);
        refuse(g, c, &block);
    }
    let before = g.height();
    for _ in 0..4 {
        move_staking(g, c);
        if g.mined_staking.last().is_some_and(|s| s.height > before) {
            break;
        }
    }
    let spent_at_tip = g.mined_staking.last().is_some_and(|s| s.height > before && s.height == g.height());
    if spent_at_tip && c.unusual(128) {
        let depth = 1 + c.below((g.height() as usize - 1).min(FORK_DEPTH_MAX));
        let len = depth + 1 + c.below(2);
        fork(g, c, depth, len);
    }
}

/// Whether the next block of `main` must refuse `tx` mined again after it was finalized: from
/// FeaturenetUpgrade2 any repeat, and from FeaturenetUpgrade3 an action whose spent version is gone.
fn refuses_finalized_repeat(g: &FuzzGen, tx: &Transaction) -> bool {
    let active = |nu: NetworkUpgrade| nu.activation_height(&g.network).is_some_and(|h| BlockHeight(g.height() + 1) >= h);
    active(NetworkUpgrade::FeaturenetUpgrade2)
        || (active(NetworkUpgrade::FeaturenetUpgrade3) && tx.staking_action().and_then(|action| action.spends()).is_some())
}

/// A staking action mined on `main`, mined again in the next block. Most often a retarget whose
/// bond is first moved back to the finalizer the retarget leaves, so the replay passes every
/// staking rule and only its hash tells it apart. The node must refuse it while the first copy is
/// not finalized; once it is, only where `refuses_finalized_repeat` says so.
fn move_replay(g: &mut FuzzGen, c: &mut Choices) {
    refresh_model(g);
    // A finalized first copy under another network upgrade would be refused for that alone.
    let next_upgrade = g.tip().next_upgrade();
    let recent: Vec<usize> = (0..g.mined_staking.len())
        .filter(|&i| {
            let depth = g.height() + 1 - g.mined_staking[i].height;
            depth < REPLAY_DEPTH_MAX || (depth > FINALIZED_BETWEEN && g.mined_staking[i].tx.network_upgrade() == Some(next_upgrade))
        })
        .collect();
    if recent.is_empty() {
        return move_staking(g, c);
    }
    let retargets: Vec<usize> = recent
        .iter()
        .copied()
        .filter(|&i| {
            let bond = &g.bonds[g.mined_staking[i].bond_i];
            g.mined_staking[i].retarget.is_some() && bond.known && bond.state == FuzzBondState::Active
        })
        .collect();
    let i = if !retargets.is_empty() && !c.unusual(64) {
        let i = retargets[c.below(retargets.len())];
        let (bond_i, (from, _)) = (g.mined_staking[i].bond_i, g.mined_staking[i].retarget.expect("a retarget"));
        let bond = g.bonds[bond_i].clone();
        if g.tip_known && bond.target != from && !c.unusual(32) {
            let spends = spends_versions(g).then_some(bond.version);
            let back = fuzz_retarget(g, &bond.seed, bond.target, from, spends);
            mine_staking(g, c, bond_i, back, FuzzBondState::Active, from, Some((bond.target, from)));
        }
        i
    } else {
        recent[c.below(recent.len())]
    };

    let replay = g.mined_staking[i].clone();
    let finalized = g.height() + 1 - replay.height > FINALIZED_BETWEEN;
    let block = g.tip().clone().next_block_with_txs(&g.miner.clone(), &[replay.tx.clone()]);
    if finalized && !refuses_finalized_repeat(g, &replay.tx) {
        // The repeat is then as valid as its bond's state lets it be, which the model does not
        // follow for an old action. Taken, it is a block at the next height that `main` lacks.
        g.tf.push_instr_load_pow(&block, ANY_OUTCOME);
        g.tf.push_instr_expect_node_alive(0);
        g.tip_known = false;
        g.rival_height = g.rival_height.max(g.height() + 1);
        g.bonds[replay.bond_i].known = false;
        return;
    }
    refuse(g, c, &block);
    if !g.tip_known {
        g.bonds[replay.bond_i].known = false;
    }
}

/// Two retargets of one active bond in one block under FeaturenetUpgrade3, which single-action
/// moves never make: mostly the second spends the first's txid, so the block is valid and leaves
/// the bond at the second's version; sometimes it spends the version both started from, so the
/// node must refuse the block.
fn move_retarget_twice(g: &mut FuzzGen, c: &mut Choices) {
    refresh_model(g);
    let active: Vec<usize> =
        (0..g.bonds.len()).filter(|&i| g.bonds[i].known && g.bonds[i].state == FuzzBondState::Active).collect();
    if !spends_versions(g) || active.is_empty() || !g.tip_known {
        return move_staking(g, c);
    }
    let bond_i = active[c.below(active.len())];
    let bond = g.bonds[bond_i].clone();
    let mid = (bond.target + 1 + c.below(2)) % 3;
    let to = (mid + 1 + c.below(2)) % 3;
    let first = fuzz_retarget(g, &bond.seed, bond.target, mid, Some(bond.version));
    let chained = !c.unusual(8);
    let second_spends = if chained { first.hash().0 } else { bond.version };
    let second = fuzz_retarget(g, &bond.seed, mid, to, Some(second_spends));

    if !chained {
        let block = g.tip().clone().next_block_with_txs(&g.miner.clone(), &[first, second]);
        refuse(g, c, &block);
        return;
    }
    let h = g.height() + 1;
    let known = mine(g, c, &[first.clone(), second.clone()]);
    let at = (h, g.main[h as usize - 1].tip.hash());
    g.bonds[bond_i] = FuzzBond { target: to, target_at: at, known, version: second.hash().0, ..bond.clone() };
    g.mined_staking.push(FuzzMinedStaking { tx: first, bond_i, height: h, block: at.1, retarget: Some((bond.target, mid)) });
    g.mined_staking.push(FuzzMinedStaking { tx: second, bond_i, height: h, block: at.1, retarget: Some((mid, to)) });
    if known {
        g.tf.push_instr_expect_bond(bond.key, TF_BOND_ACTIVE, TEST_STAKE_IGNORED, 0);
    }
}
// ---- End of transaction and staking moves ----

// ---- BFT ----
// A generator of its own: a supplied-BFT chain, SUPPLIED_FINALIZER signing everything as
// `bonded_start` arranges, over a PoW chain that only grows. Each step may first offer a block wrong
// in one field `validate` checks against the parent, which must be refused, and then offers a
// valid one, which must be decided: a proposal one finalizer gets refused, or would have got
// decided, must never keep a later honest one out.

struct BftFuzz {
    tf: TF,
    gen: BlockGen,
    miner: Address,
    /// `pow[i]` is the PoW block at height `i + 1 + BONDED_SHIFT`.
    pow: Vec<Arc<Block>>,
    /// The decided chain's length, and its tip's certificate, version and first header's index.
    decided: u32,
    tip_ptr: FatPointerToBftBlock,
    tip_version: u32,
    tip_from: usize,
}

/// The BFT block at `height` on `parent` carrying `headers`, at `version` and `do_not_include`, with
/// SUPPLIED_FINALIZER's certificate; the genesis's is empty.
fn bft_fuzz_block(parent: &FatPointerToBftBlock, height: u32, headers: &[Arc<Block>], version: u32, do_not_include: u64) -> BftBlockAndFatPointerToItWrap {
    let mut block = create_pos_and_ptr_to_finalize_pow_at_round(height, parent.clone(), headers, 1, &[]).0.block;
    block.version = version;
    block.do_not_include_until_bc_height = do_not_include;
    if height == 0 {
        return BftBlockAndFatPointerToItWrap(BftBlockAndFatPointerToIt::from_parts(block, 0, 1, &[]));
    }
    let seed = synthetic_finalizer_seed(SUPPLIED_FINALIZER);
    let fat_ptr = signed_fat_pointer(&block, u64::from(height), 1, &[seed.as_bytes()], &[0; 32]);
    BftBlockAndFatPointerToItWrap(BftBlockAndFatPointerToIt { block, fat_ptr })
}

fn bft_fuzz_grow_pow(f: &mut BftFuzz, c: &mut Choices) {
    for _ in 0..1 + c.below(3) {
        f.pow.push(f.gen.next_block(&f.miner));
        f.tf.push_instr_load_pow(&f.gen.tip, 0);
    }
}

fn bft_fuzz_step(f: &mut BftFuzz, c: &mut Choices) {
    use zebra_state::new_network::bft::BFT_BLOCK_VERSION_LATEST as LATEST;

    let sigma = HARNESS_PARAMETERS.bc_confirmation_depth_sigma as usize;
    // Linearity lets the snapshot stay where the parent's is, but not go below it.
    let (lo, hi) = (if f.decided == 0 { 0 } else { f.tip_from }, f.pow.len() - sigma);
    let from = lo + c.below(hi - lo + 1);
    let headers = f.pow[from..from + sigma].to_vec();
    let lowest_version = f.tip_version.max(1);
    let version = lowest_version + c.below((LATEST - lowest_version + 1) as usize) as u32;

    if c.unusual(128) {
        let above_latest = |c: &mut Choices| match c.below(3) {
            0 => LATEST + 1,
            1 => u32::MAX,
            _ => LATEST + 1 + c.below(1000) as u32,
        };
        let (parent, h) = (&f.tip_ptr, f.decided);
        let bad = match c.below(5) {
            1 if h > 0 => bft_fuzz_block(parent, h, &headers, c.below(f.tip_version as usize) as u32, 0),
            2 => {
                let claimed = if h > 0 && c.unusual(128) { h - 1 } else { h + 1 + c.below(3) as u32 };
                bft_fuzz_block(parent, claimed, &headers, version, 0)
            }
            3 if h > 0 && f.tip_from > 0 => {
                let below = c.below(f.tip_from);
                bft_fuzz_block(parent, h, &f.pow[below..below + sigma], version, 0)
            }
            // No slashfork is scheduled, so the parent's value, 0, is the only one allowed. A v1
            // block doesn't serialize the field, so only the latest version can carry a raise.
            4 => {
                let raised = match c.below(3) {
                    0 => 1,
                    1 => u64::MAX,
                    _ => 1 + c.below(1 << 20) as u64,
                };
                bft_fuzz_block(parent, h, &headers, LATEST, raised)
            }
            _ => bft_fuzz_block(parent, h, &headers, above_latest(c), 0),
        };
        push_decided_pos(&mut f.tf, &bad, SHOULD_FAIL);
        f.tf.push_instr_expect_node_alive(0);
        f.tf.push_instr_expect_pos_chain_length(f.decided as usize, 0);
    }

    let good = bft_fuzz_block(&f.tip_ptr, f.decided, &headers, version, 0);
    push_decided_pos(&mut f.tf, &good, 0);
    f.decided += 1;
    f.tip_ptr = good.0.fat_ptr.clone();
    f.tip_version = version;
    f.tip_from = from;
    f.tf.push_instr_expect_pos_chain_length(f.decided as usize, 0);
}

fn generate_bft(c: &mut Choices) -> Vec<u8> {
    let sigma = HARNESS_PARAMETERS.bc_confirmation_depth_sigma as usize;
    let mut tf = TF::new(&HARNESS_PARAMETERS);
    let (gen, miner, pow) = bonded_pow_chain_for(&mut tf, sigma + c.below(4));
    let mut f = BftFuzz { tf, gen, miner, pow, decided: 0, tip_ptr: FatPointerToBftBlock::null(), tip_version: 0, tip_from: 0 };
    for _ in 0..2 + c.below(14) {
        if f.tip_from + HARNESS_PARAMETERS.bc_confirmation_depth_sigma as usize == f.pow.len() || c.unusual(96) {
            bft_fuzz_grow_pow(&mut f, c);
        }
        bft_fuzz_step(&mut f, c);
    }
    f.tf.push_instr_expect_node_alive(0);
    f.tf.write_to_bytes()
}
// ---- End of BFT ----

// ---- BFT through the engine ----
// Inputs where the node's own tenderlink engine runs: the bonded four-member roster of the BFT
// rounds scenes, activated, then PoW growth (direct and over STP, with side branches) between
// heights the engine decides from the synthetic finalizers' messages, which arrive in any order,
// with duplicates and wrong-namespace votes among them, and the clock moving between them.
//
// The writer must know every decided block, to propose the next height on it, and every
// proposer. The proposer is a pure function of the roster, height and round, and the roster for
// a height is the stake at the snapshot of the block below: active bonds plus reward banks,
// which every PoW block's issuance grows. The writer replays the chain it generated up to each
// snapshot, as the node does, and picks the proposer from that. A round the node would propose
// is failed with nil votes, so the node's own proposals, whose values only it knows, are never
// decided.

struct EngineFuzz {
    tf: TF,
    scene: BftRoundsScene,
    /// `gens[i]` is the generator after the `i`-th block above the scene's, so a side branch can
    /// start below the tip.
    gens: Vec<BlockGen>,
    /// The engine's height and round, as the writer has driven it.
    height: u64,
    round: u32,
    /// The decided block the next proposal builds on, and the PoW height it finalized.
    parent: FatPointerToBftBlock,
    snapshot: u32,
    next_fresh_peer: u64,
    sides: u8,
    /// The best chain's blocks by height (genesis, which carries no staking state, as `None`).
    main: Vec<Option<Arc<Block>>>,
    /// The chain's staking, replayed through `replayed_to`.
    replay: zebra_state::StakingReplay,
    replayed_to: u32,
}

impl EngineFuzz {
    /// The stake each finalizer votes with at `snapshot`: its active bonds plus its bank, as the
    /// node aggregates them.
    fn stakes_at(&mut self, snapshot: u32) -> Vec<([u8; 32], u64)> {
        assert!(snapshot >= self.replayed_to, "snapshots only move up");
        for height in self.replayed_to + 1..=snapshot {
            let block = self.main[height as usize].clone().expect("only genesis is missing");
            let main = &self.main;
            let block_at = |at: zebra_chain::block::Height| main[at.0 as usize].clone().expect("the slash window does not reach genesis");
            // Every block pays issuance, as on the node (see staking_replay_matches).
            self.replay
                .apply_block(&self.scene.gen.network, zebra_chain::block::Height(height), &block, true, block_at)
                .expect("the generated chain replays");
        }
        self.replayed_to = snapshot;
        let mut stakes: std::collections::HashMap<[u8; 32], u64> = Default::default();
        for (bond, status) in self.replay.delegation_bonds.values() {
            if matches!(status, zebra_state::BondStatusInChain::Active) {
                *stakes.entry(bond.target_finalizer).or_default() += u64::from(bond.amount);
            }
        }
        for (finalizer, bank) in &self.replay.finalizer_rewards {
            if *bank != 0 {
                *stakes.entry(*finalizer).or_default() += bank;
            }
        }
        stakes.into_iter().collect()
    }

    /// Who proposes at the engine's height and round: a synthetic finalizer, or TF_BFT_THE_NODE.
    fn proposer(&mut self) -> u64 {
        let members = self.stakes_at(self.snapshot);
        let proposer = bft_proposer(&members, self.scene.active_roster_max_n, self.height, self.round).expect("the roster has stake");
        if proposer == self.scene.member_pub_keys[0].0 {
            return TF_BFT_THE_NODE;
        }
        self.scene
            .finalizers
            .iter()
            .map(|&k| u64::from(k))
            .find(|&k| synthetic_finalizer_pub_key(k) == proposer)
            .expect("the proposer is on the roster")
    }

    fn tip_height(&self) -> u32 {
        self.scene.gen.tip.coinbase_height().expect("generated blocks have a height").0
    }

    fn extend(&mut self, c: &mut Choices) {
        self.gens.push(self.scene.gen.clone());
        let block = self.scene.gen.next_block(&self.scene.miner.clone());
        self.main.push(Some(block.clone()));
        if c.unusual(64) {
            self.tf.push_instr_recv_pow(&block, self.next_fresh_peer, 0);
            self.next_fresh_peer += 1;
        } else {
            self.tf.push_instr_load_pow(&block, 0);
        }
        self.scene.pow.push(block);
        self.tf.push_instr_expect_pow_tip(&self.scene.gen.tip.hash(), 0);
    }

    /// A branch from two or more below the tip, shorter than the best chain, so the tip stays.
    fn side_branch(&mut self, c: &mut Choices) {
        let n = self.gens.len();
        if n < 3 {
            return self.extend(c);
        }
        let from = n - 2 - c.below((n - 2).min(4));
        let mut side = self.gens[from].clone();
        self.sides = self.sides.wrapping_add(1);
        let miner = Address::Transparent(zcash_transparent::address::TransparentAddress::PublicKeyHash([self.sides | 0x80; 20]));
        // Shorter than main: `from + 1` holds main's block at the same height as side's first.
        for _ in 0..1 + c.below(n - from - 1) {
            let block = side.next_block(&miner);
            self.tf.push_instr_load_pow(&block, 0);
        }
        self.tf.push_instr_expect_pow_tip(&self.scene.gen.tip.hash(), 0);
    }

    fn votes(&mut self, bits: u64, id: &[u8; 32]) {
        for k in self.scene.finalizers {
            self.tf.push_instr_bft_recv_vote(k, bits, self.height, self.round, id, 0);
        }
    }

    /// Gives up the current round: every synthetic finalizer prevotes and precommits nil, and the
    /// writer moves on to the next round without waiting out the timeouts. How long those take
    /// depends on whether the node had anything to propose, which the writer does not model; the
    /// next round's messages, from more than a third of the stake, pull the engine into that
    /// round at once (tendermint's round catch-up), whatever this one is doing.
    fn nil_round(&mut self) {
        self.tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
        self.votes(TF_BFT_NIL, &[0; 32]);
        self.votes(TF_BFT_PRECOMMIT | TF_BFT_NIL, &[0; 32]);
        self.round += 1;
    }

    /// Decides the engine's height on a proposal from the round's synthetic proposer. Its
    /// messages (the proposal, three prevotes, three precommits, some sent twice, some
    /// prevotes under the wrong namespace too) arrive in a random order with up to five ticks
    /// between them: a proposal and a quorum of precommits decide whatever the order.
    fn decide(&mut self, c: &mut Choices) {
        let sigma = BFT_ROUNDS_PARAMETERS.bc_confirmation_depth_sigma as u32;
        while self.tip_height() < self.snapshot + 1 + sigma {
            self.extend(c);
        }
        while self.proposer() == TF_BFT_THE_NODE {
            self.nil_round();
        }
        let proposer = u16::try_from(self.proposer()).expect("a synthetic finalizer proposes");
        let snapshot = self.snapshot + 1 + c.below((self.tip_height() - sigma - self.snapshot) as usize) as u32;
        let block = bft_proposal(&BFT_ROUNDS_PARAMETERS, self.height as u32, self.parent.clone(), 0, &self.scene.tail_above(snapshot));
        let value = bft_proposal_value(&block);
        let id = bft_value_id(&value);

        enum Message {
            Proposal,
            Vote(u16, u64),
        }
        let mut messages = vec![Message::Proposal];
        for k in self.scene.finalizers {
            messages.push(Message::Vote(k, 0));
            messages.push(Message::Vote(k, TF_BFT_PRECOMMIT));
            if c.unusual(48) {
                messages.push(Message::Vote(k, if c.below(2) == 0 { TF_BFT_PRECOMMIT } else { 0 }));
            }
            if c.unusual(32) {
                messages.push(Message::Vote(k, TF_BFT_WRONG_NAMESPACE));
            }
        }
        let mut ticks = 1;
        self.tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
        while !messages.is_empty() {
            match messages.remove(c.below(messages.len())) {
                Message::Proposal => {
                    self.tf.push_instr_bft_recv_proposal(proposer, self.height, self.round, -1, 0, &value, 0)
                }
                Message::Vote(k, bits) => self.tf.push_instr_bft_recv_vote(k, bits, self.height, self.round, &id, 0),
            }
            if ticks < 6 && c.unusual(48) {
                ticks += 1;
                self.tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
            }
        }
        // A decision waits on the node's answer before the engine starts the next height, and
        // the last message may arrive after the engine's own precommit: four ticks see both out.
        for _ in 0..4 {
            self.tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
        }

        let seeds: Vec<String> = self.scene.finalizers.iter().map(|&k| synthetic_finalizer_seed(k.into())).collect();
        let seeds: Vec<&[u8]> = seeds.iter().map(|seed| seed.as_bytes()).collect();
        self.parent = signed_fat_pointer(&block, self.height, self.round, &seeds, &[0; 32]);
        self.snapshot = snapshot;
        self.height += 1;
        self.round = 0;
        self.tf.push_instr_expect_bft_step(self.height, 0, None, 0);
        self.tf.push_instr_expect_pos_chain_length(self.height as usize, 0);
    }
}

fn generate_engine(c: &mut Choices) -> Vec<u8> {
    let mut tf = TF::new(&BFT_ROUNDS_PARAMETERS);
    let node_proposes_round_0 = c.below(2) == 0;
    let scene = bft_rounds_scene(&mut tf, &[node_proposes_round_0]);
    let zcash_primitives::bft::BftBootstrap::FromChain { roster_height, .. } = BFT_ROUNDS_PARAMETERS.bootstrap else {
        unreachable!("the BFT rounds scenes bootstrap from the chain")
    };
    let parent = FatPointerToBftBlock::from_parts(scene.genesis().blake3_hash(), 0, 0, &[]);
    // The scene loads only best-chain blocks, so they are the chain from genesis + 1 on.
    let bytes = tf.write_to_bytes();
    let mut main: Vec<Option<Arc<Block>>> = vec![None];
    for instr in &TF::read_from_bytes(&bytes).expect("the scene reads back").instrs {
        if instr.kind == TFInstr::LOAD_POW {
            let block = Block::zcash_deserialize(instr.data_slice(&bytes)).expect("the scene's blocks parse");
            assert_eq!(block.coinbase_height().map(|h| h.0 as usize), Some(main.len()), "the scene loads one chain in order");
            main.push(Some(Arc::new(block)));
        }
    }
    let replay = zebra_state::StakingReplay::new(&Default::default(), BFT_ROUNDS_PARAMETERS.staking.slash_analysis_window());
    let mut f = EngineFuzz {
        tf, scene, gens: Vec::new(), height: 1, round: 0, parent, snapshot: roster_height, next_fresh_peer: FRESH_PEERS_FROM, sides: 0,
        main, replay, replayed_to: 0,
    };
    // BFT starts at the activation height the scene loaded up to.
    f.tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    for _ in 0..2 + c.below(10) {
        match c.below(8) {
            0..=2 => {
                for _ in 0..1 + c.below(3) {
                    f.extend(c);
                }
            }
            3 => f.side_branch(c),
            _ => f.decide(c),
        }
    }
    f.tf.push_instr_expect_node_alive(0);
    f.tf.push_instr_expect_staking_replay_matches(0);
    f.tf.write_to_bytes()
}

/// One engine input, drawn from the seed like the directed scenarios: whatever order a round's
/// messages arrive in, a proposal and a quorum of precommits decide the height.
#[test]
fn crosslink_fuzz_directed_the_engine_decides_each_height_whatever_order_its_messages_arrive_in() {
    set_test_name(function_name!());
    test_bytes(generate_engine(&mut directed_choices()));
}
// ---- End of BFT through the engine ----

type Move = fn(&mut FuzzGen, &mut Choices);

/// The lowest height an input schedules FeaturenetUpgrade2 or FeaturenetUpgrade3 at, and the
/// span above it it may pick from: a run's staking moves reach a few staking periods.
const FUZZ_UTXO_UPGRADE_FROM: u32 = 4;
const FUZZ_UTXO_UPGRADE_SPAN: usize = 60;

/// The moves, most ordinary first: choice 0 picks extend.
const MOVES: &[Move] = &[move_extend, move_fork, move_invalid_block, move_mangled_block, move_spend, move_mine_pending, move_double_spend, move_staking, move_replay, move_retarget_twice];

/// The test file for `c`. Each input enables a subset of the moves, so rare combinations are not
/// drowned out by common ones. Some inputs are BFT chains instead (`generate_bft`), and some run
/// the node's BFT engine (`generate_engine`).
pub(crate) fn generate(c: &mut Choices) -> Vec<u8> {
    if c.unusual(48) {
        return generate_bft(c);
    }
    if c.unusual(64) {
        return generate_engine(c);
    }
    // The short staking calendar, so bonds can unbond and withdraw within one input.
    let params = zcash_primitives::bft::ZcashCrosslinkParameters { staking: short_staking(&HARNESS_PARAMETERS), ..HARNESS_PARAMETERS };
    // A third of the inputs bond under FeaturenetUpgrade3's spent versions and a third refuse
    // finalized repeats under FeaturenetUpgrade2, from early in the run or after some bonds act
    // without them.
    use zebra_chain::parameters::testnet::ConfiguredActivationHeights;
    let upgrades = c.below(3);
    let node_config = match upgrades {
        0 => String::new(),
        1 => node_config_with_activation_heights(ConfiguredActivationHeights {
            featurenet_upgrade_3: Some(FUZZ_UTXO_UPGRADE_FROM + c.below(FUZZ_UTXO_UPGRADE_SPAN) as u32),
            ..Default::default()
        }),
        _ => node_config_with_activation_heights(ConfiguredActivationHeights {
            featurenet_upgrade_2: Some(FUZZ_UTXO_UPGRADE_FROM + c.below(FUZZ_UTXO_UPGRADE_SPAN) as u32),
            ..Default::default()
        }),
    };
    let mut g = FuzzGen::new_with_node_config(&params, &node_config);
    let enabled: Vec<Move> = MOVES.iter().enumerate().filter(|(i, _)| *i == 0 || !c.unusual(64)).map(|(_, m)| *m).collect();
    let moves_n = 4 + c.below(28);
    // A quarter of the inputs replay an action after it is finalized.
    let finalize_at = c.unusual(64).then(|| c.below(moves_n));
    // Three quarters of those that schedule FeaturenetUpgrade3 cross it early with bonds made
    // before it, while the activation is still ahead.
    let cross_at = (upgrades == 1 && c.unusual(192)).then(|| c.below(moves_n.min(4)));
    for k in 0..moves_n {
        if cross_at == Some(k) {
            move_cross_featurenet_upgrade_3(&mut g, c);
        }
        if finalize_at == Some(k) {
            move_finalize(&mut g, c);
        }
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
            eprintln!("fuzz seed {seed}");
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

fn run_child(envs: &[(&str, String)], log: &std::path::Path) -> Verdict {
    let exe = std::env::current_exe().expect("the test binary path");
    let log_file = std::fs::File::create(log).expect("child log creates");
    let mut child = std::process::Command::new(exe)
        .args(["fuzz::crosslink_fuzz_one", "--exact", "--ignored", "--nocapture"])
        .envs(envs.iter().map(|(name, value)| (*name, value.as_str())))
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
            let got = run_child(&[("CROSSLINK_FUZZ_FILE", path.display().to_string()), ("CROSSLINK_TEST_TIMINGS", "off".to_string())], &log);
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

// # Doorways
//
// A block or transaction must get the same verdict whichever way it reaches the node. After an
// input passes, a second run sends some of its direct block loads over STP and some of its local
// transaction submissions over the wire or STP, each made to expect the verdict the first run
// saw, so a doorway that disagrees fails the second run like any other expectation, and is
// minimized like one.

/// The most instructions one doorway pass sends another way.
const DOORWAY_SWAPS_MAX: usize = 8;

/// Peer indices for the doorway pass, above any the generator uses (`FRESH_PEERS_FROM` up).
const DOORWAY_PEERS_FROM: u64 = 10_000;

/// Mixed into an input's seed for the doorway pass's choices, so the input itself is unchanged.
const DOORWAY_SEED_SALT: u64 = 0xd008_3a75;

/// What an instruction's check said in a run, by its timing row; `None` for one with no verdict
/// that the doorways could share (pending, no answer, or no check).
fn seen_verdicts(timings: &std::path::Path) -> std::collections::HashMap<usize, bool> {
    let text = std::fs::read_to_string(timings).unwrap_or_default();
    let mut seen = std::collections::HashMap::new();
    for row in text.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok()) {
        if row["type"] != "instr" {
            continue;
        }
        let (Some(index), Some(accepted), Some(message)) = (row["index"].as_u64(), row["accepted"].as_bool(), row["message"].as_str()) else {
            continue;
        };
        if accepted {
            seen.insert(index as usize, true);
        } else if message.contains("rejected") && !message.contains("pending:") && !message.contains("no verdict") {
            seen.insert(index as usize, false);
        }
    }
    seen
}

/// `bytes` with up to `DOORWAY_SWAPS_MAX` of its direct block loads and local transaction
/// submissions sent another way, each expecting the verdict `seen` gives it, or `None` when none
/// can be. A held block is left alone (a peer is not asked for a block whose parent is missing),
/// as is one that does not parse, has no coinbase height, or claims a height that does not follow
/// its parent's: a peer's STATUS cannot advertise it so that a node would ask for it. A reason
/// check after a swapped
/// instruction is dropped: the doorways word their reasons differently, and only the verdict
/// has to agree.
fn swap_doorways(bytes: &[u8], seen: &std::collections::HashMap<usize, bool>, c: &mut Choices) -> Option<(Vec<u8>, usize)> {
    let tf = TF::read_from_bytes(bytes).ok()?;
    // The heights of the blocks the file holds before each instruction, by hash. A peer's STATUS
    // places a block by its parent and height, and a node rightly ignores one whose height does
    // not follow its parent's: a mangled coinbase can claim any height under an unchanged header.
    let mut heights: std::collections::HashMap<zebra_chain::block::Hash, u32> = Default::default();
    let mut eligible: Vec<usize> = Vec::new();
    for (i, instr) in tf.instrs.iter().enumerate() {
        let block = (instr.kind == TFInstr::LOAD_POW).then(|| Block::zcash_deserialize(instr.data_slice(bytes)).ok()).flatten();
        let ok = seen.contains_key(&i)
            && match instr.kind {
                TFInstr::LOAD_POW => {
                    instr.val[1] == TF_POW_DIRECT
                        && instr.flags & SHOULD_DEFER == 0
                        && block.as_ref().and_then(|block| block.coinbase_height()).is_some_and(|height| {
                            let parent = block.as_ref().expect("a height comes from a block").header.previous_block_hash;
                            match heights.get(&parent) {
                                Some(parent_height) => height.0 == parent_height + 1,
                                // Only genesis, which no file loads, has no earlier block.
                                None => height.0 == 1,
                            }
                        })
                }
                TFInstr::SUBMIT_TX => instr.val[1] == TF_TX_LOCAL,
                _ => false,
            };
        if ok {
            eligible.push(i);
        }
        if let Some(height) = block.as_ref().and_then(|block| block.coinbase_height()) {
            heights.insert(block.expect("a height comes from a block").hash(), height.0);
        }
    }
    if eligible.is_empty() {
        return None;
    }
    let mut eligible = eligible;
    let mut picked = Vec::new();
    for _ in 0..1 + c.below(DOORWAY_SWAPS_MAX.min(eligible.len())) {
        picked.push(eligible.remove(c.below(eligible.len())));
    }
    let mut out = TF { instrs: Vec::new(), data: Vec::new() };
    let mut peer = DOORWAY_PEERS_FROM;
    let mut drop_reason_check = false;
    for (i, instr) in tf.instrs.iter().enumerate() {
        let data = instr.data_slice(bytes);
        if std::mem::take(&mut drop_reason_check) && instr.kind == TFInstr::EXPECT_REJECTION_REASON {
            continue;
        }
        if !picked.contains(&i) {
            out.push_instr_ex(instr.kind, instr.flags, data, instr.val);
            continue;
        }
        let doorway = match instr.kind {
            TFInstr::LOAD_POW => TF_POW_STP_PEER,
            _ if c.below(2) == 0 => TF_TX_WIRE,
            _ => TF_TX_STP_PEER,
        };
        let expect = if seen[&i] { 0 } else { SHOULD_FAIL };
        out.push_instr_ex(instr.kind, (instr.flags & !(SHOULD_FAIL | SHOULD_DEFER | ANY_OUTCOME)) | expect, data, [peer, doorway]);
        peer += 1;
        drop_reason_check = true;
    }
    Some((out.write_to_bytes(), picked.len()))
}

/// A campaign: CROSSLINK_FUZZ_SEEDS (`a..b`, default 0..20) within CROSSLINK_FUZZ_SECS (default
/// 300, inside nextest's per-test bound; run the test binary directly to go longer). Passing
/// inputs are deleted; a failing one is kept with its log, minimized (CROSSLINK_FUZZ_MINIMIZE
/// attempts, default 40, 0 for none), and the campaign fails listing what it found. Each passing
/// input also gets a doorway pass (see # Doorways); CROSSLINK_FUZZ_DOORWAYS=0 turns it off.
#[test]
#[ignore]
fn crosslink_fuzz() {
    let seeds = std::env::var("CROSSLINK_FUZZ_SEEDS").unwrap_or_else(|_| "0..20".to_string());
    let (a, b) = seeds.split_once("..").expect("CROSSLINK_FUZZ_SEEDS is a..b");
    let (a, b): (u64, u64) = (a.parse().expect("seed range start"), b.parse().expect("seed range end"));
    let secs: u64 = std::env::var("CROSSLINK_FUZZ_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(300);
    let attempts: usize = std::env::var("CROSSLINK_FUZZ_MINIMIZE").ok().and_then(|s| s.parse().ok()).unwrap_or(40);
    let doorways = std::env::var("CROSSLINK_FUZZ_DOORWAYS").map_or(true, |v| v != "0");
    let dir = fuzz_dir();
    let start = std::time::Instant::now();
    let mut findings = Vec::new();
    let mut ran = 0;
    let mut swapped_n = 0;
    for seed in a..b {
        if start.elapsed().as_secs() >= secs {
            break;
        }
        let log = dir.join(format!("seed-{seed}.log"));
        let timings = dir.join(format!("seed-{seed}.timings.jsonl"));
        let _ = std::fs::remove_file(&timings);
        let verdict = run_child(
            &[("CROSSLINK_FUZZ_SEED", seed.to_string()), ("CROSSLINK_TEST_TIMINGS", timings.display().to_string())],
            &log,
        );
        ran += 1;
        let input = dir.join(format!("seed-{seed}.zeccltf"));
        eprintln!("fuzz seed {seed}: {verdict:?}");
        let (name, input, verdict) = if verdict != Verdict::Pass {
            (format!("seed-{seed}"), input, verdict)
        } else {
            let swapped = if doorways {
                let bytes = std::fs::read(&input).expect("the passing input was written");
                swap_doorways(&bytes, &seen_verdicts(&timings), &mut Choices::from_seed(seed ^ DOORWAY_SEED_SALT, 256))
            } else {
                None
            };
            let _ = std::fs::remove_file(&input);
            let _ = std::fs::remove_file(&log);
            let _ = std::fs::remove_file(&timings);
            let Some((swapped, n)) = swapped else {
                continue;
            };
            swapped_n += n;
            let name = format!("seed-{seed}.doorways");
            let swapped_input = dir.join(format!("{name}.zeccltf"));
            std::fs::write(&swapped_input, &swapped).expect("the doorway input writes");
            let log = dir.join(format!("{name}.log"));
            let verdict = run_child(
                &[("CROSSLINK_FUZZ_FILE", swapped_input.display().to_string()), ("CROSSLINK_TEST_TIMINGS", "off".to_string())],
                &log,
            );
            eprintln!("fuzz seed {seed}: {n} instruction(s) through other doorways: {verdict:?}");
            if verdict == Verdict::Pass {
                let _ = std::fs::remove_file(&swapped_input);
                let _ = std::fs::remove_file(&log);
                continue;
            }
            (name, swapped_input, verdict)
        };
        let _ = std::fs::remove_file(dir.join(format!("seed-{seed}.timings.jsonl")));
        let bytes = std::fs::read(&input).expect("the failing input was written");
        if attempts > 0 {
            let same = |got: &Verdict, _: &std::path::Path| *got == verdict;
            let min = minimize(bytes.clone(), &same, &name, attempts);
            std::fs::write(dir.join(format!("{name}.min.zeccltf")), &min).expect("minimized input writes");
            eprintln!("fuzz {name}: minimized {} -> {} bytes", bytes.len(), min.len());
        }
        findings.push((name, verdict));
    }
    eprintln!("fuzz: {swapped_n} instruction(s) sent through other doorways");
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
// what the finding says. A fixed one stays as a regression test.

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
/// block 4's children. Fixed by removing the hash from the map when a block with it commits. A
/// copy that arrives after the real block never reaches the map: ingest answers it as known.
#[test]
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
// class, CVE-2012-2459 in Bitcoin). Each scenario: a few blocks, the copy (refused), the real
// block (must be taken), and a few more on top.

/// The choices for a directed scenario, from CROSSLINK_FUZZ_SEED or else a fresh random seed, so
/// each run tries the scenario another way. The seed goes straight to stderr, which the test
/// harness's capture does not hold back, since a failing scenario ends the process.
fn directed_choices() -> Choices {
    use std::hash::{BuildHasher, Hasher};
    use std::io::Write;

    let seed: u64 = match std::env::var("CROSSLINK_FUZZ_SEED") {
        Ok(seed) => seed.parse().expect("CROSSLINK_FUZZ_SEED is a number"),
        Err(_) => std::collections::hash_map::RandomState::new().build_hasher().finish(),
    };
    let _ = writeln!(std::io::stderr(), "fuzz seed {seed}: set CROSSLINK_FUZZ_SEED={seed} to run this again");
    Choices::from_seed(seed, 4096)
}

/// Three blocks, a peer serving `serve(real block's bytes)` under the real block's claim (refused
/// for `reason`, since what it serves is not that block), the real block (must be taken), and one
/// more on top.
fn claimed_garbage_then_real(serve: fn(Vec<u8>) -> Vec<u8>, reason: &str) -> Vec<u8> {
    let mut c = directed_choices();
    let mut g = FuzzGen::new(&HARNESS_PARAMETERS);
    for _ in 0..1 + c.below(6) {
        move_extend(&mut g, &mut c);
    }
    let mut next = g.tip().clone();
    let real = next.next_block(&g.miner.clone());
    let served = serve(real.zcash_serialize_to_vec().expect("a generated block serializes"));
    g.tf.push_instr_recv_pow_claimed(StpClaim::of(&real).expect("a generated block has a height"), &served, FRESH_PEERS_FROM, SHOULD_FAIL);
    g.tf.push_instr_expect_rejection_reason(reason, 0);
    g.tf.push_instr_expect_node_alive(0);
    g.tf.push_instr_load_pow(&real, 0);
    g.main.push(next);
    g.expect_tip();
    for _ in 0..1 + c.below(3) {
        move_extend(&mut g, &mut c);
    }
    g.tf.write_to_bytes()
}

#[test]
fn crosslink_fuzz_directed_truncated_bytes_under_a_claim_do_not_condemn_the_real_block() {
    set_test_name(function_name!());
    test_bytes(claimed_garbage_then_real(
        |mut bytes| {
            bytes.truncate(bytes.len() / 2);
            bytes
        },
        "Failed to deserialize block",
    ));
}

#[test]
fn crosslink_fuzz_directed_unparseable_bytes_under_a_claim_do_not_condemn_the_real_block() {
    set_test_name(function_name!());
    test_bytes(claimed_garbage_then_real(|bytes| vec![0xff; bytes.len()], "Failed to deserialize block"));
}

#[test]
fn crosslink_fuzz_directed_another_block_under_a_claim_does_not_condemn_the_real_block() {
    set_test_name(function_name!());
    // A valid block's bytes, but not the claimed one's: its header has one bit of its nonce flipped.
    test_bytes(claimed_garbage_then_real(
        |mut bytes| {
            bytes[4 + 32 + 32 + 32 + 4 + 4] ^= 1;
            bytes
        },
        "did not match advertised hash",
    ));
}

fn refused_body_then_real(mutate: fn(&mut Block)) -> Vec<u8> {
    let mut c = directed_choices();
    let mut g = FuzzGen::new(&HARNESS_PARAMETERS);
    for _ in 0..1 + c.below(6) {
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
    for _ in 0..1 + c.below(3) {
        move_extend(&mut g, &mut c);
    }
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

// # Directed: a mined transaction mined again
//
// A transaction with no spends has no nullifier or outpoint to stop it being mined twice. A
// retarget is one: once its bond is moved back to the finalizer it left, it passes every staking
// rule again, and only its hash shows it is a replay. Each scenario: a bond to finalizer 0,
// retargeted to another, `blocks_between` empty blocks and a few more, retargeted back to 0, a
// few more blocks, and the first retarget mined again.

fn replayed_retarget(
    featurenet_upgrade_2: Option<u32>,
    featurenet_upgrade_3: Option<u32>,
    blocks_between: u32,
    refused: bool,
) -> Vec<u8> {
    use zebra_chain::parameters::testnet::ConfiguredActivationHeights;

    let node_config = if featurenet_upgrade_2.is_some() || featurenet_upgrade_3.is_some() {
        node_config_with_activation_heights(ConfiguredActivationHeights {
            featurenet_upgrade_2,
            featurenet_upgrade_3,
            ..Default::default()
        })
    } else {
        String::new()
    };
    let upgrade_height = featurenet_upgrade_2.max(featurenet_upgrade_3);
    let params = zcash_primitives::bft::ZcashCrosslinkParameters { staking: short_staking(&HARNESS_PARAMETERS), ..HARNESS_PARAMETERS };
    let mut c = directed_choices();
    let mut g = FuzzGen::new_with_node_config(&params, &node_config);
    let staking = params.staking;
    while (g.height() + 1) % staking.period >= staking.day_window {
        mine(&mut g, &mut c, &[]);
    }
    let seed = b"crosslink fuzz replayed retarget".to_vec();
    let create = staking_tx_create_bond(g.tip().next_upgrade(), &seed, fuzz_finalizer(0), 0);
    let created = create.hash().0;
    assert!(mine(&mut g, &mut c, &[create]));
    // The replay must carry the network upgrade of the height it is mined at, or it is refused for that.
    while g.height() + 1 < upgrade_height.unwrap_or(0) {
        mine(&mut g, &mut c, &[]);
    }

    // Under FeaturenetUpgrade3 the first retarget spends the bond's first version, the create's
    // txid, and the second spends the first's txid.
    let away_spends = spends_versions(&g).then_some(created);
    let to = 1 + c.below(2);
    let away = fuzz_retarget(&mut g, &seed, 0, to, away_spends);
    assert!(mine(&mut g, &mut c, &[away.clone()]));
    load_empty_blocks(&mut g, blocks_between + c.below(16) as u32);
    for _ in 0..c.below(8) {
        mine(&mut g, &mut c, &[]);
    }
    let back_spends = spends_versions(&g).then_some(away.hash().0);
    let back = fuzz_retarget(&mut g, &seed, to, 0, back_spends);
    assert!(mine(&mut g, &mut c, &[back]));
    for _ in 0..c.below(4) {
        mine(&mut g, &mut c, &[]);
    }

    if refused {
        let block = g.tip().clone().next_block_with_txs(&g.miner.clone(), &[away]);
        refuse(&mut g, &mut c, &block);
    } else {
        assert!(mine(&mut g, &mut c, &[away]));
    }
    move_extend(&mut g, &mut c);
    g.tf.push_instr_expect_node_alive(0);
    g.tf.write_to_bytes()
}

/// Issue #260: the first copy is in the non-finalized chain. The replay is refused there under
/// any network upgrade, and the node keeps running.
#[test]
fn crosslink_fuzz_directed_replayed_retarget_is_refused() {
    set_test_name(function_name!());
    test_bytes(replayed_retarget(None, None, 0, true));
}

/// Under FeaturenetUpgrade3 the replay spends a bond version the retarget back already spent.
#[test]
fn crosslink_fuzz_directed_replayed_retarget_is_refused_under_featurenet_upgrade_3() {
    set_test_name(function_name!());
    test_bytes(replayed_retarget(None, Some(REPLAY_UPGRADE), 0, true));
}

/// Where the replay scenarios schedule FeaturenetUpgrade2 or FeaturenetUpgrade3: below the first
/// retarget.
const REPLAY_UPGRADE: u32 = 4;

#[test]
fn crosslink_fuzz_directed_replayed_finalized_retarget_is_refused_from_featurenet_upgrade_2() {
    set_test_name(function_name!());
    test_bytes(replayed_retarget(Some(REPLAY_UPGRADE), None, FINALIZED_BETWEEN, true));
}

/// Scheduling FeaturenetUpgrade3 alone activates FeaturenetUpgrade2 with it, so its duplicate
/// check refuses the replay before the spent version it carries is looked at.
#[test]
fn crosslink_fuzz_directed_replayed_finalized_retarget_is_refused_from_featurenet_upgrade_3() {
    set_test_name(function_name!());
    test_bytes(replayed_retarget(None, Some(REPLAY_UPGRADE), FINALIZED_BETWEEN, true));
}

/// The control: without FeaturenetUpgrade2 or FeaturenetUpgrade3 a replay of a finalized
/// transaction is valid, as it is on the deployed chain, so refusing it is those upgrades' rule.
#[test]
fn crosslink_fuzz_directed_replayed_finalized_retarget_is_accepted_without_featurenet_upgrade_2() {
    set_test_name(function_name!());
    test_bytes(replayed_retarget(None, None, FINALIZED_BETWEEN, false));
}

// # Directed: a bond's first spend
//
// Under FeaturenetUpgrade3 a bond's first version is the txid of the transaction that created it,
// never its key. The node finds that txid in the non-finalized chain or, once the create is
// finalized, in the finalized state. Each scenario: a bond made before the upgrade to a random
// finalizer, a random run of empty blocks that leaves the create in the non-finalized chain or
// finalizes it, retargets spending the bond's key and sometimes other wrong versions, in random
// order, each of which must be refused, then one spending the create's txid, which must be taken.
// A heavier branch from a random height at or below that block then undoes it, so the bond is
// back at its first version, and the same retarget or another one spending it is taken again.

fn first_bond_spend(finalized: bool) -> Vec<u8> {
    use zebra_chain::parameters::testnet::ConfiguredActivationHeights;

    let node_config = node_config_with_activation_heights(ConfiguredActivationHeights {
        featurenet_upgrade_3: Some(REPLAY_UPGRADE),
        ..Default::default()
    });
    let params = zcash_primitives::bft::ZcashCrosslinkParameters { staking: short_staking(&HARNESS_PARAMETERS), ..HARNESS_PARAMETERS };
    let mut c = directed_choices();
    let mut g = FuzzGen::new_with_node_config(&params, &node_config);
    let staking = params.staking;
    while (g.height() + 1) % staking.period >= staking.day_window {
        mine(&mut g, &mut c, &[]);
    }
    let seed = b"crosslink fuzz first bond spend".to_vec();
    let from = c.below(3);
    let create = staking_tx_create_bond(g.tip().next_upgrade(), &seed, fuzz_finalizer(from), 0);
    let created = create.hash().0;
    let key = create.staking_action().expect("a staking transaction").bond_key();
    assert!(mine(&mut g, &mut c, &[create]));
    let created_at = g.height();
    while g.height() + 1 < REPLAY_UPGRADE {
        mine(&mut g, &mut c, &[]);
    }
    let blocks_between = if finalized { FINALIZED_BETWEEN + c.below(16) as u32 } else { c.below(REPLAY_DEPTH_MAX as usize) as u32 };
    load_empty_blocks(&mut g, blocks_between);
    assert!(spends_versions(&g));
    g.tf.push_instr_expect_bond(key, TF_BOND_ACTIVE, TEST_STAKE_IGNORED, 0);
    g.tf.push_instr_expect_staking_replay_matches(0);

    let mut wrongs = vec![Some(key)];
    if c.unusual(128) {
        wrongs.push(None);
    }
    if c.unusual(128) {
        wrongs.push(Some([0xee; 32]));
    }
    for i in (1..wrongs.len()).rev() {
        wrongs.swap(i, c.below(i + 1));
    }
    let other = |c: &mut Choices| (from + 1 + c.below(2)) % 3;
    for spends in wrongs {
        let to = other(&mut c);
        let wrong = fuzz_retarget(&mut g, &seed, from, to, spends);
        let block = g.tip().clone().next_block_with_txs(&g.miner.clone(), &[wrong]);
        refuse(&mut g, &mut c, &block);
    }
    let to = other(&mut c);
    let right = fuzz_retarget(&mut g, &seed, from, to, Some(created));
    assert!(mine(&mut g, &mut c, &[right.clone()]));
    let spent_at = g.height();
    g.tf.push_instr_expect_bond(key, TF_BOND_ACTIVE, TEST_STAKE_IGNORED, 0);
    g.tf.push_instr_expect_staking_replay_matches(0);
    for _ in 0..c.below(4) {
        mine(&mut g, &mut c, &[]);
    }

    // The branch leaves the create in place: it grows from the spend's parent or a little below.
    let fork_from = spent_at - 1 - c.below((spent_at - created_at).min(FORK_DEPTH_MAX as u32) as usize) as u32;
    g.forks_made += 1;
    let fork_miner = TestKey::new(format!("crosslink fuzz fork {}", g.forks_made).as_bytes()).address();
    let mut branch: Vec<BlockGen> = g.main[..fork_from as usize].to_vec();
    let branch_len = g.main.len() + 1 + c.below(2);
    while branch.len() < branch_len {
        let mut next = branch.last().expect("the branch starts above genesis").clone();
        let block = next.next_block(&fork_miner);
        branch.push(next);
        g.deliver(&mut c, &block, 0);
    }
    g.main = branch;
    g.expect_tip();
    g.tf.push_instr_expect_staking_replay_matches(0);
    let again = if c.below(2) == 0 { right } else { fuzz_retarget(&mut g, &seed, from, other(&mut c), Some(created)) };
    assert!(mine(&mut g, &mut c, &[again]));
    g.tf.push_instr_expect_staking_replay_matches(0);
    for _ in 0..1 + c.below(3) {
        move_extend(&mut g, &mut c);
    }
    g.tf.push_instr_expect_node_alive(0);
    g.tf.write_to_bytes()
}

#[test]
fn crosslink_fuzz_directed_first_bond_spend_names_its_create() {
    set_test_name(function_name!());
    test_bytes(first_bond_spend(false));
}

#[test]
fn crosslink_fuzz_directed_first_bond_spend_names_its_finalized_create() {
    set_test_name(function_name!());
    test_bytes(first_bond_spend(true));
}

/// How often the generator emits each kind of instruction (LOAD_POW and SUBMIT_TX by doorway), with
/// which outcome flag, over CROSSLINK_FUZZ_SEEDS (default 0..200), and how often a bond is
/// expected in each status. Runs no node: it shows whether a move's paths come up at all, and
/// whether its hard expectations are drowned out by ANY_OUTCOME.
#[test]
#[ignore]
fn crosslink_fuzz_census() {
    let seeds = std::env::var("CROSSLINK_FUZZ_SEEDS").unwrap_or_else(|_| "0..200".to_string());
    let (a, b) = seeds.split_once("..").expect("CROSSLINK_FUZZ_SEEDS is a..b");
    let (a, b): (u64, u64) = (a.parse().expect("seed range start"), b.parse().expect("seed range end"));
    let mut counts: std::collections::BTreeMap<(u8, u64, &str), usize> = Default::default();
    let mut bonds: std::collections::BTreeMap<u64, usize> = Default::default();
    let mut instrs_n = 0;
    for seed in a..b {
        let tf = TF::read_from_bytes(&generate(&mut Choices::from_seed(seed, 4096))).expect("a generated file reads");
        instrs_n += tf.instrs.len();
        for instr in &tf.instrs {
            let outcome = if instr.flags & ANY_OUTCOME != 0 {
                "any"
            } else if instr.flags & SHOULD_FAIL != 0 {
                "fail"
            } else {
                "pass"
            };
            let doorway = if matches!(instr.kind, TFInstr::LOAD_POW | TFInstr::SUBMIT_TX) { instr.val[1] } else { 0 };
            *counts.entry((instr.kind as u8, doorway, outcome)).or_default() += 1;
            if instr.kind == TFInstr::EXPECT_BOND {
                *bonds.entry(instr.val[1]).or_default() += 1;
            }
        }
    }
    eprintln!("census over seeds {a}..{b}: {instrs_n} instructions");
    for ((kind, doorway, outcome), n) in &counts {
        eprintln!("  {:<32} {outcome:<4} {n}", format!("{} ({kind}.{doorway})", TFInstr::str_from_kind(u32::from(*kind))));
    }
    eprintln!("  EXPECT_BOND by status (0 active, 1 unbonding, 2 withdrawn, 255 absent): {bonds:?}");
}

// # The generator's own invariants
//
// These run no node. They read what `generate` writes over many seeds, so a mistake in the model
// fails here instead of turning up as a false finding in a campaign.

/// The seeds the generator checks read: CROSSLINK_FUZZ_SEEDS, or 0..200.
fn generator_seeds() -> std::ops::Range<u64> {
    let seeds = std::env::var("CROSSLINK_FUZZ_SEEDS").unwrap_or_else(|_| "0..200".to_string());
    let (a, b) = seeds.split_once("..").expect("CROSSLINK_FUZZ_SEEDS is a..b");
    a.parse().expect("seed range start")..b.parse().expect("seed range end")
}

/// The blocks of an input's LOAD_POWs that parse and have a height, with their flags.
fn generated_blocks(bytes: &[u8]) -> Vec<(u32, u32, Block)> {
    let tf = TF::read_from_bytes(bytes).expect("a generated file reads");
    tf.instrs
        .iter()
        .filter(|instr| instr.kind == TFInstr::LOAD_POW)
        .filter_map(|instr| {
            let block = Block::zcash_deserialize(instr.data_slice(bytes)).ok()?;
            Some((instr.flags, block.coinbase_height()?.0, block))
        })
        .collect()
}

/// Every block an input expects the node to take holds only transactions made for its height's
/// network upgrade, which its coinbase carries.
#[test]
fn crosslink_fuzz_generator_expects_blocks_under_one_network_upgrade() {
    let mut bad = Vec::new();
    for seed in generator_seeds() {
        let bytes = generate(&mut Choices::from_seed(seed, 4096));
        for (flags, height, block) in generated_blocks(&bytes) {
            let upgrade = block.transactions[0].network_upgrade();
            if flags == 0 && block.transactions.iter().any(|tx| tx.network_upgrade() != upgrade) {
                bad.push((seed, height));
            }
        }
    }
    assert!(bad.is_empty(), "blocks expected valid with a transaction from another network upgrade, as (seed, height): {bad:?}");
}

/// Some inputs mine a transaction again more than FINALIZED_BETWEEN blocks after it was first
/// mined, in a block the node must refuse.
#[test]
fn crosslink_fuzz_generator_replays_finalized_transactions() {
    let mut inputs = 0;
    for seed in generator_seeds() {
        let bytes = generate(&mut Choices::from_seed(seed, 4096));
        let mut mined: std::collections::HashMap<zebra_chain::transaction::Hash, u32> = Default::default();
        let mut found = false;
        for (flags, height, block) in generated_blocks(&bytes) {
            let txs = &block.transactions[1..];
            if flags == 0 {
                for tx in txs {
                    mined.entry(tx.hash()).or_insert(height);
                }
            } else if flags == SHOULD_FAIL {
                found |= txs.iter().any(|tx| mined.get(&tx.hash()).is_some_and(|&first| height > first + FINALIZED_BETWEEN));
            }
        }
        inputs += usize::from(found);
    }
    eprintln!("{inputs} inputs replay a finalized transaction in a block the node must refuse");
    assert!(inputs >= 5, "only {inputs} inputs replay a finalized transaction in a block the node must refuse");
}

/// Under FeaturenetUpgrade3 a bond's first spend names the txid of its create. Some inputs mine one
/// in a block the node must take with the create still in the non-finalized chain, some with the
/// create finalized, and some then move the tip off the spend's block, so the node undoes it; some
/// refuse a block whose action spends the bond's key instead.
#[test]
fn crosslink_fuzz_generator_spends_bonds_from_their_create() {
    use std::collections::HashMap;

    const KINDS: [&str; 4] = ["create in the chain", "create finalized", "spend undone by a reorg", "bond key refused"];
    let mut inputs = [0usize; 4];
    for seed in generator_seeds() {
        let bytes = generate(&mut Choices::from_seed(seed, 4096));
        let tf = TF::read_from_bytes(&bytes).expect("a generated file reads");
        let mut parents: HashMap<BlockHash, (u32, BlockHash)> = HashMap::new();
        let mut creates: HashMap<[u8; 32], ([u8; 32], u32)> = HashMap::new();
        let mut first_spends: Vec<(BlockHash, u32)> = Vec::new();
        let mut found = [false; 4];
        for instr in &tf.instrs {
            if instr.kind == TFInstr::EXPECT_POW_TIP {
                let Ok(tip) = <[u8; 32]>::try_from(instr.data_slice(&bytes)) else { continue };
                let holds = |block: BlockHash, height: u32| {
                    let mut at = BlockHash(tip);
                    while let Some(&(h, parent)) = parents.get(&at) {
                        if h <= height {
                            break;
                        }
                        at = parent;
                    }
                    at == block
                };
                found[2] |= first_spends.iter().any(|&(block, height)| !holds(block, height));
                continue;
            }
            if instr.kind != TFInstr::LOAD_POW {
                continue;
            }
            let Ok(block) = Block::zcash_deserialize(instr.data_slice(&bytes)) else { continue };
            let Some(height) = block.coinbase_height() else { continue };
            parents.insert(block.hash(), (height.0, block.header.previous_block_hash));
            for tx in &block.transactions[1..] {
                let Some(action) = tx.staking_action() else { continue };
                let key = action.bond_key();
                if !action.acts_on_existing_bond() {
                    if instr.flags == 0 {
                        creates.entry(key).or_insert((tx.hash().0, height.0));
                    }
                    continue;
                }
                match (instr.flags, action.spends(), creates.get(&key)) {
                    (0, Some(spends), Some(&(created, at))) if spends == created => {
                        found[usize::from(height.0 > at + FINALIZED_BETWEEN)] = true;
                        first_spends.push((block.hash(), height.0));
                    }
                    (SHOULD_FAIL, Some(spends), Some(_)) if spends == key => found[3] = true,
                    _ => {}
                }
            }
        }
        for (n, found) in inputs.iter_mut().zip(found) {
            *n += usize::from(found);
        }
    }
    let counts: Vec<String> = KINDS.iter().zip(inputs).map(|(kind, n)| format!("{kind}: {n}")).collect();
    eprintln!("inputs with a bond's first spend: {}", counts.join(", "));
    assert!(inputs.iter().all(|&n| n >= 10), "too few inputs with a bond's first spend: {}", counts.join(", "));
}
