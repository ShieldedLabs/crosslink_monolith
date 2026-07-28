use super::*;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const WAL_MAGIC: [u8; 8] = *b"TLWAL002";
const ANCHOR_MAGIC: [u8; 8] = *b"TLANCH02";
const WAL_VERSION: u16 = 2;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_PROPOSAL_BYTES: usize = 8 * 1024 * 1024;
const MAX_SIGNABLE_PARTS: usize = 16 * 1024;
const MAX_SIGNABLE_BYTES: usize = 16 * 1024;
pub(crate) const MAX_CONSENSUS_ROUND: u32 = 0x7fff_ffff;
const CONSENSUS_RULES_VERSION: &[u8] =
    b"ctaz-tenderlink-consensus-v2:referenced-qc:n-minus-f:durable-intent:atomic-commit";
const PENDING_COMMIT_RECOVERY_REASON: &str =
    "commit recovery is incomplete; durable store reconciliation is required";
const LEGACY_PENDING_COMMIT_REASON: &str =
    "legacy pending commit lacks exact proposal valid-round/signature evidence; automatic recovery is disabled";

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WalFailpoint {
    AfterWalWrite,
    AfterWalSync,
    AfterAnchorWrite,
    AfterAnchorSync,
    AfterCommitApplied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignerEpochBinding {
    pub public_key: PubKeyID,
    pub chain_id: [u8; 32],
    pub height: u64,
    pub parent_commit: [u8; 32],
    pub vote_namespace: [u8; 32],
    pub consensus_config_hash: [u8; 32],
    pub roster_hash: [u8; 32],
    pub roster_index: u32,
    pub active_roster_len: u32,
}

#[derive(Clone, Debug)]
pub struct DurableSignerConfig {
    pub wal_path: PathBuf,
    pub anchor_path: PathBuf,
    /// This is an action gate, not a self-attestation. The caller may set it only after proving
    /// the anchor is outside the WAL/store rollback domain and globally fences this key.
    pub independent_anchor_authorized: bool,
    /// Hash of an operator-sealed, one-time bootstrap receipt for an already-live
    /// non-genesis key. The caller must verify the receipt and global key fence
    /// outside this rollback domain before supplying it. Genesis uses no receipt.
    pub non_genesis_bootstrap_receipt_hash: Option<[u8; 32]>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignerStatus {
    Active,
    ObserverOnly(String),
    /// A certified commit intent is durable, but applying it to the PoS store or
    /// sealing its successor did not complete. This state blocks every signing
    /// path while permitting only exact recovery/completion of `digest`.
    ReconciliationRequired([u8; 32], String),
    Poisoned(String),
}

#[derive(Debug)]
pub enum SignerError {
    ObserverOnly(String),
    ReconciliationRequired([u8; 32], String),
    Conflict(String),
    Integrity(String),
    Io(std::io::Error),
}

impl From<std::io::Error> for SignerError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl std::fmt::Display for SignerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ObserverOnly(reason) => write!(f, "observer only: {reason}"),
            Self::ReconciliationRequired(_, reason) => {
                write!(f, "commit reconciliation required: {reason}")
            }
            Self::Conflict(reason) => write!(f, "signing conflict: {reason}"),
            Self::Integrity(reason) => write!(f, "WAL integrity failure: {reason}"),
            Self::Io(error) => write!(f, "WAL I/O failure: {error}"),
        }
    }
}

impl std::error::Error for SignerError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum SlotKind {
    Proposal = 1,
    Prevote = 2,
    Precommit = 3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SignerSlot {
    round: u32,
    kind: SlotKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LockValidTransition {
    pub locked_round: i64,
    pub locked_value_id: ValueId,
    pub locked_value: Vec<u8>,
    pub valid_round: i64,
    pub valid_value_id: ValueId,
    pub valid_value: Vec<u8>,
    /// Canonical raw certificate evidence. Replay must reverify it before restoring state.
    pub certificate: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum SignedIntent {
    Proposal {
        round: u32,
        valid_round: i64,
        proposal_id: ValueId,
        proposal: Vec<u8>,
        signable_parts: Vec<Vec<u8>>,
    },
    Vote {
        round: u32,
        kind: SlotKind,
        value_id: ValueId,
        signable: Vec<u8>,
        transition: Option<LockValidTransition>,
    },
}

impl SignedIntent {
    fn slot(&self) -> SignerSlot {
        match self {
            Self::Proposal { round, .. } => SignerSlot {
                round: *round,
                kind: SlotKind::Proposal,
            },
            Self::Vote { round, kind, .. } => SignerSlot {
                round: *round,
                kind: *kind,
            },
        }
    }
}

#[derive(Debug)]
struct LoadedWal {
    epoch: Option<SignerEpochBinding>,
    authorized: bool,
    intents: BTreeMap<SignerSlot, SignedIntent>,
    transition: Option<LockValidTransition>,
    poisoned: Option<String>,
    pending_commit: Option<PendingCommit>,
    commit_applied_epoch: Option<SignerEpochBinding>,
    bootstrap_origin: Option<[u8; 32]>,
    next_sequence: u64,
    last_hash: [u8; 32],
    clean_tail: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PendingCommit {
    digest: [u8; 32],
    decided_value_id: ValueId,
    proposal: Vec<u8>,
    certificate: Vec<u8>,
    proposal_evidence: PendingProposalEvidence,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PendingProposalEvidence {
    /// RECORD_COMMIT_INTENT (v1) deliberately remains readable so completed
    /// historical WALs replay, but an unfinished legacy intent cannot safely
    /// reconstruct authenticated proposal gossip.
    LegacyUnavailable,
    Exact {
        round: u32,
        valid_round: i64,
        proposal_sigs: Vec<TMSig>,
    },
}

#[derive(Clone, Debug)]
pub(super) struct PendingCommitRecovery {
    pub digest: [u8; 32],
    pub proposal: BlockValue,
    pub proposal_valid_round: i64,
    pub proposal_sigs: Vec<TMSig>,
    /// Exact certified round reconstructed solely from the durable WAL, epoch,
    /// and roster. This includes proposal bytes, valid-round, ordered proposal
    /// chunk signatures, precommit evidence, counts, and vote namespace.
    pub round_data: RoundData,
    pub fat_pointer: FatPointerToBftBlock,
}

fn validate_commit_successor(
    current: &SignerEpochBinding,
    pending: &PendingCommit,
    next: &SignerEpochBinding,
) -> Result<(), SignerError> {
    let expected_height = current
        .height
        .checked_add(1)
        .ok_or_else(|| SignerError::Integrity("signer height overflow".into()))?;
    if next.public_key != current.public_key
        || next.chain_id != current.chain_id
        || next.height != expected_height
        || next.parent_commit != pending.decided_value_id.0
        || next.consensus_config_hash != current.consensus_config_hash
    {
        return Err(SignerError::Integrity(
            "commit successor does not match the certified current epoch".into(),
        ));
    }
    if next.active_roster_len == 0
        || (next.roster_index != u32::MAX && next.roster_index >= next.active_roster_len)
    {
        return Err(SignerError::Integrity(
            "commit successor roster binding is invalid".into(),
        ));
    }
    Ok(())
}

impl Default for LoadedWal {
    fn default() -> Self {
        Self {
            epoch: None,
            authorized: false,
            intents: BTreeMap::new(),
            transition: None,
            poisoned: None,
            pending_commit: None,
            commit_applied_epoch: None,
            bootstrap_origin: None,
            next_sequence: 0,
            last_hash: [0u8; 32],
            clean_tail: true,
        }
    }
}

fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn put_bytes(out: &mut Vec<u8>, value: &[u8]) -> Result<(), SignerError> {
    let len: u32 = value
        .len()
        .try_into()
        .map_err(|_| SignerError::Integrity("field too large".into()))?;
    put_u32(out, len);
    out.extend_from_slice(value);
    Ok(())
}

#[cfg(test)]
pub(super) fn encode_legacy_commit_intent(
    decided_value_id: ValueId,
    proposal: &[u8],
    certificate: &[u8],
) -> Result<Vec<u8>, SignerError> {
    if decided_value_id == ValueId::NIL {
        return Err(SignerError::Integrity(
            "commit intent cannot decide NIL".into(),
        ));
    }
    if proposal.is_empty() || proposal.len() > MAX_PROPOSAL_BYTES {
        return Err(SignerError::Integrity(
            "commit recovery proposal exceeds its bound".into(),
        ));
    }
    if certificate.len() > MAX_RECORD_BYTES {
        return Err(SignerError::Integrity(
            "commit certificate exceeds bound".into(),
        ));
    }
    let mut payload = Vec::new();
    payload.extend_from_slice(&decided_value_id.0);
    put_bytes(&mut payload, proposal)?;
    put_bytes(&mut payload, certificate)?;
    if payload.len() > MAX_RECORD_BYTES {
        return Err(SignerError::Integrity(
            "commit intent record exceeds bound".into(),
        ));
    }
    Ok(payload)
}

fn encode_commit_intent(
    round: u32,
    decided_value_id: ValueId,
    proposal: &[u8],
    proposal_valid_round: i64,
    proposal_sigs: &[TMSig],
    certificate: &[u8],
) -> Result<Vec<u8>, SignerError> {
    if round > MAX_CONSENSUS_ROUND {
        return Err(SignerError::Integrity(
            "commit round exceeds the canonical 31-bit domain".into(),
        ));
    }
    if proposal_valid_round < -1 || proposal_valid_round >= i64::from(round) {
        return Err(SignerError::Integrity(
            "commit proposal valid-round is outside the canonical range".into(),
        ));
    }
    if decided_value_id == ValueId::NIL {
        return Err(SignerError::Integrity(
            "commit intent cannot decide NIL".into(),
        ));
    }
    if proposal.is_empty() || proposal.len() > MAX_PROPOSAL_BYTES {
        return Err(SignerError::Integrity(
            "commit recovery proposal exceeds its bound".into(),
        ));
    }
    if proposal_sigs.is_empty() || proposal_sigs.len() > MAX_SIGNABLE_PARTS {
        return Err(SignerError::Integrity(
            "commit proposal-signature manifest exceeds its bound".into(),
        ));
    }
    if proposal_sigs.iter().any(|signature| *signature == TMSig::NIL) {
        return Err(SignerError::Integrity(
            "commit proposal-signature manifest is incomplete".into(),
        ));
    }
    if certificate.len() > MAX_RECORD_BYTES {
        return Err(SignerError::Integrity(
            "commit certificate exceeds bound".into(),
        ));
    }

    let mut payload = Vec::new();
    put_u32(&mut payload, round);
    put_i64(&mut payload, proposal_valid_round);
    payload.extend_from_slice(&decided_value_id.0);
    put_bytes(&mut payload, proposal)?;
    put_u32(
        &mut payload,
        proposal_sigs
            .len()
            .try_into()
            .map_err(|_| SignerError::Integrity("too many proposal signatures".into()))?,
    );
    for signature in proposal_sigs {
        payload.extend_from_slice(&signature.0);
    }
    put_bytes(&mut payload, certificate)?;
    if payload.len() > MAX_RECORD_BYTES {
        return Err(SignerError::Integrity(
            "commit intent record exceeds bound".into(),
        ));
    }
    Ok(payload)
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    fn take(&mut self, len: usize) -> Result<&'a [u8], SignerError> {
        let end = self
            .offset
            .checked_add(len)
            .ok_or_else(|| SignerError::Integrity("length overflow".into()))?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| SignerError::Integrity("truncated record".into()))?;
        self.offset = end;
        Ok(value)
    }
    fn u8(&mut self) -> Result<u8, SignerError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, SignerError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, SignerError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, SignerError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn i64(&mut self) -> Result<i64, SignerError> {
        Ok(i64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn array32(&mut self) -> Result<[u8; 32], SignerError> {
        Ok(self.take(32)?.try_into().unwrap())
    }
    fn bytes(&mut self, max: usize) -> Result<Vec<u8>, SignerError> {
        let len: usize = self.u32()?.try_into().unwrap();
        if len > max {
            return Err(SignerError::Integrity("bounded field exceeds limit".into()));
        }
        Ok(self.take(len)?.to_vec())
    }
    fn finish(self) -> Result<(), SignerError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(SignerError::Integrity("trailing record bytes".into()))
        }
    }
}

pub fn canonical_roster_hash(roster: &[SortedRosterMember]) -> Result<[u8; 32], SignerError> {
    let active_len = active_roster_len(roster);
    if active_len == 0 {
        return Err(SignerError::Integrity("consensus roster is empty".into()));
    }
    let mut cumulative_stake = 0u64;
    let mut identities = BTreeSet::new();
    for (index, member) in roster.iter().enumerate() {
        if !identities.insert(member.pub_key) {
            return Err(SignerError::Integrity(
                "consensus roster contains a duplicate key".into(),
            ));
        }
        cumulative_stake = cumulative_stake
            .checked_add(member.stake)
            .ok_or_else(|| SignerError::Integrity("consensus roster stake overflow".into()))?;
        if member.cumulative_stake != cumulative_stake {
            return Err(SignerError::Integrity(
                "consensus roster cumulative stake is noncanonical".into(),
            ));
        }
        if index > 0 {
            let previous = &roster[index - 1];
            if (previous.stake, previous.pub_key) < (member.stake, member.pub_key) {
                return Err(SignerError::Integrity(
                    "consensus roster ordering is noncanonical".into(),
                ));
            }
        }
    }
    let mut encoded = Vec::with_capacity(32 + roster.len() * 48);
    encoded.extend_from_slice(b"tenderlink-roster-v1");
    put_u32(
        &mut encoded,
        roster
            .len()
            .try_into()
            .map_err(|_| SignerError::Integrity("roster too large".into()))?,
    );
    put_u32(
        &mut encoded,
        active_len
            .try_into()
            .map_err(|_| SignerError::Integrity("roster too large".into()))?,
    );
    for member in roster {
        encoded.extend_from_slice(&member.pub_key.0);
        put_u64(&mut encoded, member.stake);
        put_u64(&mut encoded, member.cumulative_stake);
    }
    Ok(blake3::hash(&encoded).into())
}

fn validate_epoch_consensus_roster(
    epoch: &SignerEpochBinding,
    roster: &[SortedRosterMember],
) -> Result<(), SignerError> {
    let active_len = active_roster_len(roster);
    if active_len != epoch.active_roster_len as usize {
        return Err(SignerError::Integrity(
            "epoch active-roster length mismatch".into(),
        ));
    }
    if canonical_roster_hash(roster)? != epoch.roster_hash {
        return Err(SignerError::Integrity(
            "epoch roster fingerprint mismatch".into(),
        ));
    }
    Ok(())
}

fn validate_epoch_roster(
    epoch: &SignerEpochBinding,
    roster: &[SortedRosterMember],
) -> Result<(), SignerError> {
    validate_epoch_consensus_roster(epoch, roster)?;
    let active_len = active_roster_len(roster);
    let roster_index: usize = epoch.roster_index.try_into().map_err(|_| {
        SignerError::Integrity("epoch roster index does not fit this platform".into())
    })?;
    if roster_index >= active_len || roster[roster_index].pub_key != epoch.public_key {
        return Err(SignerError::Integrity(
            "epoch signer index does not resolve to its public key".into(),
        ));
    }
    Ok(())
}

pub fn consensus_hash_keys_fingerprint(hash_keys: &HashKeys) -> [u8; 32] {
    let mut encoded = Vec::with_capacity(128 + 28);
    encoded.extend_from_slice(CONSENSUS_RULES_VERSION);
    encoded.extend_from_slice(&WAL_VERSION.to_le_bytes());
    encoded.extend_from_slice(&(ROSTER_MAX_N as u64).to_le_bytes());
    encoded.extend_from_slice(&(PROPOSAL_CHUNK_DATA_SIZE as u64).to_le_bytes());
    encoded.extend_from_slice(&hash_keys.proposer.0);
    encoded.extend_from_slice(&hash_keys.value_id.0);
    encoded.extend_from_slice(&hash_keys.connect_contention.0);
    encoded.extend_from_slice(&hash_keys.proposal_sig.0);
    blake3::hash(&encoded).into()
}

pub fn canonical_prevote_certificate(
    round_data: &RoundData,
    roster: &[SortedRosterMember],
) -> Result<Vec<u8>, SignerError> {
    if round_data.round > MAX_CONSENSUS_ROUND {
        return Err(SignerError::Integrity(
            "prevote certificate round exceeds the canonical 31-bit domain".into(),
        ));
    }
    let active_len = active_roster_len(roster);
    if round_data.msg_val_sigs.len() < active_len || round_data.roster.len() < active_len {
        return Err(SignerError::Integrity(
            "round evidence is shorter than the active roster".into(),
        ));
    }
    let mut out = Vec::with_capacity(128 + active_len * 144);
    out.extend_from_slice(b"tenderlink-prevote-qc-v1");
    put_u64(&mut out, round_data.height);
    put_u32(&mut out, round_data.round);
    out.extend_from_slice(&round_data.proposal_id.0);
    out.extend_from_slice(&round_data.vote_namespace);
    put_u32(
        &mut out,
        active_len
            .try_into()
            .map_err(|_| SignerError::Integrity("active roster too large".into()))?,
    );
    for (index, member) in roster[..active_len].iter().enumerate() {
        if round_data.roster[index].pub_key != member.pub_key
            || round_data.roster[index].stake != member.stake
            || round_data.roster[index].cumulative_stake != member.cumulative_stake
        {
            return Err(SignerError::Integrity(
                "round roster differs from epoch roster".into(),
            ));
        }
        out.extend_from_slice(&member.pub_key.0);
        put_u64(&mut out, member.stake);
        put_u64(&mut out, member.cumulative_stake);
        out.extend_from_slice(&round_data.msg_val_sigs[index][0].0 .0);
        out.extend_from_slice(&round_data.msg_val_sigs[index][0].1 .0);
    }
    Ok(out)
}

pub fn canonical_precommit_certificate(
    round_data: &RoundData,
    roster: &[SortedRosterMember],
) -> Result<Vec<u8>, SignerError> {
    if round_data.round > MAX_CONSENSUS_ROUND {
        return Err(SignerError::Integrity(
            "precommit certificate round exceeds the canonical 31-bit domain".into(),
        ));
    }
    let active_len = active_roster_len(roster);
    if round_data.msg_val_sigs.len() < active_len || round_data.roster.len() < active_len {
        return Err(SignerError::Integrity(
            "round evidence is shorter than the active roster".into(),
        ));
    }
    let mut out = Vec::with_capacity(128 + active_len * 144);
    out.extend_from_slice(b"tenderlink-precommit-qc-v1");
    put_u64(&mut out, round_data.height);
    put_u32(&mut out, round_data.round);
    out.extend_from_slice(&round_data.proposal_id.0);
    out.extend_from_slice(&round_data.vote_namespace);
    put_u32(
        &mut out,
        active_len
            .try_into()
            .map_err(|_| SignerError::Integrity("active roster too large".into()))?,
    );
    for (index, member) in roster[..active_len].iter().enumerate() {
        if round_data.roster[index].pub_key != member.pub_key
            || round_data.roster[index].stake != member.stake
            || round_data.roster[index].cumulative_stake != member.cumulative_stake
        {
            return Err(SignerError::Integrity(
                "round roster differs from epoch roster".into(),
            ));
        }
        out.extend_from_slice(&member.pub_key.0);
        put_u64(&mut out, member.stake);
        put_u64(&mut out, member.cumulative_stake);
        out.extend_from_slice(&round_data.msg_val_sigs[index][1].0 .0);
        out.extend_from_slice(&round_data.msg_val_sigs[index][1].1 .0);
    }
    Ok(out)
}

pub fn verify_precommit_certificate(
    certificate: &[u8],
    expected_round: u32,
    decided_value_id: ValueId,
    epoch: &SignerEpochBinding,
    roster: &[SortedRosterMember],
) -> Result<(), SignerError> {
    // Decision verification is roster/quorum work and must also succeed for an intentional
    // off-roster observer. Local signer membership is enforced separately on signing paths.
    validate_epoch_consensus_roster(epoch, roster)?;
    let mut decoder = Decoder::new(certificate);
    if decoder.take(b"tenderlink-precommit-qc-v1".len())? != b"tenderlink-precommit-qc-v1" {
        return Err(SignerError::Integrity(
            "precommit certificate domain mismatch".into(),
        ));
    }
    if decoder.u64()? != epoch.height {
        return Err(SignerError::Integrity(
            "precommit certificate height mismatch".into(),
        ));
    }
    let round = decoder.u32()?;
    if round != expected_round || round > MAX_CONSENSUS_ROUND {
        return Err(SignerError::Integrity(
            "precommit certificate round mismatch".into(),
        ));
    }
    let proposal_id = ValueId(decoder.array32()?);
    if proposal_id != decided_value_id || proposal_id == ValueId::NIL {
        return Err(SignerError::Integrity(
            "precommit certificate proposal ID mismatch".into(),
        ));
    }
    if decoder.array32()? != epoch.vote_namespace {
        return Err(SignerError::Integrity(
            "precommit certificate namespace mismatch".into(),
        ));
    }
    let active_len: usize = decoder.u32()?.try_into().unwrap();
    if active_len != epoch.active_roster_len as usize || active_len != active_roster_len(roster) {
        return Err(SignerError::Integrity(
            "precommit certificate active-roster length mismatch".into(),
        ));
    }

    let mut total_power = 0u64;
    let mut yes_power = 0u64;
    for member in &roster[..active_len] {
        if PubKeyID(decoder.array32()?) != member.pub_key
            || decoder.u64()? != member.stake
            || decoder.u64()? != member.cumulative_stake
        {
            return Err(SignerError::Integrity(
                "precommit certificate roster mismatch".into(),
            ));
        }
        total_power = total_power
            .checked_add(member.stake)
            .ok_or_else(|| SignerError::Integrity("precommit total-power overflow".into()))?;
        if total_power != member.cumulative_stake {
            return Err(SignerError::Integrity(
                "precommit cumulative stake mismatch".into(),
            ));
        }
        let value_id = ValueId(decoder.array32()?);
        let sig = TMSig(decoder.take(64)?.try_into().unwrap());
        if value_id == ValueId::NIL {
            if sig != TMSig::NIL {
                let signed =
                    make_vote_sign_datas(member.pub_key, true, epoch.height, round, value_id)[0];
                sig.verify_with_namespace(member.pub_key, &signed, &epoch.vote_namespace)
                    .map_err(|_| {
                        SignerError::Integrity(
                            "invalid NIL precommit signature in certificate".into(),
                        )
                    })?;
            }
            continue;
        }
        if sig == TMSig::NIL {
            return Err(SignerError::Integrity(
                "precommit certificate contains an unsigned non-NIL vote".into(),
            ));
        }
        let signed = make_vote_sign_datas(member.pub_key, true, epoch.height, round, value_id)[1];
        sig.verify_with_namespace(member.pub_key, &signed, &epoch.vote_namespace)
            .map_err(|_| {
                SignerError::Integrity("invalid YES precommit signature in certificate".into())
            })?;
        if value_id == proposal_id {
            yes_power = yes_power
                .checked_add(member.stake)
                .ok_or_else(|| SignerError::Integrity("precommit YES-power overflow".into()))?;
        }
    }
    decoder.finish()?;
    if total_power == 0 {
        return Err(SignerError::Integrity(
            "zero-power precommit certificate".into(),
        ));
    }
    let quorum = quorum_threshold(total_power);
    if yes_power < quorum {
        return Err(SignerError::Integrity(
            "precommit certificate is below quorum".into(),
        ));
    }
    Ok(())
}

fn recover_fat_pointer_from_commit_certificate(
    pending: &PendingCommit,
    epoch: &SignerEpochBinding,
    roster: &[SortedRosterMember],
) -> Result<
    (
        u32,
        FatPointerToBftBlock,
        Vec<(ValueId, TMSig)>,
    ),
    SignerError,
> {
    let mut header = Decoder::new(&pending.certificate);
    if header.take(b"tenderlink-precommit-qc-v1".len())?
        != b"tenderlink-precommit-qc-v1"
    {
        return Err(SignerError::Integrity(
            "precommit recovery certificate domain mismatch".into(),
        ));
    }
    if header.u64()? != epoch.height {
        return Err(SignerError::Integrity(
            "precommit recovery certificate height mismatch".into(),
        ));
    }
    let round = header.u32()?;
    verify_precommit_certificate(
        &pending.certificate,
        round,
        pending.decided_value_id,
        epoch,
        roster,
    )?;

    let proposal_id = ValueId(header.array32()?);
    if proposal_id != pending.decided_value_id || header.array32()? != epoch.vote_namespace {
        return Err(SignerError::Integrity(
            "precommit recovery certificate value or namespace mismatch".into(),
        ));
    }
    let active_len: usize = header.u32()?.try_into().unwrap();
    if active_len != active_roster_len(roster) {
        return Err(SignerError::Integrity(
            "precommit recovery active-roster length mismatch".into(),
        ));
    }
    let mut signatures = Vec::new();
    let mut precommits = Vec::with_capacity(active_len);
    for member in &roster[..active_len] {
        if PubKeyID(header.array32()?) != member.pub_key
            || header.u64()? != member.stake
            || header.u64()? != member.cumulative_stake
        {
            return Err(SignerError::Integrity(
                "precommit recovery roster mismatch".into(),
            ));
        }
        let value_id = ValueId(header.array32()?);
        let signature = TMSig(header.take(64)?.try_into().unwrap());
        precommits.push((value_id, signature));
        if value_id == pending.decided_value_id && signature != TMSig::NIL {
            signatures.push(FatPointerSignature {
                pub_key: member.pub_key,
                vote_signature: signature.0,
            });
        }
    }
    header.finish()?;

    let mut vote_for_block_without_finalizer_public_key = [0u8; 76 - 32];
    pending
        .decided_value_id
        .0
        .write_to(&mut vote_for_block_without_finalizer_public_key[0..32]);
    epoch
        .height
        .write_to(&mut vote_for_block_without_finalizer_public_key[32..]);
    canonical_vote_round(round, true)
        .ok_or_else(|| SignerError::Integrity("commit round is outside the canonical domain".into()))?
        .write_to(&mut vote_for_block_without_finalizer_public_key[40..]);
    Ok((
        round,
        FatPointerToBftBlock {
            vote_for_block_without_finalizer_public_key,
            signatures,
        },
        precommits,
    ))
}

pub(super) fn verify_proposal_signature_manifest(
    hash_keys: &HashKeys,
    epoch: &SignerEpochBinding,
    roster: &[SortedRosterMember],
    round: u32,
    proposal_valid_round: i64,
    proposal: &BlockValue,
    proposal_id: ValueId,
    proposal_sigs: &[TMSig],
) -> Result<(), SignerError> {
    validate_epoch_consensus_roster(epoch, roster)?;
    if round > MAX_CONSENSUS_ROUND {
        return Err(SignerError::Integrity(
            "proposal manifest round exceeds the canonical domain".into(),
        ));
    }
    if proposal_valid_round < -1 || proposal_valid_round >= i64::from(round) {
        return Err(SignerError::Integrity(
            "proposal manifest valid-round is outside the canonical range".into(),
        ));
    }
    if proposal.0.is_empty() || proposal.0.len() > MAX_PROPOSAL_BYTES {
        return Err(SignerError::Integrity(
            "proposal manifest value exceeds its bound".into(),
        ));
    }
    if proposal.id_from_value(hash_keys) != proposal_id || proposal_id == ValueId::NIL {
        return Err(SignerError::Integrity(
            "proposal manifest bytes do not match the decided value ID".into(),
        ));
    }
    let chunks_n = proposal.chunks_n();
    if chunks_n == 0
        || chunks_n > MAX_SIGNABLE_PARTS
        || proposal_sigs.len() != chunks_n
        || proposal_sigs.iter().any(|signature| *signature == TMSig::NIL)
    {
        return Err(SignerError::Integrity(
            "proposal signature manifest is incomplete or noncanonical".into(),
        ));
    }
    let (proposer_i, proposer_pub_key) =
        TMState::proposer_from_height_round(hash_keys, roster, epoch.height, round);
    if proposer_i.is_none() || proposer_pub_key == PubKeyID::NIL {
        return Err(SignerError::Integrity(
            "proposal manifest has no canonical proposer".into(),
        ));
    }

    let mut header = PacketProposalChunkHeader {
        height: epoch.height,
        round,
        chunk_i: 0,
        proposal_size: proposal
            .0
            .len()
            .try_into()
            .map_err(|_| SignerError::Integrity("proposal size does not fit u32".into()))?,
        proposal_id,
        valid_round: proposal_valid_round,
    };
    for (chunk_i, signature) in proposal_sigs.iter().enumerate() {
        header.chunk_i = chunk_i
            .try_into()
            .map_err(|_| SignerError::Integrity("proposal chunk index does not fit u32".into()))?;
        let (chunk_offset, chunk_size) = proposal.chunk_o_size(chunk_i);
        let mut signable = vec![0u8; PacketProposalChunkHeader::SERIALIZED_SIZE + chunk_size];
        let header_len = header.write_to(&mut signable);
        signable[header_len..]
            .copy_from_slice(&proposal.0[chunk_offset..chunk_offset + chunk_size]);
        signature
            .verify_with_namespace(proposer_pub_key, &signable, &epoch.vote_namespace)
            .map_err(|_| {
                SignerError::Integrity(
                    "proposal signature manifest contains an invalid chunk signature".into(),
                )
            })?;
    }
    Ok(())
}

pub fn verify_transition_certificate(
    transition: &LockValidTransition,
    epoch: &SignerEpochBinding,
    hash_keys: &HashKeys,
    roster: &[SortedRosterMember],
) -> Result<(), SignerError> {
    validate_epoch_consensus_roster(epoch, roster)?;
    validate_transition_shape(transition)?;
    let valid_value = BlockValue(transition.valid_value.clone());
    if valid_value.id_from_value(hash_keys) != transition.valid_value_id {
        return Err(SignerError::Integrity(
            "valid value bytes do not match the persisted ID".into(),
        ));
    }
    if transition.locked_round >= 0 {
        let locked_value = BlockValue(transition.locked_value.clone());
        if locked_value.id_from_value(hash_keys) != transition.locked_value_id {
            return Err(SignerError::Integrity(
                "locked value bytes do not match the persisted ID".into(),
            ));
        }
    }

    let mut decoder = Decoder::new(&transition.certificate);
    if decoder.take(b"tenderlink-prevote-qc-v1".len())? != b"tenderlink-prevote-qc-v1" {
        return Err(SignerError::Integrity("certificate domain mismatch".into()));
    }
    if decoder.u64()? != epoch.height {
        return Err(SignerError::Integrity("certificate height mismatch".into()));
    }
    let round = decoder.u32()?;
    if round > MAX_CONSENSUS_ROUND || i64::from(round) != transition.valid_round {
        return Err(SignerError::Integrity(
            "certificate round does not establish valid state".into(),
        ));
    }
    let proposal_id = ValueId(decoder.array32()?);
    if proposal_id != transition.valid_value_id {
        return Err(SignerError::Integrity(
            "certificate proposal ID mismatch".into(),
        ));
    }
    if decoder.array32()? != epoch.vote_namespace {
        return Err(SignerError::Integrity(
            "certificate namespace mismatch".into(),
        ));
    }
    let active_len: usize = decoder.u32()?.try_into().unwrap();
    if active_len != epoch.active_roster_len as usize || active_len != active_roster_len(roster) {
        return Err(SignerError::Integrity(
            "certificate active-roster length mismatch".into(),
        ));
    }

    let mut total_power = 0u64;
    let mut yes_power = 0u64;
    for member in &roster[..active_len] {
        if PubKeyID(decoder.array32()?) != member.pub_key
            || decoder.u64()? != member.stake
            || decoder.u64()? != member.cumulative_stake
        {
            return Err(SignerError::Integrity("certificate roster mismatch".into()));
        }
        total_power = total_power
            .checked_add(member.stake)
            .ok_or_else(|| SignerError::Integrity("certificate total-power overflow".into()))?;
        if total_power != member.cumulative_stake {
            return Err(SignerError::Integrity(
                "certificate cumulative stake mismatch".into(),
            ));
        }
        let value_id = ValueId(decoder.array32()?);
        let sig = TMSig(decoder.take(64)?.try_into().unwrap());
        if value_id == ValueId::NIL {
            if sig != TMSig::NIL {
                let signed =
                    make_vote_sign_datas(member.pub_key, false, epoch.height, round, value_id)[0];
                sig.verify_with_namespace(member.pub_key, &signed, &epoch.vote_namespace)
                    .map_err(|_| {
                        SignerError::Integrity(
                            "invalid NIL prevote signature in certificate".into(),
                        )
                    })?;
            }
            continue;
        }
        if sig == TMSig::NIL {
            return Err(SignerError::Integrity(
                "certificate contains an unsigned non-NIL prevote".into(),
            ));
        }
        let signed = make_vote_sign_datas(member.pub_key, false, epoch.height, round, value_id)[1];
        sig.verify_with_namespace(member.pub_key, &signed, &epoch.vote_namespace)
            .map_err(|_| {
                SignerError::Integrity("invalid YES prevote signature in certificate".into())
            })?;
        if value_id == proposal_id {
            yes_power = yes_power
                .checked_add(member.stake)
                .ok_or_else(|| SignerError::Integrity("certificate YES-power overflow".into()))?;
        }
    }
    decoder.finish()?;
    if total_power == 0 {
        return Err(SignerError::Integrity("zero-power certificate".into()));
    }
    let quorum = quorum_threshold(total_power);
    if yes_power < quorum {
        return Err(SignerError::Integrity("certificate is below quorum".into()));
    }
    if transition.locked_round == transition.valid_round
        && (transition.locked_value_id != transition.valid_value_id
            || transition.locked_value != transition.valid_value)
    {
        return Err(SignerError::Integrity(
            "same-round lock and valid values differ".into(),
        ));
    }
    Ok(())
}

const RECORD_EPOCH: u8 = 1;
const RECORD_INTENT: u8 = 2;
const RECORD_STATE: u8 = 3;
const RECORD_POISON: u8 = 4;
const RECORD_COMMIT_INTENT: u8 = 5;
const RECORD_COMMIT_APPLIED: u8 = 6;
const RECORD_BOOTSTRAP_ORIGIN: u8 = 7;
/// Versioned commit intent that carries the exact proposer valid-round and the
/// ordered, complete proposal-chunk signature manifest. Tag 5 remains readable
/// only for compatibility with already-completed historical WAL epochs.
const RECORD_COMMIT_INTENT_V2: u8 = 8;

fn encode_epoch(epoch: &SignerEpochBinding, authorized: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 * 6 + 24);
    out.extend_from_slice(&epoch.public_key.0);
    out.extend_from_slice(&epoch.chain_id);
    put_u64(&mut out, epoch.height);
    out.extend_from_slice(&epoch.parent_commit);
    out.extend_from_slice(&epoch.vote_namespace);
    out.extend_from_slice(&epoch.consensus_config_hash);
    out.extend_from_slice(&epoch.roster_hash);
    put_u32(&mut out, epoch.roster_index);
    put_u32(&mut out, epoch.active_roster_len);
    out.push(authorized as u8);
    out
}

fn decode_epoch(payload: &[u8]) -> Result<(SignerEpochBinding, bool), SignerError> {
    let mut decoder = Decoder::new(payload);
    let epoch = SignerEpochBinding {
        public_key: PubKeyID(decoder.array32()?),
        chain_id: decoder.array32()?,
        height: decoder.u64()?,
        parent_commit: decoder.array32()?,
        vote_namespace: decoder.array32()?,
        consensus_config_hash: decoder.array32()?,
        roster_hash: decoder.array32()?,
        roster_index: decoder.u32()?,
        active_roster_len: decoder.u32()?,
    };
    let authorized = match decoder.u8()? {
        0 => false,
        1 => true,
        _ => {
            return Err(SignerError::Integrity(
                "invalid epoch authorization flag".into(),
            ))
        }
    };
    decoder.finish()?;
    Ok((epoch, authorized))
}

fn encode_transition(
    out: &mut Vec<u8>,
    transition: &LockValidTransition,
) -> Result<(), SignerError> {
    put_i64(out, transition.locked_round);
    out.extend_from_slice(&transition.locked_value_id.0);
    put_bytes(out, &transition.locked_value)?;
    put_i64(out, transition.valid_round);
    out.extend_from_slice(&transition.valid_value_id.0);
    put_bytes(out, &transition.valid_value)?;
    put_bytes(out, &transition.certificate)
}

fn decode_transition(decoder: &mut Decoder<'_>) -> Result<LockValidTransition, SignerError> {
    Ok(LockValidTransition {
        locked_round: decoder.i64()?,
        locked_value_id: ValueId(decoder.array32()?),
        locked_value: decoder.bytes(MAX_PROPOSAL_BYTES)?,
        valid_round: decoder.i64()?,
        valid_value_id: ValueId(decoder.array32()?),
        valid_value: decoder.bytes(MAX_PROPOSAL_BYTES)?,
        certificate: decoder.bytes(MAX_RECORD_BYTES)?,
    })
}

fn validate_transition_shape(next: &LockValidTransition) -> Result<(), SignerError> {
    for (label, round, value_id, value) in [
        (
            "locked",
            next.locked_round,
            next.locked_value_id,
            &next.locked_value,
        ),
        (
            "valid",
            next.valid_round,
            next.valid_value_id,
            &next.valid_value,
        ),
    ] {
        if round < -1 {
            return Err(SignerError::Conflict(format!("invalid {label} round")));
        }
        if round > i64::from(MAX_CONSENSUS_ROUND) {
            return Err(SignerError::Conflict(format!(
                "{label} round exceeds the canonical 31-bit domain"
            )));
        }
        if round == -1 && (value_id != ValueId::NIL || !value.is_empty()) {
            return Err(SignerError::Conflict(format!(
                "{label} value without a round"
            )));
        }
        if round >= 0 && (value_id == ValueId::NIL || value.is_empty()) {
            return Err(SignerError::Conflict(format!(
                "{label} round without a value"
            )));
        }
    }
    if next.certificate.len() > MAX_RECORD_BYTES {
        return Err(SignerError::Conflict("certificate exceeds bound".into()));
    }
    if next.locked_round > next.valid_round {
        return Err(SignerError::Conflict(
            "locked round is newer than valid round".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_transition(
    previous: Option<&LockValidTransition>,
    next: &LockValidTransition,
) -> Result<(), SignerError> {
    validate_transition_shape(next)?;
    if let Some(previous) = previous {
        for (label, old_round, old_value, old_bytes, new_round, new_value, new_bytes) in [
            (
                "locked",
                previous.locked_round,
                previous.locked_value_id,
                &previous.locked_value,
                next.locked_round,
                next.locked_value_id,
                &next.locked_value,
            ),
            (
                "valid",
                previous.valid_round,
                previous.valid_value_id,
                &previous.valid_value,
                next.valid_round,
                next.valid_value_id,
                &next.valid_value,
            ),
        ] {
            if new_round < old_round {
                return Err(SignerError::Conflict(format!(
                    "non-monotonic {label} round"
                )));
            }
            if new_round == old_round
                && new_round >= 0
                && (new_value != old_value || new_bytes != old_bytes)
            {
                return Err(SignerError::Conflict(format!(
                    "same-round {label} value changed"
                )));
            }
        }
        if next.locked_round > previous.locked_round
            && (next.locked_round != next.valid_round
                || next.locked_value_id != next.valid_value_id
                || next.locked_value != next.valid_value)
        {
            return Err(SignerError::Conflict(
                "a new lock is not established by the current valid certificate".into(),
            ));
        }
    } else if next.locked_round >= 0
        && (next.locked_round != next.valid_round
            || next.locked_value_id != next.valid_value_id
            || next.locked_value != next.valid_value)
    {
        return Err(SignerError::Conflict(
            "initial lock lacks its own exact quorum certificate".into(),
        ));
    }
    Ok(())
}

fn encode_intent(intent: &SignedIntent) -> Result<Vec<u8>, SignerError> {
    let mut out = Vec::new();
    match intent {
        SignedIntent::Proposal {
            round,
            valid_round,
            proposal_id,
            proposal,
            signable_parts,
        } => {
            out.push(SlotKind::Proposal as u8);
            put_u32(&mut out, *round);
            put_i64(&mut out, *valid_round);
            out.extend_from_slice(&proposal_id.0);
            put_bytes(&mut out, proposal)?;
            let count: u32 = signable_parts
                .len()
                .try_into()
                .map_err(|_| SignerError::Integrity("too many proposal parts".into()))?;
            put_u32(&mut out, count);
            for part in signable_parts {
                put_bytes(&mut out, part)?;
            }
        }
        SignedIntent::Vote {
            round,
            kind,
            value_id,
            signable,
            transition,
        } => {
            out.push(*kind as u8);
            put_u32(&mut out, *round);
            out.extend_from_slice(&value_id.0);
            put_bytes(&mut out, signable)?;
            out.push(transition.is_some() as u8);
            if let Some(transition) = transition {
                encode_transition(&mut out, transition)?;
            }
        }
    }
    Ok(out)
}

fn decode_intent(payload: &[u8]) -> Result<SignedIntent, SignerError> {
    let mut decoder = Decoder::new(payload);
    let kind = decoder.u8()?;
    let round = decoder.u32()?;
    if round > MAX_CONSENSUS_ROUND {
        return Err(SignerError::Integrity(
            "round collides with vote-step high bit".into(),
        ));
    }
    let intent = match kind {
        1 => {
            let valid_round = decoder.i64()?;
            let proposal_id = ValueId(decoder.array32()?);
            let proposal = decoder.bytes(MAX_PROPOSAL_BYTES)?;
            let count: usize = decoder.u32()?.try_into().unwrap();
            if count > MAX_SIGNABLE_PARTS {
                return Err(SignerError::Integrity("too many proposal parts".into()));
            }
            let mut signable_parts = Vec::with_capacity(count);
            for _ in 0..count {
                signable_parts.push(decoder.bytes(MAX_SIGNABLE_BYTES)?);
            }
            SignedIntent::Proposal {
                round,
                valid_round,
                proposal_id,
                proposal,
                signable_parts,
            }
        }
        2 | 3 => {
            let value_id = ValueId(decoder.array32()?);
            let signable = decoder.bytes(MAX_SIGNABLE_BYTES)?;
            let transition = match decoder.u8()? {
                0 => None,
                1 => Some(decode_transition(&mut decoder)?),
                _ => return Err(SignerError::Integrity("invalid transition flag".into())),
            };
            SignedIntent::Vote {
                round,
                kind: if kind == 2 {
                    SlotKind::Prevote
                } else {
                    SlotKind::Precommit
                },
                value_id,
                signable,
                transition,
            }
        }
        _ => return Err(SignerError::Integrity("unknown signer slot kind".into())),
    };
    decoder.finish()?;
    Ok(intent)
}

fn frame_bytes(
    kind: u8,
    sequence: u64,
    previous_hash: [u8; 32],
    payload: &[u8],
) -> Result<(Vec<u8>, [u8; 32]), SignerError> {
    if payload.len() > MAX_RECORD_BYTES {
        return Err(SignerError::Integrity("record exceeds size limit".into()));
    }
    let payload_len: u32 = payload
        .len()
        .try_into()
        .map_err(|_| SignerError::Integrity("record length overflow".into()))?;
    let mut frame = Vec::with_capacity(88 + payload.len());
    frame.extend_from_slice(&WAL_MAGIC);
    put_u16(&mut frame, WAL_VERSION);
    frame.push(kind);
    frame.push(0);
    put_u32(&mut frame, payload_len);
    put_u64(&mut frame, sequence);
    frame.extend_from_slice(&previous_hash);
    frame.extend_from_slice(payload);
    let digest: [u8; 32] = blake3::hash(&frame).into();
    frame.extend_from_slice(&digest);
    Ok((frame, digest))
}

fn apply_record(loaded: &mut LoadedWal, kind: u8, payload: &[u8]) -> Result<(), SignerError> {
    match kind {
        RECORD_BOOTSTRAP_ORIGIN => {
            if loaded.epoch.is_some() || loaded.bootstrap_origin.is_some() {
                return Err(SignerError::Integrity(
                    "bootstrap origin must be the first and only origin record".into(),
                ));
            }
            if payload.len() != 32 {
                return Err(SignerError::Integrity(
                    "bootstrap origin hash must be exactly 32 bytes".into(),
                ));
            }
            let mut hash = [0u8; 32];
            hash.copy_from_slice(payload);
            loaded.bootstrap_origin = Some(hash);
        }
        RECORD_EPOCH => {
            let (epoch, authorized) = decode_epoch(payload)?;
            if let Some(current) = loaded.epoch.as_ref() {
                if !loaded.authorized {
                    return Err(SignerError::Integrity(
                        "unauthorized epoch cannot advance".into(),
                    ));
                }
                let expected = loaded.commit_applied_epoch.as_ref().ok_or_else(|| {
                    SignerError::Integrity(
                        "epoch advance without a durable commit-applied successor".into(),
                    )
                })?;
                if &epoch != expected || !authorized {
                    return Err(SignerError::Integrity(
                        "next epoch differs from the durable commit-applied successor".into(),
                    ));
                }
                let pending = loaded.pending_commit.as_ref().ok_or_else(|| {
                    SignerError::Integrity("next epoch has no pending certified commit".into())
                })?;
                validate_commit_successor(current, pending, &epoch)?;
            }
            loaded.epoch = Some(epoch);
            loaded.authorized = authorized;
            loaded.intents.clear();
            loaded.transition = None;
            loaded.poisoned = None;
            loaded.pending_commit = None;
            loaded.commit_applied_epoch = None;
        }
        RECORD_INTENT => {
            if loaded.epoch.is_none() {
                return Err(SignerError::Integrity("intent before epoch".into()));
            }
            let intent = decode_intent(payload)?;
            let slot = intent.slot();
            if let Some(existing) = loaded.intents.get(&slot) {
                if existing != &intent {
                    return Err(SignerError::Integrity(
                        "conflicting durable intents in one slot".into(),
                    ));
                }
            } else {
                if let SignedIntent::Vote {
                    transition: Some(transition),
                    ..
                } = &intent
                {
                    validate_transition(loaded.transition.as_ref(), transition)?;
                    loaded.transition = Some(transition.clone());
                }
                loaded.intents.insert(slot, intent);
            }
        }
        RECORD_STATE => {
            if loaded.epoch.is_none() {
                return Err(SignerError::Integrity("state before epoch".into()));
            }
            let mut decoder = Decoder::new(payload);
            let transition = decode_transition(&mut decoder)?;
            decoder.finish()?;
            validate_transition(loaded.transition.as_ref(), &transition)?;
            loaded.transition = Some(transition);
        }
        RECORD_POISON => {
            let mut decoder = Decoder::new(payload);
            let reason = String::from_utf8(decoder.bytes(4096)?)
                .map_err(|_| SignerError::Integrity("poison reason is not UTF-8".into()))?;
            decoder.finish()?;
            loaded.poisoned = Some(reason);
            loaded.authorized = false;
        }
        RECORD_COMMIT_INTENT => {
            if loaded.epoch.is_none() {
                return Err(SignerError::Integrity("commit record before epoch".into()));
            }
            if payload.len() > MAX_RECORD_BYTES {
                return Err(SignerError::Integrity("commit record exceeds limit".into()));
            }
            if loaded.commit_applied_epoch.is_some() {
                return Err(SignerError::Integrity(
                    "commit intent follows an unapplied epoch successor".into(),
                ));
            }
            let mut decoder = Decoder::new(payload);
            let decided_value_id = ValueId(decoder.array32()?);
            if decided_value_id == ValueId::NIL {
                return Err(SignerError::Integrity(
                    "commit intent cannot decide NIL".into(),
                ));
            }
            let proposal = decoder.bytes(MAX_PROPOSAL_BYTES)?;
            if proposal.is_empty() {
                return Err(SignerError::Integrity(
                    "commit intent has an empty recovery proposal".into(),
                ));
            }
            let certificate = decoder.bytes(MAX_RECORD_BYTES)?;
            decoder.finish()?;
            let digest: [u8; 32] = blake3::hash(payload).into();
            let pending = PendingCommit {
                digest,
                decided_value_id,
                proposal,
                certificate,
                proposal_evidence: PendingProposalEvidence::LegacyUnavailable,
            };
            if let Some(existing) = &loaded.pending_commit {
                if existing != &pending {
                    return Err(SignerError::Integrity(
                        "conflicting commit intents in one epoch".into(),
                    ));
                }
            }
            loaded.pending_commit = Some(pending);
        }
        RECORD_COMMIT_INTENT_V2 => {
            if loaded.epoch.is_none() {
                return Err(SignerError::Integrity("commit record before epoch".into()));
            }
            if payload.len() > MAX_RECORD_BYTES {
                return Err(SignerError::Integrity("commit record exceeds limit".into()));
            }
            if loaded.commit_applied_epoch.is_some() {
                return Err(SignerError::Integrity(
                    "commit intent follows an unapplied epoch successor".into(),
                ));
            }
            let mut decoder = Decoder::new(payload);
            let round = decoder.u32()?;
            if round > MAX_CONSENSUS_ROUND {
                return Err(SignerError::Integrity(
                    "commit round exceeds the canonical domain".into(),
                ));
            }
            let valid_round = decoder.i64()?;
            if valid_round < -1 || valid_round >= i64::from(round) {
                return Err(SignerError::Integrity(
                    "commit proposal valid-round is outside the canonical range".into(),
                ));
            }
            let decided_value_id = ValueId(decoder.array32()?);
            if decided_value_id == ValueId::NIL {
                return Err(SignerError::Integrity(
                    "commit intent cannot decide NIL".into(),
                ));
            }
            let proposal = decoder.bytes(MAX_PROPOSAL_BYTES)?;
            if proposal.is_empty() {
                return Err(SignerError::Integrity(
                    "commit intent has an empty recovery proposal".into(),
                ));
            }
            let signature_count: usize = decoder.u32()?.try_into().unwrap();
            if signature_count == 0 || signature_count > MAX_SIGNABLE_PARTS {
                return Err(SignerError::Integrity(
                    "commit proposal-signature count exceeds its bound".into(),
                ));
            }
            let mut proposal_sigs = Vec::with_capacity(signature_count);
            for _ in 0..signature_count {
                let signature = TMSig(decoder.take(64)?.try_into().unwrap());
                if signature == TMSig::NIL {
                    return Err(SignerError::Integrity(
                        "commit proposal-signature manifest is incomplete".into(),
                    ));
                }
                proposal_sigs.push(signature);
            }
            let certificate = decoder.bytes(MAX_RECORD_BYTES)?;
            decoder.finish()?;
            let digest: [u8; 32] = blake3::hash(payload).into();
            let pending = PendingCommit {
                digest,
                decided_value_id,
                proposal,
                certificate,
                proposal_evidence: PendingProposalEvidence::Exact {
                    round,
                    valid_round,
                    proposal_sigs,
                },
            };
            if let Some(existing) = &loaded.pending_commit {
                if existing != &pending {
                    return Err(SignerError::Integrity(
                        "conflicting commit intents in one epoch".into(),
                    ));
                }
            }
            loaded.pending_commit = Some(pending);
        }
        RECORD_COMMIT_APPLIED => {
            let current = loaded.epoch.as_ref().ok_or_else(|| {
                SignerError::Integrity("commit-applied marker before epoch".into())
            })?;
            let pending = loaded.pending_commit.as_ref().ok_or_else(|| {
                SignerError::Integrity("commit-applied marker without intent".into())
            })?;
            if loaded.commit_applied_epoch.is_some() {
                return Err(SignerError::Integrity(
                    "duplicate commit-applied successor".into(),
                ));
            }
            if payload.len() < 32 || payload[..32] != pending.digest {
                return Err(SignerError::Integrity(
                    "commit-applied digest mismatch".into(),
                ));
            }
            let (next_epoch, authorized) = decode_epoch(&payload[32..])?;
            if !authorized {
                return Err(SignerError::Integrity(
                    "commit-applied successor is unauthorized".into(),
                ));
            }
            validate_commit_successor(current, pending, &next_epoch)?;
            loaded.commit_applied_epoch = Some(next_epoch);
        }
        _ => return Err(SignerError::Integrity("unknown WAL record kind".into())),
    }
    Ok(())
}

fn load_wal_bytes(bytes: &[u8]) -> Result<LoadedWal, SignerError> {
    const HEADER: usize = 56;
    const DIGEST: usize = 32;
    let mut loaded = LoadedWal::default();
    let mut offset = 0usize;
    while offset < bytes.len() {
        if bytes.len() - offset < HEADER {
            loaded.clean_tail = false;
            break;
        }
        let header = &bytes[offset..offset + HEADER];
        if header[..8] != WAL_MAGIC {
            return Err(SignerError::Integrity("WAL magic mismatch".into()));
        }
        let mut decoder = Decoder::new(&header[8..]);
        if decoder.u16()? != WAL_VERSION {
            return Err(SignerError::Integrity("unsupported WAL version".into()));
        }
        let kind = decoder.u8()?;
        if decoder.u8()? != 0 {
            return Err(SignerError::Integrity("nonzero WAL reserved byte".into()));
        }
        let payload_len: usize = decoder.u32()?.try_into().unwrap();
        let sequence = decoder.u64()?;
        let previous_hash = decoder.array32()?;
        decoder.finish()?;
        if payload_len > MAX_RECORD_BYTES {
            return Err(SignerError::Integrity("WAL payload exceeds limit".into()));
        }
        if sequence != loaded.next_sequence {
            return Err(SignerError::Integrity("WAL sequence gap or reorder".into()));
        }
        if previous_hash != loaded.last_hash {
            return Err(SignerError::Integrity("WAL hash-chain mismatch".into()));
        }
        let frame_len = HEADER
            .checked_add(payload_len)
            .and_then(|n| n.checked_add(DIGEST))
            .ok_or_else(|| SignerError::Integrity("WAL frame length overflow".into()))?;
        if bytes.len() - offset < frame_len {
            loaded.clean_tail = false;
            break;
        }
        let payload_start = offset + HEADER;
        let payload_end = payload_start + payload_len;
        let expected_digest: [u8; 32] =
            bytes[payload_end..payload_end + DIGEST].try_into().unwrap();
        let actual_digest: [u8; 32] = blake3::hash(&bytes[offset..payload_end]).into();
        if expected_digest != actual_digest {
            return Err(SignerError::Integrity("WAL record digest mismatch".into()));
        }
        apply_record(&mut loaded, kind, &bytes[payload_start..payload_end])?;
        loaded.last_hash = expected_digest;
        loaded.next_sequence += 1;
        offset += frame_len;
    }
    Ok(loaded)
}

#[derive(Debug, Default)]
struct LoadedAnchor {
    next_sequence: u64,
    last_wal_hash: [u8; 32],
    last_hash: [u8; 32],
    clean_tail: bool,
}

fn anchor_frame_bytes(
    sequence: u64,
    wal_hash: [u8; 32],
    previous_hash: [u8; 32],
) -> (Vec<u8>, [u8; 32]) {
    let mut frame = Vec::with_capacity(116);
    frame.extend_from_slice(&ANCHOR_MAGIC);
    put_u16(&mut frame, WAL_VERSION);
    put_u16(&mut frame, 0);
    put_u64(&mut frame, sequence);
    frame.extend_from_slice(&wal_hash);
    frame.extend_from_slice(&previous_hash);
    let digest: [u8; 32] = blake3::hash(&frame).into();
    frame.extend_from_slice(&digest);
    (frame, digest)
}

fn load_anchor_bytes(bytes: &[u8]) -> Result<LoadedAnchor, SignerError> {
    const FRAME: usize = 116;
    let mut loaded = LoadedAnchor {
        clean_tail: true,
        ..LoadedAnchor::default()
    };
    let mut offset = 0usize;
    while offset < bytes.len() {
        if bytes.len() - offset < FRAME {
            loaded.clean_tail = false;
            break;
        }
        let frame = &bytes[offset..offset + FRAME];
        if frame[..8] != ANCHOR_MAGIC {
            return Err(SignerError::Integrity("anchor magic mismatch".into()));
        }
        let mut decoder = Decoder::new(&frame[8..84]);
        if decoder.u16()? != WAL_VERSION {
            return Err(SignerError::Integrity("anchor version mismatch".into()));
        }
        if decoder.u16()? != 0 {
            return Err(SignerError::Integrity(
                "anchor reserved field is nonzero".into(),
            ));
        }
        let sequence = decoder.u64()?;
        let wal_hash = decoder.array32()?;
        let previous_hash = decoder.array32()?;
        decoder.finish()?;
        if sequence != loaded.next_sequence {
            return Err(SignerError::Integrity(
                "anchor sequence gap or reorder".into(),
            ));
        }
        if previous_hash != loaded.last_hash {
            return Err(SignerError::Integrity("anchor hash-chain mismatch".into()));
        }
        let expected: [u8; 32] = frame[84..116].try_into().unwrap();
        let actual: [u8; 32] = blake3::hash(&frame[..84]).into();
        if expected != actual {
            return Err(SignerError::Integrity("anchor digest mismatch".into()));
        }
        loaded.next_sequence += 1;
        loaded.last_wal_hash = wal_hash;
        loaded.last_hash = expected;
        offset += FRAME;
    }
    Ok(loaded)
}

fn sync_parent(path: &Path) -> Result<(), SignerError> {
    let parent = path
        .parent()
        .ok_or_else(|| SignerError::Integrity("WAL path has no parent".into()))?;
    #[cfg(unix)]
    {
        File::open(parent)?.sync_all()?;
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
    }
    Ok(())
}

fn open_locked_file(path: &Path) -> Result<(File, bool), SignerError> {
    let existed = path.exists();
    let mut options = OpenOptions::new();
    options.read(true).append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() {
        return Err(SignerError::Integrity(
            "WAL path is not a regular file".into(),
        ));
    }
    #[cfg(unix)]
    {
        use nix::{fcntl::FlockArg, unistd::geteuid};
        use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
        if metadata.uid() != geteuid().as_raw() {
            return Err(SignerError::Integrity("WAL owner mismatch".into()));
        }
        if metadata.mode() & 0o077 != 0 {
            return Err(SignerError::Integrity(
                "WAL permissions are broader than 0600".into(),
            ));
        }
        #[allow(deprecated)]
        let lock_result =
            nix::fcntl::flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock);
        if lock_result.is_err() {
            return Err(SignerError::Integrity(
                "another signer process holds this WAL".into(),
            ));
        }
    }
    if !existed {
        sync_parent(path)?;
    }
    Ok((file, !existed))
}

fn read_all(file: &mut File) -> Result<Vec<u8>, SignerError> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    file.seek(SeekFrom::End(0))?;
    Ok(bytes)
}

#[derive(Debug)]
struct WalFiles {
    wal: File,
    anchor: File,
    loaded: LoadedWal,
    loaded_anchor: LoadedAnchor,
    #[cfg(test)]
    failpoint: Option<WalFailpoint>,
}

impl WalFiles {
    #[cfg(test)]
    fn fail_if(&mut self, point: WalFailpoint) -> Result<(), SignerError> {
        if self.failpoint == Some(point) {
            self.failpoint = None;
            return Err(SignerError::Io(std::io::Error::new(
                std::io::ErrorKind::Other,
                format!("injected WAL fault at {point:?}"),
            )));
        }
        Ok(())
    }

    fn append(&mut self, kind: u8, payload: &[u8]) -> Result<[u8; 32], SignerError> {
        if self.loaded.next_sequence != self.loaded_anchor.next_sequence {
            return Err(SignerError::Integrity(
                "WAL/anchor sequence mismatch".into(),
            ));
        }
        let sequence = self.loaded.next_sequence;
        let (frame, wal_hash) = frame_bytes(kind, sequence, self.loaded.last_hash, payload)?;
        self.wal.write_all(&frame)?;
        #[cfg(test)]
        self.fail_if(WalFailpoint::AfterWalWrite)?;
        self.wal.sync_all()?;
        #[cfg(test)]
        self.fail_if(WalFailpoint::AfterWalSync)?;

        let (anchor_frame, anchor_hash) =
            anchor_frame_bytes(sequence, wal_hash, self.loaded_anchor.last_hash);
        self.anchor.write_all(&anchor_frame)?;
        #[cfg(test)]
        self.fail_if(WalFailpoint::AfterAnchorWrite)?;
        self.anchor.sync_all()?;
        #[cfg(test)]
        self.fail_if(WalFailpoint::AfterAnchorSync)?;

        apply_record(&mut self.loaded, kind, payload)?;
        self.loaded.next_sequence += 1;
        self.loaded.last_hash = wal_hash;
        self.loaded_anchor.next_sequence += 1;
        self.loaded_anchor.last_wal_hash = wal_hash;
        self.loaded_anchor.last_hash = anchor_hash;
        Ok(wal_hash)
    }
}

pub struct DurableSigner {
    signing_key: SigningKey,
    epoch: SignerEpochBinding,
    status: SignerStatus,
    files: Option<WalFiles>,
    pending_commit: Option<PendingCommit>,
    intents: BTreeMap<SignerSlot, SignedIntent>,
    transition: Option<LockValidTransition>,
}

impl std::fmt::Debug for DurableSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableSigner")
            .field("public_key", &self.epoch.public_key)
            .field("height", &self.epoch.height)
            .field("status", &self.status)
            .field("intent_count", &self.intents.len())
            .finish()
    }
}

impl DurableSigner {
    pub fn observer_only(
        signing_key: SigningKey,
        epoch: SignerEpochBinding,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            signing_key,
            epoch,
            status: SignerStatus::ObserverOnly(reason.into()),
            files: None,
            pending_commit: None,
            intents: BTreeMap::new(),
            transition: None,
        }
    }

    #[cfg(any(test, feature = "simulation"))]
    pub fn ephemeral_for_simulation(signing_key: SigningKey, epoch: SignerEpochBinding) -> Self {
        Self {
            signing_key,
            epoch,
            status: SignerStatus::Active,
            files: None,
            pending_commit: None,
            intents: BTreeMap::new(),
            transition: None,
        }
    }

    pub fn open(
        signing_key: SigningKey,
        config: DurableSignerConfig,
        epoch: SignerEpochBinding,
    ) -> Result<Self, SignerError> {
        let public_key = PubKeyID(VerificationKeyBytes::from(&signing_key).into());
        if public_key != epoch.public_key {
            return Err(SignerError::Integrity(
                "signing key does not match epoch public key".into(),
            ));
        }
        if config.wal_path == config.anchor_path {
            return Err(SignerError::Integrity(
                "WAL and anchor paths must differ".into(),
            ));
        }
        // Authority is a prerequisite to opening either journal. In particular,
        // OpenOptions::create must never materialize an empty WAL/anchor pair that
        // can later be mistaken for initialized signer history.
        if !config.independent_anchor_authorized {
            return Ok(Self::observer_only(
                signing_key,
                epoch,
                "independent anti-rollback/key-fencing authority is absent",
            ));
        }
        if epoch.height > 0
            && config
                .non_genesis_bootstrap_receipt_hash
                .filter(|hash| *hash != [0u8; 32])
                .is_none()
        {
            return Ok(Self::observer_only(
                signing_key,
                epoch,
                "exact non-genesis bootstrap receipt is absent",
            ));
        }

        let (mut wal, _) = open_locked_file(&config.wal_path)?;
        let (mut anchor, _) = open_locked_file(&config.anchor_path)?;
        let wal_bytes = read_all(&mut wal)?;
        let anchor_bytes = read_all(&mut anchor)?;
        let loaded = load_wal_bytes(&wal_bytes)?;
        let loaded_anchor = load_anchor_bytes(&anchor_bytes)?;

        let mut files = WalFiles {
            wal,
            anchor,
            loaded,
            loaded_anchor,
            #[cfg(test)]
            failpoint: None,
        };
        let empty = files.loaded.next_sequence == 0 && files.loaded_anchor.next_sequence == 0;
        if empty {
            let bootstrap_origin = if epoch.height == 0 {
                Some([0u8; 32])
            } else {
                config
                    .non_genesis_bootstrap_receipt_hash
                    .filter(|hash| *hash != [0u8; 32])
            };
            if let Some(origin) = bootstrap_origin {
                files.append(RECORD_BOOTSTRAP_ORIGIN, &origin)?;
            }
            let authorized = config.independent_anchor_authorized && bootstrap_origin.is_some();
            files.append(RECORD_EPOCH, &encode_epoch(&epoch, authorized))?;
        }

        let pre_recovery_anchor_matches = files.loaded.next_sequence
            == files.loaded_anchor.next_sequence
            && files.loaded.last_hash == files.loaded_anchor.last_wal_hash;
        let recovery_pair = files
            .loaded
            .epoch
            .clone()
            .zip(files.loaded.pending_commit.clone());
        let bootstrap_matches = match files.loaded.bootstrap_origin {
            Some(origin) if origin == [0u8; 32] => true,
            Some(origin) => config.non_genesis_bootstrap_receipt_hash == Some(origin),
            None => false,
        };
        if files.loaded.clean_tail
            && files.loaded_anchor.clean_tail
            && pre_recovery_anchor_matches
            && files.loaded.poisoned.is_none()
            && files.loaded.authorized
            && config.independent_anchor_authorized
            && bootstrap_matches
        {
            if let Some((current, pending)) = recovery_pair {
                if validate_commit_successor(&current, &pending, &epoch).is_ok() {
                    let may_finish = match files.loaded.commit_applied_epoch.as_ref() {
                        Some(expected) => expected == &epoch,
                        None => {
                            let mut applied =
                                Vec::with_capacity(32 + encode_epoch(&epoch, true).len());
                            applied.extend_from_slice(&pending.digest);
                            applied.extend_from_slice(&encode_epoch(&epoch, true));
                            files.append(RECORD_COMMIT_APPLIED, &applied)?;
                            true
                        }
                    };
                    if may_finish {
                        files.append(RECORD_EPOCH, &encode_epoch(&epoch, true))?;
                    }
                }
            }
        }

        let anchor_matches = files.loaded.next_sequence == files.loaded_anchor.next_sequence
            && files.loaded.last_hash == files.loaded_anchor.last_wal_hash;
        let epoch_matches = files.loaded.epoch.as_ref() == Some(&epoch);
        let status = if !files.loaded.clean_tail || !files.loaded_anchor.clean_tail {
            SignerStatus::ObserverOnly("torn WAL or anchor tail; unknown signing history".into())
        } else if !anchor_matches {
            SignerStatus::ObserverOnly("WAL and independent high-water anchor disagree".into())
        } else if let Some(reason) = &files.loaded.poisoned {
            SignerStatus::Poisoned(reason.clone())
        } else if !epoch_matches {
            SignerStatus::ObserverOnly("durable signer epoch does not match committed state".into())
        } else if epoch.roster_index >= epoch.active_roster_len {
            SignerStatus::ObserverOnly("signing key is not in the active epoch roster".into())
        } else if let Some(pending) = files.loaded.pending_commit.as_ref() {
            match &pending.proposal_evidence {
                PendingProposalEvidence::Exact { .. } => SignerStatus::ReconciliationRequired(
                    pending.digest,
                    PENDING_COMMIT_RECOVERY_REASON.into(),
                ),
                PendingProposalEvidence::LegacyUnavailable => {
                    SignerStatus::ObserverOnly(LEGACY_PENDING_COMMIT_REASON.into())
                }
            }
        } else if !bootstrap_matches {
            SignerStatus::ObserverOnly(
                "genesis origin or exact non-genesis bootstrap receipt is absent".into(),
            )
        } else if !config.independent_anchor_authorized {
            SignerStatus::ObserverOnly(
                "independent anti-rollback/key-fencing authority is absent".into(),
            )
        } else if !files.loaded.authorized {
            SignerStatus::ObserverOnly("unknown pre-WAL key history at a non-genesis height".into())
        } else {
            SignerStatus::Active
        };

        let intents = files.loaded.intents.clone();
        let transition = files.loaded.transition.clone();
        let pending_commit = files.loaded.pending_commit.clone();
        Ok(Self {
            signing_key,
            epoch,
            status,
            files: Some(files),
            pending_commit,
            intents,
            transition,
        })
    }

    pub fn open_or_observer(
        signing_key: SigningKey,
        config: DurableSignerConfig,
        epoch: SignerEpochBinding,
    ) -> Self {
        let observer_key = signing_key.clone();
        match Self::open(signing_key, config, epoch.clone()) {
            Ok(signer) => signer,
            Err(error) => Self::observer_only(
                observer_key,
                epoch,
                format!("durable signer open failed; consensus signing disabled: {error}"),
            ),
        }
    }

    pub fn status(&self) -> &SignerStatus {
        &self.status
    }
    pub fn is_active(&self) -> bool {
        self.status == SignerStatus::Active
    }
    pub fn epoch(&self) -> &SignerEpochBinding {
        &self.epoch
    }

    // Observer nodes still need to authenticate their transport identity to collect
    // evidence. Restrict that escape hatch to an already domain-separated 32-byte
    // digest, so no caller can feed raw proposal or vote bytes through it.
    pub(super) fn sign_auxiliary_digest(&self, digest: &[u8; 32]) -> TMSig {
        TMSig(self.signing_key.sign(digest).to_bytes())
    }

    pub fn fail_closed(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        self.poison(reason);
    }

    /// Latch an ambiguous/transient decision-application boundary without
    /// writing a poison record. The supplied digest must name the one exact
    /// durable pending commit. Restart derives the same state from that WAL
    /// record, so this cannot grant authority or select a different value.
    pub fn require_reconciliation(
        &mut self,
        commit_intent_digest: [u8; 32],
        reason: impl Into<String>,
    ) -> Result<(), SignerError> {
        let reason = reason.into();
        let Some(pending) = self.pending_commit.as_ref() else {
            self.status = SignerStatus::ObserverOnly(format!(
                "{reason}; no durable pending commit exists"
            ));
            return Err(SignerError::Integrity(
                "cannot enter reconciliation without a pending commit".into(),
            ));
        };
        if pending.digest != commit_intent_digest {
            let conflict =
                "reconciliation digest does not match the durable pending commit".to_string();
            self.poison(conflict.clone());
            return Err(SignerError::Conflict(conflict));
        }
        if !matches!(
            &pending.proposal_evidence,
            PendingProposalEvidence::Exact { .. }
        ) {
            self.status = SignerStatus::ObserverOnly(LEGACY_PENDING_COMMIT_REASON.into());
            return Err(SignerError::Integrity(LEGACY_PENDING_COMMIT_REASON.into()));
        }
        self.status = SignerStatus::ReconciliationRequired(commit_intent_digest, reason);
        Ok(())
    }

    fn require_active(&self) -> Result<(), SignerError> {
        match &self.status {
            SignerStatus::Active => Ok(()),
            SignerStatus::ObserverOnly(reason) => Err(SignerError::ObserverOnly(reason.clone())),
            SignerStatus::ReconciliationRequired(digest, reason) => Err(
                SignerError::ReconciliationRequired(*digest, reason.clone()),
            ),
            SignerStatus::Poisoned(reason) => Err(SignerError::Conflict(reason.clone())),
        }
    }

    fn poison(&mut self, reason: String) {
        let mut payload = Vec::new();
        let durable_reason = if put_bytes(&mut payload, reason.as_bytes()).is_ok() {
            self.files
                .as_mut()
                .and_then(|files| files.append(RECORD_POISON, &payload).ok())
                .is_some()
        } else {
            false
        };
        self.status = SignerStatus::Poisoned(if durable_reason {
            reason
        } else {
            format!("{reason}; poison persistence failed")
        });
    }

    fn prepare_intent(&mut self, intent: SignedIntent) -> Result<(), SignerError> {
        self.require_active()?;
        let slot = intent.slot();
        if slot.round > MAX_CONSENSUS_ROUND {
            self.poison("round collides with the vote-step high bit".into());
            return Err(SignerError::Conflict(
                "round collides with the vote-step high bit".into(),
            ));
        }
        if let Some(existing) = self.intents.get(&slot) {
            if existing == &intent {
                return Ok(());
            }
            let reason = format!("different exact bytes requested for slot {:?}", slot);
            self.poison(reason.clone());
            return Err(SignerError::Conflict(reason));
        }
        if let SignedIntent::Vote {
            transition: Some(transition),
            ..
        } = &intent
        {
            if let Err(error) = validate_transition(self.transition.as_ref(), transition) {
                let reason = error.to_string();
                self.poison(reason.clone());
                return Err(SignerError::Conflict(reason));
            }
        }
        let payload = encode_intent(&intent)?;
        if let Some(files) = &mut self.files {
            if let Err(error) = files.append(RECORD_INTENT, &payload) {
                self.status = SignerStatus::ObserverOnly(format!(
                    "durability failure before signing: {error}"
                ));
                return Err(error);
            }
        }
        if let SignedIntent::Vote {
            transition: Some(transition),
            ..
        } = &intent
        {
            self.transition = Some(transition.clone());
        }
        self.intents.insert(slot, intent);
        Ok(())
    }

    pub fn sign_proposal(
        &mut self,
        hash_keys: &HashKeys,
        roster: &[SortedRosterMember],
        round: u32,
        valid_round: i64,
        proposal_id: ValueId,
        proposal: &[u8],
        signable_parts: &[Vec<u8>],
    ) -> Result<Vec<TMSig>, SignerError> {
        self.require_active()?;
        if let Err(error) = validate_epoch_roster(&self.epoch, roster) {
            let reason = format!("proposal roster failed epoch binding: {error}");
            self.poison(reason.clone());
            return Err(SignerError::Conflict(reason));
        }
        if round > MAX_CONSENSUS_ROUND {
            self.poison("proposal round collides with the vote-step high bit".into());
            return Err(SignerError::Conflict(
                "proposal round collides with the vote-step high bit".into(),
            ));
        }
        if valid_round < -1 || valid_round >= i64::from(round) {
            self.poison("proposal valid-round is outside the safe range".into());
            return Err(SignerError::Conflict(
                "proposal valid-round is outside the safe range".into(),
            ));
        }
        if proposal.is_empty()
            || proposal.len() > MAX_PROPOSAL_BYTES
            || signable_parts.len() > MAX_SIGNABLE_PARTS
        {
            return Err(SignerError::Integrity(
                "proposal manifest exceeds bound".into(),
            ));
        }
        if signable_parts
            .iter()
            .any(|part| part.len() > MAX_SIGNABLE_BYTES)
        {
            return Err(SignerError::Integrity(
                "proposal signable part exceeds bound".into(),
            ));
        }
        let computed_id = BlockValue(proposal.to_vec()).id_from_value(hash_keys);
        if computed_id != proposal_id {
            self.poison("proposal bytes do not match the requested value ID".into());
            return Err(SignerError::Conflict(
                "proposal bytes do not match the requested value ID".into(),
            ));
        }
        let mut expected_parts = Vec::with_capacity(BlockValue(proposal.to_vec()).chunks_n());
        let proposal_value = BlockValue(proposal.to_vec());
        let mut header = PacketProposalChunkHeader {
            height: self.epoch.height,
            round,
            chunk_i: 0,
            proposal_size: proposal.len().try_into().unwrap(),
            proposal_id,
            valid_round,
        };
        let mut buffer = [0u8; 2048];
        for chunk_i in 0..proposal_value.chunks_n() {
            header.chunk_i = chunk_i.try_into().unwrap();
            let mut offset = header.write_to(&mut buffer);
            let (chunk_offset, chunk_size) = proposal_value.chunk_o_size(chunk_i);
            offset +=
                proposal[chunk_offset..chunk_offset + chunk_size].write_to(&mut buffer[offset..]);
            expected_parts.push(buffer[..offset].to_vec());
        }
        if signable_parts != expected_parts {
            self.poison("proposal signable chunks are not the canonical manifest".into());
            return Err(SignerError::Conflict(
                "proposal signable chunks are not the canonical manifest".into(),
            ));
        }
        let intent = SignedIntent::Proposal {
            round,
            valid_round,
            proposal_id,
            proposal: proposal.to_vec(),
            signable_parts: signable_parts.to_vec(),
        };
        self.prepare_intent(intent)?;
        Ok(signable_parts
            .iter()
            .map(|part| {
                TMSig(sign_with_namespace(
                    &self.signing_key,
                    part,
                    &self.epoch.vote_namespace,
                ))
            })
            .collect())
    }

    pub fn sign_vote(
        &mut self,
        hash_keys: &HashKeys,
        roster: &[SortedRosterMember],
        round: u32,
        is_precommit: bool,
        value_id: ValueId,
        signable: &[u8],
        transition: Option<LockValidTransition>,
    ) -> Result<TMSig, SignerError> {
        self.require_active()?;
        if let Err(error) = validate_epoch_roster(&self.epoch, roster) {
            let reason = format!("vote roster failed epoch binding: {error}");
            self.poison(reason.clone());
            return Err(SignerError::Conflict(reason));
        }
        if round > MAX_CONSENSUS_ROUND {
            self.poison("vote round collides with the vote-step high bit".into());
            return Err(SignerError::Conflict(
                "vote round collides with the vote-step high bit".into(),
            ));
        }
        if signable.len() > MAX_SIGNABLE_BYTES {
            return Err(SignerError::Integrity(
                "vote signable bytes exceed bound".into(),
            ));
        }
        let expected = make_vote_sign_datas(
            self.epoch.public_key,
            is_precommit,
            self.epoch.height,
            round,
            value_id,
        )[1];
        if signable != expected {
            self.poison("vote signable bytes do not match the epoch/round/step/value".into());
            return Err(SignerError::Conflict(
                "vote signable bytes do not match the epoch/round/step/value".into(),
            ));
        }
        match (is_precommit, value_id == ValueId::NIL, transition.as_ref()) {
            (true, false, Some(transition)) => {
                if transition.locked_round != i64::from(round)
                    || transition.locked_value_id != value_id
                    || transition.valid_round != i64::from(round)
                    || transition.valid_value_id != value_id
                {
                    self.poison("precommit transition does not exactly bind this vote".into());
                    return Err(SignerError::Conflict(
                        "precommit transition does not exactly bind this vote".into(),
                    ));
                }
                if let Err(error) =
                    verify_transition_certificate(transition, &self.epoch, hash_keys, roster)
                {
                    let reason = format!(
                        "precommit transition certificate failed signer verification: {error}"
                    );
                    self.poison(reason.clone());
                    return Err(SignerError::Conflict(reason));
                }
            }
            (true, false, None) => {
                self.poison("non-NIL precommit is missing its durable quorum transition".into());
                return Err(SignerError::Conflict(
                    "non-NIL precommit is missing its durable quorum transition".into(),
                ));
            }
            (_, _, Some(_)) => {
                self.poison("precommit transition does not exactly bind this vote".into());
                return Err(SignerError::Conflict(
                    "precommit transition does not exactly bind this vote".into(),
                ));
            }
            (_, _, None) => {}
        }
        let intent = SignedIntent::Vote {
            round,
            kind: if is_precommit {
                SlotKind::Precommit
            } else {
                SlotKind::Prevote
            },
            value_id,
            signable: signable.to_vec(),
            transition,
        };
        self.prepare_intent(intent)?;
        Ok(TMSig(sign_with_namespace(
            &self.signing_key,
            signable,
            &self.epoch.vote_namespace,
        )))
    }

    pub fn persist_transition(
        &mut self,
        transition: LockValidTransition,
        hash_keys: &HashKeys,
        roster: &[SortedRosterMember],
    ) -> Result<(), SignerError> {
        self.require_active()?;
        if let Err(error) =
            verify_transition_certificate(&transition, &self.epoch, hash_keys, roster)
        {
            let reason =
                format!("state transition certificate failed signer verification: {error}");
            self.poison(reason.clone());
            return Err(SignerError::Conflict(reason));
        }
        if let Err(error) = validate_transition(self.transition.as_ref(), &transition) {
            let reason = error.to_string();
            self.poison(reason.clone());
            return Err(SignerError::Conflict(reason));
        }
        if self.transition.as_ref() == Some(&transition) {
            return Ok(());
        }
        let mut payload = Vec::new();
        encode_transition(&mut payload, &transition)?;
        if let Some(files) = &mut self.files {
            if let Err(error) = files.append(RECORD_STATE, &payload) {
                self.status = SignerStatus::ObserverOnly(format!(
                    "durability failure before state use: {error}"
                ));
                return Err(error);
            }
        }
        self.transition = Some(transition);
        Ok(())
    }

    pub fn begin_commit(
        &mut self,
        hash_keys: &HashKeys,
        round: u32,
        decided_value_id: ValueId,
        proposal: &BlockValue,
        proposal_valid_round: i64,
        proposal_sigs: &[TMSig],
        commit_certificate: &[u8],
        roster: &[SortedRosterMember],
    ) -> Result<[u8; 32], SignerError> {
        self.require_active()?;
        if proposal.id_from_value(hash_keys) != decided_value_id {
            let reason = "commit proposal bytes do not match the decided value ID".to_string();
            self.poison(reason.clone());
            return Err(SignerError::Conflict(reason));
        }
        if let Err(error) = verify_proposal_signature_manifest(
            hash_keys,
            &self.epoch,
            roster,
            round,
            proposal_valid_round,
            proposal,
            decided_value_id,
            proposal_sigs,
        ) {
            let reason = format!("commit proposal manifest failed signer verification: {error}");
            self.poison(reason.clone());
            return Err(SignerError::Conflict(reason));
        }
        if let Err(error) = verify_precommit_certificate(
            commit_certificate,
            round,
            decided_value_id,
            &self.epoch,
            roster,
        ) {
            let reason = format!("commit certificate failed signer verification: {error}");
            self.poison(reason.clone());
            return Err(SignerError::Conflict(reason));
        }
        let payload = encode_commit_intent(
            round,
            decided_value_id,
            &proposal.0,
            proposal_valid_round,
            proposal_sigs,
            commit_certificate,
        )?;
        let digest: [u8; 32] = blake3::hash(&payload).into();
        if let Some(files) = &mut self.files {
            if let Err(error) = files.append(RECORD_COMMIT_INTENT_V2, &payload) {
                self.status = SignerStatus::ObserverOnly(format!(
                    "commit intent durability failure: {error}"
                ));
                return Err(error);
            }
        }
        self.pending_commit = Some(PendingCommit {
            digest,
            decided_value_id,
            proposal: proposal.0.clone(),
            certificate: commit_certificate.to_vec(),
            proposal_evidence: PendingProposalEvidence::Exact {
                round,
                valid_round: proposal_valid_round,
                proposal_sigs: proposal_sigs.to_vec(),
            },
        });
        Ok(digest)
    }

    /// Persist a new commit intent for an active signer, or resume the one exact
    /// pending intent after the durable PoS store has been idempotently reconciled.
    /// Ordinary observers return `None` and never gain signing authority here.
    pub fn begin_or_resume_commit(
        &mut self,
        hash_keys: &HashKeys,
        round: u32,
        decided_value_id: ValueId,
        proposal: &BlockValue,
        proposal_valid_round: i64,
        proposal_sigs: &[TMSig],
        commit_certificate: &[u8],
        roster: &[SortedRosterMember],
    ) -> Result<Option<[u8; 32]>, SignerError> {
        if self.is_active() {
            return self
                .begin_commit(
                    hash_keys,
                    round,
                    decided_value_id,
                    proposal,
                    proposal_valid_round,
                    proposal_sigs,
                    commit_certificate,
                    roster,
                )
                .map(Some);
        }
        let recovery_digest = match &self.status {
            SignerStatus::ReconciliationRequired(digest, _) => Some(*digest),
            _ => None,
        };
        let Some(recovery_digest) = recovery_digest else {
            return Ok(None);
        };
        let pending = self
            .pending_commit
            .clone()
            .ok_or_else(|| {
                SignerError::Integrity("recovery observer has no durable commit intent".into())
            })?;
        verify_precommit_certificate(
            commit_certificate,
            round,
            decided_value_id,
            &self.epoch,
            roster,
        )?;
        verify_proposal_signature_manifest(
            hash_keys,
            &self.epoch,
            roster,
            round,
            proposal_valid_round,
            proposal,
            decided_value_id,
            proposal_sigs,
        )?;
        if proposal.id_from_value(hash_keys) != decided_value_id {
            let reason = "observed recovery proposal does not match the pending value ID".to_string();
            self.poison(reason.clone());
            return Err(SignerError::Conflict(reason));
        }
        let payload = encode_commit_intent(
            round,
            decided_value_id,
            &proposal.0,
            proposal_valid_round,
            proposal_sigs,
            commit_certificate,
        )?;
        let digest: [u8; 32] = blake3::hash(&payload).into();
        if digest != recovery_digest {
            let reason = "observed decision digest conflicts with reconciliation state".to_string();
            self.poison(reason.clone());
            return Err(SignerError::Conflict(reason));
        }
        if pending.decided_value_id != decided_value_id
            || pending.proposal != proposal.0
            || pending.certificate != commit_certificate
            || pending.proposal_evidence
                != (PendingProposalEvidence::Exact {
                    round,
                    valid_round: proposal_valid_round,
                    proposal_sigs: proposal_sigs.to_vec(),
                })
            || pending.digest != digest
        {
            let reason = "observed decision conflicts with the pending durable commit".to_string();
            self.poison(reason.clone());
            return Err(SignerError::Conflict(reason));
        }
        Ok(Some(digest))
    }

    /// Return the one exact certified decision needed to reconcile a crash that
    /// happened after commit-intent durability but before PoS-store durability.
    /// This never authorizes signing; the caller must apply and durably reread the
    /// decision, then call `complete_commit` before the signer can become active.
    pub(super) fn pending_commit_recovery(
        &self,
        hash_keys: &HashKeys,
        roster: &[SortedRosterMember],
    ) -> Result<Option<PendingCommitRecovery>, SignerError> {
        let Some(pending) = self.pending_commit.as_ref() else {
            return Ok(None);
        };
        let status_digest = match &self.status {
            SignerStatus::ReconciliationRequired(digest, _) => *digest,
            _ => {
                return Err(SignerError::Integrity(
                    "pending commit exists outside exact reconciliation state".into(),
                ))
            }
        };
        if status_digest != pending.digest {
            return Err(SignerError::Integrity(
                "reconciliation status digest differs from the pending commit".into(),
            ));
        }
        let (recorded_round, proposal_valid_round, proposal_sigs) =
            match &pending.proposal_evidence {
                PendingProposalEvidence::Exact {
                    round,
                    valid_round,
                    proposal_sigs,
                } => (*round, *valid_round, proposal_sigs.clone()),
                PendingProposalEvidence::LegacyUnavailable => {
                    return Err(SignerError::Integrity(LEGACY_PENDING_COMMIT_REASON.into()))
                }
            };
        let proposal = BlockValue(pending.proposal.clone());
        if proposal.id_from_value(hash_keys) != pending.decided_value_id {
            return Err(SignerError::Integrity(
                "durable recovery proposal bytes do not match the certified value ID".into(),
            ));
        }
        let (certificate_round, fat_pointer, precommits) =
            recover_fat_pointer_from_commit_certificate(pending, &self.epoch, roster)?;
        if certificate_round != recorded_round {
            return Err(SignerError::Integrity(
                "durable proposal manifest round differs from its commit certificate".into(),
            ));
        }
        verify_proposal_signature_manifest(
            hash_keys,
            &self.epoch,
            roster,
            recorded_round,
            proposal_valid_round,
            &proposal,
            pending.decided_value_id,
            &proposal_sigs,
        )?;

        let active_len = active_roster_len(roster);
        if precommits.len() != active_len {
            return Err(SignerError::Integrity(
                "recovered precommit evidence length differs from the active roster".into(),
            ));
        }
        let mut msg_val_sigs = vec![[(ValueId::NIL, TMSig::NIL); 2]; roster.len()];
        let mut precommit_power = 0u64;
        let mut yes_precommit_power = 0u64;
        for (index, (value_id, signature)) in precommits.into_iter().enumerate() {
            msg_val_sigs[index][1] = (value_id, signature);
            if signature != TMSig::NIL {
                precommit_power = precommit_power
                    .checked_add(roster[index].stake)
                    .ok_or_else(|| SignerError::Integrity("precommit power overflow".into()))?;
                if value_id == pending.decided_value_id {
                    yes_precommit_power = yes_precommit_power
                        .checked_add(roster[index].stake)
                        .ok_or_else(|| {
                            SignerError::Integrity("YES precommit power overflow".into())
                        })?;
                }
            }
        }
        let round_data = RoundData {
            height: self.epoch.height,
            round: recorded_round,
            proposal,
            proposal_valid_round,
            proposal_sigs_n: proposal_sigs.len(),
            proposal_sigs,
            proposal_id: pending.decided_value_id,
            msg_val_sigs,
            roster: roster.to_vec(),
            counts: ConsensusCounts {
                anys: precommit_power,
                prevotes: 0,
                nil_prevotes: 0,
                yes_prevotes: 0,
                precommits: precommit_power,
                yes_precommits: yes_precommit_power,
            },
            vote_namespace: self.epoch.vote_namespace,
            ..RoundData::EMPTY
        };
        verify_reconstructed_precommit_quorum(&round_data, roster)
            .map_err(SignerError::Integrity)?;
        Ok(Some(PendingCommitRecovery {
            digest: pending.digest,
            proposal: round_data.proposal.clone(),
            proposal_valid_round: round_data.proposal_valid_round,
            proposal_sigs: round_data.proposal_sigs.clone(),
            round_data,
            fat_pointer,
        }))
    }

    pub fn complete_commit(
        &mut self,
        commit_intent_digest: [u8; 32],
        durable_store_readback: [u8; 32],
        next_vote_namespace: [u8; 32],
        next_roster: &[SortedRosterMember],
    ) -> Result<(), SignerError> {
        match self.status.clone() {
            SignerStatus::Active => {}
            SignerStatus::ReconciliationRequired(digest, _)
                if digest == commit_intent_digest => {}
            SignerStatus::ReconciliationRequired(_, _) => {
                let reason =
                    "commit completion digest conflicts with reconciliation state".to_string();
                self.poison(reason.clone());
                return Err(SignerError::Conflict(reason));
            }
            _ => self.require_active()?,
        }
        if let Some(files) = &self.files {
            let Some(pending) = files.loaded.pending_commit.as_ref() else {
                let reason = "commit completion has no durable pending intent".to_string();
                self.poison(reason.clone());
                return Err(SignerError::Conflict(reason));
            };
            if pending.digest != commit_intent_digest {
                let reason =
                    "commit completion does not match the durable pending intent".to_string();
                self.poison(reason.clone());
                return Err(SignerError::Conflict(reason));
            }
            if pending.decided_value_id.0 != durable_store_readback {
                let reason =
                    "durable store readback is not the value authorized by the commit certificate"
                        .to_string();
                self.poison(reason.clone());
                return Err(SignerError::Conflict(reason));
            }
        }
        let next_epoch = (|| -> Result<SignerEpochBinding, SignerError> {
            let active_len = active_roster_len(next_roster);
            let roster_hash = canonical_roster_hash(next_roster)?;
            let roster_index =
                roster_i_from_pub_key(&next_roster[..active_len], self.epoch.public_key)
                    .map(|index| index.try_into().unwrap())
                    .unwrap_or(u32::MAX);
            Ok(SignerEpochBinding {
                public_key: self.epoch.public_key,
                chain_id: self.epoch.chain_id,
                height: self
                    .epoch
                    .height
                    .checked_add(1)
                    .ok_or_else(|| SignerError::Integrity("signer height overflow".into()))?,
                parent_commit: durable_store_readback,
                vote_namespace: next_vote_namespace,
                consensus_config_hash: self.epoch.consensus_config_hash,
                roster_hash,
                roster_index,
                active_roster_len: active_len
                    .try_into()
                    .map_err(|_| SignerError::Integrity("active roster too large".into()))?,
            })
        })();
        let next_epoch = match next_epoch {
            Ok(next_epoch) => next_epoch,
            Err(error) => {
                self.status = SignerStatus::ReconciliationRequired(
                    commit_intent_digest,
                    format!("could not derive the exact successor epoch: {error}"),
                );
                return Err(error);
            }
        };
        let mut applied = Vec::with_capacity(32 + encode_epoch(&next_epoch, true).len());
        applied.extend_from_slice(&commit_intent_digest);
        applied.extend_from_slice(&encode_epoch(&next_epoch, true));
        if let Some(files) = &mut self.files {
            if let Err(error) = files.append(RECORD_COMMIT_APPLIED, &applied) {
                self.status = SignerStatus::ReconciliationRequired(
                    commit_intent_digest,
                    format!("commit-applied durability failure: {error}"),
                );
                return Err(error);
            }
            #[cfg(test)]
            if let Err(error) = files.fail_if(WalFailpoint::AfterCommitApplied) {
                self.status = SignerStatus::ReconciliationRequired(
                    commit_intent_digest,
                    format!("injected crash after commit-applied durability: {error}"),
                );
                return Err(error);
            }
            if let Err(error) = files.append(RECORD_EPOCH, &encode_epoch(&next_epoch, true)) {
                self.status = SignerStatus::ReconciliationRequired(
                    commit_intent_digest,
                    format!("next-epoch durability failure: {error}"),
                );
                return Err(error);
            }
        }
        self.epoch = next_epoch;
        self.pending_commit = None;
        self.intents.clear();
        self.transition = None;
        self.status = if self.epoch.roster_index < self.epoch.active_roster_len {
            SignerStatus::Active
        } else {
            SignerStatus::ObserverOnly(
                "signing key is not in the active epoch roster".into(),
            )
        };
        Ok(())
    }

    pub(super) fn replay_intents(&self) -> Vec<SignedIntent> {
        self.intents.values().cloned().collect()
    }

    pub fn durable_transition(&self) -> Option<&LockValidTransition> {
        self.transition.as_ref()
    }

    #[cfg(test)]
    pub(super) fn append_legacy_pending_commit_for_test(
        &mut self,
        decided_value_id: ValueId,
        proposal: &BlockValue,
        certificate: &[u8],
    ) -> Result<[u8; 32], SignerError> {
        self.require_active()?;
        let payload = encode_legacy_commit_intent(
            decided_value_id,
            &proposal.0,
            certificate,
        )?;
        let digest: [u8; 32] = blake3::hash(&payload).into();
        self.files
            .as_mut()
            .ok_or_else(|| SignerError::Integrity("test requires a durable WAL".into()))?
            .append(RECORD_COMMIT_INTENT, &payload)?;
        self.pending_commit = Some(PendingCommit {
            digest,
            decided_value_id,
            proposal: proposal.0.clone(),
            certificate: certificate.to_vec(),
            proposal_evidence: PendingProposalEvidence::LegacyUnavailable,
        });
        Ok(digest)
    }

    #[cfg(test)]
    pub(super) fn set_failpoint(&mut self, point: WalFailpoint) {
        self.files
            .as_mut()
            .expect("durable signer required for WAL failpoint")
            .failpoint = Some(point);
    }
}
