//! `fin` database access and write methods.

use zebra_chain::block::{self, Height};

use crate::service::finalized_state::{
    disk_db::DiskWriteBatch,
    disk_format::fin::{FinKey, FinMarker},
    zebra_db::ZebraDb,
    TypedColumnFamily,
};

/// The name of the `fin` column family.
pub const CROSSLINK_FIN: &str = "crosslink_fin";

/// The type for reading `fin` from the database.
pub type CrosslinkFinCf<'cf> = TypedColumnFamily<'cf, FinKey, FinMarker>;

impl ZebraDb {
    fn crosslink_fin_cf(&self) -> CrosslinkFinCf<'_> {
        CrosslinkFinCf::new(&self.db, CROSSLINK_FIN)
            .expect("column family was created when database was created")
    }

    /// The block `fin` names, or `None` on a node that has never finalized one.
    pub fn crosslink_fin(&self) -> Option<(Height, block::Hash)> {
        self.crosslink_fin_cf().zs_get(&FinKey).map(|m| (m.height, m.hash))
    }

    /// Move `fin` to `hash` at `height`. The caller has already checked that the new marker is a
    /// descendant of the old one (FINALITY.md §3.2); this only records the decision.
    pub fn write_crosslink_fin(
        &self,
        height: Height,
        hash: block::Hash,
    ) -> Result<(), rocksdb::Error> {
        let mut batch = DiskWriteBatch::new();
        self.crosslink_fin_cf()
            .with_batch_for_writing(&mut batch)
            .zs_insert(&FinKey, &FinMarker { height, hash });
        self.db.write(batch)
    }
}
