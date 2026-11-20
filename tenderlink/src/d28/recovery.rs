use super::*;
impl<S: Storage> Engine<S> {
    /// Validate scalar publication metadata before dereferencing a physical
    /// record or allowing any signing continuation. A torn/unpublished staging
    /// record is irrelevant; a malformed *published* record fails closed.
    pub(super) fn validate_descriptor(&self) -> Result<()> {
        let d = self.d();
        if d.locked_round.is_some() != d.locked_id.is_some()
            || d.locked_round.is_some_and(|r| r > d.round)
            || d.valid_round.is_some_and(|r| r > d.round)
            || d.locked_round
                .is_some_and(|r| d.valid_round.is_none_or(|v| v < r))
            || (d.justified && d.slot.is_none())
            || (d.proposal_published && d.proposal_intent.is_none())
        {
            return Err(Error::Corrupt);
        }
        if d.step == Step::PreRound
            && (d.round != 0
                || d.entry.is_some()
                || d.vc.is_some()
                || d.locked_round.is_some()
                || d.proposal_intent.is_some()
                || d.intents.iter().any(Option::is_some)
                || d.timers.iter().any(|t| t.active))
        {
            return Err(Error::Corrupt);
        }
        for (index, intent) in d.intents.iter().enumerate() {
            if let Some(v) = intent {
                if self.me == usize::MAX
                    || v.signer as usize != self.me
                    || v.height != d.height
                    || v.round != d.round
                    || v.kind.index() != index
                    || (index == 0 && v.id.is_some())
                {
                    return Err(Error::Corrupt);
                }
                if index == 2
                    && v.id.is_some()
                    && (d.locked_round != Some(d.round)
                        || v.id != d.locked_id
                        || d.valid_round != Some(d.round)
                        || d.vc.is_none()
                        || d.slot.is_none()
                        || !matches!(d.step, Step::Precommit | Step::Decided))
                {
                    return Err(Error::Corrupt);
                }
            }
        }
        for k in [VoteKind::Enter, VoteKind::Prevote, VoteKind::Precommit] {
            if let Some(v) = self.own_current(k) {
                if !v.verify(self.c())
                    || v.content.signer as usize != self.me
                    || d.intents[k.index()] != Some(v.content)
                {
                    return Err(Error::Corrupt);
                }
            }
        }
        if let Some(i) = d.proposal_intent {
            let p = i.content;
            if i.slot >= 3
                || p.height != d.height
                || p.round != d.round
                || p.signer as usize != self.me
                || self.me != self.c().proposer(d.height, d.round)
                || p.valid_round.is_some_and(|r| r >= p.round)
            {
                return Err(Error::Corrupt);
            }
            if !d.proposal_published
                && (self.store.body(i.slot).len() > self.c().max_body
                    || body_id(self.store.body(i.slot)) != p.id)
            {
                return Err(Error::Corrupt);
            }
            if let Some(h) = self
                .store
                .own_header()
                .filter(|h| h.content.height == d.height && h.content.round == d.round)
            {
                if h.content != p || !h.verify(self.c()) {
                    return Err(Error::Corrupt);
                }
            }
        } else if self
            .store
            .own_header()
            .is_some_and(|h| h.content.height == d.height && h.content.round == d.round)
        {
            return Err(Error::Corrupt);
        }
        for s in [
            d.slot,
            d.vc.map(|v| v.proposal),
            d.pending.map(|p| p.proposal),
        ]
        .into_iter()
        .flatten()
        {
            if s.index >= 3 {
                return Err(Error::Corrupt);
            }
        }
        for r in [
            d.entry,
            d.vc.map(|v| v.certificate),
            d.pending.map(|p| p.certificate),
        ]
        .into_iter()
        .flatten()
        {
            if r.index >= 4 {
                return Err(Error::Corrupt);
            }
        }
        if let Some(r) = d.entry {
            let header = self.store.region_header(r.index).ok_or(Error::Corrupt)?;
            let matching = match header.kind {
                CertKind::Timeout { round } => round.checked_add(1) == Some(d.round),
                CertKind::Jump { round } => round == d.round,
                _ => false,
            };
            if !matching || d.round == 0 {
                return Err(Error::Corrupt);
            }
        } else if d.round != 0 {
            return Err(Error::Corrupt);
        }
        if d.step == Step::Decided {
            let p = d.pending.ok_or(Error::Corrupt)?;
            if d.statuses[p.proposal.index] != Status::Valid {
                return Err(Error::Corrupt);
            }
        }
        Ok(())
    }
}
