//! Tests and test methods for low-level RocksDB access.

#![allow(dead_code)]

use std::{ops::Deref, sync::atomic::Ordering};

use semver::Version;
use zebra_chain::parameters::Network;

use crate::{
    service::finalized_state::disk_db::{DiskDb, DB},
    Config,
};

// Enable older test code to automatically access the inner database via Deref coercion.
impl Deref for DiskDb {
    type Target = DB;

    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

impl DiskDb {
    /// Returns a list of column family names in this database.
    pub fn list_cf(&self) -> Result<Vec<String>, rocksdb::Error> {
        let opts = DiskDb::options();
        let path = self.path();

        rocksdb::DB::list_cf(&opts, path)
    }
}

/// Check that zs_iter_opts returns an upper bound one greater than provided inclusive end bounds.
#[test]
fn zs_iter_opts_increments_key_by_one() {
    let _init_guard = zebra_test::init();

    // TODO: add an empty key (`()` type or `[]` when serialized) test case
    let keys: [u32; 14] = [
        0,
        1,
        200,
        255,
        256,
        257,
        65535,
        65536,
        65537,
        16777215,
        16777216,
        16777217,
        16777218,
        u32::MAX,
    ];

    for key in keys {
        let (_, bytes) = DiskDb::zs_iter_bounds(&..=key.to_be_bytes().to_vec());
        let mut extra_bytes = bytes.expect("there should be an upper bound");
        let bytes = extra_bytes.split_off(extra_bytes.len() - 4);
        let upper_bound = u32::from_be_bytes(bytes.clone().try_into().expect("should be 4 bytes"));
        let expected_upper_bound = key.wrapping_add(1);

        assert_eq!(
            expected_upper_bound, upper_bound,
            "the upper bound should be 1 greater than the original key"
        );

        if expected_upper_bound == 0 {
            assert_eq!(
                extra_bytes,
                vec![1],
                "there should be an extra byte with a value of 1"
            );
        } else {
            assert_eq!(extra_bytes.len(), 0, "there should be no extra bytes");
        }
    }
}

/// Opens an ephemeral database with one test column family.
fn new_size_cache_test_db() -> DiskDb {
    DiskDb::new(
        &Config::ephemeral(),
        "cached-size-test",
        &Version::new(1, 0, 0),
        &Network::Mainnet,
        ["cached_size".to_owned()],
        false,
    )
}

/// Writes and flushes enough data for the test database to occupy disk space.
fn flush_test_data(db: &DiskDb) {
    let cf = db
        .cf_handle("cached_size")
        .expect("the test column family was configured");

    db.put_cf(cf, b"key", [0xa5; 4096])
        .expect("writing the test value should succeed");
    db.flush_cf(cf)
        .expect("flushing the test column family should succeed");
}

#[test]
fn opening_the_database_populates_the_cached_size() {
    let _init_guard = zebra_test::init();

    let db = new_size_cache_test_db();

    assert_eq!(
        db.cached_size(),
        db.size(),
        "the cache should be measured at open, so the first `getblockchaininfo` \
         does not report a size of zero"
    );
}

#[test]
fn printing_metrics_refreshes_the_cached_size() {
    let _init_guard = zebra_test::init();

    let db = new_size_cache_test_db();
    flush_test_data(&db);

    let measured_size = db.size();
    assert!(
        measured_size > 0,
        "the flushed SST file should use disk space"
    );

    // Staleness that only a refresh can correct.
    db.cached_size.store(u64::MAX, Ordering::Relaxed);

    assert_eq!(
        db.size(),
        measured_size,
        "an on-demand measurement must not return the cached estimate"
    );

    db.print_db_metrics();

    assert_eq!(
        db.cached_size(),
        measured_size,
        "printing metrics measures every column family, so it should hand the \
         result to the cache"
    );
}

#[test]
fn refreshing_the_cached_size_tracks_the_database() {
    let _init_guard = zebra_test::init();

    let db = new_size_cache_test_db();
    let empty_size = db.cached_size();

    flush_test_data(&db);
    db.refresh_cached_size();

    let grown_size = db.cached_size();
    assert!(
        grown_size > empty_size,
        "the cached size should follow the database's growth: {empty_size} -> {grown_size}"
    );
    assert_eq!(
        grown_size,
        db.size(),
        "a refreshed cache should agree with an on-demand measurement"
    );
}

/// Reports the measurement cost this cache removes from the `getblockchaininfo` path.
///
/// Not a correctness test, so it does not run by default:
///
/// ```text
/// cargo test -p zebra-state -p wallet --lib -- --ignored --nocapture size_measurement_cost
/// ```
#[test]
#[ignore = "measurement, not a correctness check"]
fn size_measurement_cost() {
    use std::time::Instant;

    use crate::service::finalized_state::STATE_COLUMN_FAMILIES_IN_CODE;

    let _init_guard = zebra_test::init();

    let db = DiskDb::new(
        &Config::ephemeral(),
        "size-cost-test",
        &Version::new(1, 0, 0),
        &Network::Mainnet,
        STATE_COLUMN_FAMILIES_IN_CODE
            .iter()
            .map(ToString::to_string),
        false,
    );

    const ITERATIONS: u32 = 200;

    let start = Instant::now();
    for _ in 0..ITERATIONS {
        std::hint::black_box(db.size());
    }
    let measured = start.elapsed() / ITERATIONS;

    let start = Instant::now();
    for _ in 0..ITERATIONS {
        std::hint::black_box(db.cached_size());
    }
    let cached = start.elapsed() / ITERATIONS;

    println!(
        "column families: {}\n  db.size():        {measured:?} per call\n  db.cached_size(): {cached:?} per call",
        STATE_COLUMN_FAMILIES_IN_CODE.len(),
    );
}
