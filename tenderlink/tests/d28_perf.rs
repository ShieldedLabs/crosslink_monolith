//! Dependency-free D28 receive harness (Fix #412).
//!
//! Release benchmark (prints parseable `D28_PERF ...` lines):
//!   cargo test --locked --release --manifest-path tenderlink/Cargo.toml \
//!     --test d28_perf -- --ignored --nocapture
//! Env: D28_PERF_N="4,16,64,128" (add 256 optionally), D28_PERF_REPEATS (receives
//! per sample; default max(1, 256/N)), D28_PERF_SAMPLES (default 5).
//!
//! Every fixture (config, keys, real ed25519 signatures, frames, engines, cached
//! prestate) is built outside the timed interval; only the `receive` calls are
//! timed. The local engine is member N-1 and never appears among the fixture
//! signers. Each sample asserts the exact post-state and a checksum, so the work
//! cannot be elided and must hit the intended path. Units are typed signed votes
//! and proposal body bytes (always 0), not wire bytes or memory.
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use std::hint::black_box;
use std::time::Instant;
use tenderlink::d28::*;

fn key(i: usize) -> SigningKey {
    let mut b = [0x5a; 32];
    b[..4].copy_from_slice(&(i as u32 + 1).to_le_bytes());
    SigningKey::from(b)
}
fn config(n: usize) -> Config {
    Config::new(
        [28; 32],
        (0..n)
            .map(|i| Member {
                public_key: VerificationKeyBytes::from(&key(i)).into(),
                weight: 1,
            })
            .collect(),
        1024,
        3,
        Timeouts {
            propose: 30,
            prevote: 15,
            precommit: 25,
            inc_propose: 5,
            inc_prevote: 1,
            inc_precommit: 3,
        },
    )
    .unwrap()
}
fn vote(c: &Config, i: usize, r: u64, id: Option<Id>) -> Vote {
    Vote::sign(
        c,
        &key(i),
        VoteContent {
            height: 0,
            round: r,
            kind: VoteKind::Prevote,
            id,
            signer: i as u32,
        },
    )
}
#[derive(Default)]
struct Counter {
    sent: usize,
    decided: usize,
}
impl Sink for Counter {
    fn send(&mut self, _: Target, _: Frame<'_>) {
        self.sent += 1;
    }
    fn decided(&mut self, _: u64, _: Proposal<'_>, _: Group<'_>) {
        self.decided += 1;
    }
}
type Eng = (Engine<MemoryStore>, Counter);
fn engine(c: &Config, n: usize) -> Eng {
    let mut e = Engine::boot(MemoryStore::default(), key(n - 1), c.clone()).unwrap();
    let mut s = Counter::default();
    assert!(e.init(c, 0, 0, &mut s).unwrap());
    assert_eq!(e.local_index(), Some(n - 1));
    (e, s)
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Quorum,
    Conflict,
    JumpDense,
    JumpRows,
}
impl Kind {
    const ALL: [Kind; 4] = [
        Kind::Quorum,
        Kind::Conflict,
        Kind::JumpDense,
        Kind::JumpRows,
    ];
    fn name(self) -> &'static str {
        match self {
            Kind::Quorum => "quorum_polka",
            Kind::Conflict => "conflict_repair",
            Kind::JumpDense => "jump_dense",
            Kind::JumpRows => "jump_rows",
        }
    }
}
struct Fixture {
    kind: Kind,
    n: usize,
    c: Config,
    group: CertKind,
    votes: Vec<Vote>,
    rows: Vec<Option<Vote>>,
    pre: Vec<Vote>,
    jump_round: u64,
    jump_count: usize,
}
fn quorum_size(n: usize) -> usize {
    2 * n / 3 + 1
}
fn fixture(kind: Kind, n: usize) -> Fixture {
    assert!(n >= 4);
    let c = config(n);
    let votes: Vec<Vote>;
    let (mut pre, mut group) = (vec![], CertKind::Jump { round: 1 });
    let (mut jump_round, mut jump_count) = (0, 0);
    match kind {
        Kind::Quorum | Kind::Conflict => {
            let q = quorum_size(n);
            assert!(c.quorum(q as u64) && !c.quorum(q as u64 - 1) && q < n);
            let id = body_id(b"d28-perf-agreed");
            group = CertKind::Polka { round: 0, id };
            votes = (0..q).map(|i| vote(&c, i, 0, Some(id))).collect();
            if kind == Kind::Conflict {
                let bad = body_id(b"d28-perf-conflict");
                // At most one quarter Byzantine; never the local signer (n-1).
                pre = (0..n / 4).map(|i| vote(&c, i, 0, Some(bad))).collect();
                assert!(!pre.is_empty() && pre.len() * 4 <= n && pre.len() < q);
            }
        }
        Kind::JumpDense | Kind::JumpRows => {
            // Every non-local signer, mixed future rounds 1..=8.
            let rounds: Vec<u64> = (0..n - 1).map(|i| 1 + (i as u64 * 5) % 8).collect();
            votes = (0..n - 1).map(|i| vote(&c, i, rounds[i], None)).collect();
            jump_round = (1..=8u64)
                .rev()
                .find(|r| c.witness(rounds.iter().filter(|x| *x >= r).count() as u64))
                .unwrap();
            jump_count = rounds.iter().filter(|x| **x >= jump_round).count();
        }
    }
    let mut rows = vec![None; n];
    if kind == Kind::JumpRows {
        for v in &votes {
            rows[v.content.signer as usize] = Some(*v);
        }
    }
    Fixture {
        kind,
        n,
        c,
        group,
        votes,
        rows,
        pre,
        jump_round,
        jump_count,
    }
}
impl Fixture {
    /// Fresh engine with cached conflicting prestate; untimed.
    fn prepared(&self) -> Eng {
        let (mut e, mut s) = engine(&self.c, self.n);
        for v in &self.pre {
            e.receive(
                Frame {
                    message: Message::Ordinary,
                    outer: Some(v),
                    proposal: None,
                    groups: &[],
                },
                Status::Indeterminate,
                &mut s,
            )
            .unwrap();
        }
        for (i, v) in self.pre.iter().enumerate() {
            assert_eq!(e.prevotes()[i], Some(*v), "conflict not cached");
        }
        assert_eq!(
            e.prevotes().iter().flatten().count(),
            self.pre.len(),
            "unexpected prestate"
        );
        (e, s)
    }
    fn units(&self) -> usize {
        // outer vote (duplicate of the first group vote) plus group votes
        1 + self.votes.len()
    }
    fn receive(&self, e: &mut Eng) {
        let groups = [Group {
            kind: self.group,
            votes: if self.kind == Kind::JumpRows {
                Votes::Rows(&self.rows)
            } else {
                Votes::Dense(&self.votes)
            },
        }];
        e.0.receive(
            Frame {
                message: Message::Ordinary,
                outer: Some(&self.votes[0]),
                proposal: None,
                groups: &groups,
            },
            Status::Indeterminate,
            &mut e.1,
        )
        .unwrap();
    }
    /// Assert the intended path ran; return a deterministic checksum.
    fn check(&self, e: &Eng) -> u64 {
        let mut h = 0xcbf29ce484222325u64;
        let mut mix = |b: &[u8]| {
            for x in b {
                h = (h ^ *x as u64).wrapping_mul(0x100000001b3);
            }
        };
        assert_eq!(e.1.decided, 0);
        match self.kind {
            Kind::Quorum | Kind::Conflict => {
                let pv = e.0.prevotes();
                for (i, v) in self.votes.iter().enumerate() {
                    assert_eq!(pv[i], Some(*v), "row {i} not repaired");
                    mix(&v.signature);
                }
                // No fixture signer outside the quorum was cached.
                for slot in &pv[self.votes.len()..self.n - 1] {
                    assert!(slot.is_none());
                }
            }
            Kind::JumpDense | Kind::JumpRows => {
                let d = e.0.view().unwrap();
                assert_eq!(d.round, self.jump_round, "wrong jump round");
                let entry = d.entry.expect("jump entry");
                let rh = e.0.storage().region_header(entry.index).unwrap();
                assert_eq!(
                    rh.kind,
                    CertKind::Jump {
                        round: self.jump_round
                    }
                );
                assert_eq!(rh.count, self.jump_count);
                mix(&rh.digest);
                mix(&d.round.to_le_bytes());
            }
        }
        h
    }
}

struct Report {
    median_ns: u128,
}
fn run(kind: Kind, n: usize, repeats: usize, samples: usize, emit: bool) -> Report {
    let f = fixture(kind, n);
    let mut times = vec![];
    let mut sum = None;
    for _ in 0..samples {
        let mut engines: Vec<Eng> = (0..repeats).map(|_| f.prepared()).collect();
        let t = Instant::now();
        for e in engines.iter_mut() {
            f.receive(black_box(e));
        }
        let dt = t.elapsed().as_nanos();
        let mut total = 0xcbf29ce484222325u64;
        for e in &engines {
            total = total.rotate_left(7).wrapping_add(f.check(black_box(e)));
        }
        // Ordered accumulation does not cancel identical even-count engines.
        let expect = *sum.get_or_insert(total);
        assert_eq!(total, expect, "non-deterministic result");
        times.push(dt);
    }
    let mut sorted = times.clone();
    sorted.sort();
    let median_ns = sorted[sorted.len() / 2];
    if emit {
        let list: Vec<String> = times.iter().map(|t| t.to_string()).collect();
        println!(
            "D28_PERF workload={} n={} repeats={} samples={} median_ns={} per_receive_ns={} \
             min_ns={} max_ns={} samples_ns={} signed_votes={} groups=1 body_bytes=0 \
             cached_conflicts={} jump_round={} checksum={:016x}",
            kind.name(),
            n,
            repeats,
            samples,
            median_ns,
            median_ns / repeats as u128,
            sorted[0],
            sorted[sorted.len() - 1],
            list.join(","),
            f.units(),
            f.pre.len(),
            f.jump_round,
            sum.unwrap()
        );
    }
    Report { median_ns }
}
fn env_list(name: &str, default: &[usize]) -> Vec<usize> {
    match std::env::var(name) {
        Ok(s) => s.split(',').map(|x| x.trim().parse().unwrap()).collect(),
        Err(_) => default.to_vec(),
    }
}
fn env_one(name: &str) -> Option<usize> {
    std::env::var(name).ok().map(|s| s.trim().parse().unwrap())
}

/// Cheap deterministic fixture/path assertions in the ordinary suite.
#[test]
fn perf_fixtures_exercise_target_paths() {
    for n in [4, 7, 16] {
        for kind in Kind::ALL {
            run(kind, n, 1, 2, false);
        }
    }
}

#[test]
#[ignore]
fn d28_receive_perf() {
    let samples = env_one("D28_PERF_SAMPLES").unwrap_or(5);
    assert!(samples >= 1);
    for n in env_list("D28_PERF_N", &[4, 16, 64, 128]) {
        let repeats = env_one("D28_PERF_REPEATS").unwrap_or((256 / n).max(1));
        assert!(repeats >= 1);
        for kind in Kind::ALL {
            let r = run(kind, n, repeats, samples, true);
            black_box(r.median_ns);
        }
    }
}
