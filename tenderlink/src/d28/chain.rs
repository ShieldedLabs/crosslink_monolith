//! D28's boundary to the existing chain vote and fat-pointer formats.
//!
//! The host supplies an agreed, height-specific roster in active-chain order
//! and its namespace. PV/PC signatures bind that namespace, not D28's config
//! hash or signer index; the
//! chain contract cannot distinguish configs with the same namespace and keys.
//! ENTER and proposal headers remain config-bound internal D28 messages.
use super::*;
use zcash_primitives::bft::{
    BftBlock, Blake3Hash, FAT_POINTER_SIGNATURES_MAX, FatPointerSignature, FatPointerToBftBlock,
    PubKeyID, Vote as ChainVote,
};

/// The chain's high round bit encodes PRECOMMIT, leaving 31 round bits.
pub const MAX_ROUND: Round = 0x7fff_ffff;

pub(super) fn vote_bytes(config: &Config, content: VoteContent) -> Result<[u8; 76]> {
    config.check_round(content.round)?;
    let round = i32::try_from(content.round).map_err(|_| Error::Exhausted)?;
    if content.kind == VoteKind::Enter || content.id == Some([0; 32]) {
        return Err(Error::Invalid);
    }
    let member = config
        .members
        .get(content.signer as usize)
        .ok_or(Error::Invalid)?;
    Ok(ChainVote {
        validator_address: PubKeyID(member.public_key),
        value: Blake3Hash(content.id.unwrap_or([0; 32])),
        height: content.height,
        typ: content.kind == VoteKind::Precommit,
        round,
    }
    .to_bytes())
}

/// Export the exact PRECOMMIT signatures already accepted by D28, without
/// signing after decision or changing their order. This checks a canonical BFT
/// body, its height and keyed ID, the internal header, and a distinct weighted
/// quorum in active-roster order (as published by the core). Unordered input
/// is rejected rather than silently reordered: the chain requires this order.
/// The host still supplies chain-state validity (PoW, ledger, roster provenance);
/// this boundary does not revalidate those external rules.
pub fn decision_fat_pointer(
    config: &Config,
    height: u64,
    proposal: Proposal<'_>,
    certificate: Group<'_>,
) -> Result<FatPointerToBftBlock> {
    if !config.is_crosslink() {
        return Err(Error::Config);
    }
    let p = proposal.header.content;
    if proposal.body.len() > config.max_body
        || p.height != height
        || !proposal.header.verify(config)
        || config.body_id(proposal.body) != p.id
        || certificate.kind
            != (CertKind::Decision {
                round: p.round,
                id: p.id,
            })
        || certificate.votes.storage_len() > config.members.len()
        || certificate.votes.is_empty()
        || certificate.votes.len() > FAT_POINTER_SIGNATURES_MAX
    {
        return Err(Error::Invalid);
    }
    let mut remaining = proposal.body;
    let block = BftBlock::zcash_deserialize(&mut remaining).map_err(|_| Error::Invalid)?;
    if !remaining.is_empty()
        || u64::from(block.height) != height
        || block.zcash_serialize_to_vec().map_err(|_| Error::Invalid)? != proposal.body
    {
        return Err(Error::Invalid);
    }

    let mut weight = 0u64;
    let mut signatures = Vec::with_capacity(certificate.votes.len());
    let mut previous_signer = None;
    for vote in certificate.votes.iter() {
        let v = vote.content;
        if v.height != height
            || v.round != p.round
            || v.kind != VoteKind::Precommit
            || v.id != Some(p.id)
            || previous_signer.is_some_and(|old| old >= v.signer)
            || !vote.verify(config)
        {
            return Err(Error::Invalid);
        }
        previous_signer = Some(v.signer);
        let member = &config.members[v.signer as usize]; // verified membership
        weight = weight.checked_add(member.weight).ok_or(Error::Invalid)?;
        signatures.push(FatPointerSignature {
            pub_key: PubKeyID(member.public_key),
            vote_signature: vote.signature,
        });
    }
    if !config.quorum(weight) {
        return Err(Error::Invalid);
    }
    let round = u32::try_from(p.round).map_err(|_| Error::Invalid)?;
    Ok(FatPointerToBftBlock::from_parts(
        Blake3Hash(p.id),
        height,
        round,
        &signatures,
    ))
}
