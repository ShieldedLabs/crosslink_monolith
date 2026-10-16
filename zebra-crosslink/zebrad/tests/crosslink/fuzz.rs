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
/// (height and block hash on `main`).
#[derive(Clone)]
pub(crate) struct FuzzBond {
    pub seed: Vec<u8>,
    pub key: [u8; 32],
    pub state: FuzzBondState,
    pub at: (u32, BlockHash),
    pub known: bool,
}

impl FuzzGen {
    pub(crate) fn new(params: &zcash_primitives::bft::ZcashCrosslinkParameters) -> FuzzGen {
        let network = regtest_network(params);
        let key = TestKey::new(b"crosslink test miner");
        let miner = key.address();
        let gen = BlockGen::init_at_genesis_plus_1(network.clone(), BlockGen::REGTEST_GENESIS_HASH, &miner);
        let mut tf = TF::new(params);
        tf.push_instr_load_pow(&gen.tip, 0);
        let model_tip = (1, gen.tip.hash());
        FuzzGen {
            tf, network, key, miner, main: vec![gen], tip_known: true, rival_height: 0, next_fresh_peer: FRESH_PEERS_FROM, forks_made: 0,
            params: params.clone(), pending: Vec::new(), mined_spends: Vec::new(), unsure_coins: Default::default(), bonds: Vec::new(),
            txs_made: 0, model_rival_height: 0, model_tip,
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
    let (kept, dropped): (Vec<FuzzMinedSpend>, Vec<FuzzMinedSpend>) =
        std::mem::take(&mut g.mined_spends).into_iter().partition(|s| on_main(g, (s.height, s.block)));
    g.mined_spends = kept;
    g.unsure_coins.extend(dropped.into_iter().map(|s| s.coin.0));
    for i in 0..g.bonds.len() {
        let bond = &g.bonds[i];
        if bond.at.0 != 0 && !on_main(g, bond.at) {
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
        let wire = zebra_network::wire::tx_message_bytes(&g.network, zebra_chain::transaction::UnminedTx::from(tx.clone()))
            .expect("a transaction frames");
        g.tf.push_instr_recv_tx_wire(&wire, 0, 0);
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
        &zebra_crosslink::rng_private_public_key_from_address(format!("crosslink fuzz finalizer {i}").as_bytes()).1,
    )
}

/// A bond action in the next block: create, unbond or withdraw, on a new bond or one made
/// before, valid or not. Whether it is valid follows from the bond's state and the staking
/// calendar, the rules the node applies: actions only on staking days, and each action at least
/// `action_delay` after the bond's last one. Sometimes first mines up to the next height where
/// that action could be valid, so valid actions are common rather than rare.
fn move_staking(g: &mut FuzzGen, c: &mut Choices) {
    refresh_model(g);
    let staking = g.params.staking;
    // Mostly an action on a bond in the state it needs, so bonds move through their whole life;
    // sometimes a bond in any state, or one never created, for the refusals.
    let needs = |action: usize| [FuzzBondState::Absent, FuzzBondState::Active, FuzzBondState::Unbonding][action];
    let fitting = |g: &FuzzGen, action: usize| -> Vec<usize> {
        (0..g.bonds.len()).filter(|&i| g.bonds[i].known && g.bonds[i].state == needs(action)).collect()
    };
    // Advancing a bond that exists is the ordinary choice: a whole life takes three actions, and
    // staking is one move in eight.
    let mut action = if !fitting(g, 2).is_empty() && !c.unusual(128) {
        2
    } else if !fitting(g, 1).is_empty() && !c.unusual(128) {
        1
    } else {
        c.below(3)
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
        g.txs_made += 1;
        let seed = format!("crosslink fuzz bond {}", g.txs_made).into_bytes();
        let key = staking_tx_create_bond(g.tip().next_upgrade(), &seed, fuzz_finalizer(0), 0)
            .staking_action()
            .expect("a staking transaction")
            .bond_key();
        g.bonds.push(FuzzBond { seed, key, state: FuzzBondState::Absent, at: (0, BlockHash([0; 32])), known: true });
        g.bonds.len() - 1
    };

    let is_staking_day = |h: u32| h % staking.period < staking.day_window;
    let bond = g.bonds[bond_i].clone();
    if !c.unusual(96) {
        let earliest = if bond.at.0 == 0 { 0 } else { bond.at.0 + staking.action_delay };
        while g.height() + 1 < earliest || !is_staking_day(g.height() + 1) {
            mine(g, c, &[]);
        }
    }

    let h = g.height() + 1;
    let upgrade = g.tip().next_upgrade();
    let wrong_amount = action == 2 && c.unusual(32);
    let (tx, valid, after) = match action {
        0 => (
            staking_tx_create_bond(upgrade, &bond.seed, fuzz_finalizer(c.below(3)), 0),
            bond.state == FuzzBondState::Absent,
            FuzzBondState::Active,
        ),
        1 => (staking_tx_unbond(upgrade, &bond.seed), bond.state == FuzzBondState::Active, FuzzBondState::Unbonding),
        _ => (
            staking_tx_withdraw(upgrade, &bond.seed, u64::from(wrong_amount), 0),
            bond.state == FuzzBondState::Unbonding && !wrong_amount,
            FuzzBondState::Withdrawn,
        ),
    };
    let delay_met = bond.at.0 == 0 || h >= bond.at.0 + staking.action_delay;
    let valid = valid && delay_met && is_staking_day(h);

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
        let known = mine(g, c, &[tx]);
        let bond = &mut g.bonds[bond_i];
        bond.state = after;
        bond.at = (h, g.main[h as usize - 1].tip.hash());
        bond.known = known;
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
// ---- End of transaction and staking moves ----

type Move = fn(&mut FuzzGen, &mut Choices);

/// The moves, most ordinary first: choice 0 picks extend.
const MOVES: &[Move] = &[move_extend, move_fork, move_invalid_block, move_mangled_block, move_spend, move_mine_pending, move_double_spend, move_staking];

/// The test file for `c`. Each input enables a subset of the moves, so rare combinations are not
/// drowned out by common ones.
pub(crate) fn generate(c: &mut Choices) -> Vec<u8> {
    // The short staking calendar, so bonds can unbond and withdraw within one input.
    let params = zcash_primitives::bft::ZcashCrosslinkParameters { staking: short_staking(&HARNESS_PARAMETERS), ..HARNESS_PARAMETERS };
    let mut g = FuzzGen::new(&params);
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

/// How often the generator emits each kind of instruction, with which outcome flag, over
/// CROSSLINK_FUZZ_SEEDS (default 0..200), and how often a bond is expected in each status. Runs
/// no node: it shows whether a move's paths come up at all, and whether its hard expectations are
/// drowned out by ANY_OUTCOME.
#[test]
#[ignore]
fn crosslink_fuzz_census() {
    let seeds = std::env::var("CROSSLINK_FUZZ_SEEDS").unwrap_or_else(|_| "0..200".to_string());
    let (a, b) = seeds.split_once("..").expect("CROSSLINK_FUZZ_SEEDS is a..b");
    let (a, b): (u64, u64) = (a.parse().expect("seed range start"), b.parse().expect("seed range end"));
    let mut counts: std::collections::BTreeMap<(u8, &str), usize> = Default::default();
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
            *counts.entry((instr.kind as u8, outcome)).or_default() += 1;
            if instr.kind == TFInstr::EXPECT_BOND {
                *bonds.entry(instr.val[1]).or_default() += 1;
            }
        }
    }
    eprintln!("census over seeds {a}..{b}: {instrs_n} instructions");
    for ((kind, outcome), n) in &counts {
        eprintln!("  kind {kind:>3} {outcome:<4} {n}");
    }
    eprintln!("  EXPECT_BOND by status (0 active, 1 unbonding, 2 withdrawn, 255 absent): {bonds:?}");
}
