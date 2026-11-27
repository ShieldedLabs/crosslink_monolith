use super::*;
impl<S: Storage> Engine<S> {
    pub(super) fn group_valid(&self, g: Group<'_>) -> bool {
        self.group_valid_with_outer(g, None)
    }
    // Only a fully verified, current-frame outer vote may supply this shortcut.
    // Equality includes the signature bytes; group kind, distinctness and weight
    // are still checked independently. Recovery has no verified outer to reuse.
    fn group_valid_with_outer(&self, g: Group<'_>, verified_outer: Option<&Vote>) -> bool {
        let n = self.c().members.len();
        if g.votes.is_empty() || g.votes.len() > n {
            return false;
        }
        let mut weight = 0;
        for (i, v) in g.votes.iter().enumerate() {
            let c = v.content;
            if c.height != self.d().height
                || !(verified_outer == Some(v) || v.verify(self.c()))
                || g.votes.iter().take(i).any(|x| x.content.signer == c.signer)
            {
                return false;
            }
            let matches = match g.kind {
                CertKind::Polka { round, id } => {
                    c.kind == VoteKind::Prevote && c.round == round && c.id == Some(id)
                }
                CertKind::Decision { round, id } => {
                    c.kind == VoteKind::Precommit && c.round == round && c.id == Some(id)
                }
                CertKind::Timeout { round } => c.kind == VoteKind::Precommit && c.round == round,
                CertKind::Jump { round } => c.round >= round,
                CertKind::Row { round, kind } => {
                    kind != VoteKind::Enter && c.round == round && c.kind == kind
                }
            };
            if !matches {
                return false;
            }
            weight += self.c().members[c.signer as usize].weight;
        }
        match g.kind {
            CertKind::Polka { .. } | CertKind::Decision { .. } | CertKind::Timeout { .. } => {
                self.c().quorum(weight)
            }
            CertKind::Jump { .. } => self.c().witness(weight),
            CertKind::Row { .. } => true,
        }
    }
    pub(super) fn accepted_vote<'a>(&self, f: Frame<'a>, position: usize) -> Option<&'a Vote> {
        let &(group, row) = self.rx_positions.get(position)?;
        if position == 0 && f.outer.is_some() {
            return f.outer.filter(|_| self.outer_ok);
        }
        if !self.group_ok[group] {
            return None;
        }
        match f.groups[group].votes {
            Votes::Dense(votes) => votes.get(row),
            Votes::Rows(votes) => votes.get(row).and_then(Option::as_ref),
        }
    }
    fn positions(f: Frame<'_>) -> usize {
        usize::from(f.outer.is_some()) + f.groups.iter().map(|g| g.votes.len()).sum::<usize>()
    }
    pub(super) fn clear_rx(&mut self) {
        self.rx_positions.clear();
        self.rx_weights.clear();
        self.outer_ok = false;
        self.group_ok.fill(false);
        self.selected.fill(None);
    }
    // Called only after admission. Each signature is checked at most once;
    // physical coordinates make later accepted-vote reads O(1), even for Rows.
    pub(super) fn index_frame(&mut self, f: Frame<'_>) {
        self.clear_rx();
        self.outer_ok = f
            .outer
            .is_some_and(|v| v.content.height == self.d().height && v.verify(self.c()));
        if f.outer.is_some() {
            self.rx_positions.push((0, 0));
        }
        for (group, g) in f.groups.iter().enumerate() {
            self.group_ok[group] =
                self.group_valid_with_outer(*g, f.outer.filter(|_| self.outer_ok));
            match g.votes {
                Votes::Dense(votes) => {
                    self.rx_positions
                        .extend((0..votes.len()).map(|row| (group, row)));
                }
                Votes::Rows(votes) => {
                    self.rx_positions.extend(
                        votes
                            .iter()
                            .enumerate()
                            .filter_map(|(row, v)| v.is_some().then_some((group, row))),
                    );
                }
            }
        }
        self.rx_weights.resize(self.rx_positions.len(), 0);
    }
    // P <= N+1 admitted positions. Lazily cache a conflicting vote's tuple
    // weight in O(N+P) work; all repairs together take O(P*(N+P)) worst case.
    // Common nonconflicting frames need no weight scan. Distinctness is per
    // (round, kind, id), NOT per frame: equivocations may contribute a signer
    // to several different tuple weights. Zero means uncached (weights >= 1).
    pub(super) fn rx_weight(&mut self, f: Frame<'_>, position: usize) -> u64 {
        if self.rx_weights[position] != 0 {
            return self.rx_weights[position];
        }
        let c = self
            .accepted_vote(f, position)
            .expect("accepted RX vote")
            .content;
        let mut weight = 0;
        self.selected.fill(None);
        for i in 0..self.rx_positions.len() {
            if let Some(v) = self.accepted_vote(f, i) {
                let x = v.content;
                if (x.round, x.kind, x.id) == (c.round, c.kind, c.id) {
                    let signer = x.signer as usize;
                    if self.selected[signer].is_none() {
                        self.selected[signer] = Some(i);
                        weight += self.c().members[signer].weight;
                    }
                }
            }
        }
        for i in 0..self.rx_positions.len() {
            if self.accepted_vote(f, i).is_some_and(|v| {
                let x = v.content;
                (x.round, x.kind, x.id) == (c.round, c.kind, c.id)
            }) {
                self.rx_weights[i] = weight;
            }
        }
        weight
    }
    // Reuse the jump/weight scratch only after jump staging has finished. Keep
    // the FIRST matching RX precommit, and always prefer the retained PC row.
    pub(super) fn index_timeout(&mut self, f: Frame<'_>) {
        self.selected.fill(None);
        for position in 0..self.rx_positions.len() {
            if let Some(v) = self.accepted_vote(f, position) {
                let c = v.content;
                if c.round == self.d().round && c.kind == VoteKind::Precommit {
                    let selected = &mut self.selected[c.signer as usize];
                    if selected.is_none() {
                        *selected = Some(position);
                    }
                }
            }
        }
    }
    pub(super) fn timeout_vote(&self, f: Frame<'_>, signer: usize) -> Option<Vote> {
        self.pc[signer]
            .or_else(|| self.selected[signer].and_then(|i| self.accepted_vote(f, i).copied()))
    }
    fn timeout_weight(&self, f: Frame<'_>) -> u64 {
        (0..self.c().members.len())
            .filter(|i| self.timeout_vote(f, *i).is_some())
            .map(|i| self.c().members[i].weight)
            .sum()
    }
    pub fn receive(
        &mut self,
        frame: Frame<'_>,
        status: Status,
        sink: &mut impl Sink,
    ) -> Result<()> {
        self.receive_body(frame, status, None, sink)
    }
    /// Runtime-only ingress for immutable bytes whose configured hash was
    /// checked at assembly. The token is not header/certificate or validity
    /// authority: all other receive checks run, with Indeterminate validity.
    pub(super) fn receive_checked_body(
        &mut self,
        frame: Frame<'_>,
        body: &checked_body::CheckedBody,
        sink: &mut impl Sink,
    ) -> Result<()> {
        // Never trust the frame's separate body slice, even if the token matches
        // its header. Only the exact immutable allocation checked by the token
        // may take this path; no token is persisted or reused across recovery.
        let frame = Frame {
            proposal: frame.proposal.map(|p| Proposal {
                header: p.header,
                body: body.bytes(),
            }),
            ..frame
        };
        self.receive_body(frame, Status::Indeterminate, Some(body), sink)
    }
    fn receive_body(
        &mut self,
        frame: Frame<'_>,
        status: Status,
        checked: Option<&checked_body::CheckedBody>,
        sink: &mut impl Sink,
    ) -> Result<()> {
        let r = self
            .receive_inner(frame, status, checked, sink)
            .and_then(|()| {
                // A completed retained body must survive even when its validity is
                // pending and Guided mode emits no proposal or signer output.
                if frame.proposal.is_some() {
                    self.store.flush_publication()?;
                }
                Ok(())
            });
        self.clear_rx();
        self.finish(r)
    }
    fn receive_inner(
        &mut self,
        f: Frame<'_>,
        status: Status,
        checked: Option<&checked_body::CheckedBody>,
        sink: &mut impl Sink,
    ) -> Result<()> {
        self.check()?;
        let n = self.c().members.len();
        if f.groups.len() > n + 1
            || f.groups.iter().any(|g| g.votes.storage_len() > n)
            || Self::positions(f) > n + 1
            || f.proposal.is_some_and(|p| p.body.len() > self.c().max_body)
        {
            return Err(Error::Invalid);
        }
        if let Some(p) = f.proposal {
            if p.header.content.height != self.d().height || !p.header.verify(self.c()) {
                return Err(Error::Invalid);
            }
            if let Some(body) = checked {
                if !body.matches(self.c(), self.d().height, p.header.content.id) {
                    return Err(Error::Invalid);
                }
            } else {
                #[cfg(test)]
                checked_body::record_hash(p.body.len());
                if self.c().body_id(p.body) != p.header.content.id {
                    return Err(Error::Invalid);
                }
            }
        } else if checked.is_some() {
            return Err(Error::Invalid);
        }
        self.index_frame(f);
        if f.message == Message::Dec {
            if let Some(p) = f.proposal {
                for (index, g) in f.groups.iter().enumerate() {
                    if g.kind
                        == (CertKind::Decision {
                            round: p.header.content.round,
                            id: p.header.content.id,
                        })
                        && self.group_ok[index]
                    {
                        if self.d().step != Step::Decided {
                            self.pin(p, *g, status)?;
                            self.drive(sink)?;
                        }
                        return Ok(());
                    }
                }
            }
            return Err(Error::Invalid);
        }
        if self.d().step == Step::Decided {
            return Ok(());
        }
        // A verified fresh copy can resolve retained evidence even when its
        // round/header is not eligible to replace the current proposal slot.
        if let Some(p) = f.proposal {
            self.resolve_id(p.header.content.id, status)?;
        }
        if self.d().step != Step::PreRound {
            self.jump(f, sink)?;
        }
        // Independent complete groups repair rows, but never own signing bytes.
        for position in 0..self.rx_positions.len() {
            let Some(v) = self.accepted_vote(f, position).copied() else {
                continue;
            };
            if v.content.round != self.d().round || v.content.kind == VoteKind::Enter {
                continue;
            }
            let signer = v.content.signer as usize;
            if signer == self.me && self.own_current(v.content.kind).is_some() {
                continue;
            }
            let old = if v.content.kind == VoteKind::Prevote {
                self.pv[signer]
            } else {
                self.pc[signer]
            };
            if old.is_none_or(|old| old.content == v.content) || {
                let weight = self.rx_weight(f, position);
                self.c().quorum(weight)
            } {
                if v.content.kind == VoteKind::Prevote {
                    self.pv[signer] = Some(v);
                } else {
                    self.pc[signer] = Some(v);
                }
            }
        }
        if let Some(p) = f.proposal {
            if p.header.content.round == self.d().round {
                let id = p.header.content.id;
                let replace = self.d().slot.is_none_or(|old| {
                    let old_id = self.proposal(old).header.content.id;
                    old.header == p.header.identity(self.c())
                        || ((self
                            .c()
                            .quorum(self.row_weight(VoteKind::Prevote, Some(Some(id))))
                            || self
                                .c()
                                .quorum(self.row_weight(VoteKind::Precommit, Some(Some(id)))))
                            && !(self
                                .c()
                                .quorum(self.row_weight(VoteKind::Prevote, Some(Some(old_id))))
                                || self.c().quorum(
                                    self.row_weight(VoteKind::Precommit, Some(Some(old_id))),
                                )))
                });
                if replace {
                    let exact = self
                        .d()
                        .slot
                        .is_some_and(|s| s.header == p.header.identity(self.c()));
                    if !exact {
                        let mut d = self.d().clone();
                        d.slot = None;
                        d.justified = false;
                        self.commit(d)?;
                        let s = self.install(p, status)?;
                        let mut d = self.d().clone();
                        d.slot = Some(s);
                        d.justified = false;
                        self.commit(d)?;
                    } else {
                        if status == Status::Invalid {
                            if let Some(slot) = self.d().slot {
                                if self.d().statuses[slot.index] == Status::Indeterminate {
                                    let mut d = self.d().clone();
                                    d.statuses[slot.index] = status;
                                    self.commit(d)?;
                                }
                            }
                        }
                        self.resolve_id(id, status)?;
                    }
                    if p.header.content.valid_round.is_some_and(|vr| {
                        f.groups.iter().enumerate().any(|(i, g)| {
                            g.kind == (CertKind::Polka { round: vr, id }) && self.group_ok[i]
                        })
                    }) {
                        let mut d = self.d().clone();
                        d.justified = true;
                        self.commit(d)?;
                    }
                }
            }
        }
        self.drive(sink)?;
        if self.d().step != Step::PreRound
            && self.d().step != Step::Decided
            && self.d().timers[3].active
            && self.d().timers[3].remaining == 0
        {
            // drive may have entered a new round; select timeout evidence for
            // the round as it stands now, only when its timer needs evidence.
            self.index_timeout(f);
            if self.c().quorum(self.timeout_weight(f)) {
                let r = self.d().round;
                let entry =
                    self.stage_selected(CertKind::Timeout { round: r }, Selection::Timeout(f))?;
                self.start(
                    r.checked_add(1).ok_or(Error::Exhausted)?,
                    Some(entry),
                    false,
                    sink,
                )?;
            }
        }
        if !matches!(self.d().step, Step::PreRound | Step::Decided) {
            if let Some(v) = f.outer {
                if self.emission != Emission::Guided
                    && v.content.kind == VoteKind::Enter
                    && self.current(v)
                    && self.outer_ok
                    && v.content.signer as usize != self.me
                    && !self.replied[v.content.signer as usize]
                    && self.d().slot.is_some()
                    && self.c().proposer(self.d().height, self.d().round) == self.me
                {
                    self.emit_proposal(Target::Peer(v.content.signer), sink)?;
                    self.replied[v.content.signer as usize] = true;
                }
            }
        }
        self.drive(sink)
    }
    fn jump(&mut self, f: Frame<'_>, sink: &mut impl Sink) -> Result<()> {
        self.selected.fill(None);
        let current = self.d().round;
        for position in 0..self.rx_positions.len() {
            let Some(v) = self.accepted_vote(f, position) else {
                continue;
            };
            let c = v.content;
            let signer = c.signer as usize;
            if signer == self.me || c.round <= current {
                continue;
            }
            let better = self.selected[signer].is_none_or(|old| {
                let old = self.accepted_vote(f, old).unwrap().content;
                (c.round, c.kind) > (old.round, old.kind)
                    || ((c.round, c.kind) == (old.round, old.kind) && c.id < old.id)
            });
            if better {
                self.selected[signer] = Some(position);
            }
        }
        let mut target = None;
        for position in self.selected.iter().flatten() {
            let round = self.accepted_vote(f, *position).unwrap().content.round;
            let weight = self
                .selected
                .iter()
                .enumerate()
                .filter(|(_, p)| {
                    p.is_some_and(|p| self.accepted_vote(f, p).unwrap().content.round >= round)
                })
                .map(|(s, _)| self.c().members[s].weight)
                .sum();
            if self.c().witness(weight) && target.is_none_or(|r| round > r) {
                target = Some(round);
            }
        }
        if let Some(round) = target {
            for signer in 0..self.selected.len() {
                if self.selected[signer]
                    .is_some_and(|i| self.accepted_vote(f, i).unwrap().content.round < round)
                {
                    self.selected[signer] = None;
                }
            }
            let entry = self.stage_selected(CertKind::Jump { round }, Selection::Frame(f))?;
            self.start(round, Some(entry), false, sink)?;
        }
        Ok(())
    }
    fn install(&mut self, p: Proposal<'_>, status: Status) -> Result<SlotRef> {
        let identity = p.header.identity(self.c());
        for i in 0..3 {
            if self.slot_used(i) && self.store.header(i) == Some(p.header) {
                self.resolve_id(p.header.content.id, status)?;
                return Ok(SlotRef {
                    index: i,
                    header: identity,
                });
            }
        }
        let index = self.slot_free()?;
        self.store.write_body(index, p.body)?;
        self.store.write_header(index, *p.header)?;
        let mut d = self.d().clone();
        d.statuses[index] = status;
        d.body_generations[index] = d.next_generation;
        d.next_generation = d.next_generation.checked_add(1).ok_or(Error::Exhausted)?;
        self.commit(d)?;
        Ok(SlotRef {
            index,
            header: identity,
        })
    }
    fn pin(&mut self, p: Proposal<'_>, g: Group<'_>, status: Status) -> Result<()> {
        if self.d().pending.is_some() {
            self.resolve_id(p.header.content.id, status)?;
            return Ok(());
        }
        let slot = self.install(p, status)?;
        let region = self.stage_selected(g.kind, Selection::Group(g))?;
        let mut d = self.d().clone();
        d.pending = Some(Certified {
            proposal: slot,
            certificate: region,
        });
        self.commit(d)
    }
    fn resolve_id(&mut self, id: Id, status: Status) -> Result<()> {
        if status != Status::Valid {
            return Ok(());
        }
        let mut d = self.d().clone();
        let mut changed = false;
        for s in [
            d.slot,
            d.vc.map(|v| v.proposal),
            d.pending.map(|p| p.proposal),
        ]
        .into_iter()
        .flatten()
        {
            if self.proposal(s).header.content.id == id && d.statuses[s.index] != Status::Valid {
                d.statuses[s.index] = Status::Valid;
                changed = true;
            }
        }
        if changed {
            self.commit(d)?;
        }
        Ok(())
    }
    pub fn body_tag(&self, slot: SlotRef) -> BodyTag {
        BodyTag {
            height: self.d().height,
            header: slot.header,
            generation: self.d().body_generations[slot.index],
        }
    }
    pub fn validity(&mut self, tag: BodyTag, status: Status, sink: &mut impl Sink) -> Result<()> {
        let r = self.validity_inner(tag, status, sink);
        self.finish(r)
    }
    fn validity_inner(&mut self, tag: BodyTag, status: Status, sink: &mut impl Sink) -> Result<()> {
        self.check()?;
        if tag.height != self.d().height {
            return Err(Error::Stale);
        }
        let found = [self.d().slot, self.d().pending.map(|p| p.proposal)]
            .into_iter()
            .flatten()
            .find(|s| {
                s.header == tag.header && self.d().body_generations[s.index] == tag.generation
            })
            .ok_or(Error::Stale)?;
        let old = self.d().statuses[found.index];
        if old != Status::Indeterminate && status != old {
            return Err(Error::Invalid);
        }
        let mut d = self.d().clone();
        d.statuses[found.index] = status;
        self.commit(d)?;
        if status == Status::Valid {
            let id = self.proposal(found).header.content.id;
            self.resolve_id(id, status)?;
        }
        self.drive(sink)
    }
    fn arm(&mut self, index: usize) -> Result<bool> {
        if self.d().timers[index].active {
            return Ok(false);
        }
        let mut d = self.d().clone();
        d.timers[index] = Timer {
            active: true,
            remaining: self.c().timeouts.at(index, d.round)?,
            height: d.height,
            round: d.round,
        };
        self.commit(d)?;
        Ok(true)
    }
    fn expired(&self, index: usize) -> bool {
        self.d().timers[index].active && self.d().timers[index].remaining == 0
    }
    fn decide_output(&mut self, sink: &mut impl Sink) -> Result<()> {
        self.publication_checkpoint()?;
        let p = self.d().pending.expect("decided package");
        sink.decided(
            self.d().height,
            self.proposal(p.proposal),
            self.group(p.certificate),
        );
        Ok(())
    }
    pub(super) fn drive(&mut self, sink: &mut impl Sink) -> Result<()> {
        loop {
            if self.d().step == Step::Decided {
                return Ok(());
            }
            if let Some(p) = self.d().pending {
                if self.d().statuses[p.proposal.index] == Status::Valid {
                    let mut d = self.d().clone();
                    d.step = Step::Decided;
                    self.commit(d)?;
                    self.acquiring = false;
                    self.decide_output(sink)?;
                    return Ok(());
                }
            }
            if let Some(s) = self.d().slot {
                let id = self.proposal(s).header.content.id;
                if self
                    .c()
                    .quorum(self.row_weight(VoteKind::Precommit, Some(Some(id))))
                    && self.d().pending.is_none()
                {
                    let region = self.stage_selected(
                        CertKind::Decision {
                            round: self.d().round,
                            id,
                        },
                        Selection::Pc(Some(Some(id))),
                    )?;
                    let mut d = self.d().clone();
                    d.pending = Some(Certified {
                        proposal: s,
                        certificate: region,
                    });
                    self.commit(d)?;
                    continue;
                }
            }
            if self.d().step == Step::PreRound {
                if self.d().height_remaining == 0 {
                    self.start(0, None, true, sink)?;
                    continue;
                }
                return Ok(());
            }
            self.request_proposal(sink)?;
            if self.d().step == Step::Propose
                && self.d().vc.is_some()
                && self.c().proposer(self.d().height, self.d().round) == self.me
                && self.d().proposal_intent.is_none()
                && self.own_current(VoteKind::Prevote).is_none()
            {
                let tag = ProposalTag {
                    height: self.d().height,
                    round: self.d().round,
                };
                self.supply_inner(tag, &[], Status::Valid, sink)?;
                continue;
            }
            let step = self.d().step;
            if step == Step::Propose {
                if self.c().quorum(self.row_weight(VoteKind::Prevote, None)) {
                    self.arm(1)?;
                }
                let choice = self.d().slot.and_then(|s| {
                    let p = self.proposal(s).header.content;
                    match self.d().statuses[s.index] {
                        Status::Indeterminate => None,
                        Status::Invalid => Some(None),
                        Status::Valid => {
                            if p.valid_round.is_some() && !self.d().justified {
                                return None;
                            }
                            let allowed = if let Some(vr) = p.valid_round {
                                (self.d().locked_round.is_none_or(|lr| lr <= vr)
                                    || self.d().locked_id == Some(p.id))
                            } else {
                                self.d().locked_round.is_none() || self.d().locked_id == Some(p.id)
                            };
                            Some(allowed.then_some(p.id))
                        }
                    }
                });
                if let Some(id) = choice {
                    self.vote(VoteKind::Prevote, id, sink)?;
                    continue;
                }
                if self.expired(0) || self.expired(1) {
                    self.vote(VoteKind::Prevote, None, sink)?;
                    continue;
                }
            }
            if matches!(step, Step::Prevote | Step::Precommit) {
                if let Some(s) = self.d().slot {
                    let id = self.proposal(s).header.content.id;
                    if self.d().statuses[s.index] == Status::Valid
                        && self
                            .c()
                            .quorum(self.row_weight(VoteKind::Prevote, Some(Some(id))))
                        && self.d().valid_round != Some(self.d().round)
                    {
                        let region = self.stage_selected(
                            CertKind::Polka {
                                round: self.d().round,
                                id,
                            },
                            Selection::Pv(Some(id)),
                        )?;
                        let mut d = self.d().clone();
                        d.vc = Some(Certified {
                            proposal: s,
                            certificate: region,
                        });
                        d.valid_round = Some(d.round);
                        if step == Step::Prevote {
                            d.locked_round = Some(d.round);
                            d.locked_id = Some(id);
                            d.step = Step::Precommit;
                            if self.me != usize::MAX {
                                d.intents[2] = Some(VoteContent {
                                    height: d.height,
                                    round: d.round,
                                    kind: VoteKind::Precommit,
                                    id: Some(id),
                                    signer: self.me as u32,
                                });
                            }
                        }
                        self.commit(d)?;
                        if step == Step::Prevote {
                            self.materialize(VoteKind::Precommit)?;
                            self.emit_vote(VoteKind::Precommit, Target::All, sink)?;
                        }
                        continue;
                    }
                }
            }
            if step == Step::Prevote {
                if self.c().quorum(self.row_weight(VoteKind::Prevote, None)) && self.arm(2)? {
                    self.emit_vote(VoteKind::Prevote, Target::All, sink)?;
                }
                if self
                    .c()
                    .quorum(self.row_weight(VoteKind::Prevote, Some(None)))
                    || self.expired(2)
                {
                    self.vote(VoteKind::Precommit, None, sink)?;
                    continue;
                }
            }
            if self.c().quorum(self.row_weight(VoteKind::Precommit, None)) {
                self.arm(3)?;
            }
            if self.expired(3) && self.c().quorum(self.row_weight(VoteKind::Precommit, None)) {
                let r = self.d().round;
                let region =
                    self.stage_selected(CertKind::Timeout { round: r }, Selection::Pc(None))?;
                self.start(
                    r.checked_add(1).ok_or(Error::Exhausted)?,
                    Some(region),
                    false,
                    sink,
                )?;
                continue;
            }
            return Ok(());
        }
    }
    fn periodic(&mut self, sink: &mut impl Sink) -> Result<()> {
        self.publication_checkpoint()?;
        if self.d().step == Step::Decided {
            if self.emission == Emission::Guided {
                return Ok(());
            }
            let p = self.d().pending.unwrap();
            let groups = [self.group(p.certificate)];
            sink.send(
                Target::All,
                Frame {
                    message: Message::Dec,
                    outer: None,
                    proposal: Some(self.proposal(p.proposal)),
                    groups: &groups,
                },
            );
            return Ok(());
        }
        if self.d().step != Step::PreRound {
            self.emit_replay(VoteKind::Enter, sink)?;
            self.emit_proposal(Target::All, sink)?;
            self.emit_replay(VoteKind::Prevote, sink)?;
            self.emit_replay(VoteKind::Precommit, sink)?;
        }
        for (i, s) in [self.d().slot, self.d().pending.map(|p| p.proposal)]
            .into_iter()
            .enumerate()
        {
            if let Some(s) = s {
                if self.d().statuses[s.index] == Status::Indeterminate
                    && !(i == 1 && self.d().slot == Some(s))
                {
                    sink.retry(self.body_tag(s), self.proposal(s));
                }
            }
        }
        Ok(())
    }
    pub fn elapse(&mut self, online: u64, sink: &mut impl Sink) -> Result<()> {
        let r = self.elapse_inner(online, sink);
        self.finish(r)
    }
    /// Runtime-only elapsed time for a Guided sink that retains the latest own
    /// vote of each kind and deduplicates body retries until the next exchange.
    /// Consume all online time at every consensus deadline, but collapse repeated
    /// rho-only replays between deadlines. Publication/signing writes still run
    /// synchronously; the public reference `elapse` keeps every individual tick.
    pub(super) fn elapse_coalesced(&mut self, online: u64, sink: &mut impl Sink) -> Result<()> {
        let r = self.elapse_coalesced_inner(online, sink);
        self.finish(r)
    }
    fn elapse_coalesced_inner(&mut self, mut online: u64, sink: &mut impl Sink) -> Result<()> {
        self.check()?;
        if self.emission != Emission::Guided {
            return Err(Error::Invalid);
        }
        self.drive(sink)?;
        while online > 0 {
            // Only consensus deadlines split the elapsed interval. In particular,
            // do not replay storage wall time once for each missed rho tick.
            let mut delta = online;
            if self.d().step == Step::PreRound {
                delta = delta.min(self.d().height_remaining);
            } else if self.d().step != Step::Decided {
                for t in self.d().timers {
                    if t.active && t.remaining > 0 {
                        delta = delta.min(t.remaining);
                    }
                }
            }
            let grid = self.d().grid_remaining;
            let crossed = delta >= grid;
            let remainder = if crossed {
                (delta - grid) % self.c().rho
            } else {
                0
            };
            let mut d = self.d().clone();
            // rho is nonzero. Subtract before taking the remainder, avoiding a
            // multiplication/addition overflow even for the full u64 interval.
            // grid == 0 is a valid reference crash cut awaiting its rho replay.
            d.grid_remaining = if crossed {
                self.c().rho - remainder
            } else {
                grid - delta
            };
            if d.step == Step::PreRound {
                d.height_remaining -= delta;
            } else if d.step != Step::Decided {
                for t in &mut d.timers {
                    if t.active {
                        t.remaining = t.remaining.saturating_sub(delta);
                    }
                }
            }
            self.commit_progress(d)?;
            online -= delta;
            // A strictly earlier grid tick belongs to the pre-deadline state.
            // Keep it even when another tick coincides with the deadline: a round
            // transition may retire a vote or retry without overwriting its event.
            if delta > grid {
                self.periodic(sink)?;
            }
            if self.d().step == Step::PreRound && self.d().height_remaining == 0 {
                self.start(0, None, true, sink)?;
            }
            self.drive(sink)?;
            // The reference drives consensus before a coincident rho replay.
            if crossed && remainder == 0 {
                self.periodic(sink)?;
            }
        }
        Ok(())
    }
    fn elapse_inner(&mut self, mut online: u64, sink: &mut impl Sink) -> Result<()> {
        self.check()?;
        self.drive(sink)?;
        while online > 0 {
            let mut delta = online.min(self.d().grid_remaining);
            if self.d().step == Step::PreRound {
                delta = delta.min(self.d().height_remaining);
            } else if self.d().step != Step::Decided {
                for t in self.d().timers {
                    if t.active && t.remaining > 0 {
                        delta = delta.min(t.remaining);
                    }
                }
            }
            let mut d = self.d().clone();
            d.grid_remaining = d.grid_remaining.saturating_sub(delta);
            if d.step == Step::PreRound {
                d.height_remaining = d.height_remaining.saturating_sub(delta);
            } else if d.step != Step::Decided {
                for t in &mut d.timers {
                    if t.active {
                        t.remaining = t.remaining.saturating_sub(delta);
                    }
                }
            }
            self.commit_progress(d)?;
            online -= delta;
            if self.d().step == Step::PreRound && self.d().height_remaining == 0 {
                self.start(0, None, true, sink)?;
            }
            self.drive(sink)?;
            if self.d().grid_remaining == 0 {
                let mut d = self.d().clone();
                d.grid_remaining = self.c().rho;
                self.commit_progress(d)?;
                self.periodic(sink)?;
            }
        }
        Ok(())
    }
    pub fn recover(store: S, key: SigningKey, sink: &mut impl Sink) -> Result<Self> {
        Self::recover_with_emission(store, key, Emission::Reference, sink)
    }
    /// Recover with the emission policy selected BEFORE any recovery output,
    /// so a guided node never leaks a complete frame while restarting.
    pub fn recover_with_emission(
        store: S,
        key: SigningKey,
        emission: Emission,
        sink: &mut impl Sink,
    ) -> Result<Self> {
        let mut e = Self::attach(store, key)?;
        e.emission = emission;
        let r = e.recover_inner(sink);
        e.finish(r)?;
        Ok(e)
    }
    fn recover_inner(&mut self, sink: &mut impl Sink) -> Result<()> {
        self.check()?;
        if self.d().config != self.c().hash {
            return Err(Error::Corrupt);
        }
        let c = self.c();
        if Config::build(
            c.domain,
            c.members.clone(),
            c.max_body,
            c.rho,
            c.timeouts,
            c.crosslink,
        )?
        .hash
            != c.hash
        {
            return Err(Error::Corrupt);
        }
        self.validate_descriptor()?;
        for s in [
            self.d().slot,
            self.d().vc.map(|v| v.proposal),
            self.d().pending.map(|p| p.proposal),
        ]
        .into_iter()
        .flatten()
        {
            if s.index >= 3 {
                return Err(Error::Corrupt);
            }
            let h = self.store.header(s.index).ok_or(Error::Corrupt)?;
            if h.identity(self.c()) != s.header
                || h.content.height != self.d().height
                || !h.verify(self.c())
                || self.store.body(s.index).len() > self.c().max_body
                || self.c().body_id(self.store.body(s.index)) != h.content.id
            {
                return Err(Error::Corrupt);
            }
        }
        for r in [
            self.d().entry,
            self.d().vc.map(|v| v.certificate),
            self.d().pending.map(|p| p.certificate),
        ]
        .into_iter()
        .flatten()
        {
            if r.index >= 4 {
                return Err(Error::Corrupt);
            }
            let h = self.store.region_header(r.index).ok_or(Error::Corrupt)?;
            let g = self.group(r);
            if !self.group_valid(g) || h.count != g.votes.len() {
                return Err(Error::Corrupt);
            }
            let mut hash = region_hasher(h.kind);
            for v in g.votes.iter() {
                hash_vote(&mut hash, v);
            }
            if *hash.finalize().as_bytes() != h.digest || h.digest != r.digest {
                return Err(Error::Corrupt);
            }
        }
        if let Some(s) = self.d().slot {
            if self.proposal(s).header.content.round != self.d().round {
                return Err(Error::Corrupt);
            }
        }
        if let Some(vc) = self.d().vc {
            if self.d().valid_round.is_none()
                || self.d().statuses[vc.proposal.index] != Status::Valid
                || self.group(vc.certificate).kind
                    != (CertKind::Polka {
                        round: self.d().valid_round.unwrap(),
                        id: self.proposal(vc.proposal).header.content.id,
                    })
            {
                return Err(Error::Corrupt);
            }
        } else if self.d().valid_round.is_some() {
            return Err(Error::Corrupt);
        }
        if let Some(p) = self.d().pending {
            let h = self.proposal(p.proposal).header.content;
            if self.group(p.certificate).kind
                != (CertKind::Decision {
                    round: h.round,
                    id: h.id,
                })
            {
                return Err(Error::Corrupt);
            }
        }
        self.clear_volatile();
        self.clear_rx();
        if self.d().step == Step::Decided {
            if self.d().pending.is_none() {
                return Err(Error::Corrupt);
            }
            self.decide_output(sink)?;
            return Ok(());
        }
        if self.d().step != Step::PreRound {
            self.complete_proposal()?;
            for k in [VoteKind::Enter, VoteKind::Prevote, VoteKind::Precommit] {
                self.materialize(k)?;
            }
            for k in [VoteKind::Prevote, VoteKind::Precommit] {
                if let Some(v) = self.own_current(k).copied() {
                    if !v.verify(self.c()) || v.content.signer as usize != self.me {
                        return Err(Error::Corrupt);
                    }
                    self.insert_own(v);
                    let mut d = self.d().clone();
                    if k == VoteKind::Precommit {
                        d.step = Step::Precommit;
                    } else if d.step == Step::Propose {
                        d.step = Step::Prevote;
                    }
                    self.commit(d)?;
                }
            }
            let mut d = self.d().clone();
            for t in &mut d.timers {
                if t.height != d.height || t.round != d.round {
                    *t = Timer::off(d.height, d.round);
                }
            }
            if d.step == Step::Propose && !d.timers[0].active {
                d.timers[0] = Timer {
                    active: true,
                    remaining: self.c().timeouts.at(0, d.round)?,
                    height: d.height,
                    round: d.round,
                };
            }
            self.commit(d)?;
            self.request_proposal(sink)?;
            for k in [VoteKind::Enter, VoteKind::Prevote, VoteKind::Precommit] {
                self.emit_replay(k, sink)?;
            }
            self.emit_proposal(Target::All, sink)?;
        }
        self.drive(sink)
    }
}
