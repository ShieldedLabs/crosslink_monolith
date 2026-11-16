//! Backfill the `finalizer_address_by_key` index for existing databases.
//!
//! Databases written by format 31.3.0 and later index every finalized Create target and
//! Retarget from/to [`FinalizerAddress`](zcash_primitives::bft::FinalizerAddress) as each block
//! is committed. A database from an earlier format has the column family created empty when it
//! is opened, so this upgrade rescans the staking actions of every finalized block up to the
//! initial tip and indexes them in block order, keeping the first verified address per key.
//! Blocks committed while the upgrade runs are indexed by the normal commit path.

use crossbeam_channel::{Receiver, TryRecvError};
use semver::Version;

use zebra_chain::block::Height;

use crate::{
    service::finalized_state::{DiskWriteBatch, ZebraDb},
    HashOrHeight,
};

use super::{CancelFormatChange, DiskFormatUpgrade};

/// Implements [`DiskFormatUpgrade`] for backfilling the finalizer address index.
pub struct Upgrade;

impl DiskFormatUpgrade for Upgrade {
    fn version(&self) -> Version {
        Version::new(31, 3, 0)
    }

    fn description(&self) -> &'static str {
        "add finalizer address by key index (backfill from finalized staking actions)"
    }

    #[allow(clippy::unwrap_in_result)]
    fn run(
        &self,
        initial_tip_height: Height,
        db: &ZebraDb,
        cancel_receiver: &Receiver<CancelFormatChange>,
    ) -> Result<(), CancelFormatChange> {
        // Genesis never carries staking actions (its bond batch is skipped at commit).
        for height in 1..=initial_tip_height.0 {
            check_cancelled(cancel_receiver)?;

            let Some(block) = db.block(HashOrHeight::Height(Height(height))) else {
                continue;
            };
            let mut actions = block
                .transactions
                .iter()
                .filter_map(|tx| tx.staking_action())
                .peekable();
            if actions.peek().is_none() {
                continue;
            }

            // One batch per block: the next block's first-seen check reads this one's writes.
            let mut batch = DiskWriteBatch::new();
            batch.prepare_finalizer_addresses_batch(db, actions);
            db.write_batch(batch)
                .expect("backfilling finalizer addresses should always succeed");
        }

        Ok(())
    }

    fn validate(
        &self,
        db: &ZebraDb,
        cancel_receiver: &Receiver<CancelFormatChange>,
    ) -> Result<Result<(), String>, CancelFormatChange> {
        // Every row must be keyed by its own public key and carry a valid signature: the index
        // may only ever hold addresses a key holder actually signed.
        for (key, address) in db.all_finalizer_addresses() {
            check_cancelled(cancel_receiver)?;

            if address.pub_key.0 != key {
                return Ok(Err(format!(
                    "finalizer address for key {key:02x?} is keyed under the wrong public key"
                )));
            }
            if !address.verify() {
                return Ok(Err(format!(
                    "finalizer address for key {key:02x?} does not verify"
                )));
            }
        }

        Ok(Ok(()))
    }
}

fn check_cancelled(
    cancel_receiver: &Receiver<CancelFormatChange>,
) -> Result<(), CancelFormatChange> {
    match cancel_receiver.try_recv() {
        Err(TryRecvError::Empty) => Ok(()),
        _ => Err(CancelFormatChange),
    }
}

#[cfg(test)]
mod tests {
    use zcash_primitives::bft::{finalizer_key_from_seed, FinalizerAddress, TMSig};
    use zebra_chain::parameters::Network;

    use crate::{service::finalized_state::FinalizedState, Config};

    use super::*;

    fn address(seed: &[u8]) -> FinalizerAddress {
        let (key, _) = finalizer_key_from_seed(seed);
        FinalizerAddress::create(&key)
    }

    fn validate(db: &ZebraDb) -> Result<(), String> {
        let (_cancel_sender, cancel_receiver) = crossbeam_channel::bounded(1);
        Upgrade.validate(db, &cancel_receiver).expect("not cancelled")
    }

    #[test]
    fn validate_rejects_forged_or_misfiled_addresses() {
        let state = FinalizedState::new(
            &Config::ephemeral(),
            &Network::Mainnet,
            #[cfg(feature = "elasticsearch")]
            false,
        )
        .expect("opening an ephemeral database should succeed");
        let db = &state.db;
        let (a, b) = (address(b"a"), address(b"b"));
        let insert = |key: [u8; 32], address: FinalizerAddress| {
            db.finalizer_address_by_key_cf()
                .new_batch_for_writing()
                .zs_insert(&key, &address)
                .write_batch()
                .unwrap();
        };

        assert_eq!(validate(db), Ok(()));
        insert(a.pub_key.0, a);
        assert_eq!(validate(db), Ok(()));

        insert(b.pub_key.0, FinalizerAddress { sig: TMSig([9; 64]), ..b });
        assert!(validate(db).unwrap_err().contains("does not verify"));

        insert(b.pub_key.0, a);
        assert!(validate(db).unwrap_err().contains("wrong public key"));
    }
}
